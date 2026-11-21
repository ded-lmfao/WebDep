//! Account administration: list, create, edit, delete users, and reset 2FA.
//!
//! Who may do what:
//! - admins (main admin or role `admin`) may do everything below, except that only the
//!   main admin may create admins, change roles, or edit the main admin;
//! - users holding panel powers may act within them: `users_view` lists accounts,
//!   `users_create` creates them, `users_edit` edits or disables non-admin accounts;
//! - nobody can give powers they do not hold (see `access::ensure_can_grant`).

use std::collections::HashSet;

use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;

use super::access::{ensure_can_grant, parse_scope_id, AppRef};
use super::common::*;
use crate::auth::grants::{self, PANEL_ACCOUNT_POWERS, SCOPE_GLOBAL};
use crate::auth::hash_password;

type ApiError = (StatusCode, String);

const USER_LIST_POWERS: &[&str] = &["users_view", "users_create", "users_edit", "requests_review"];
pub(crate) const MIN_PASSWORD_LEN: usize = 8;

fn bad(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn forbidden(msg: impl Into<String>) -> ApiError {
    (StatusCode::FORBIDDEN, msg.into())
}

fn db_error(e: sqlx::Error) -> ApiError {
    if let sqlx::Error::Database(ref d) = e {
        if d.is_unique_violation() {
            return (StatusCode::CONFLICT, "That username or email is already in use.".into());
        }
    }
    (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {e}"))
}

pub(crate) fn validate_username(raw: &str) -> Result<String, ApiError> {
    let u = raw.trim();
    let ok = (3..=32).contains(&u.len())
        && u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        return Err(bad("Username must be 3-32 characters: letters, numbers, dot, dash, or underscore."));
    }
    Ok(u.to_string())
}

/// Lowercased email, or None when blank. Rejects obviously malformed addresses.
pub(crate) fn validate_email(raw: &str) -> Result<Option<String>, ApiError> {
    let e = raw.trim().to_lowercase();
    if e.is_empty() {
        return Ok(None);
    }
    let valid = e.len() <= 254
        && !e.chars().any(char::is_whitespace)
        && e.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
        });
    if !valid {
        return Err(bad("Enter a valid email address."));
    }
    Ok(Some(e))
}

pub(crate) fn validate_display_name(raw: &str) -> Result<String, ApiError> {
    let n = raw.trim();
    if n.chars().count() > 80 || n.chars().any(char::is_control) {
        return Err(bad("Name must be at most 80 characters."));
    }
    Ok(n.to_string())
}

fn validate_password(raw: &str) -> Result<(), ApiError> {
    if raw.len() < MIN_PASSWORD_LEN {
        return Err(bad(format!("Password must be at least {MIN_PASSWORD_LEN} characters.")));
    }
    Ok(())
}

fn validate_panel_powers(powers: &[String]) -> Result<(), ApiError> {
    match powers.iter().find(|p| !PANEL_ACCOUNT_POWERS.contains(&p.as_str())) {
        Some(p) => Err(bad(format!("Unknown panel permission '{p}'."))),
        None => Ok(()),
    }
}

async fn notify_access_changed(state: &AppState, user_id: i64, reason: &str) {
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

pub(crate) async fn list_users_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<User>>, ApiError> {
    let actor = require_current_user(&state, &headers).await?;
    if !actor.has_any_power(USER_LIST_POWERS) {
        return Err(forbidden("You don't have permission to view users."));
    }
    let list = state.auth.list_users().await.map_err(db_error)?;
    Ok(Json(list))
}

#[derive(Deserialize)]
pub(crate) struct GrantInput {
    scope: String,
    /// App id, or `"all"` for every app of that scope (admins only).
    id: serde_json::Value,
    #[serde(default)]
    powers: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateUserPayload {
    username: String,
    password: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    display_name: String,
    #[serde(default = "default_true")]
    must_change_password: bool,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    panel_powers: Vec<String>,
    #[serde(default)]
    grants: Vec<GrantInput>,
}

fn default_true() -> bool {
    true
}

pub(crate) struct NewAccount<'a> {
    pub username: &'a str,
    pub email: Option<&'a str>,
    pub display_name: &'a str,
    pub password_hash: &'a str,
    pub role: &'a str,
    pub must_change_password: bool,
}

/// Inserts a user row. Legacy permission columns are left empty: access lives in grants.
pub(crate) async fn insert_account(
    conn: &mut sqlx::PgConnection,
    acc: &NewAccount<'_>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO users(username, password_hash, disabled, permissions, allowed_projects, created_at, email, display_name, role, must_change_password)
         VALUES($1, $2, FALSE, '[]', '[]', $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(acc.username)
    .bind(acc.password_hash)
    .bind(now())
    .bind(acc.email)
    .bind(acc.display_name)
    .bind(acc.role)
    .bind(acc.must_change_password)
    .fetch_one(conn)
    .await
}

pub(crate) async fn create_user_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateUserPayload>,
) -> Result<Json<User>, ApiError> {
    let actor = require_current_user(&state, &headers).await?;
    if !actor.has_power("users_create") {
        return Err(forbidden("You don't have permission to create accounts."));
    }

    let username = validate_username(&payload.username)?;
    let email = validate_email(&payload.email)?;
    let display_name = validate_display_name(&payload.display_name)?;
    validate_password(&payload.password)?;

    let role = payload.role.as_deref().unwrap_or("user");
    match role {
        "user" => {}
        "admin" if actor.is_main_admin => {}
        "admin" => return Err(forbidden("Only the main admin can create admins.")),
        _ => return Err(bad("Role must be 'admin' or 'user'.")),
    }

    validate_panel_powers(&payload.panel_powers)?;
    ensure_can_grant(&actor, SCOPE_GLOBAL, None, &payload.panel_powers)?;
    let mut app_grants = Vec::with_capacity(payload.grants.len());
    for g in &payload.grants {
        let app = AppRef::parse(&g.scope, parse_scope_id(&g.id)?)?;
        ensure_can_grant(&actor, app.scope, app.id, &g.powers)?;
        app_grants.push((app, g.powers.clone()));
    }

    let hashed = hash_password(&payload.password).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let mut tx = state.db.begin().await.map_err(db_error)?;
    let id = insert_account(
        &mut tx,
        &NewAccount {
            username: &username,
            email: email.as_deref(),
            display_name: &display_name,
            password_hash: &hashed,
            role,
            must_change_password: payload.must_change_password,
        },
    )
    .await
    .map_err(db_error)?;
    if role != "admin" {
        let mut set = grants::Grants {
            panel: payload.panel_powers.iter().cloned().collect(),
            ..Default::default()
        };
        for (app, powers) in app_grants {
            app.map_mut(&mut set).insert(app.id, powers.into_iter().collect());
        }
        grants::replace(&mut tx, id, &set, Some(actor.id)).await.map_err(db_error)?;
    }
    tx.commit().await.map_err(db_error)?;

    log_audit(&state, &actor.username, "user.create", Some(&username), Some(&format!("Created {role} account"))).await;
    state.cache.invalidate_all();

    let user = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "User not found after create".to_string()))?;
    Ok(Json(user))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateUserPayload {
    disabled: Option<bool>,
    password: Option<String>,
    email: Option<String>,
    display_name: Option<String>,
    must_change_password: Option<bool>,
    role: Option<String>,
    panel_powers: Option<Vec<String>>,
}

pub(crate) async fn update_user_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<UpdateUserPayload>,
) -> Result<Json<User>, ApiError> {
    let actor = require_current_user(&state, &headers).await?;
    let target = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;

    if !actor.has_power("users_edit") {
        return Err(forbidden("You don't have permission to edit users."));
    }
    if target.is_main_admin && !actor.is_main_admin {
        return Err(forbidden("Only the main admin can modify the main admin account."));
    }
    if target.is_admin && !actor.is_main_admin && target.id != actor.id {
        return Err(forbidden("Only the main admin can change another admin's account."));
    }
    if target.id == actor.id && !actor.is_admin {
        return Err(forbidden("Use My account to change your own details."));
    }

    let mut changes: Vec<&str> = Vec::new();
    let mut revoke_reason: Option<&str> = None;

    if let Some(pwd) = payload.password.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        validate_password(pwd)?;
        let hashed = hash_password(pwd).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
            .bind(hashed)
            .bind(id)
            .execute(&state.db)
            .await
            .map_err(db_error)?;
        if target.is_main_admin {
            crate::config::Config::sync_dotenv_admin(Some(&target.username), Some(pwd));
        }
        changes.push("password");
        if target.id != actor.id {
            revoke_reason = Some("Password reset by an administrator");
        }
    }

    if let Some(email) = payload
        .email
        .as_deref()
        .map(validate_email)
        .transpose()?
        .filter(|e| e.as_deref().unwrap_or("") != target.email)
    {
        sqlx::query("UPDATE users SET email = $1 WHERE id = $2")
            .bind(email)
            .bind(id)
            .execute(&state.db)
            .await
            .map_err(db_error)?;
        changes.push("email");
    }

    if let Some(name) = payload
        .display_name
        .as_deref()
        .map(validate_display_name)
        .transpose()?
        .filter(|n| *n != target.display_name)
    {
        sqlx::query("UPDATE users SET display_name = $1 WHERE id = $2")
            .bind(name)
            .bind(id)
            .execute(&state.db)
            .await
            .map_err(db_error)?;
        changes.push("name");
    }

    if let Some(flag) = payload
        .must_change_password
        .filter(|f| !target.is_main_admin && *f != target.must_change_password)
    {
        sqlx::query("UPDATE users SET must_change_password = $1 WHERE id = $2")
            .bind(flag)
            .bind(id)
            .execute(&state.db)
            .await
            .map_err(db_error)?;
        changes.push("password policy");
    }

    if let Some(disabled) = payload.disabled.filter(|d| !target.is_main_admin && *d != target.disabled) {
        if target.id == actor.id {
            return Err(bad("You can't disable your own account."));
        }
        sqlx::query("UPDATE users SET disabled = $1 WHERE id = $2")
            .bind(disabled)
            .bind(id)
            .execute(&state.db)
            .await
            .map_err(db_error)?;
        changes.push(if disabled { "disabled" } else { "enabled" });
        if disabled {
            revoke_reason = Some("User account disabled");
        }
    }

    if let Some(role) = payload.role.as_deref().filter(|r| *r != target.role) {
        if !actor.is_main_admin {
            return Err(forbidden("Only the main admin can promote or demote admins."));
        }
        if target.is_main_admin {
            return Err(bad("The main admin's role can't be changed."));
        }
        if role != "admin" && role != "user" {
            return Err(bad("Role must be 'admin' or 'user'."));
        }
        sqlx::query("UPDATE users SET role = $1 WHERE id = $2")
            .bind(role)
            .bind(id)
            .execute(&state.db)
            .await
            .map_err(db_error)?;
        changes.push(if role == "admin" { "promoted to admin" } else { "demoted to user" });
    }

    if let Some(powers) = payload.panel_powers.as_ref().filter(|_| !target.is_admin) {
        validate_panel_powers(powers)?;
        let current = &target.grants.panel;
        let editable: HashSet<&str> = PANEL_ACCOUNT_POWERS.iter().copied().collect();
        // A delegate may only add or remove account powers they hold themselves.
        let touched: Vec<String> = editable
            .iter()
            .filter(|p| current.contains(**p) != powers.iter().any(|x| x == **p))
            .map(|p| p.to_string())
            .collect();
        ensure_can_grant(&actor, SCOPE_GLOBAL, None, &touched)?;
        // Panel powers outside the editor (legacy settings/system/app copies) are kept.
        let mut next: Vec<String> = current.iter().filter(|p| !editable.contains(p.as_str())).cloned().collect();
        next.extend(powers.iter().cloned());
        if next.is_empty() {
            grants::remove(&state.db, id, SCOPE_GLOBAL, None).await.map_err(db_error)?;
        } else {
            grants::upsert(&state.db, id, SCOPE_GLOBAL, None, &next, Some(actor.id)).await.map_err(db_error)?;
        }
        changes.push("panel permissions");
    }

    if let Some(reason) = revoke_reason {
        state.auth.revoke_all_user_sessions(id, reason).await;
    }
    notify_access_changed(&state, id, "User account updated").await;
    if !changes.is_empty() {
        log_audit(&state, &actor.username, "user.update", Some(&target.username), Some(&format!("Changed: {}", changes.join(", ")))).await;
    }

    let updated = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;
    Ok(Json(updated))
}

pub(crate) async fn delete_user_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let target = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;

    if target.is_main_admin {
        return Err(bad("The main admin account can't be deleted."));
    }
    if target.id == actor.id {
        return Err(bad("You can't delete your own account."));
    }
    if target.is_admin && !actor.is_main_admin {
        return Err(forbidden("Only the main admin can delete an admin."));
    }

    state.auth.revoke_all_user_sessions(id, "User account deleted").await;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(db_error)?;

    log_audit(&state, &actor.username, "user.delete", Some(&target.username), Some("Deleted user account")).await;
    state.cache.invalidate_all();
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) async fn admin_reset_user_2fa_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let target = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found".to_string()))?;
    if target.is_admin && !actor.is_main_admin {
        return Err(forbidden("Only the main admin can reset an admin's 2FA."));
    }

    sqlx::query(
        "UPDATE users SET totp_secret = NULL, totp_pending_secret = NULL, totp_enabled = FALSE, totp_backup_codes = '[]' WHERE id = $1",
    )
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(db_error)?;

    state.auth.invalidate_sessions();
    state.cache.invalidate_all();
    log_audit(&state, &actor.username, "user.update", Some(&target.username), Some("Reset two-factor authentication")).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_usernames_and_emails() {
        assert!(validate_username("dev.ops-1").is_ok());
        assert!(validate_username("ab").is_err());
        assert!(validate_username("has space").is_err());
        assert_eq!(validate_email(" Sam@Example.COM ").unwrap().as_deref(), Some("sam@example.com"));
        assert_eq!(validate_email("").unwrap(), None);
        assert!(validate_email("nope").is_err());
        assert!(validate_email("a@b").is_err());
        assert!(validate_email("a b@c.io").is_err());
    }
}
