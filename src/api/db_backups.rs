//! Manual backups of managed databases: create, list, restore, download, lock and delete.
//! The work itself (dump and all-or-nothing restore) lives in `crate::db_backups`.

use axum::{
    body::Body,
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::common::*;
use super::databases::get_authorized_database;
use super::limits::BACKUP_LIMIT_TAG;

/// A database owner or anyone with the backups power may use these.
const POWERS: &[&str] = &["backups", "databases_manage"];

#[derive(Serialize)]
struct DbBackupItem {
    id: i64,
    ts: i64,
    kind: &'static str,
    size: i64,
    note: String,
    status: String,
    error: String,
    #[serde(rename = "checksumSha256")]
    checksum: String,
    locked: bool,
    #[serde(rename = "createdBy")]
    created_by: String,
    #[serde(rename = "completedAt")]
    completed_at: i64,
    format: &'static str,
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

type ApiResult<T> = Result<T, (StatusCode, String)>;

/// Backups are made and restored on the server where the database runs; databases on other
/// VPSes are not supported yet.
async fn ensure_local(state: &AppState, db: &crate::databases::ManagedDatabase) -> ApiResult<()> {
    if state
        .cluster
        .should_proxy_to_node(db.node_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
        .is_some()
    {
        return Err((
            StatusCode::CONFLICT,
            "Backups of databases that run on another VPS aren't available yet.".to_string(),
        ));
    }
    Ok(())
}

fn ensure_running(db: &crate::databases::ManagedDatabase) -> ApiResult<()> {
    if db.status != "running" {
        return Err((
            StatusCode::CONFLICT,
            "Start the database first: backups and restores need it running.".to_string(),
        ));
    }
    Ok(())
}

async fn conn_for(state: &AppState, db: &crate::databases::ManagedDatabase) -> ApiResult<crate::db_backups::DbConn> {
    let password = db
        .db_password
        .clone()
        .filter(|p| !p.is_empty())
        .ok_or((StatusCode::CONFLICT, "This database's password isn't available, so it can't be backed up.".to_string()))?;
    let host = state.databases.connect_host(db).await.map_err(|e| (StatusCode::CONFLICT, e))?;
    Ok(crate::db_backups::DbConn {
        host,
        port: db.port,
        user: db.db_user.clone(),
        password,
        database: db.db_name.clone(),
    })
}

pub(crate) async fn list_db_backups_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let (_user, _db) = get_authorized_database(&state, &headers, POWERS, id).await?;
    let rows = sqlx::query(
        "SELECT id, created_at, size_bytes, note, status, error, checksum_sha256, locked, created_by, completed_at
         FROM database_backups WHERE database_id = $1 ORDER BY id DESC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;
    let items: Vec<DbBackupItem> = rows
        .iter()
        .map(|r| DbBackupItem {
            id: r.get(0),
            ts: r.get(1),
            kind: "database-dump",
            size: r.get(2),
            note: r.get(3),
            status: r.get(4),
            error: r.get(5),
            checksum: r.get(6),
            locked: r.get(7),
            created_by: r.get(8),
            completed_at: r.get(9),
            format: "dump",
        })
        .collect();
    let limit = crate::app_limits::get(&state.db, crate::app_limits::Kind::Database, id)
        .await
        .ok()
        .flatten()
        .map_or(0, |l| l.backups);
    Ok(Json(serde_json::json!({ "items": items, "limit": limit })))
}

#[derive(Deserialize, Default)]
pub(crate) struct CreateDbBackupPayload {
    #[serde(default)]
    note: Option<String>,
}

pub(crate) async fn create_db_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    payload: Option<Json<CreateDbBackupPayload>>,
) -> ApiResult<Json<serde_json::Value>> {
    let (actor, db) = get_authorized_database(&state, &headers, POWERS, id).await?;
    ensure_local(&state, &db).await?;
    ensure_running(&db)?;
    let conn = conn_for(&state, &db).await?;

    let limit = crate::app_limits::get(&state.db, crate::app_limits::Kind::Database, id)
        .await
        .ok()
        .flatten()
        .map_or(0, |l| l.backups);
    if limit > 0 && crate::db_backups::count_held(&state.db, id).await >= i64::from(limit) {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "{BACKUP_LIMIT_TAG}This database already holds its limit of {limit} backup(s). Delete one first or ask an admin to raise the limit."
            ),
        ));
    }
    let note: String = payload
        .and_then(|Json(p)| p.note)
        .map(|s| s.trim().chars().take(200).collect())
        .filter(|s: &String| !s.is_empty())
        .unwrap_or_else(|| "Manual backup".to_string());

    let backup_id: i64 = sqlx::query_scalar(
        "INSERT INTO database_backups(database_id, created_at, status, note, created_by) VALUES($1, $2, 'running', $3, $4) RETURNING id",
    )
    .bind(id)
    .bind(crate::db::now())
    .bind(&note)
    .bind(&actor.username)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    let data_dir = state.cluster.data_dir().to_path_buf();
    let dest = crate::db_backups::dump_path(&data_dir, id, backup_id);
    let (state2, actor_name, db_name) = (state.clone(), actor.username.clone(), database_tag(db.id));
    tokio::spawn(async move {
        let outcome = match crate::databases::PgBinaries::detect() {
            Ok(bins) => crate::db_backups::create_dump(&bins, &conn, &dest).await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(info) => {
                let _ = sqlx::query(
                    "UPDATE database_backups SET status = 'complete', error = '', size_bytes = $2, checksum_sha256 = $3,
                            archive_path = $4, completed_at = $5 WHERE id = $1",
                )
                .bind(backup_id)
                .bind(info.size as i64)
                .bind(&info.sha256)
                .bind(dest.to_string_lossy().to_string())
                .bind(crate::db::now())
                .execute(&state2.db)
                .await;
                log_audit(&state2, &actor_name, "db.backup", Some(&db_name), Some(&format!("Backup #{backup_id} complete, {} KB", info.size / 1024))).await;
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&dest).await;
                let _ = sqlx::query("UPDATE database_backups SET status = 'failed', error = $2, completed_at = $3 WHERE id = $1")
                    .bind(backup_id)
                    .bind(&e)
                    .bind(crate::db::now())
                    .execute(&state2.db)
                    .await;
                log_audit(&state2, &actor_name, "db.backup.failed", Some(&db_name), Some(&format!("Backup #{backup_id} failed: {e}"))).await;
            }
        }
    });
    Ok(Json(serde_json::json!({ "backupId": backup_id, "status": "running" })))
}

#[derive(Deserialize)]
pub(crate) struct RestoreDbPayload {
    #[serde(rename = "backupId")]
    backup_id: i64,
}

pub(crate) async fn restore_db_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<RestoreDbPayload>,
) -> ApiResult<Json<serde_json::Value>> {
    let (actor, db) = get_authorized_database(&state, &headers, POWERS, id).await?;
    ensure_local(&state, &db).await?;
    ensure_running(&db)?;
    let conn = conn_for(&state, &db).await?;
    let row = sqlx::query("SELECT status, archive_path, checksum_sha256 FROM database_backups WHERE id = $1 AND database_id = $2")
        .bind(payload.backup_id)
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, format!("Backup #{} not found", payload.backup_id)))?;
    let (status, path, checksum): (String, String, String) = (row.get(0), row.get(1), row.get(2));
    if status != "complete" {
        return Err((StatusCode::CONFLICT, "This backup isn't complete, so it can't be restored.".to_string()));
    }
    let file = std::path::PathBuf::from(&path);
    if path.is_empty() || !file.is_file() {
        return Err((StatusCode::NOT_FOUND, "The backup file is missing from this server.".to_string()));
    }
    let data_dir = state.cluster.data_dir().to_path_buf();
    if !crate::db_backups::begin_restore(&data_dir, id, payload.backup_id) {
        return Err((StatusCode::CONFLICT, "This database is already being restored.".to_string()));
    }
    let (state2, actor_name, db_name, backup_id) = (state.clone(), actor.username.clone(), database_tag(db.id), payload.backup_id);
    tokio::spawn(async move {
        let result = match crate::databases::PgBinaries::detect() {
            Ok(bins) => crate::db_backups::restore_dump(&bins, &conn, &file, &checksum).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {
                crate::db_backups::finish_restore(&data_dir, id, true, format!("Restored backup #{backup_id}."));
                log_audit(&state2, &actor_name, "db.restore", Some(&db_name), Some(&format!("Restored backup #{backup_id}"))).await;
            }
            Err(e) => {
                crate::db_backups::finish_restore(&data_dir, id, false, e.clone());
                log_audit(&state2, &actor_name, "db.restore.failed", Some(&db_name), Some(&format!("Restore of backup #{backup_id} failed: {e}"))).await;
            }
        }
        state2.cache.invalidate_all();
    });
    Ok(Json(serde_json::json!({ "status": "running", "backupId": payload.backup_id })))
}

pub(crate) async fn db_restore_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    get_authorized_database(&state, &headers, POWERS, id).await?;
    Ok(Json(match crate::db_backups::restore_status(state.cluster.data_dir(), id) {
        Some(status) => serde_json::json!(status),
        None => serde_json::json!({ "state": "idle" }),
    }))
}

#[derive(Deserialize)]
pub(crate) struct LockDbBackupPayload {
    locked: bool,
}

pub(crate) async fn lock_db_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, backup_id)): AxPath<(i64, i64)>,
    Json(payload): Json<LockDbBackupPayload>,
) -> ApiResult<Json<serde_json::Value>> {
    let (actor, db) = get_authorized_database(&state, &headers, POWERS, id).await?;
    let done = sqlx::query("UPDATE database_backups SET locked = $3 WHERE id = $1 AND database_id = $2")
        .bind(backup_id)
        .bind(id)
        .bind(payload.locked)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, format!("Backup #{backup_id} not found")));
    }
    log_audit(&state, &actor.username, if payload.locked { "db.backup.lock" } else { "db.backup.unlock" }, Some(&database_tag(db.id)), Some(&format!("Backup #{backup_id}"))).await;
    Ok(Json(serde_json::json!({ "ok": true, "locked": payload.locked })))
}

pub(crate) async fn delete_db_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, backup_id)): AxPath<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let (actor, db) = get_authorized_database(&state, &headers, POWERS, id).await?;
    let row = sqlx::query("SELECT status, locked, archive_path FROM database_backups WHERE id = $1 AND database_id = $2")
        .bind(backup_id)
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, format!("Backup #{backup_id} not found")))?;
    let (status, locked, path): (String, bool, String) = (row.get(0), row.get(1), row.get(2));
    if status == "running" {
        return Err((StatusCode::CONFLICT, "This backup is still being created.".to_string()));
    }
    if locked {
        return Err((StatusCode::CONFLICT, "This backup is locked. Unlock it before deleting.".to_string()));
    }
    let data_dir = state.cluster.data_dir();
    // Only ever remove a file inside this database's own backup folder.
    let file = std::path::PathBuf::from(&path);
    if !path.is_empty() && file.starts_with(crate::db_backups::backups_dir(data_dir, id)) {
        let _ = tokio::fs::remove_file(&file).await;
    }
    sqlx::query("DELETE FROM database_backups WHERE id = $1 AND database_id = $2")
        .bind(backup_id)
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    log_audit(&state, &actor.username, "db.backup.delete", Some(&database_tag(db.id)), Some(&format!("Deleted backup #{backup_id}"))).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Streams a finished dump to the browser.
pub(crate) async fn download_db_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, backup_id)): AxPath<(i64, i64)>,
) -> ApiResult<Response> {
    let (actor, db) = get_authorized_database(&state, &headers, POWERS, id).await?;
    let row = sqlx::query("SELECT status, archive_path FROM database_backups WHERE id = $1 AND database_id = $2")
        .bind(backup_id)
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, format!("Backup #{backup_id} not found")))?;
    let (status, path): (String, String) = (row.get(0), row.get(1));
    if status != "complete" {
        return Err((StatusCode::CONFLICT, "This backup isn't complete yet.".to_string()));
    }
    let missing = || (StatusCode::NOT_FOUND, "The backup file is missing from this server.".to_string());
    let root = tokio::fs::canonicalize(crate::db_backups::backups_dir(state.cluster.data_dir(), id))
        .await
        .map_err(|_| missing())?;
    let file_path = tokio::fs::canonicalize(&path).await.map_err(|_| missing())?;
    if !file_path.starts_with(&root) {
        return Err((StatusCode::FORBIDDEN, "That file isn't a WebDep backup.".to_string()));
    }
    let file = tokio::fs::File::open(&file_path).await.map_err(internal)?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let safe: String = db.name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect();
    let name = format!("webdep-db-backup-{}-{backup_id}.dump", if safe.is_empty() { "database" } else { &safe });
    let stream = futures_util::stream::unfold(file, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 64 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    log_audit(&state, &actor.username, "db.backup.download", Some(&database_tag(db.id)), Some(&format!("Downloaded backup #{backup_id}"))).await;
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .header(axum::http::header::CONTENT_LENGTH, len.to_string())
        .header(axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))
        .body(Body::from_stream(stream))
        .map_err(internal)
}
