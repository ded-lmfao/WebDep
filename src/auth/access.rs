use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::user::User;
use crate::db::DbPool;

pub const ACCESS_REVOKE_CHANNEL: &str = "webdep_access_revoke";

pub const KNOWN_POWERS: &[&str] = &[
    "*",
    "projects_view",
    "projects_manage",
    "deploy",
    "env",
    "services_manage",
    "files",
    "files_write",
    "domains",
    "logs",
    "backups",
    "databases",
    "databases_manage",
    "monitor",
    "audit",
    "settings_manage",
    "system_manage",
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleTemplate {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub scopes: &'static [&'static str],
    pub powers: &'static [&'static str],
}

pub const ROLE_TEMPLATES: &[RoleTemplate] = &[
    RoleTemplate {
        id: "frontend_deployer",
        name: "Frontend Deployer",
        description: "Deploy builds, view logs, inspect deployments, and view app metrics.",
        scopes: &["project", "global"],
        powers: &["projects_view", "deploy", "logs", "monitor"],
    },
    RoleTemplate {
        id: "support_read_only",
        name: "Support Read-Only",
        description: "Read-only access to projects, logs, monitoring, databases, and audit trail.",
        scopes: &["global", "project", "node", "database"],
        powers: &["projects_view", "logs", "monitor", "databases", "audit"],
    },
    RoleTemplate {
        id: "db_operator",
        name: "DB Operator",
        description: "Manage PostgreSQL databases, backups, and inspect database health.",
        scopes: &["database", "project", "global"],
        powers: &["databases", "databases_manage", "backups", "monitor"],
    },
    RoleTemplate {
        id: "node_maintainer",
        name: "Node Maintainer",
        description: "Manage services, domains, backups, logs, and monitoring on a node.",
        scopes: &["node", "global"],
        powers: &[
            "projects_view",
            "services_manage",
            "domains",
            "logs",
            "backups",
            "monitor",
        ],
    },
    RoleTemplate {
        id: "full_app_admin",
        name: "App / Resource Admin",
        description: "Full operational control within the assigned scope (app, node, or database).",
        scopes: &["project", "database", "node"],
        powers: &["*"],
    },
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccessGrant {
    pub id: i64,
    pub user_id: i64,
    pub scope_type: String,
    pub scope_id: Option<i64>,
    pub role_preset: String,
    pub powers: Vec<String>,
    pub granted_by: Option<i64>,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl AccessGrant {
    pub fn from_pg_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        let powers_json: String = row.try_get(5)?;
        let powers: Vec<String> = serde_json::from_str(&powers_json).unwrap_or_default();
        Ok(Self {
            id: row.try_get(0)?,
            user_id: row.try_get(1)?,
            scope_type: row.try_get(2)?,
            scope_id: row.try_get(3)?,
            role_preset: row.try_get(4)?,
            powers,
            granted_by: row.try_get(6)?,
            expires_at: row.try_get(7)?,
            created_at: row.try_get(8)?,
            updated_at: row.try_get(9)?,
        })
    }

    pub fn is_active(&self, now_s: i64) -> bool {
        match self.expires_at {
            Some(exp) => exp > now_s,
            None => true,
        }
    }

    pub fn has_power(&self, power: &str) -> bool {
        self.powers.iter().any(|p| p == "*" || p == power)
    }

    pub fn has_any_power(&self, candidates: &[&str]) -> bool {
        candidates.iter().any(|p| self.has_power(p))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessRevocationEvent {
    pub kind: String,
    pub user_id: i64,
    pub scope_type: Option<String>,
    pub scope_id: Option<i64>,
    pub reason: String,
    pub ts: i64,
}

impl AccessRevocationEvent {
    #[cfg(test)]
    pub fn matches_user_project(&self, user_id: i64, project_id: i64) -> bool {
        if self.user_id != user_id {
            return false;
        }
        if matches!(self.kind.as_str(), "session_revoked" | "user_disabled") {
            return true;
        }
        match self.scope_type.as_deref() {
            None | Some("global") | Some("node") => true,
            Some("project") => self.scope_id == Some(project_id),
            Some(_) => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertGrantInput {
    pub user_id: i64,
    pub scope_type: String,
    pub scope_id: Option<i64>,
    #[serde(default)]
    pub role_preset: Option<String>,
    #[serde(default)]
    pub powers: Option<Vec<String>>,
    pub expires_at: Option<i64>,
}

pub fn find_role_template(id: &str) -> Option<&'static RoleTemplate> {
    ROLE_TEMPLATES.iter().find(|t| t.id == id)
}

pub fn normalize_scope(scope_type: &str, scope_id: Option<i64>) -> Result<(&'static str, Option<i64>), String> {
    match scope_type.trim().to_ascii_lowercase().as_str() {
        "global" => Ok(("global", None)),
        "node" => match scope_id {
            Some(id) if id > 0 => Ok(("node", Some(id))),
            _ => Err("A positive scopeId is required for node grants.".to_string()),
        },
        "project" => match scope_id {
            Some(id) if id > 0 => Ok(("project", Some(id))),
            _ => Err("A positive scopeId is required for project grants.".to_string()),
        },
        "database" => match scope_id {
            Some(id) if id > 0 => Ok(("database", Some(id))),
            _ => Err("A positive scopeId is required for database grants.".to_string()),
        },
        other => Err(format!("Unsupported access scope type '{}'.", other)),
    }
}

pub fn resolve_grant_powers(
    role_preset: Option<&str>,
    raw_powers: Option<&[String]>,
) -> Result<(String, Vec<String>), String> {
    let preset_clean = role_preset.unwrap_or("custom").trim();
    let template = if preset_clean.is_empty() || preset_clean == "custom" {
        None
    } else {
        find_role_template(preset_clean)
    };

    if !preset_clean.is_empty() && preset_clean != "custom" && template.is_none() {
        return Err(format!("Unknown role preset '{}'.", preset_clean));
    }

    let powers = match raw_powers {
        Some(list) if !list.is_empty() => normalize_powers(list)?,
        _ => {
            if let Some(tpl) = template {
                tpl.powers.iter().map(|s| (*s).to_string()).collect()
            } else {
                Vec::new()
            }
        }
    };

    let preset_name = template.map(|t| t.id.to_string()).unwrap_or_else(|| "custom".to_string());
    Ok((preset_name, powers))
}

fn normalize_powers(list: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for raw in list {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !KNOWN_POWERS.contains(&trimmed) {
            return Err(format!("Unknown power '{}'.", trimmed));
        }
        if !out.iter().any(|existing: &String| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

fn find_active_grant<'a>(
    grants: &'a [AccessGrant],
    scope_type: &str,
    scope_id: Option<i64>,
    now_s: i64,
) -> Option<&'a AccessGrant> {
    grants
        .iter()
        .find(|g| g.scope_type == scope_type && g.scope_id == scope_id && g.is_active(now_s))
}

/// Evaluate whether `user` can access/view project `project_id` given active `grants`.
/// Precedence: Main Admin -> explicit `project` grant -> active `global` grant -> legacy `users` fields.
pub fn evaluate_project_access(
    user: &User,
    grants: &[AccessGrant],
    project_id: i64,
    now_s: i64,
) -> bool {
    if user.disabled {
        return false;
    }
    if user.is_main_admin {
        return true;
    }
    if let Some(explicit) = find_active_grant(grants, "project", Some(project_id), now_s) {
        return !explicit.powers.is_empty();
    }
    if let Some(global) = find_active_grant(grants, "global", None, now_s) {
        if global.powers.is_empty() {
            return false;
        }
        if global.has_power("*") {
            return true;
        }
        if user.can_access_project(project_id) {
            return true;
        }
        return user.allowed_projects.is_empty()
            && global.has_any_power(&[
                "projects_view",
                "projects_manage",
                "deploy",
                "logs",
                "services_manage",
                "domains",
                "backups",
                "env",
                "files",
                "files_write",
            ]);
    }
    user.can_access_project(project_id)
}

/// Evaluate whether `user` satisfies at least one of `required_powers` within `(scope_type, scope_id)`.
/// Precedence: Main Admin -> explicit `(scope_type, scope_id)` grant -> `global` grant -> legacy `users` fields.
pub fn evaluate_scoped_powers(
    user: &User,
    grants: &[AccessGrant],
    required_powers: &[&str],
    scope_type: &str,
    scope_id: Option<i64>,
    now_s: i64,
) -> bool {
    if user.disabled {
        return false;
    }
    if user.is_main_admin {
        return true;
    }
    if let Some(id) = scope_id {
        if let Some(explicit) = find_active_grant(grants, scope_type, Some(id), now_s) {
            return explicit.has_any_power(required_powers);
        }
        if scope_type == "project" && !evaluate_project_access(user, grants, id, now_s) {
            return false;
        }
    }
    if let Some(global) = find_active_grant(grants, "global", None, now_s) {
        return global.has_any_power(required_powers);
    }
    user.has_any_power(required_powers)
}

/// Evaluate whether `user` can access/act with `required_powers` on database `db_id`.
/// Precedence: Main Admin -> explicit `database` grant -> linked project scope (if linked) -> `global`/legacy.
pub fn evaluate_database_power(
    user: &User,
    grants: &[AccessGrant],
    required_powers: &[&str],
    db_id: i64,
    linked_project_id: Option<i64>,
    now_s: i64,
) -> bool {
    if user.disabled {
        return false;
    }
    if user.is_main_admin {
        return true;
    }
    if let Some(explicit_db) = find_active_grant(grants, "database", Some(db_id), now_s) {
        return explicit_db.has_any_power(required_powers);
    }
    if let Some(pid) = linked_project_id {
        return evaluate_project_access(user, grants, pid, now_s)
            && evaluate_scoped_powers(user, grants, required_powers, "project", Some(pid), now_s);
    }
    let has_power = evaluate_scoped_powers(user, grants, required_powers, "global", None, now_s);
    if !has_power {
        return false;
    }
    if let Some(global) = find_active_grant(grants, "global", None, now_s) {
        return user.all_projects || global.has_any_power(&["*", "databases_manage"]);
    }
    user.all_projects || user.has_power("databases_manage")
}

/// Whether `user` can access the databases list endpoint at all (global power OR any active database/project grant).
pub fn can_list_databases(user: &User, grants: &[AccessGrant], now_s: i64) -> bool {
    if user.disabled {
        return false;
    }
    if user.is_main_admin {
        return true;
    }
    if evaluate_scoped_powers(user, grants, &["databases", "databases_manage"], "global", None, now_s) {
        return true;
    }
    grants.iter().any(|g| {
        g.is_active(now_s)
            && matches!(g.scope_type.as_str(), "database" | "project")
            && g.has_any_power(&["databases", "databases_manage"])
    })
}

/// Whether `user` holds any active grant enabling database credential viewing for `db_id`/`linked_project_id`.
pub fn can_view_db_credentials(
    user: &User,
    grants: &[AccessGrant],
    db_id: i64,
    linked_project_id: Option<i64>,
    now_s: i64,
) -> bool {
    evaluate_database_power(
        user,
        grants,
        &["databases_manage", "env"],
        db_id,
        linked_project_id,
        now_s,
    )
}

const SELECT_GRANT_SQL: &str = "SELECT id, user_id, scope_type, scope_id, role_preset, powers_json, granted_by, expires_at, created_at, updated_at FROM access_grants";

pub async fn list_all_grants(db: &DbPool) -> Result<Vec<AccessGrant>, sqlx::Error> {
    let sql = format!("{SELECT_GRANT_SQL} ORDER BY user_id ASC, scope_type ASC, COALESCE(scope_id, 0) ASC");
    let rows = sqlx::query(&sql).fetch_all(db).await?;
    rows.iter().map(AccessGrant::from_pg_row).collect()
}

pub async fn list_user_grants(db: &DbPool, user_id: i64) -> Result<Vec<AccessGrant>, sqlx::Error> {
    let sql = format!("{SELECT_GRANT_SQL} WHERE user_id = $1 ORDER BY scope_type ASC, COALESCE(scope_id, 0) ASC");
    let rows = sqlx::query(&sql).bind(user_id).fetch_all(db).await?;
    rows.iter().map(AccessGrant::from_pg_row).collect()
}

pub async fn list_active_user_grants(
    db: &DbPool,
    user_id: i64,
    now_s: i64,
) -> Result<Vec<AccessGrant>, sqlx::Error> {
    let sql = format!(
        "{SELECT_GRANT_SQL} WHERE user_id = $1 AND (expires_at IS NULL OR expires_at > $2) ORDER BY scope_type ASC, COALESCE(scope_id, 0) ASC"
    );
    let rows = sqlx::query(&sql).bind(user_id).bind(now_s).fetch_all(db).await?;
    rows.iter().map(AccessGrant::from_pg_row).collect()
}

pub async fn list_resource_grants(
    db: &DbPool,
    scope_type: &str,
    scope_id: Option<i64>,
) -> Result<Vec<AccessGrant>, sqlx::Error> {
    let sql = if scope_id.is_some() {
        format!("{SELECT_GRANT_SQL} WHERE scope_type = $1 AND scope_id = $2 ORDER BY user_id ASC")
    } else {
        format!("{SELECT_GRANT_SQL} WHERE scope_type = $1 AND scope_id IS NULL ORDER BY user_id ASC")
    };
    let mut query = sqlx::query(&sql).bind(scope_type);
    if let Some(id) = scope_id {
        query = query.bind(id);
    }
    let rows = query.fetch_all(db).await?;
    rows.iter().map(AccessGrant::from_pg_row).collect()
}

pub async fn get_grant_by_id(db: &DbPool, id: i64) -> Result<Option<AccessGrant>, sqlx::Error> {
    let sql = format!("{SELECT_GRANT_SQL} WHERE id = $1");
    let row = sqlx::query(&sql).bind(id).fetch_optional(db).await?;
    match row {
        Some(r) => Ok(Some(AccessGrant::from_pg_row(&r)?)),
        None => Ok(None),
    }
}

pub async fn upsert_grant(
    db: &DbPool,
    input: &UpsertGrantInput,
    granted_by: Option<i64>,
    now_s: i64,
) -> Result<AccessGrant, String> {
    let (scope_type, scope_id) = normalize_scope(&input.scope_type, input.scope_id)?;
    let (role_preset, powers) = resolve_grant_powers(
        input.role_preset.as_deref(),
        input.powers.as_deref(),
    )?;
    if let Some(exp) = input.expires_at {
        if exp <= now_s {
            return Err("Expiration timestamp must be in the future.".to_string());
        }
    }
    let powers_json = serde_json::to_string(&powers).map_err(|e| e.to_string())?;

    let row = sqlx::query(
        "INSERT INTO access_grants(user_id, scope_type, scope_id, role_preset, powers_json, granted_by, expires_at, created_at, updated_at) \
         VALUES($1, $2, $3, $4, $5, $6, $7, $8, $8) \
         ON CONFLICT (user_id, scope_type, COALESCE(scope_id, 0)) DO UPDATE SET \
             role_preset = EXCLUDED.role_preset, \
             powers_json = EXCLUDED.powers_json, \
             granted_by = EXCLUDED.granted_by, \
             expires_at = EXCLUDED.expires_at, \
             updated_at = EXCLUDED.updated_at \
         RETURNING id, user_id, scope_type, scope_id, role_preset, powers_json, granted_by, expires_at, created_at, updated_at",
    )
    .bind(input.user_id)
    .bind(scope_type)
    .bind(scope_id)
    .bind(&role_preset)
    .bind(&powers_json)
    .bind(granted_by)
    .bind(input.expires_at)
    .bind(now_s)
    .fetch_one(db)
    .await
    .map_err(|e| format!("Failed to save access grant: {e}"))?;

    AccessGrant::from_pg_row(&row).map_err(|e| format!("Failed to decode grant: {e}"))
}

pub async fn delete_grant_by_id(db: &DbPool, id: i64) -> Result<Option<AccessGrant>, sqlx::Error> {
    let sql = format!(
        "DELETE FROM access_grants WHERE id = $1 RETURNING id, user_id, scope_type, scope_id, role_preset, powers_json, granted_by, expires_at, created_at, updated_at"
    );
    let row = sqlx::query(&sql).bind(id).fetch_optional(db).await?;
    match row {
        Some(r) => Ok(Some(AccessGrant::from_pg_row(&r)?)),
        None => Ok(None),
    }
}

pub async fn delete_expired_grants(db: &DbPool, now_s: i64) -> Result<Vec<AccessGrant>, sqlx::Error> {
    let rows = sqlx::query(
        "DELETE FROM access_grants WHERE expires_at IS NOT NULL AND expires_at <= $1 \
         RETURNING id, user_id, scope_type, scope_id, role_preset, powers_json, granted_by, expires_at, created_at, updated_at",
    )
    .bind(now_s)
    .fetch_all(db)
    .await?;
    rows.iter().map(AccessGrant::from_pg_row).collect()
}

pub async fn publish_revocation(db: &DbPool, event: &AccessRevocationEvent) {
    if let Ok(payload) = serde_json::to_string(event) {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(ACCESS_REVOKE_CHANNEL)
            .bind(&payload)
            .execute(db)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::user::User;

    fn mk_user(id: i64, main_admin: bool, all_projects: bool, allowed: Vec<i64>, perms: Vec<&str>) -> User {
        User {
            id,
            username: format!("user{id}"),
            disabled: false,
            permissions: perms.into_iter().map(String::from).collect(),
            allowed_projects: allowed,
            all_projects,
            is_main_admin: main_admin,
            totp_enabled: false,
            presence: "online".to_string(),
            created_at: 1_700_000_000,
        }
    }

    fn mk_grant(
        user_id: i64,
        scope_type: &str,
        scope_id: Option<i64>,
        powers: Vec<&str>,
        expires_at: Option<i64>,
    ) -> AccessGrant {
        AccessGrant {
            id: 1,
            user_id,
            scope_type: scope_type.to_string(),
            scope_id,
            role_preset: "custom".to_string(),
            powers: powers.into_iter().map(String::from).collect(),
            granted_by: Some(1),
            expires_at,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[test]
    fn test_precedence_explicit_overrides_global_and_legacy() {
        let now = 2_000_000_000;
        let user = mk_user(2, false, true, vec![], vec!["deploy", "logs", "env"]);
        let grants = vec![
            mk_grant(2, "global", None, vec!["deploy", "logs"], None),
            // Explicit downgrade on project 10: logs only
            mk_grant(2, "project", Some(10), vec!["projects_view", "logs"], None),
        ];

        assert!(evaluate_project_access(&user, &grants, 10, now));
        assert!(evaluate_scoped_powers(&user, &grants, &["logs"], "project", Some(10), now));
        // Even though global + legacy grant deploy & env, project 10 explicit grant restricts it!
        assert!(!evaluate_scoped_powers(&user, &grants, &["deploy"], "project", Some(10), now));
        assert!(!evaluate_scoped_powers(&user, &grants, &["env"], "project", Some(10), now));

        // Project 20 falls back to global grant (has deploy/logs, not env from legacy)
        assert!(evaluate_scoped_powers(&user, &grants, &["deploy"], "project", Some(20), now));
        assert!(!evaluate_scoped_powers(&user, &grants, &["env"], "project", Some(20), now));
    }

    #[test]
    fn test_explicit_grant_unlocks_unlisted_project_and_expires() {
        let now = 2_000_000_000;
        let user = mk_user(3, false, false, vec![1], vec!["projects_view"]);
        let grants = vec![
            mk_grant(3, "project", Some(42), vec!["projects_view", "deploy"], Some(now + 300)),
            mk_grant(3, "project", Some(99), vec!["projects_view", "deploy"], Some(now - 1)),
        ];

        assert!(evaluate_project_access(&user, &grants, 42, now));
        assert!(evaluate_scoped_powers(&user, &grants, &["deploy"], "project", Some(42), now));
        // Project 1 still allowed via legacy, with legacy permissions
        assert!(evaluate_project_access(&user, &grants, 1, now));
        assert!(!evaluate_scoped_powers(&user, &grants, &["deploy"], "project", Some(1), now));
        // Expired grant on project 99 is ignored
        assert!(!evaluate_project_access(&user, &grants, 99, now));
        assert!(!evaluate_scoped_powers(&user, &grants, &["deploy"], "project", Some(99), now));
    }

    #[test]
    fn test_database_precedence_and_main_admin_bypass() {
        let now = 2_000_000_000;
        let user = mk_user(4, false, false, vec![], vec![]);
        let grants = vec![mk_grant(4, "database", Some(7), vec!["databases", "databases_manage"], None)];

        assert!(can_list_databases(&user, &grants, now));
        assert!(evaluate_database_power(&user, &grants, &["databases_manage"], 7, None, now));
        assert!(!evaluate_database_power(&user, &grants, &["databases"], 8, None, now));

        let admin = mk_user(1, true, false, vec![], vec![]);
        assert!(evaluate_database_power(&admin, &[], &["databases_manage"], 999, None, now));
    }

    #[test]
    fn test_revocation_event_matching() {
        let ev = AccessRevocationEvent {
            kind: "grant_revoked".to_string(),
            user_id: 5,
            scope_type: Some("project".to_string()),
            scope_id: Some(12),
            reason: "revoked".to_string(),
            ts: 100,
        };
        assert!(ev.matches_user_project(5, 12));
        assert!(!ev.matches_user_project(5, 13));
        assert!(!ev.matches_user_project(6, 12));
    }

    #[test]
    fn test_resolve_role_template_powers() {
        let (preset, powers) = resolve_grant_powers(Some("frontend_deployer"), None).unwrap();
        assert_eq!(preset, "frontend_deployer");
        assert!(powers.contains(&"deploy".to_string()));
        assert!(!powers.contains(&"env".to_string()));
        assert!(resolve_grant_powers(Some("invalid_preset"), None).is_err());
    }

    #[tokio::test]
    async fn test_pg_upsert_list_expire_and_delete_grants() {
        let pool = crate::pg_control::test_db::isolated_pool().await;

        let uid1: i64 = sqlx::query_scalar(
            "INSERT INTO users(username, password_hash, permissions, allowed_projects, created_at) \
             VALUES('alice_access_test', 'h', '[]', '[]', 1700000000) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let now = 1_700_000_000i64;
        let g = upsert_grant(
            &pool,
            &UpsertGrantInput {
                user_id: uid1,
                scope_type: "project".to_string(),
                scope_id: Some(10),
                role_preset: Some("frontend_deployer".to_string()),
                powers: None,
                expires_at: Some(now + 100),
            },
            None,
            now,
        )
        .await
        .unwrap();
        assert_eq!(g.role_preset, "frontend_deployer");
        assert_eq!(g.scope_type, "project");
        assert_eq!(g.scope_id, Some(10));

        let g2 = upsert_grant(
            &pool,
            &UpsertGrantInput {
                user_id: uid1,
                scope_type: "project".to_string(),
                scope_id: Some(10),
                role_preset: Some("support_read_only".to_string()),
                powers: None,
                expires_at: Some(now + 50),
            },
            None,
            now + 10,
        )
        .await
        .unwrap();
        assert_eq!(g2.id, g.id);
        assert_eq!(g2.role_preset, "support_read_only");
        assert_eq!(list_user_grants(&pool, uid1).await.unwrap().len(), 1);

        let purged = delete_expired_grants(&pool, now + 60).await.unwrap();
        assert_eq!(purged.len(), 1);
        assert!(list_user_grants(&pool, uid1).await.unwrap().is_empty());
    }
}
