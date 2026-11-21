//! Admin Panel > IP Addresses: node port pools, app ports, and extra ports.
//!
//! Routes (admins only):
//! - `GET    /api/cluster/nodes/:id/ports`           pool, open/closed state, who uses each port
//! - `PUT    /api/cluster/nodes/:id/port-range`      `{ min, max }`
//! - `POST   /api/cluster/nodes/:id/ports`           `{ ports: "8100, 8105-8110" }` add ports
//! - `PATCH  /api/cluster/nodes/:id/ports`           `{ ports: "...", open: bool }`
//! - `DELETE /api/cluster/nodes/:id/ports/:port`     remove a hand-added port
//! - `POST   /api/cluster/nodes/:id/firewall-sync`   re-apply open/closed ports to the firewall
//! - `PUT    /api/projects/:id/port`                 `{ port }` move an app to another port (empty = next free)
//! - `PUT    /api/projects/:id/extra-ports`          `{ ports: "..." }` extra ports for an app

use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;

use super::common::*;
use crate::cluster::ports;

type ApiError = (StatusCode, String);

fn bad(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn internal(e: impl ToString) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

async fn load_node(state: &AppState, node_id: i64) -> Result<crate::cluster::ClusterNode, ApiError> {
    state
        .cluster
        .get_node(node_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Node #{node_id} not found")))
}

/// Applies firewall changes on the node that owns the ports (locally or over RPC).
/// Best effort: an unreachable node is reported, the pool change itself still stands.
async fn apply_firewall(state: &AppState, node: &crate::cluster::ClusterNode, open: Vec<u16>, close: Vec<u16>) -> Option<String> {
    if open.is_empty() && close.is_empty() {
        return None;
    }
    if node.is_local {
        return match tokio::task::spawn_blocking(move || crate::runtime::port::sync_firewall_ports(&open, &close)).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(e) => Some(e.to_string()),
        };
    }
    if !state.cluster.is_node_connected(node.id).await {
        return Some(format!("{} is offline; its firewall updates when you sync it again.", node.name));
    }
    state
        .cluster
        .call_node(node.id, "firewall.ports", serde_json::json!({ "open": open, "close": close }), std::time::Duration::from_secs(60))
        .await
        .err()
        .map(|(_, msg)| msg)
}

async fn pool_response(state: &AppState, node_id: i64, firewall_note: Option<String>) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = ports::load_pool(&state.db, node_id).await.map_err(internal)?;
    Ok(Json(serde_json::json!({
        "pool": pool,
        "firewallManaged": crate::runtime::port::firewall_management_enabled(),
        "firewallNote": firewall_note,
    })))
}

pub(crate) async fn get_node_ports_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_admin(&state, &headers).await?;
    load_node(&state, node_id).await?;
    pool_response(&state, node_id, None).await
}

#[derive(Deserialize)]
pub(crate) struct RangePayload {
    min: u16,
    max: u16,
}

pub(crate) async fn set_port_range_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
    Json(p): Json<RangePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    let (added, removed) = ports::set_range(&state.db, node_id, p.min, p.max).await.map_err(bad)?;
    let note = apply_firewall(&state, &node, added.clone(), removed.clone()).await;
    log_audit(
        &state,
        &actor.username,
        "ports.range",
        Some(&node.name),
        Some(&format!("Port range {}-{} ({} opened, {} removed)", p.min, p.max, added.len(), removed.len())),
    )
    .await;
    pool_response(&state, node_id, note).await
}

#[derive(Deserialize)]
pub(crate) struct PortsPayload {
    ports: String,
    #[serde(default)]
    open: Option<bool>,
}

pub(crate) async fn add_node_ports_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
    Json(p): Json<PortsPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    let list = ports::parse_port_list(&p.ports).map_err(bad)?;
    let added = ports::add_ports(&state.db, node_id, &list).await.map_err(bad)?;
    let note = apply_firewall(&state, &node, added.clone(), Vec::new()).await;
    log_audit(&state, &actor.username, "ports.add", Some(&node.name), Some(&format!("Added {} port(s): {}", added.len(), p.ports.trim()))).await;
    pool_response(&state, node_id, note).await
}

pub(crate) async fn set_node_ports_open_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
    Json(p): Json<PortsPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    let open = p.open.ok_or_else(|| bad("Say whether to open or close the ports."))?;
    let list = ports::parse_port_list(&p.ports).map_err(bad)?;
    let changed = ports::set_open(&state.db, node_id, &list, open).await.map_err(bad)?;
    let note = if open {
        apply_firewall(&state, &node, changed.clone(), Vec::new()).await
    } else {
        apply_firewall(&state, &node, Vec::new(), changed.clone()).await
    };
    log_audit(
        &state,
        &actor.username,
        if open { "ports.open" } else { "ports.close" },
        Some(&node.name),
        Some(&format!("{} {} port(s): {}", if open { "Opened" } else { "Closed" }, changed.len(), p.ports.trim())),
    )
    .await;
    pool_response(&state, node_id, note).await
}

pub(crate) async fn remove_node_port_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((node_id, port)): AxPath<(i64, u16)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    ports::remove_port(&state.db, node_id, port).await.map_err(bad)?;
    let note = apply_firewall(&state, &node, Vec::new(), vec![port]).await;
    log_audit(&state, &actor.username, "ports.remove", Some(&node.name), Some(&format!("Removed port {port}"))).await;
    pool_response(&state, node_id, note).await
}

pub(crate) async fn sync_node_firewall_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    let (open, closed) = ports::firewall_plan(&state.db, node_id).await;
    let note = apply_firewall(&state, &node, open, closed).await;
    pool_response(&state, node_id, note).await
}

/// Node an app runs on (apps without a node run on the local Core node).
async fn app_node(state: &AppState, project: &crate::projects::Project) -> Result<crate::cluster::ClusterNode, ApiError> {
    let node_id = match project.node_id {
        Some(id) => id,
        None => state
            .cluster
            .default_local_node_id()
            .await
            .ok_or_else(|| bad("This app has no node."))?,
    };
    load_node(state, node_id).await
}

#[derive(Deserialize)]
pub(crate) struct AppPortPayload {
    /// Port to use; `None` picks the node's next free port.
    port: Option<u16>,
}

/// Moves an app to another port on its node, then refreshes its process, firewall and Caddy
/// routes the same way saving its settings does.
pub(crate) async fn set_app_port_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(p): Json<AppPortPayload>,
) -> Result<Json<crate::projects::Project>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let prev = state
        .projects
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "App not found".to_string()))?;
    let node = app_node(&state, &prev).await?;
    let current = prev.assigned_port.or(prev.port);
    let pooled = ports::has_pool(&state.db, node.id).await;
    let target = match (p.port, pooled) {
        (Some(port), true) => {
            ports::check_assignable(&state.db, node.id, port, Some(id)).await.map_err(bad)?;
            port
        }
        (Some(port), false) => crate::db::allocate_global_unique_port(&state.db, Some(port), port, Some(id), None)
            .await
            .map_err(bad)?,
        (None, true) => ports::next_free_port(&state.db, node.id, &current.into_iter().collect::<Vec<_>>())
            .await
            .map_err(internal)?
            .ok_or_else(|| bad("No free open port left on this node. Add or open more ports."))?,
        (None, false) => return Err(bad("Choose a port: this node has no port range yet.")),
    };
    if Some(target) == current {
        return Ok(Json(prev));
    }
    if target < ports::MIN_POOL_PORT {
        return Err(bad(format!("Ports below {} are reserved for the system.", ports::MIN_POOL_PORT)));
    }

    sqlx::query("UPDATE projects SET port = $1, assigned_port = CASE WHEN assigned_port IS NULL THEN NULL ELSE $1 END WHERE id = $2")
        .bind(i32::from(target))
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    let updated = state
        .projects
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "App not found".to_string()))?;

    super::projects::propagate_project_change(&state, Some(&prev), &updated).await?;
    // Saving drops the old port's firewall rule; a pool port that is still open keeps it.
    if let Some(old) = current.filter(|_| pooled) {
        let still_open: bool = sqlx::query_scalar("SELECT is_open FROM node_ports WHERE node_id = $1 AND port = $2")
            .bind(node.id)
            .bind(i32::from(old))
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
        if still_open {
            let _ = apply_firewall(&state, &node, vec![old, target], Vec::new()).await;
        }
    }
    if prev.app_status == "running" || prev.app_status == "maintenance" {
        super::projects::restart_project_internal(&state, &actor, &updated).await?;
    }
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        "app.port",
        Some(&updated.name),
        Some(&format!("Moved from port {} to {target}", current.map_or("-".to_string(), |c| c.to_string()))),
    )
    .await;
    state.cache.invalidate_all();
    Ok(Json(updated))
}

pub(crate) async fn set_extra_ports_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(p): Json<PortsPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let project = state
        .projects
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "App not found".to_string()))?;
    let node = app_node(&state, &project).await?;
    if !ports::has_pool(&state.db, node.id).await {
        return Err(bad("Give this node a port range first."));
    }
    let list = if p.ports.trim().is_empty() { Vec::new() } else { ports::parse_port_list(&p.ports).map_err(bad)? };
    if list.len() > 20 {
        return Err(bad("An app can have up to 20 extra ports."));
    }
    let main = project.assigned_port.or(project.port);
    if main.is_some_and(|m| list.contains(&m)) {
        return Err(bad("That port is already the app's main port."));
    }
    ports::set_extra_ports(&state.db, node.id, id, &list).await.map_err(bad)?;

    // The app reads its extra ports from EXTRA_PORTS (comma separated).
    let value = list.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    if value.is_empty() {
        let _ = state.projects.delete_env_var(id, "EXTRA_PORTS").await;
    } else {
        state.projects.set_env_var(id, "EXTRA_PORTS", &value, false).await.map_err(internal)?;
    }
    let note = apply_firewall(&state, &node, list.clone(), Vec::new()).await;
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        "app.extra_ports",
        Some(&project.name),
        Some(&if value.is_empty() { "Removed extra ports".to_string() } else { format!("Extra ports: {value} (restart the app to use them)") }),
    )
    .await;
    state.cache.invalidate_all();
    Ok(Json(serde_json::json!({ "ok": true, "extraPorts": list, "firewallNote": note })))
}
