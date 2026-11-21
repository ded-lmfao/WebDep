//! Per-app limits (CPU, memory, disk, backups) for websites, services, and databases.
//!
//! Every field uses 0 for "no limit", so an empty form field simply means unlimited.
//! CPU and memory are enforced on Linux with cgroups (see `runtime::limits`), disk is a soft
//! limit checked on a timer (the app is stopped when it grows past it), and the backup limit is
//! the number of backups an app may hold at a time.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::db::DbPool;

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
    #[serde(flatten)]
    pub limits: AppLimits,
}

/// Every app with its limits, for the admin App Limits tab.
pub async fn list_all(db: &DbPool) -> Result<Vec<AppLimitRow>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT CASE WHEN project_kind = 'service' THEN 'service' ELSE 'website' END, id, name, node_id,
                cpu_limit, memory_limit_mb, disk_limit_mb, backup_limit FROM projects
         UNION ALL
         SELECT 'database', id, name, node_id, cpu_limit, memory_limit_mb, disk_limit_mb, backup_limit FROM databases
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
