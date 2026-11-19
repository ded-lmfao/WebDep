//! Per-user access grants stored in `access_grants`.
//!
//! A grant row gives a user a set of powers on one scope:
//! - `global`   (scope_id NULL): panel-level powers (settings, system, account admin…)
//! - `project`  (scope_id = project id, or NULL for every project): websites and services
//! - `database` (scope_id = database id, or NULL for every database)
//!
//! Having a grant row for an app is what gives access to that app, even with an empty
//! power list. `"*"` inside a power list means every power for that scope.

use std::collections::{HashMap, HashSet};

use sqlx::{PgConnection, Row};

use crate::db::DbPool;

pub const ALL_POWERS: &str = "*";
const LEGACY_MIGRATION_META_KEY: &str = "legacy_grants_migrated_at";

pub const SCOPE_GLOBAL: &str = "global";
pub const SCOPE_PROJECT: &str = "project";
pub const SCOPE_DATABASE: &str = "database";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grants {
    pub panel: HashSet<String>,
    /// `None` key = applies to every project, including ones created later.
    pub projects: HashMap<Option<i64>, HashSet<String>>,
    /// `None` key = applies to every database, including ones created later.
    pub databases: HashMap<Option<i64>, HashSet<String>>,
}

fn set_has_any(set: &HashSet<String>, powers: &[&str]) -> bool {
    set.contains(ALL_POWERS) || powers.iter().any(|p| set.contains(*p))
}

fn scoped_has_any(map: &HashMap<Option<i64>, HashSet<String>>, id: i64, powers: &[&str]) -> bool {
    [Some(id), None]
        .iter()
        .filter_map(|k| map.get(k))
        .any(|set| set_has_any(set, powers))
}

impl Grants {
    pub fn panel_has_any(&self, powers: &[&str]) -> bool {
        set_has_any(&self.panel, powers)
    }

    pub fn can_access_project(&self, project_id: i64) -> bool {
        self.projects.contains_key(&Some(project_id)) || self.projects.contains_key(&None)
    }

    pub fn project_has_any(&self, project_id: i64, powers: &[&str]) -> bool {
        scoped_has_any(&self.projects, project_id, powers)
    }

    pub fn database_has_any(&self, database_id: i64, powers: &[&str]) -> bool {
        scoped_has_any(&self.databases, database_id, powers)
    }

    /// Converts the pre-grants permission model (one global power list plus an allowed
    /// project list) into grants that authorize exactly the same requests:
    /// - every power stays available for panel-level (unscoped) checks;
    /// - each allowed project (or every project for `*`) gets the same powers;
    /// - databases not linked to a project were reachable by "all projects" users and by
    ///   holders of `databases_manage`, so those users get an every-database grant.
    pub fn from_legacy(permissions: &[String], all_projects: bool, allowed_projects: &[i64]) -> Self {
        let powers: HashSet<String> = permissions.iter().cloned().collect();
        let mut out = Grants {
            panel: powers.clone(),
            ..Default::default()
        };
        if all_projects {
            out.projects.insert(None, powers.clone());
        } else {
            for pid in allowed_projects {
                out.projects.insert(Some(*pid), powers.clone());
            }
        }
        if all_projects || set_has_any(&powers, &["databases_manage"]) {
            out.databases.insert(None, powers);
        }
        out
    }

    fn rows(&self) -> Vec<(&'static str, Option<i64>, &HashSet<String>)> {
        let mut rows = Vec::new();
        if !self.panel.is_empty() {
            rows.push((SCOPE_GLOBAL, None, &self.panel));
        }
        for (id, set) in &self.projects {
            rows.push((SCOPE_PROJECT, *id, set));
        }
        for (id, set) in &self.databases {
            rows.push((SCOPE_DATABASE, *id, set));
        }
        rows
    }
}

/// Parses the legacy `users.permissions` JSON array. Invalid JSON yields no powers.
pub fn parse_legacy_permissions(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

/// Parses the legacy `users.allowed_projects` JSON array into (all_projects, ids).
/// Invalid JSON is treated as "all projects", matching the original behaviour.
pub fn parse_legacy_projects(json: &str) -> (bool, Vec<i64>) {
    let Ok(vals) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return (true, Vec::new());
    };
    let mut all = false;
    let mut ids = Vec::new();
    for v in vals {
        if v.as_str() == Some(ALL_POWERS) {
            all = true;
        } else if let Some(pid) = v.as_i64() {
            ids.push(pid);
        } else if let Some(pid) = v.as_str().and_then(|s| s.parse::<i64>().ok()) {
            ids.push(pid);
        }
    }
    (all, ids)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Loads the unexpired grants of one user.
pub async fn load(db: &DbPool, user_id: i64) -> Result<Grants, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT scope_type, scope_id, powers_json FROM access_grants
         WHERE user_id = $1 AND (expires_at IS NULL OR expires_at > $2)",
    )
    .bind(user_id)
    .bind(now_ms())
    .fetch_all(db)
    .await?;

    let mut out = Grants::default();
    for row in rows {
        let scope: String = row.try_get(0)?;
        let scope_id: Option<i64> = row.try_get(1)?;
        let powers: HashSet<String> = parse_legacy_permissions(&row.try_get::<String, _>(2)?)
            .into_iter()
            .collect();
        match scope.as_str() {
            SCOPE_GLOBAL => out.panel.extend(powers),
            SCOPE_PROJECT => out.projects.entry(scope_id).or_default().extend(powers),
            SCOPE_DATABASE => out.databases.entry(scope_id).or_default().extend(powers),
            _ => {}
        }
    }
    Ok(out)
}

/// Replaces every grant of `user_id` with `grants`. Run inside a transaction.
pub async fn replace(
    conn: &mut PgConnection,
    user_id: i64,
    grants: &Grants,
    granted_by: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM access_grants WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    let ts = now_ms();
    for (scope, scope_id, set) in grants.rows() {
        let mut powers: Vec<&String> = set.iter().collect();
        powers.sort();
        sqlx::query(
            "INSERT INTO access_grants(user_id, scope_type, scope_id, role_preset, powers_json, granted_by, created_at, updated_at)
             VALUES($1, $2, $3, 'custom', $4, $5, $6, $6)",
        )
        .bind(user_id)
        .bind(scope)
        .bind(scope_id)
        .bind(serde_json::to_string(&powers).unwrap_or_else(|_| "[]".into()))
        .bind(granted_by)
        .bind(ts)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Rebuilds a user's grants from legacy permission JSON (used while the user editor still
/// speaks the legacy format).
pub async fn replace_from_legacy_json(
    db: &DbPool,
    user_id: i64,
    permissions_json: &str,
    allowed_projects_json: &str,
    granted_by: Option<i64>,
) -> Result<(), sqlx::Error> {
    let perms = parse_legacy_permissions(permissions_json);
    let (all, ids) = parse_legacy_projects(allowed_projects_json);
    let grants = Grants::from_legacy(&perms, all, &ids);
    let mut tx = db.begin().await?;
    replace(&mut tx, user_id, &grants, granted_by).await?;
    tx.commit().await
}

/// One-time conversion of every existing user's legacy permissions into grants.
/// Idempotent: guarded by a `_meta` marker written in the same transaction.
pub async fn migrate_legacy_once(db: &DbPool) -> Result<usize, sqlx::Error> {
    let mut tx = db.begin().await?;
    // Serialize concurrent boots on the marker row.
    sqlx::query("LOCK TABLE _meta IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let done: Option<String> = sqlx::query_scalar("SELECT value FROM _meta WHERE key = $1")
        .bind(LEGACY_MIGRATION_META_KEY)
        .fetch_optional(&mut *tx)
        .await?;
    if done.is_some() {
        return Ok(0);
    }

    let users = sqlx::query("SELECT id, username, permissions, allowed_projects FROM users")
        .fetch_all(&mut *tx)
        .await?;
    let mut migrated = 0;
    for row in users {
        let id: i64 = row.try_get(0)?;
        let username: String = row.try_get(1)?;
        if super::user::User::is_main_admin_identity(id, &username) {
            continue;
        }
        let perms = parse_legacy_permissions(&row.try_get::<Option<String>, _>(2)?.unwrap_or_default());
        let projects_json = row.try_get::<Option<String>, _>(3)?.unwrap_or_else(|| "[\"*\"]".into());
        let (all, ids) = parse_legacy_projects(&projects_json);
        replace(&mut tx, id, &Grants::from_legacy(&perms, all, &ids), None).await?;
        migrated += 1;
    }

    sqlx::query("INSERT INTO _meta(key, value) VALUES($1, $2)")
        .bind(LEGACY_MIGRATION_META_KEY)
        .bind(now_ms().to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(migrated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn legacy_all_projects_maps_to_wildcards() {
        let g = Grants::from_legacy(&perms(&["deploy", "logs"]), true, &[]);
        assert!(g.can_access_project(42));
        assert!(g.project_has_any(42, &["deploy"]));
        assert!(!g.project_has_any(42, &["env"]));
        assert!(g.panel_has_any(&["logs"]));
        assert!(g.databases.contains_key(&None));
    }

    #[test]
    fn legacy_restricted_projects_map_to_project_grants() {
        let g = Grants::from_legacy(&perms(&["files"]), false, &[10, 20]);
        assert!(g.can_access_project(10));
        assert!(g.can_access_project(20));
        assert!(!g.can_access_project(30));
        assert!(g.project_has_any(10, &["files", "files_write"]));
        assert!(!g.project_has_any(30, &["files"]));
        assert!(g.databases.is_empty());
    }

    #[test]
    fn legacy_access_without_powers_still_grants_project_access() {
        let g = Grants::from_legacy(&[], false, &[7]);
        assert!(g.can_access_project(7));
        assert!(!g.project_has_any(7, &["deploy"]));
    }

    #[test]
    fn databases_manage_reaches_unlinked_databases() {
        let g = Grants::from_legacy(&perms(&["databases_manage"]), false, &[1]);
        assert!(g.database_has_any(99, &["databases", "databases_manage"]));
    }

    #[test]
    fn wildcard_power_matches_everything() {
        let g = Grants::from_legacy(&perms(&["*"]), false, &[3]);
        assert!(g.project_has_any(3, &["anything"]));
        assert!(g.panel_has_any(&["system_manage"]));
        assert!(g.database_has_any(5, &["databases"]));
    }

    #[test]
    fn legacy_project_json_parsing() {
        assert_eq!(parse_legacy_projects("[\"*\"]"), (true, vec![]));
        assert_eq!(parse_legacy_projects("[1, \"2\"]"), (false, vec![1, 2]));
        assert_eq!(parse_legacy_projects("not json"), (true, vec![]));
    }

    #[tokio::test]
    async fn legacy_users_keep_their_access_after_migration() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        let auth = crate::auth::Auth::new(db.clone(), 3600);
        auth.ensure_admin().await;

        let insert = |name: &'static str, perms: &'static str, projects: &'static str| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "INSERT INTO users(username, password_hash, permissions, allowed_projects, created_at)
                     VALUES($1, 'hash', $2, $3, 0) RETURNING id",
                )
                .bind(name)
                .bind(perms)
                .bind(projects)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        let everyone = insert("all_dev", r#"["deploy","logs"]"#, r#"["*"]"#).await;
        let scoped = insert("scoped_dev", r#"["files"]"#, "[10]").await;

        assert_eq!(migrate_legacy_once(&db).await.unwrap(), 2);
        // Second run is a no-op even if the user list changes.
        assert_eq!(migrate_legacy_once(&db).await.unwrap(), 0);

        let all_dev = auth.get_user_by_id(everyone).await.unwrap();
        assert!(all_dev.can_access_project(12345), "all-projects user reaches future projects");
        assert!(all_dev.has_project_power(12345, &["deploy"]));
        assert!(!all_dev.has_project_power(12345, &["env"]));
        assert!(all_dev.has_power("logs"));

        let scoped_dev = auth.get_user_by_id(scoped).await.unwrap();
        assert!(scoped_dev.can_access_project(10));
        assert!(scoped_dev.has_project_power(10, &["files"]));
        assert!(!scoped_dev.can_access_project(11));
        assert!(!scoped_dev.has_database_power(1, None, &["databases"]));

        let admin = auth.get_user_by_id(1).await.unwrap();
        assert!(admin.has_project_power(999, &["anything"]));
    }
}
