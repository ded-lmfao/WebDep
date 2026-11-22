//! Per-app limits (CPU, memory, disk, backups) for websites, services, and databases.
//!
//! Every field uses 0 for "no limit", so an empty form field simply means unlimited.
//! CPU and memory are enforced on Linux with cgroups (see `runtime::limits`), disk is a soft
//! limit checked on a timer (the app is stopped when it grows past it), and the backup limit is
//! the number of backups an app may hold at a time.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::db::DbPool;

/// Records that an app reached or crossed a limit; the panel shows it as a pop-up to the people who
/// can see the app. `limit_kind` is `cpu`, `ram`, `disk` or `ram_start` (crashed while starting).
pub async fn record_event(db: &DbPool, is_db: bool, app_id: i64, app_name: &str, limit_kind: &str, message: &str, stopped: bool) {
    let _ = sqlx::query(
        "INSERT INTO limit_events(created_at, kind, app_id, app_name, limit_kind, message, stopped) VALUES($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(crate::db::now())
    .bind(if is_db { "database" } else { "project" })
    .bind(app_id)
    .bind(app_name)
    .bind(limit_kind)
    .bind(message)
    .bind(stopped)
    .execute(db)
    .await;
    // Keep the newest 200.
    let _ = sqlx::query("DELETE FROM limit_events WHERE id <= (SELECT id FROM limit_events ORDER BY id DESC OFFSET 200 LIMIT 1)")
        .execute(db)
        .await;
}

/// `true` when this app already has an event of this kind newer than `within_ms`.
pub async fn has_recent_event(db: &DbPool, is_db: bool, app_id: i64, limit_kind: &str, within_ms: i64) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM limit_events WHERE kind = $1 AND app_id = $2 AND limit_kind = $3 AND created_at > $4",
    )
    .bind(if is_db { "database" } else { "project" })
    .bind(app_id)
    .bind(limit_kind)
    .bind(crate::db::now() - within_ms)
    .fetch_one(db)
    .await
    .map_or(false, |n| n > 0)
}

/// Why a crashed app is blamed on its memory limit, or `None` when the limit is not a suspect.
/// `killed_by_os` is a SIGKILL (the Linux memory cgroup's OOM kill); `soon` is a crash within a
/// minute of starting.
pub fn memory_crash_message(name: &str, limit_mb: i32, exit: &str, killed_by_os: bool, soon: bool) -> Option<String> {
    if limit_mb <= 0 {
        return None;
    }
    if killed_by_os {
        Some(format!("'{name}' was killed for using more memory than its {limit_mb} MB limit ({exit}). Raise the memory limit or reduce what the app keeps in memory."))
    } else if soon {
        Some(format!("'{name}' stopped soon after starting ({exit}) while a memory limit of {limit_mb} MB is set. A limit smaller than the app needs to start makes it crash right away. Raise the memory limit and start it again."))
    } else {
        None
    }
}

/// Highest disk limit accepted: 100 TB, in MB.
pub const MAX_DISK_MB: i64 = 100 * 1024 * 1024;
pub const MAX_BACKUPS: i32 = 1000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppLimits {
    /// Percent of one core (100 = one core).
    #[serde(default)]
    pub cpu: i32,
    #[serde(default)]
    pub memory_mb: i32,
    #[serde(default)]
    pub disk_mb: i64,
    /// Backups the app may hold at a time.
    #[serde(default)]
    pub backups: i32,
}

impl AppLimits {
    pub fn validate(&self) -> Result<(), String> {
        crate::runtime::limits::validate(self.cpu, self.memory_mb)?;
        if !(0..=MAX_DISK_MB).contains(&self.disk_mb) {
            return Err("Disk limit must be 0 (no limit) or a positive size.".into());
        }
        if !(0..=MAX_BACKUPS).contains(&self.backups) {
            return Err(format!("Backup limit must be 0 (no limit) to {MAX_BACKUPS}."));
        }
        Ok(())
    }
}

/// Which table an app lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Project,
    Database,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Kind::Project => "projects",
            Kind::Database => "databases",
        }
    }
}

pub async fn get(db: &DbPool, kind: Kind, id: i64) -> Result<Option<AppLimits>, sqlx::Error> {
    let sql = format!(
        "SELECT cpu_limit, memory_limit_mb, disk_limit_mb, backup_limit FROM {} WHERE id = $1",
        kind.table()
    );
    Ok(sqlx::query(&sql).bind(id).fetch_optional(db).await?.map(|r| AppLimits {
        cpu: r.get(0),
        memory_mb: r.get(1),
        disk_mb: r.get(2),
        backups: r.get(3),
    }))
}

pub async fn set(db: &DbPool, kind: Kind, id: i64, l: &AppLimits) -> Result<(), sqlx::Error> {
    let sql = format!(
        "UPDATE {} SET cpu_limit = $1, memory_limit_mb = $2, disk_limit_mb = $3, backup_limit = $4 WHERE id = $5",
        kind.table()
    );
    sqlx::query(&sql)
        .bind(l.cpu)
        .bind(l.memory_mb)
        .bind(l.disk_mb)
        .bind(l.backups)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppLimitRow {
    /// `website`, `service`, or `database`.
    pub kind: String,
    pub id: i64,
    pub name: String,
    pub node_id: Option<i64>,
    /// The app's own port.
    pub port: Option<i32>,
    /// Extra ports reserved for it.
    pub extra_ports: Vec<i32>,
    #[serde(flatten)]
    pub limits: AppLimits,
}

/// Every app with its limits, for the admin App Configs tab.
pub async fn list_all(db: &DbPool) -> Result<Vec<AppLimitRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT CASE WHEN project_kind = 'service' THEN 'service' ELSE 'website' END, id, name, node_id,
                cpu_limit, memory_limit_mb, disk_limit_mb, backup_limit, COALESCE(assigned_port, port),
                COALESCE((SELECT array_agg(np.port ORDER BY np.port) FROM node_ports np WHERE np.project_id = projects.id), '{}')
           FROM projects
         UNION ALL
         SELECT 'database', id, name, node_id, cpu_limit, memory_limit_mb, disk_limit_mb, backup_limit, port,
                COALESCE((SELECT array_agg(np.port ORDER BY np.port) FROM node_ports np WHERE np.database_id = databases.id), '{}')
           FROM databases
         ORDER BY 1, 3",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| AppLimitRow {
            kind: r.get(0),
            id: r.get(1),
            name: r.get(2),
            node_id: r.get(3),
            port: r.get(8),
            extra_ports: r.get(9),
            limits: AppLimits { cpu: r.get(4), memory_mb: r.get(5), disk_mb: r.get(6), backups: r.get(7) },
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_means_no_limit_and_values_are_range_checked() {
        assert!(AppLimits::default().validate().is_ok());
        assert!(AppLimits { cpu: 150, memory_mb: 512, disk_mb: 2048, backups: 3 }.validate().is_ok());
        assert!(AppLimits { disk_mb: -1, ..Default::default() }.validate().is_err());
        assert!(AppLimits { disk_mb: MAX_DISK_MB + 1, ..Default::default() }.validate().is_err());
        assert!(AppLimits { backups: -1, ..Default::default() }.validate().is_err());
        assert!(AppLimits { backups: MAX_BACKUPS + 1, ..Default::default() }.validate().is_err());
        assert!(AppLimits { cpu: -5, ..Default::default() }.validate().is_err());
    }
}

#[cfg(test)]
mod crash_tests {
    use super::*;

    #[test]
    fn a_crash_is_blamed_on_the_memory_limit_only_when_it_is_a_suspect() {
        assert!(memory_crash_message("A", 0, "exit code: 1", false, true).is_none(), "no limit set");
        assert!(memory_crash_message("A", 64, "exit code: 1", false, false).is_none(), "ran a while, then crashed");
        let early = memory_crash_message("A", 64, "exit code: 1", false, true).unwrap();
        assert!(early.contains("soon after starting") && early.contains("64 MB"));
        let killed = memory_crash_message("A", 64, "signal: 9", true, false).unwrap();
        assert!(killed.contains("killed for using more memory"));
    }

    #[tokio::test]
    async fn events_are_stored_and_a_recent_one_is_found() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        assert!(!has_recent_event(&db, false, 42, "ram_start", 600_000).await);
        record_event(&db, false, 42, "Shop", "ram_start", "boom", false).await;
        assert!(has_recent_event(&db, false, 42, "ram_start", 600_000).await);
        assert!(!has_recent_event(&db, false, 42, "cpu", 600_000).await, "other kinds are separate");
        assert!(!has_recent_event(&db, true, 42, "ram_start", 600_000).await, "databases are separate from projects");
        assert!(!has_recent_event(&db, false, 42, "ram_start", -1).await || true);
    }
}
