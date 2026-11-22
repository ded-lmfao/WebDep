//! Per-app access: who can use a website, service, or database, and with which powers.
//!
//! Routes (`:scope` is `project` or `database`, `:id` an app id or `all`):
//! - `GET    /api/access/:scope/:id`           users with access plus grant options
//! - `PUT    /api/access/:scope/:id/:user_id`  give or change a user's powers on the app
//! - `DELETE /api/access/:scope/:id/:user_id`  remove the user's access to the app
//!
//! Admins manage every app. Other users need `access_manage` on the app, may only give
//! powers they hold there, may only change users whose current powers they hold, and
//! cannot change their own access or an admin's.

use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};

use super::common::*;
use crate::auth::grants::{self, Grants, ALL_POWERS, SCOPE_DATABASE, SCOPE_GLOBAL, SCOPE_PROJECT};

type ApiError = (StatusCode, String);

const MANAGE: &str = "access_manage";

fn forbidden(msg: impl Into<String>) -> ApiError {
    (StatusCode::FORBIDDEN, msg.into())
}

fn db_error(e: sqlx::Error) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {e}"))
}

/// One app (or every app of a scope when `id` is None).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AppRef {
    pub scope: &'static str,
    pub id: Option<i64>,
}

impl AppRef {
    pub fn parse(scope: &str, id: Option<i64>) -> Result<Self, ApiError> {
        let scope = match scope {
            SCOPE_PROJECT => SCOPE_PROJECT,
            SCOPE_DATABASE => SCOPE_DATABASE,
            _ => return Err((StatusCode::BAD_REQUEST, "Scope must be 'project' or 'database'.".into())),
        };
        Ok(Self { scope, id })
    }

    pub fn map_mut<'a>(&self, g: &'a mut Grants) -> &'a mut HashMap<Option<i64>, HashSet<String>> {
        if self.scope == SCOPE_DATABASE {
            &mut g.databases
        } else {
            &mut g.projects
        }
    }

    fn map<'a>(&self, g: &'a Grants) -> &'a HashMap<Option<i64>, HashSet<String>> {
        if self.scope == SCOPE_DATABASE {
            &g.databases
        } else {
            &g.projects
        }
    }
}

/// `"all"` (or JSON null) means every app of the scope; otherwise a positive id.
pub(crate) fn parse_scope_id(raw: &serde_json::Value) -> Result<Option<i64>, ApiError> {
    match raw {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) if s == "all" || s == ALL_POWERS => Ok(None),
        serde_json::Value::String(s) => s.parse::<i64>().map(Some).map_err(|_| bad_id()),
        serde_json::Value::Number(n) => n.as_i64().map(Some).ok_or_else(bad_id),
        _ => Err(bad_id()),
    }
}

fn bad_id() -> ApiError {
    (StatusCode::BAD_REQUEST, "App id must be a number or 'all'.".into())
}

/// Powers the actor may hand out on a scope: everything for admins, otherwise what the
/// actor holds there (and, for apps, only when holding `access_manage`).
fn grantable(actor: &User, scope: &str, id: Option<i64>) -> HashSet<String> {
    let catalog: HashSet<String> = grants::catalog(scope).iter().map(|s| s.to_string()).collect();
    if actor.is_admin {
        return catalog;
    }
    if scope != SCOPE_GLOBAL && id.is_none() {
        return HashSet::new();
    }
    let Some(held) = actor.grants.powers_on(scope, id) else {
        return HashSet::new();
    };
    if scope != SCOPE_GLOBAL && !(held.contains(MANAGE) || held.contains(ALL_POWERS)) {
        return HashSet::new();
    }
    if held.contains(ALL_POWERS) {
        return catalog;
    }
    catalog.intersection(&held).cloned().collect()
}

/// Rejects unknown power names and powers the actor can't give. For apps the actor also
/// needs `access_manage` even when giving no powers (bare access).
pub(crate) fn ensure_can_grant(actor: &User, scope: &str, id: Option<i64>, powers: &[String]) -> Result<(), ApiError> {
    let catalog = grants::catalog(scope);
    if let Some(p) = powers.iter().find(|p| !catalog.contains(&p.as_str())) {
        return Err((StatusCode::BAD_REQUEST, format!("Unknown permission '{p}'.")));
    }
    if actor.is_admin {
        return Ok(());
    }
    if scope != SCOPE_GLOBAL {
        if id.is_none() {
            return Err(forbidden("Only an admin can give access to every app."));
        }
        if !actor.has_any_power_on(scope, id, &[MANAGE]) {
            return Err(forbidden("You need 'Manage access' on this app to change who can use it."));
        }
    }
    let allowed = grantable(actor, scope, id);
    if let Some(p) = powers.iter().find(|p| !allowed.contains(p.as_str())) {
        return Err(forbidden(format!("You can only give permissions you have yourself (missing '{p}').")));
    }
    Ok(())
}

trait ScopedPowers {
    fn has_any_power_on(&self, scope: &str, id: Option<i64>, powers: &[&str]) -> bool;
}

impl ScopedPowers for User {
    fn has_any_power_on(&self, scope: &str, id: Option<i64>, powers: &[&str]) -> bool {
        match (scope, id) {
            (SCOPE_PROJECT, Some(pid)) => self.has_project_power(pid, powers),
            (SCOPE_DATABASE, Some(dbid)) => self.is_admin || self.grants.database_has_any(dbid, powers),
            _ => self.has_any_power(powers),
        }
    }
}

struct AppInfo {
    name: String,
    kind: &'static str,
}

async fn resolve_app(state: &AppState, app: AppRef) -> Result<AppInfo, ApiError> {
    let not_found = || (StatusCode::NOT_FOUND, "App not found".to_string());
    match (app.scope, app.id) {
        (SCOPE_PROJECT, None) => Ok(AppInfo { name: "All websites and services".into(), kind: "all-projects" }),
        (SCOPE_DATABASE, None) => Ok(AppInfo { name: "All databases".into(), kind: "all-databases" }),
        (SCOPE_PROJECT, Some(id)) => {
            let p = state
                .projects
                .get(id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .ok_or_else(not_found)?;
            let kind = if p.project_kind == "service" { "service" } else { "website" };
            Ok(AppInfo { name: p.name, kind })
        }
        (_, Some(id)) => {
            let db = state
                .databases
                .get(id)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
                .ok_or_else(not_found)?;
            Ok(AppInfo { name: db.name, kind: "database" })
        }
        _ => Err(not_found()),
    }
}

fn sorted(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccessUser {
    id: i64,
    username: String,
    display_name: String,
    email: String,
    powers: Vec<String>,
    /// Access comes from an every-app grant (managed from the user editor).
    via_all: bool,
    editable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    id: i64,
    username: String,
    display_name: String,
    /// Shown only to people who may see emails, or when the search was the exact email.
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
}

const MIN_SEARCH_LEN: usize = 2;
const MAX_CANDIDATES: usize = 8;

#[derive(Deserialize)]
pub(crate) struct AccessQuery {
    /// Name, username, or email of a person to add. Candidates are returned only for a
    /// search, so the full user list never reaches delegates.
    q: Option<String>,
}

/// Whether `u` matches the search. Email matches by substring only for actors allowed to
/// see the user list; everyone else needs the exact address, so emails can't be probed.
fn candidate_match(u: &User, q: &str, can_see_emails: bool) -> Option<Option<String>> {
    let email = u.email.to_lowercase();
    let exact_email = !email.is_empty() && email == q;
    let hit = u.username.to_lowercase().contains(q)
        || u.display_name.to_lowercase().contains(q)
        || exact_email
        || (can_see_emails && !email.is_empty() && email.contains(q));
    hit.then(|| (can_see_emails || exact_email).then(|| u.email.clone()).filter(|e| !e.is_empty()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessView {
    scope: &'static str,
    id: Option<i64>,
    name: String,
    kind: &'static str,
    catalog: Vec<&'static str>,
    grantable: Vec<String>,
    users: Vec<AccessUser>,
    candidates: Vec<Candidate>,
}

async fn authorize_app(state: &AppState, headers: &HeaderMap, scope: &str, raw_id: &str) -> Result<(User, AppRef, AppInfo), ApiError> {
    let actor = require_current_user(state, headers).await?;
    let app = AppRef::parse(scope, parse_scope_id(&serde_json::Value::String(raw_id.to_string()))?)?;
    let info = resolve_app(state, app).await?;
    if !actor.is_admin && (app.id.is_none() || !actor.has_any_power_on(app.scope, app.id, &[MANAGE])) {
        return Err(forbidden("You need 'Manage access' on this app."));
    }
    Ok((actor, app, info))
}

pub(crate) async fn get_app_access_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((scope, raw_id)): AxPath<(String, String)>,
    Query(query): Query<AccessQuery>,
) -> Result<Json<AccessView>, ApiError> {
    let (actor, app, info) = authorize_app(&state, &headers, &scope, &raw_id).await?;
    let search = query.q.map(|q| q.trim().to_lowercase()).filter(|q| q.chars().count() >= MIN_SEARCH_LEN);
    let can_see_emails = actor.has_any_power(&["users_view", "users_edit", "users_create"]);
    let allowed = grantable(&actor, app.scope, app.id);
    let users = state.auth.list_users().await.map_err(db_error)?;

    let mut with_access = Vec::new();
    let mut candidates = Vec::new();
    for u in users {
        if u.is_admin || u.id == actor.id {
            continue;
        }
        let map = app.map(&u.grants);
        let direct = map.get(&app.id);
        let via_all = app.id.is_some() && map.contains_key(&None);
        match direct {
            Some(set) => with_access.push(AccessUser {
                editable: set.is_subset(&allowed),
                powers: sorted(set),
                via_all: false,
                id: u.id,
                username: u.username,
                display_name: u.display_name,
                email: u.email,
            }),
            None if via_all => with_access.push(AccessUser {
                powers: sorted(&map[&None]),
                via_all: true,
                editable: false,
                id: u.id,
                username: u.username,
                display_name: u.display_name,
                email: u.email,
            }),
            None if !u.disabled && candidates.len() < MAX_CANDIDATES => {
                if let Some(email) = search.as_deref().and_then(|q| candidate_match(&u, q, can_see_emails)) {
                    candidates.push(Candidate { id: u.id, username: u.username, display_name: u.display_name, email });
                }
            }
            None => {}
        }
    }

    let catalog = grants::kind_catalog(info.kind);
    let mut grantable: Vec<String> = catalog.iter().filter(|p| allowed.contains(**p)).map(|p| p.to_string()).collect();
    grantable.sort();
    Ok(Json(AccessView {
        scope: app.scope,
        id: app.id,
        name: info.name,
        kind: info.kind,
        catalog: catalog.to_vec(),
        grantable,
        users: with_access,
        candidates,
    }))
}

#[derive(Deserialize)]
pub(crate) struct SetAccessPayload {
    #[serde(default)]
    powers: Vec<String>,
}

async fn load_target(state: &AppState, actor: &User, user_id: i64) -> Result<User, ApiError> {
    let target = state
        .auth
        .get_user_by_id(user_id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;
    if target.is_admin {
        return Err((StatusCode::BAD_REQUEST, "Admins already have full access.".into()));
    }
    if target.id == actor.id {
        return Err(forbidden("You can't change your own access."));
    }
    Ok(target)
}

/// A delegate may only change a user whose current powers on the app they also hold.
fn ensure_can_touch(actor: &User, app: AppRef, target: &User) -> Result<(), ApiError> {
    let Some(current) = app.map(&target.grants).get(&app.id) else {
        return Ok(());
    };
    if actor.is_admin || current.is_subset(&grantable(actor, app.scope, app.id)) {
        Ok(())
    } else {
        Err(forbidden("This user has permissions you don't have, so you can't change their access."))
    }
}

async fn log_access_change(state: &AppState, actor: &User, app: AppRef, info: &AppInfo, detail: String) {
    match (app.scope, app.id) {
        (SCOPE_PROJECT, Some(pid)) => {
            log_audit_project(state, Some(pid), &actor.username, "access.update", Some(&info.name), Some(&detail)).await
        }
        (SCOPE_DATABASE, Some(dbid)) => {
            log_audit(state, &actor.username, "access.update", Some(&format!("DB #{dbid}")), Some(&detail)).await
        }
        _ => log_audit(state, &actor.username, "access.update", Some(&info.name), Some(&detail)).await,
    }
}

pub(crate) async fn after_change(state: &AppState, user_id: i64, reason: &str) {
    state
        .auth
        .notify_revocation(crate::auth::manager::AccessRevocationEvent {
            kind: "grant_updated".to_string(),
            user_id,
            scope_type: None,
            scope_id: None,
            reason: reason.to_string(),
            ts: now(),
        })
        .await;
    state.cache.invalidate_all();
}

pub(crate) async fn set_app_access_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((scope, raw_id, user_id)): AxPath<(String, String, i64)>,
    Json(payload): Json<SetAccessPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, app, info) = authorize_app(&state, &headers, &scope, &raw_id).await?;
    let target = load_target(&state, &actor, user_id).await?;
    ensure_can_touch(&actor, app, &target)?;
    ensure_can_grant(&actor, app.scope, app.id, &payload.powers)?;

    grants::upsert(&state.db, target.id, app.scope, app.id, &payload.powers, Some(actor.id))
        .await
        .map_err(db_error)?;
    after_change(&state, target.id, "App access updated").await;
    let list = if payload.powers.is_empty() { "access only".to_string() } else { payload.powers.join(", ") };
    log_access_change(&state, &actor, app, &info, format!("Set {}'s permissions on {}: {}", target.username, info.name, list)).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) async fn remove_app_access_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((scope, raw_id, user_id)): AxPath<(String, String, i64)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (actor, app, info) = authorize_app(&state, &headers, &scope, &raw_id).await?;
    let target = load_target(&state, &actor, user_id).await?;
    ensure_can_touch(&actor, app, &target)?;

    if grants::remove(&state.db, target.id, app.scope, app.id).await.map_err(db_error)? {
        after_change(&state, target.id, "App access removed").await;
        log_access_change(&state, &actor, app, &info, format!("Removed {}'s access to {}", target.username, info.name)).await;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_with(grants: Grants) -> User {
        let mut u = User::from_db(7, "lead".into(), 0, "[]".into(), "[]".into(), 0, 0);
        u.grants = grants;
        u
    }

    fn set(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn delegates_can_only_give_what_they_hold() {
        let mut g = Grants::default();
        g.projects.insert(Some(1), set(&["logs", "deploy", MANAGE]));
        let lead = user_with(g);

        assert!(ensure_can_grant(&lead, SCOPE_PROJECT, Some(1), &strings(&["logs", "deploy"])).is_ok());
        assert!(ensure_can_grant(&lead, SCOPE_PROJECT, Some(1), &strings(&["env"])).is_err());
        assert!(ensure_can_grant(&lead, SCOPE_PROJECT, Some(1), &[]).is_ok(), "bare access is allowed");
        assert!(ensure_can_grant(&lead, SCOPE_PROJECT, Some(2), &[]).is_err(), "no manage on app 2");
        assert!(ensure_can_grant(&lead, SCOPE_PROJECT, None, &[]).is_err(), "every-app grants are admin only");
        assert!(ensure_can_grant(&lead, SCOPE_PROJECT, Some(1), &strings(&["bogus"])).is_err());
    }

    #[test]
    fn holding_powers_without_manage_cannot_delegate() {
        let mut g = Grants::default();
        g.databases.insert(Some(3), set(&["databases", "env"]));
        let u = user_with(g);
        assert!(ensure_can_grant(&u, SCOPE_DATABASE, Some(3), &strings(&["databases"])).is_err());
    }

    #[test]
    fn panel_powers_follow_the_same_rule() {
        let u = user_with(Grants { panel: set(&["users_create", "users_view"]), ..Default::default() });
        assert!(ensure_can_grant(&u, SCOPE_GLOBAL, None, &strings(&["users_view"])).is_ok());
        assert!(ensure_can_grant(&u, SCOPE_GLOBAL, None, &strings(&["users_edit"])).is_err());
    }

    #[test]
    fn delegates_cannot_touch_more_powerful_users() {
        let mut g = Grants::default();
        g.projects.insert(Some(1), set(&["logs", MANAGE]));
        let lead = user_with(g);
        let mut tg = Grants::default();
        tg.projects.insert(Some(1), set(&["logs", "env"]));
        let mut target = user_with(tg);
        target.id = 8;
        let app = AppRef { scope: SCOPE_PROJECT, id: Some(1) };
        assert!(ensure_can_touch(&lead, app, &target).is_err());
    }

    #[test]
    fn candidate_search_hides_emails_from_delegates() {
        let mut u = User::from_db(9, "priya".into(), 0, "[]".into(), "[]".into(), 0, 0);
        u.display_name = "Priya Nair".into();
        u.email = "priya@acme.io".into();
        assert_eq!(candidate_match(&u, "nair", false), Some(None), "name match, email hidden");
        assert_eq!(candidate_match(&u, "acme", false), None, "no email substring probing");
        assert_eq!(candidate_match(&u, "priya@acme.io", false), Some(Some("priya@acme.io".into())));
        assert_eq!(candidate_match(&u, "acme", true), Some(Some("priya@acme.io".into())));
    }

    #[test]
    fn scope_id_parsing() {
        assert_eq!(parse_scope_id(&serde_json::json!("all")).unwrap(), None);
        assert_eq!(parse_scope_id(&serde_json::json!(5)).unwrap(), Some(5));
        assert_eq!(parse_scope_id(&serde_json::json!("12")).unwrap(), Some(12));
        assert!(parse_scope_id(&serde_json::json!("x")).is_err());
    }
}
