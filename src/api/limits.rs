//! Admin Panel > App Limits: CPU, memory, disk, and backup limits per website, service, and database.
//!
//! Routes (admins only; creators set limits once, when they create the app):
//! - `GET /api/limits`                 every app with its limits
//! - `PUT /api/projects/:id/limits`    `{ cpu, memoryMb, diskMb, backups }` for a website or service
//! - `PUT /api/databases/:id/limits`   same body for a database
//!
//! Every field uses 0 for "no limit".
//!
//! Disk is a soft limit: a timer measures local apps, stops one that grew past its limit, and
//! starting it again is refused until it is under the limit or an admin raises it. Apps on
//! other nodes are not measured yet.

use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};

use super::common::*;
use crate::app_limits::{self, AppLimits, Kind};

type ApiError = (StatusCode, String);

fn internal(e: impl ToString) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn describe(l: &AppLimits) -> String {
    let part = |value: String, set: bool, name: &str| if set { format!("{name} {value}") } else { format!("{name} unlimited") };
    [
        part(format!("{}%", l.cpu), l.cpu > 0, "CPU"),
        part(format!("{} MB", l.memory_mb), l.memory_mb > 0, "memory"),
        part(format!("{} MB", l.disk_mb), l.disk_mb > 0, "disk"),
        part(l.backups.to_string(), l.backups > 0, "backups"),
    ]
    .join(", ")
}

pub(crate) async fn list_limits_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_admin(&state, &headers).await?;
    let rows = app_limits::list_all(&state.db).await.map_err(internal)?;
    Ok(Json(serde_json::json!({ "limits": rows, "enforcement": crate::runtime::limits::enforcement_status(), "isolation": crate::runtime::isolation::status() })))
}

/// Sets a website or service's limits. A running app restarts when its CPU or memory limit
/// changes so the new cgroup limits take effect; disk and backup limits apply immediately.
pub(crate) async fn set_project_limits_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(l): Json<AppLimits>,
) -> Result<Json<crate::projects::Project>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    l.validate().map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let prev = state
        .projects
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "App not found".to_string()))?;
    let before = AppLimits {
        cpu: prev.cpu_limit,
        memory_mb: prev.memory_limit_mb,
        disk_mb: prev.disk_limit_mb,
        backups: prev.backup_limit,
    };
    if before == l {
        return Ok(Json(prev));
    }
    app_limits::set(&state.db, Kind::Project, id, &l).await.map_err(internal)?;
    let updated = state
        .projects
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "App not found".to_string()))?;
    super::projects::propagate_project_change(&state, Some(&prev), &updated).await?;
    let needs_restart = before.cpu != l.cpu || before.memory_mb != l.memory_mb;
    if needs_restart && (prev.app_status == "running" || prev.app_status == "maintenance") {
        super::projects::restart_project_internal(&state, &actor, &updated).await?;
    }
    log_audit_project(&state, Some(id), &actor.username, "app.limits", Some(&updated.name), Some(&format!("Limits: {}", describe(&l)))).await;
    state.cache.invalidate_all();
    Ok(Json(updated))
}

/// Sets a database's limits. CPU and memory apply the next time the database starts.
pub(crate) async fn set_database_limits_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(l): Json<AppLimits>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    l.validate().map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let db = state
        .databases
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Database not found".to_string()))?;
    app_limits::set(&state.db, Kind::Database, id, &l).await.map_err(internal)?;
    log_audit(&state, &actor.username, "database.limits", Some(&db.name), Some(&format!("Limits: {}", describe(&l)))).await;
    state.cache.invalidate_all();
    Ok(Json(serde_json::json!({ "limits": l, "appliesOnRestart": l.cpu > 0 || l.memory_mb > 0 })))
}

const MB: u64 = 1024 * 1024;

/// Marks an error as an app-limit refusal so the panel can show a dialog explaining it (the UI
/// strips the tag). Kinds: `disk`, `backups`.
pub(crate) const DISK_LIMIT_TAG: &str = "[app-limit:disk] ";
pub(crate) const BACKUP_LIMIT_TAG: &str = "[app-limit:backups] ";

fn over_message(name: &str, used: u64, limit: u64) -> String {
    format!(
        "'{name}' is over its disk limit ({:.1} MB used of {:.1} MB). Free space or ask an admin to raise the limit.",
        used as f64 / MB as f64,
        limit as f64 / MB as f64
    )
}

/// `Some((used, limit))` in bytes when a website or service on this server is over its disk limit.
fn project_over_disk(p: &crate::projects::Project) -> Option<(u64, u64)> {
    if p.disk_limit_mb <= 0 {
        return None;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let (used, _) = crate::api::system::compute_project_disk_formatted(p, &cwd);
    let limit = p.disk_limit_mb as u64 * MB;
    (used > limit).then_some((used, limit))
}

fn database_over_disk(data_dir: &str, disk_limit_mb: i64) -> Option<(u64, u64)> {
    if disk_limit_mb <= 0 {
        return None;
    }
    let used = crate::monitor::get_path_disk_size(std::path::Path::new(data_dir));
    let limit = disk_limit_mb as u64 * MB;
    (used > limit).then_some((used, limit))
}

/// Refuses to start a local website or service that is over its disk limit.
pub(crate) fn ensure_project_under_disk_limit(p: &crate::projects::Project) -> Result<(), ApiError> {
    match project_over_disk(p) {
        Some((used, limit)) => Err((StatusCode::CONFLICT, format!("{DISK_LIMIT_TAG}{}", over_message(&p.name, used, limit)))),
        None => Ok(()),
    }
}

/// Refuses to start a database that is over its disk limit.
pub(crate) async fn ensure_database_under_disk_limit(state: &AppState, id: i64) -> Result<(), ApiError> {
    let Some(db) = state.databases.get(id).await.map_err(internal)? else { return Ok(()) };
    let limit = app_limits::get(&state.db, Kind::Database, id).await.map_err(internal)?.map_or(0, |l| l.disk_mb);
    match database_over_disk(&db.data_dir, limit) {
        Some((used, limit)) => Err((StatusCode::CONFLICT, format!("{DISK_LIMIT_TAG}{}", over_message(&db.name, used, limit)))),
        None => Ok(()),
    }
}

/// Stops every local app that is over its disk limit. Runs on a timer.
pub(crate) async fn enforce_disk_limits(state: &AppState) {
    let local = state.cluster.default_local_node_id().await;
    let on_this_server = |node: Option<i64>| node.is_none() || node == local;

    if let Ok(projects) = state.projects.list().await {
        for p in projects.into_iter().filter(|p| p.disk_limit_mb > 0 && on_this_server(p.node_id)) {
            if p.app_status != "running" && p.app_status != "maintenance" {
                continue;
            }
            let probe = p.clone();
            let Some((used, limit)) = tokio::task::spawn_blocking(move || project_over_disk(&probe)).await.ok().flatten() else {
                continue;
            };
            if state.runtime.stop_project(&p).await.is_ok() {
                let msg = over_message(&p.name, used, limit);
                log_audit_project(state, Some(p.id), "system", "app.disk_stop", Some(&p.name), Some(&format!("Stopped: {msg}"))).await;
                crate::maintenance::notify(&state.db, "app.disk", format!("WebDep: stopped {}", msg)).await;
                state.cache.invalidate_all();
            }
        }
    }

    let rows: Vec<(i64, String, String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, name, data_dir, disk_limit_mb, node_id FROM databases WHERE disk_limit_mb > 0 AND status = 'running'",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    for (id, name, data_dir, limit_mb, node) in rows {
        if !on_this_server(node) {
            continue;
        }
        let Some((used, limit)) = tokio::task::spawn_blocking(move || database_over_disk(&data_dir, limit_mb)).await.ok().flatten() else {
            continue;
        };
        if state.databases.stop(id).await.is_ok() {
            let msg = over_message(&name, used, limit);
            log_audit(state, "system", "database.disk_stop", Some(&name), Some(&format!("Stopped: {msg}"))).await;
            crate::maintenance::notify(&state.db, "app.disk", format!("WebDep: stopped {}", msg)).await;
            state.cache.invalidate_all();
        }
    }
}
