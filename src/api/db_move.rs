//! Admin Panel > Move Apps: moving a database to another node.
//!
//! Routes (admins only):
//! - `POST /api/databases/:id/move`         `{ nodeId }` start moving the database
//! - `GET  /api/databases/:id/move-status`  progress of the latest move
//!
//! The data is copied with `pg_dump` / `pg_restore` over the mesh, while the old database keeps
//! running. Only when the copy is complete does the database switch to the new node; any failure
//! before that removes the new copy and leaves the old database exactly as it was. The app linked
//! to the database is stopped for the copy (so nothing is written in between), gets the new
//! connection settings, and is started again if it was running.

use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use super::common::*;
use crate::cluster::ports;
use crate::databases::{ManagedDatabase, PgBinaries};

type ApiError = (StatusCode, String);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MoveStatus {
    /// `running`, `done` or `failed`.
    pub state: String,
    pub step: String,
    pub message: String,
    pub node_id: i64,
}

static MOVES: Mutex<Option<HashMap<i64, MoveStatus>>> = Mutex::new(None);

fn set_status(id: i64, state: &str, step: &str, message: &str, node_id: i64) {
    let mut guard = MOVES.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(HashMap::new).insert(
        id,
        MoveStatus { state: state.into(), step: step.into(), message: message.into(), node_id },
    );
}

fn get_status(id: i64) -> Option<MoveStatus> {
    MOVES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&id).cloned())
}

fn is_moving(id: i64) -> bool {
    get_status(id).is_some_and(|s| s.state == "running")
}

#[derive(Deserialize)]
pub(crate) struct MovePayload {
    #[serde(alias = "nodeId", alias = "node_id")]
    node_id: i64,
}

pub(crate) async fn move_database_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(p): Json<MovePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let db = state
        .databases
        .get(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Database not found".to_string()))?;
    if is_moving(id) {
        return Err((StatusCode::CONFLICT, "This database is already being moved.".to_string()));
    }
    let target = state
        .cluster
        .get_node(p.node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Node not found".to_string()))?;
    let local = state.cluster.default_local_node_id().await;
    if db.node_id.or(local) == Some(target.id) {
        return Err((StatusCode::BAD_REQUEST, "The database is already on that node.".to_string()));
    }
    if !target.is_local && !state.cluster.is_node_connected(target.id).await {
        return Err((StatusCode::SERVICE_UNAVAILABLE, format!("Node '{}' is offline.", target.name)));
    }

    set_status(id, "running", "Starting", "", target.id);
    let (task_state, node_id) = (state.clone(), target.id);
    tokio::spawn(async move {
        match run_move(&task_state, &actor, db, target).await {
            Ok(()) => set_status(id, "done", "Moved", "", node_id),
            Err(e) => {
                tracing::warn!("Moving database #{id} failed: {e}");
                set_status(id, "failed", "Failed", &e, node_id);
            }
        }
        task_state.cache.invalidate_all();
    });
    Ok(Json(serde_json::json!({ "started": true })))
}

pub(crate) async fn move_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_admin(&state, &headers).await?;
    Ok(Json(serde_json::json!({ "status": get_status(id) })))
}

/// Picks a port for the database on the node: the lowest free pool port, or a free one when the
/// node has no pool.
async fn pick_port(state: &AppState, node_id: i64, node_name: &str) -> Result<u16, String> {
    if ports::has_pool(&state.db, node_id).await {
        return ports::next_free_port(&state.db, node_id, &[])
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| ports::no_free_port_message(node_name));
    }
    crate::db::allocate_global_unique_port(&state.db, None, 5432, None, None).await
}

async fn run_move(state: &AppState, actor: &crate::auth::User, rec: ManagedDatabase, target: crate::cluster::ClusterNode) -> Result<(), String> {
    let id = rec.id;
    let step = |s: &str| set_status(id, "running", s, "", target.id);

    // 1. Everything this needs must be in place before anything is touched.
    step("Checking the new node");
    let bins = PgBinaries::detect().map_err(|_| "PostgreSQL tools are missing on this server, so the data can't be copied.".to_string())?;
    let target_remote = state.databases.remote_node(Some(target.id)).await;
    match &target_remote {
        Some((cluster, nid)) => {
            state.databases.call_remote(cluster, *nid, "db.check", serde_json::json!({}), 30).await?;
        }
        None => {
            PgBinaries::detect().map_err(|_| "PostgreSQL is not installed on this server.".to_string())?;
        }
    }
    let target_host = match &target_remote {
        Some(_) => {
            if target.mesh_ip.trim().is_empty() {
                return Err(format!("Node '{}' has no mesh address yet, so it can't be reached from here.", target.name));
            }
            target.mesh_ip.trim().to_string()
        }
        None => "127.0.0.1".to_string(),
    };
    let new_port = pick_port(state, target.id, &target.name).await?;
    let source_host = state.databases.connect_host(&rec).await?;
    let password = rec.db_password.clone().filter(|p| !p.is_empty()).ok_or("The database password isn't available.")?;

    // 2. The old database must be running to be copied.
    if rec.status != "running" {
        step("Starting the database");
        state.databases.start(id).await?;
    }

    // 3. Start the empty copy on the new node.
    step("Starting the database engine on the new node");
    let mut spec = state.databases.engine_spec(&rec).await;
    spec.port = new_port;
    let target_dir = state.databases.local_dir(id);
    match &target_remote {
        Some((cluster, nid)) => {
            state.databases.call_remote(cluster, *nid, "db.start", serde_json::json!({ "spec": spec }), 180).await?;
        }
        None => {
            spec.db_dir = target_dir.clone();
            let _ = tokio::fs::remove_dir_all(&target_dir).await;
            let _ = tokio::fs::create_dir_all(&target_dir).await;
            crate::databases::Databases::sync_firewall_rules(new_port, &rec.access_mode, &rec.allowed_ips).await;
            crate::databases::start_engine(&spec, &|_, _| {}).await?;
        }
    }
    let cleanup_target = |reason: String| {
        let (state, target_remote, target_dir, allowed) = (state.clone(), target_remote.clone(), target_dir.clone(), rec.allowed_ips.clone());
        async move {
            match target_remote {
                Some((cluster, nid)) => {
                    let params = serde_json::json!({ "id": id, "port": new_port, "allowedIps": allowed });
                    let _ = state.databases.call_remote(&cluster, nid, "db.delete", params, 120).await;
                }
                None => {
                    crate::databases::stop_engine(&target_dir, new_port).await;
                    crate::databases::Databases::remove_firewall_rules(new_port, &allowed).await;
                    let _ = tokio::fs::remove_dir_all(&target_dir).await;
                }
            }
            reason
        }
    };

    // 4. Stop the linked app so nothing is written while the data is copied.
    let linked = match rec.linked_project_id {
        Some(pid) => state.projects.get(pid).await.ok().flatten(),
        None => None,
    };
    let app_was_running = linked.as_ref().is_some_and(|p| p.app_status == "running" || p.app_status == "maintenance");
    if let (Some(p), true) = (&linked, app_was_running) {
        step("Stopping the linked app");
        if let Err(e) = stop_app(state, p).await {
            return Err(cleanup_target(format!("The linked app could not be stopped, so nothing was moved: {e}")).await);
        }
    }

    // 5. Copy the data.
    step("Copying the data");
    let dump_file = state.cluster.data_dir().join("tmp").join(format!("db-move-{id}.dump"));
    let src = crate::db_backups::DbConn { host: source_host, port: rec.port, user: rec.db_user.clone(), password: password.clone(), database: rec.db_name.clone() };
    let dst = crate::db_backups::DbConn { host: target_host, port: new_port, user: rec.db_user.clone(), password, database: rec.db_name.clone() };
    let copied = async {
        let info = crate::db_backups::create_dump(&bins, &src, &dump_file).await?;
        crate::db_backups::restore_dump(&bins, &dst, &dump_file, &info.sha256).await
    }
    .await;
    let _ = tokio::fs::remove_file(&dump_file).await;
    if let Err(e) = copied {
        // The old database was never touched: bring the app back and drop the copy.
        if let (Some(p), true) = (&linked, app_was_running) {
            let _ = crate::api::projects::restart_project_internal(state, actor, p).await;
        }
        return Err(cleanup_target(format!("Copying failed, so the database stays where it was. {e}")).await);
    }

    // 6. Switch over, then remove the old copy.
    step("Switching to the new node");
    let source_remote = state.databases.remote_node(rec.node_id).await;
    sqlx::query("UPDATE databases SET node_id = $1, port = $2, status = 'running', started_at = $3, updated_at = $3 WHERE id = $4")
        .bind(target.id)
        .bind(i32::from(new_port))
        .bind(crate::db::now())
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| format!("The data was copied but the switch failed: {e}"))?;

    step("Removing the old copy");
    match source_remote {
        Some((cluster, nid)) => {
            let params = serde_json::json!({ "id": id, "port": rec.port, "allowedIps": rec.allowed_ips });
            let _ = state.databases.call_remote(&cluster, nid, "db.delete", params, 120).await;
        }
        None => {
            let old_dir = state.databases.local_dir(id);
            // A move back onto this server reuses the same folder, which now holds the new copy.
            if target_remote.is_some() {
                crate::databases::stop_engine(&old_dir, rec.port).await;
                crate::databases::Databases::remove_firewall_rules(rec.port, &rec.allowed_ips).await;
                let _ = tokio::fs::remove_dir_all(&old_dir).await;
            }
        }
    }

    // 7. Point the linked app at the new place.
    if let Some(p) = &linked {
        step("Updating the linked app");
        if let Ok(Some(fresh)) = state.databases.get(id).await {
            let _ = super::databases::write_link_env(state, p, &fresh).await;
        }
        if app_was_running {
            let _ = crate::api::projects::restart_project_internal(state, actor, p).await;
        }
    }
    log_audit(
        state,
        &actor.username,
        "database.move",
        Some(&database_tag(id)),
        Some(&format!("Moved to node '{}' (port {new_port})", target.name)),
    )
    .await;
    Ok(())
}

async fn stop_app(state: &AppState, p: &crate::projects::Project) -> Result<(), String> {
    match state.cluster.should_proxy_to_node(p.node_id).await? {
        Some(nid) => state
            .cluster
            .call_node(nid, "project.stop", serde_json::json!({ "project": p }), std::time::Duration::from_secs(30))
            .await
            .map(|_| ())
            .map_err(|(_, m)| m),
        None => state.runtime.stop_project(p).await,
    }
}
