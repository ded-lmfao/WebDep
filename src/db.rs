use sqlx::PgPool;
use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::pg_control::{self, ControlPgHandle, DEFAULT_CONTROL_PG_PORT};

pub type DbPool = PgPool;

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Opens (or auto-bootstraps) the control-plane PostgreSQL database, runs SQL migrations
/// and startup hygiene.
pub async fn open_db(
    data_dir: &Path,
) -> Result<(DbPool, Option<ControlPgHandle>), Box<dyn std::error::Error + Send + Sync>> {
    tokio::fs::create_dir_all(data_dir).await?;

    let (pool, handle) = pg_control::ensure_control_pg_pool(data_dir).await?;

    seed_github_token(&pool).await;
    ensure_project_deploy_dirs(&pool).await;
    purge_orphaned_project_data(&pool).await;

    Ok((pool, handle))
}

async fn seed_github_token(pool: &DbPool) {
    let existing_token: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'github_token'")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    if existing_token.trim().is_empty() {
        let mut detected_tok = std::env::var("GITHUB_TOKEN").unwrap_or_default();
        if detected_tok.trim().is_empty() {
            if let Ok(out) = tokio::process::Command::new("gh").args(["auth", "token"]).output().await {
                if out.status.success() {
                    detected_tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
                }
            }
        }
        let trimmed = detected_tok.trim();
        if !trimmed.is_empty() {
            let _ = sqlx::query(
                "INSERT INTO settings(key, value) VALUES('github_token', $1)
                 ON CONFLICT(key) DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(trimmed)
            .execute(pool)
            .await;
        }
    }
}

async fn ensure_project_deploy_dirs(pool: &DbPool) {
    if let Ok(dirs) = sqlx::query_scalar::<_, String>("SELECT deploy_dir FROM projects")
        .fetch_all(pool)
        .await
    {
        for dir in dirs {
            if !dir.trim().is_empty() {
                let _ = tokio::fs::create_dir_all(&dir).await;
            }
        }
    }
}

/// Cap total stored log rows to prevent unbounded growth on busy services.
pub async fn prune_old_logs(pool: &DbPool, max_rows: usize) {
    if max_rows == 0 {
        return;
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    if count as usize > max_rows {
        let offset = (max_rows - 1) as i64;
        let _ = sqlx::query(
            "DELETE FROM logs WHERE id < (SELECT id FROM logs ORDER BY id DESC LIMIT 1 OFFSET $1)",
        )
        .bind(offset)
        .execute(pool)
        .await;
    }
}

/// Clean expired sessions and any orphan references cleanly.
pub async fn purge_orphaned_project_data(pool: &DbPool) {
    let now_s = now() / 1000;
    let _ = sqlx::query("DELETE FROM sessions WHERE expires_at <= $1")
        .bind(now_s)
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "DELETE FROM logs WHERE project_id IS NOT NULL AND project_id NOT IN (SELECT id FROM projects)",
    )
    .execute(pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM logs WHERE deployment_id IS NOT NULL AND deployment_id NOT IN (SELECT id FROM deployments)",
    )
    .execute(pool)
    .await;
}

/// Global unified TCP port allocator across all platform workloads:
/// - Guarantees non-conflicting ports across websites, backend services/bots, and databases
/// - Protects reserved system, daemon & control-pg ports (22 SSH, 80 HTTP, 443 HTTPS, 4600 WebDep daemon, 5431 control-pg)
/// - Probes physical OS socket availability via TcpListener on 127.0.0.1
/// - Preserves running workload assignments across configuration updates
pub async fn allocate_global_unique_port(
    pool: &DbPool,
    requested: Option<u16>,
    default_start: u16,
    exclude_project_id: Option<i64>,
    exclude_database_id: Option<i64>,
) -> Result<u16, String> {
    if let (Some(id), Some(req)) = (exclude_project_id, requested) {
        if req > 0 {
            let current_port: Option<i32> = sqlx::query_scalar("SELECT port FROM projects WHERE id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
                .flatten();
            if current_port == Some(req as i32) {
                return Ok(req);
            }
        }
    }

    if let (Some(id), Some(req)) = (exclude_database_id, requested) {
        if req > 0 {
            let current_port: Option<i32> = sqlx::query_scalar("SELECT port FROM databases WHERE id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten();
            if current_port == Some(req as i32) {
                return Ok(req);
            }
        }
    }

    let mut used = HashSet::new();
    used.insert(22);
    used.insert(80);
    used.insert(443);
    used.insert(4600);
    used.insert(DEFAULT_CONTROL_PG_PORT);

    let proj_ports: Vec<i32> = match exclude_project_id {
        Some(id) => sqlx::query_scalar("SELECT port FROM projects WHERE port IS NOT NULL AND id != $1")
            .bind(id)
            .fetch_all(pool)
            .await
            .unwrap_or_default(),
        None => sqlx::query_scalar("SELECT port FROM projects WHERE port IS NOT NULL")
            .fetch_all(pool)
            .await
            .unwrap_or_default(),
    };
    for p in proj_ports {
        if let Ok(port) = u16::try_from(p) {
            used.insert(port);
        }
    }

    let db_ports: Vec<i32> = match exclude_database_id {
        Some(id) => sqlx::query_scalar("SELECT port FROM databases WHERE port IS NOT NULL AND id != $1")
            .bind(id)
            .fetch_all(pool)
            .await
            .unwrap_or_default(),
        None => sqlx::query_scalar("SELECT port FROM databases WHERE port IS NOT NULL")
            .fetch_all(pool)
            .await
            .unwrap_or_default(),
    };
    for p in db_ports {
        if let Ok(port) = u16::try_from(p) {
            used.insert(port);
        }
    }

    if let Some(req) = requested {
        if req > 0 && req < 65535 && !used.contains(&req) && std::net::TcpListener::bind(("127.0.0.1", req)).is_ok() {
            return Ok(req);
        }
    }

    let start = requested.filter(|&r| r > 0).unwrap_or(default_start).max(1024);
    for port in start..65535 {
        if used.contains(&port) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }

    for port in 1024..start {
        if used.contains(&port) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }

    Err("No free TCP ports available in range 1024-65535".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pg_schema_and_settings_seeded() {
        let pool = crate::pg_control::test_db::isolated_pool().await;

        let min_free: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'min_free_mb'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(min_free, "500");

        let idx_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_indexes WHERE tablename = 'sessions' AND indexname = 'idx_sessions_last_active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(idx_count, 1);

        pool.close().await;
    }

    #[tokio::test]
    async fn test_prune_old_logs_caps_rows() {
        let pool = crate::pg_control::test_db::isolated_pool().await;
        for i in 0..15 {
            sqlx::query("INSERT INTO logs (channel, level, message, ts) VALUES ('sys', 'info', $1, $2)")
                .bind(format!("msg-{}", i))
                .bind(i as i64)
                .execute(&pool)
                .await
                .unwrap();
        }

        prune_old_logs(&pool, 5).await;
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 5);

        pool.close().await;
    }

    #[tokio::test]
    async fn test_zero_config_control_pg_lifecycle() {
        let temp_dir = std::env::temp_dir().join(format!("webdep-ctrlpg-{}", now()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let free_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let test_port = free_listener.local_addr().unwrap().port();
        drop(free_listener);

        let (pool, handle) = crate::pg_control::ensure_local_pg_with_port(&temp_dir, Some(test_port))
            .await
            .expect("zero-config local control-pg should initialize and boot");
        let handle = handle.expect("expected ControlPgHandle");
        pool.close().await;

        let (pool2, handle2) = crate::pg_control::ensure_local_pg_with_port(&temp_dir, Some(test_port))
            .await
            .expect("reconnecting to control-pg should succeed");
        pool2.close().await;
        if let Some(h) = handle2 {
            h.stop().await;
        } else {
            handle.stop().await;
        }

        std::fs::remove_dir_all(&temp_dir).ok();
    }
}
