//! Housekeeping owned by Web-Core: audit retention, scheduled control-database backups, and
//! webhook notifications.
//!
//! Settings (Platform Settings › Maintenance):
//! - `audit_retention_days` (default 365; 0 keeps everything)
//! - `db_backup_keep` daily backups to keep (default 7; 0 turns daily backups off)
//! - `notify_webhook_url` (empty: no notifications). Discord and Slack webhooks work as-is:
//!   the JSON body carries both `content` and `text`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::db::DbPool;

pub const AUDIT_RETENTION_SETTING: &str = "audit_retention_days";
pub const BACKUP_KEEP_SETTING: &str = "db_backup_keep";
pub const WEBHOOK_SETTING: &str = "notify_webhook_url";

const DEFAULT_AUDIT_RETENTION_DAYS: i64 = 365;
const DEFAULT_BACKUP_KEEP: usize = 7;
const DAY_MS: i64 = 86_400_000;

async fn setting(db: &DbPool, key: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = $1")
        .bind(key)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
}

/// Validates a maintenance setting value; `Err` holds a message for the admin.
pub fn validate_setting(key: &str, value: &str) -> Result<(), String> {
    let v = value.trim();
    match key {
        AUDIT_RETENTION_SETTING => match v.parse::<i64>() {
            Ok(n) if (0..=3650).contains(&n) => Ok(()),
            _ => Err("Keep audit entries for 0 to 3650 days (0 keeps everything).".into()),
        },
        BACKUP_KEEP_SETTING => match v.parse::<i64>() {
            Ok(n) if (0..=60).contains(&n) => Ok(()),
            _ => Err("Keep 0 to 60 daily backups (0 turns daily backups off).".into()),
        },
        WEBHOOK_SETTING if v.is_empty() || is_valid_webhook(v) => Ok(()),
        WEBHOOK_SETTING => Err("The webhook must be an http(s) URL.".into()),
        _ => Ok(()),
    }
}

pub fn is_valid_webhook(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() <= 500
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

// ---------------------------------------------------------------------------
// Audit retention
// ---------------------------------------------------------------------------

/// Deletes audit entries older than the retention period. Returns rows removed.
pub async fn prune_audit(db: &DbPool) -> u64 {
    let days = setting(db, AUDIT_RETENTION_SETTING)
        .await
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_AUDIT_RETENTION_DAYS);
    if days <= 0 {
        return 0;
    }
    sqlx::query("DELETE FROM audit WHERE ts < $1")
        .bind(crate::db::now() - days * DAY_MS)
        .execute(db)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Control database backups
// ---------------------------------------------------------------------------

pub fn backup_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups").join("control-db")
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupFile {
    pub name: String,
    pub size: u64,
    pub created_at: i64,
    /// `daily`, `manual`, or `pre-upgrade`.
    pub kind: String,
}

pub fn list_backups(data_dir: &Path) -> Vec<BackupFile> {
    let mut out: Vec<BackupFile> = std::fs::read_dir(backup_dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".dump") {
                return None;
            }
            let meta = e.metadata().ok()?;
            let created_at = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as i64;
            let kind = name.split('-').next().unwrap_or("manual").to_string();
            let kind = if kind == "pre" { "pre-upgrade".into() } else { kind };
            Some(BackupFile { name, size: meta.len(), created_at, kind })
        })
        .collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

/// A backup file name we created (safe to serve or delete: no paths, known shape).
pub fn is_backup_name(name: &str) -> bool {
    name.ends_with(".dump")
        && !name.contains(['/', '\\'])
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && (name.starts_with("daily-") || name.starts_with("manual-") || name.starts_with("pre-upgrade-"))
}

/// Dumps the control database (local zero-config PostgreSQL only) to `<kind>-<time>.dump`.
pub async fn backup_now(data_dir: &Path, kind: &str) -> Result<BackupFile, String> {
    if std::env::var("DATABASE_URL").is_ok_and(|v| !v.trim().is_empty()) {
        return Err("This panel uses an external DATABASE_URL: back that database up with your own tools.".into());
    }
    let conn_path = data_dir.join("control-pg").join("conn.json");
    let raw = tokio::fs::read_to_string(&conn_path).await.map_err(|e| format!("Can't read {}: {e}", conn_path.display()))?;
    let cfg: crate::pg_control::ControlPgConfig = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let bins = crate::databases::binaries::PgBinaries::detect()?;
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let pg_dump = bins.psql.with_file_name(format!("pg_dump{ext}"));
    if !pg_dump.is_file() {
        return Err("pg_dump wasn't found next to the PostgreSQL tools.".into());
    }
    let dir = backup_dir(data_dir);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let stamp = crate::db::now() / 1000;
    let name = format!("{kind}-{stamp}.dump");
    let file = dir.join(&name);
    let out = tokio::process::Command::new(&pg_dump)
        .args(["-h", "127.0.0.1", "-p", &cfg.port.to_string(), "-U", &cfg.user, "-d", &cfg.database, "-F", "c", "-f"])
        .arg(&file)
        .env("PGPASSWORD", &cfg.password)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&file).await;
        return Err(format!("pg_dump failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let size = tokio::fs::metadata(&file).await.map(|m| m.len()).unwrap_or(0);
    Ok(BackupFile { name, size, created_at: crate::db::now(), kind: kind.to_string() })
}

/// Keeps the newest `keep` files that start with `prefix`.
pub fn prune_backups(data_dir: &Path, prefix: &str, keep: usize) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(backup_dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix)))
        .collect();
    files.sort();
    while files.len() > keep {
        let _ = std::fs::remove_file(files.remove(0));
    }
}

/// Takes the daily backup when the newest one is a day old (or missing).
pub async fn daily_backup_if_due(db: &DbPool, data_dir: &Path) {
    let keep = setting(db, BACKUP_KEEP_SETTING)
        .await
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BACKUP_KEEP);
    if keep == 0 || std::env::var("DATABASE_URL").is_ok_and(|v| !v.trim().is_empty()) {
        return;
    }
    let newest_daily = list_backups(data_dir).into_iter().filter(|b| b.kind == "daily").map(|b| b.created_at).max();
    if newest_daily.is_some_and(|t| crate::db::now() - t < DAY_MS) {
        return;
    }
    match backup_now(data_dir, "daily").await {
        Ok(b) => {
            prune_backups(data_dir, "daily-", keep);
            tracing::info!("Daily control database backup written: {}", b.name);
        }
        Err(e) => {
            tracing::warn!("Daily control database backup failed: {e}");
            notify(db, "backup.failed", format!("WebDep: the daily database backup failed: {e}")).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Webhook notifications
// ---------------------------------------------------------------------------

fn last_sent() -> &'static Mutex<HashMap<String, i64>> {
    static MAP: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

/// Sends `text` to the configured webhook (fire and forget). Uses `curl`, which ships with
/// Linux servers and Windows 10+, so the panel needs no HTTP client of its own.
pub async fn notify(db: &DbPool, event: &str, text: String) {
    let Some(url) = setting(db, WEBHOOK_SETTING).await.filter(|u| is_valid_webhook(u)) else {
        return;
    };
    send_webhook(url, event.to_string(), text);
}

/// Like `notify`, but at most once per `key` every `every_ms`.
pub async fn notify_throttled(db: &DbPool, key: &str, every_ms: i64, event: &str, text: String) {
    let now = crate::db::now();
    {
        let Ok(mut map) = last_sent().lock() else { return };
        if map.get(key).is_some_and(|t| now - t < every_ms) {
            return;
        }
        map.insert(key.to_string(), now);
    }
    notify(db, event, text).await;
}

pub fn send_webhook(url: String, event: String, text: String) {
    tokio::spawn(async move {
        let body = serde_json::json!({ "content": text, "text": text, "event": event }).to_string();
        let child = tokio::process::Command::new("curl")
            .args(["-sS", "--max-time", "10", "-X", "POST", "-H", "Content-Type: application/json", "--data-binary", "@-", "--url"])
            .arg(&url)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            tracing::warn!("Couldn't send a webhook notification: curl is not installed");
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(body.as_bytes()).await;
        }
        match child.wait_with_output().await {
            Ok(o) if o.status.success() => {}
            Ok(o) => tracing::warn!("Webhook notification failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => tracing::warn!("Webhook notification failed: {e}"),
        }
    });
}

/// Warns when a node with a port range has 10% or fewer free open ports left.
pub async fn check_port_pools(db: &DbPool) {
    let nodes: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, name, COALESCE(NULLIF(display_name, ''), name) FROM nodes WHERE port_min IS NOT NULL",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for (id, _, label) in nodes {
        let Ok((free, open)) = crate::cluster::ports::free_and_open_counts(db, id).await else { continue };
        if open > 0 && free * 10 <= open {
            notify_throttled(
                db,
                &format!("ports-low-{id}"),
                6 * 3_600_000,
                "ports.low",
                format!("WebDep: {label} has only {free} free port(s) left out of {open} open. Add or open more in Admin Panel > IP Addresses."),
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_maintenance_settings() {
        assert!(validate_setting(AUDIT_RETENTION_SETTING, "90").is_ok());
        assert!(validate_setting(AUDIT_RETENTION_SETTING, "-1").is_err());
        assert!(validate_setting(BACKUP_KEEP_SETTING, "61").is_err());
        assert!(validate_setting(WEBHOOK_SETTING, "").is_ok());
        assert!(validate_setting(WEBHOOK_SETTING, "https://hooks.example.test/x").is_ok());
        assert!(validate_setting(WEBHOOK_SETTING, "-o /etc/passwd").is_err());
        assert!(validate_setting(WEBHOOK_SETTING, "https://a b").is_err());
    }

    #[test]
    fn only_our_backup_files_are_served() {
        assert!(is_backup_name("daily-1700000000.dump"));
        assert!(is_backup_name("pre-upgrade-v0007-to-v0009-1.dump"));
        assert!(!is_backup_name("../conn.json"));
        assert!(!is_backup_name("daily-1/../../x.dump"));
        assert!(!is_backup_name("other.dump"));
    }

    #[tokio::test]
    async fn prunes_old_audit_entries() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        let old = crate::db::now() - 400 * DAY_MS;
        sqlx::query("INSERT INTO audit(ts, actor, action) VALUES($1, 'a', 'old'), ($2, 'a', 'new')")
            .bind(old)
            .bind(crate::db::now())
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(prune_audit(&db).await, 1, "default retention is 365 days");
        sqlx::query("INSERT INTO settings(key, value) VALUES($1, '0')").bind(AUDIT_RETENTION_SETTING).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO audit(ts, actor, action) VALUES($1, 'a', 'old')").bind(old).execute(&db).await.unwrap();
        assert_eq!(prune_audit(&db).await, 0, "0 keeps everything");
    }
}
