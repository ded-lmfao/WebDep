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

/// Panel powers an admin (or a delegate holding them) can hand out from the user editor.
pub const PANEL_ACCOUNT_POWERS: &[&str] = &[
    "users_view", "users_create", "users_edit", "requests_review",
    "create_websites", "create_services", "create_databases",
];

/// Panel power that lets a user create apps of `kind` ("website", "service", "database").
pub fn create_power(kind: &str) -> &'static str {
    match kind {
        "service" => "create_services",
        "database" => "create_databases",
        _ => "create_websites",
    }
}

/// Makes `user_id` the owner of an app they just created: every power on that app.
pub async fn grant_owner(db: &DbPool, user_id: i64, scope: &str, app_id: i64, kind: &str) -> Result<(), sqlx::Error> {
    let powers: Vec<String> = kind_catalog(kind).iter().map(|p| p.to_string()).collect();
    upsert(db, user_id, scope, Some(app_id), &powers, Some(user_id)).await
}
/// Powers a website or service grant may hold.
pub const PROJECT_POWERS: &[&str] = &[
    "logs", "files", "files_write", "deploy", "process", "backups", "env", "domains",
    "projects_manage", "services_manage", "activity", "access_manage", "app_delete",
];
/// Powers a database grant may hold.
pub const DATABASE_POWERS: &[&str] = &["databases", "env", "databases_manage", "backups", "activity", "access_manage", "app_delete"];

/// Checklist shown for one kind of app, in display order.
pub fn kind_catalog(kind: &str) -> &'static [&'static str] {
    match kind {
        "website" => &[
            "logs", "files", "files_write", "deploy", "process", "backups", "env", "domains",
            "projects_manage", "activity", "access_manage", "app_delete",
        ],
        "service" => &[
            "logs", "files", "files_write", "deploy", "process", "backups", "env",
            "services_manage", "activity", "access_manage", "app_delete",
        ],
        "database" | "all-databases" => DATABASE_POWERS,
        _ => PROJECT_POWERS,
    }
}

pub fn catalog(scope: &str) -> &'static [&'static str] {
    match scope {
        SCOPE_PROJECT => PROJECT_POWERS,
        SCOPE_DATABASE => DATABASE_POWERS,
        _ => PANEL_ACCOUNT_POWERS,
    }
}

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

/// JSON shape: `{"panel": [...], "projects": {"*": [...], "12": [...]}, "databases": {...}}`
/// where `"*"` is the every-app grant.
impl serde::Serialize for Grants {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        fn sorted(set: &HashSet<String>) -> Vec<&String> {
            let mut v: Vec<&String> = set.iter().collect();
            v.sort();
            v
        }
        fn scoped(map: &HashMap<Option<i64>, HashSet<String>>) -> std::collections::BTreeMap<String, Vec<&String>> {
            map.iter()
                .map(|(k, v)| (k.map_or_else(|| ALL_POWERS.to_string(), |id| id.to_string()), sorted(v)))
                .collect()
        }
        let mut m = ser.serialize_map(Some(3))?;
        m.serialize_entry("panel", &sorted(&self.panel))?;
        m.serialize_entry("projects", &scoped(&self.projects))?;
        m.serialize_entry("databases", &scoped(&self.databases))?;
        m.end()
    }
}

impl Grants {
    /// Every power held on one app (its own grant plus the every-app grant), or `None`
    /// when the user has no grant reaching it.
    pub fn powers_on(&self, scope: &str, id: Option<i64>) -> Option<HashSet<String>> {
        let map = match scope {
            SCOPE_PROJECT => &self.projects,
            SCOPE_DATABASE => &self.databases,
            _ => return Some(self.panel.clone()),
        };
        let mut out: Option<HashSet<String>> = None;
        let keys = if id.is_some() { vec![id, None] } else { vec![None] };
        for key in &keys {
            if let Some(set) = map.get(key) {
                out.get_or_insert_with(HashSet::new).extend(set.iter().cloned());
            }
        }
        out
    }

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

/// Parses a grant's `powers_json` array. Invalid JSON yields no powers.
fn parse_powers(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
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
        let powers: HashSet<String> = parse_powers(&row.try_get::<String, _>(2)?)
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

/// Loads unexpired grants of every user, keyed by user id.
pub async fn load_all(db: &DbPool) -> Result<HashMap<i64, Grants>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT user_id, scope_type, scope_id, powers_json FROM access_grants
         WHERE expires_at IS NULL OR expires_at > $1",
    )
    .bind(now_ms())
    .fetch_all(db)
    .await?;
    let mut out: HashMap<i64, Grants> = HashMap::new();
    for row in rows {
        let g = out.entry(row.try_get(0)?).or_default();
        let scope_id: Option<i64> = row.try_get(2)?;
        let powers = parse_powers(&row.try_get::<String, _>(3)?);
        match row.try_get::<String, _>(1)?.as_str() {
            SCOPE_GLOBAL => g.panel.extend(powers),
            SCOPE_PROJECT => g.projects.entry(scope_id).or_default().extend(powers),
            SCOPE_DATABASE => g.databases.entry(scope_id).or_default().extend(powers),
            _ => {}
        }
    }
    Ok(out)
}

/// Creates or replaces one grant row (`scope_id` NULL = every app of that scope, or the
/// panel for `global`).
pub async fn upsert(
    db: &DbPool,
    user_id: i64,
    scope: &str,
    scope_id: Option<i64>,
    powers: &[String],
    granted_by: Option<i64>,
) -> Result<(), sqlx::Error> {
    let mut sorted: Vec<&String> = powers.iter().collect();
    sorted.sort();
    sorted.dedup();
    let ts = now_ms();
    sqlx::query(
        "INSERT INTO access_grants(user_id, scope_type, scope_id, role_preset, powers_json, granted_by, created_at, updated_at)
         VALUES($1, $2, $3, 'custom', $4, $5, $6, $6)
         ON CONFLICT (user_id, scope_type, COALESCE(scope_id, 0))
         DO UPDATE SET powers_json = EXCLUDED.powers_json, granted_by = EXCLUDED.granted_by, updated_at = EXCLUDED.updated_at",
    )
    .bind(user_id)
    .bind(scope)
    .bind(scope_id)
    .bind(serde_json::to_string(&sorted).unwrap_or_else(|_| "[]".into()))
    .bind(granted_by)
    .bind(ts)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn remove(db: &DbPool, user_id: i64, scope: &str, scope_id: Option<i64>) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "DELETE FROM access_grants WHERE user_id = $1 AND scope_type = $2 AND scope_id IS NOT DISTINCT FROM $3",
    )
    .bind(user_id)
    .bind(scope)
    .bind(scope_id)
    .execute(db)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Drops grants that point at deleted apps.
pub async fn remove_for_app(db: &DbPool, scope: &str, scope_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM access_grants WHERE scope_type = $1 AND scope_id = $2")
        .bind(scope)
        .bind(scope_id)
        .execute(db)
        .await?;
    Ok(())
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

/// Test fixtures: `powers` on the panel and on the listed websites (`None` = every website
/// and database).
#[cfg(test)]
impl Grants {
    pub fn fixture(powers: &[&str], projects: Option<&[i64]>) -> Self {
        let set: HashSet<String> = powers.iter().map(|p| p.to_string()).collect();
        let mut out = Grants { panel: set.clone(), ..Default::default() };
        match projects {
            None => {
                out.projects.insert(None, set.clone());
                out.databases.insert(None, set);
            }
            Some(ids) => out.projects.extend(ids.iter().map(|id| (Some(*id), set.clone()))),
        }
        out
    }
}

#[cfg(test)]
pub async fn set_fixture(db: &DbPool, user_id: i64, powers: &[&str], projects: Option<&[i64]>) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    replace(&mut tx, user_id, &Grants::fixture(powers, projects), None).await?;
    tx.commit().await
}
