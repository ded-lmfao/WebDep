//! Admin Panel > IP Addresses: node port pools, app ports, and extra ports.
//!
//! Routes (admins only):
//! - `GET    /api/cluster/nodes/:id/ports`           pool, open/closed state, who uses each port
//! - `PUT    /api/cluster/nodes/:id/port-range`      `{ min, max }`
//! - `POST   /api/cluster/nodes/:id/ports`           `{ ports: "8100, 8105-8110" }` add ports
//! - `PATCH  /api/cluster/nodes/:id/ports`           `{ ports: "...", open: bool }`
//! - `DELETE /api/cluster/nodes/:id/ports/:port`     remove an unused port
//! - `POST   /api/cluster/nodes/:id/ports/remove`    `{ ports: "..." }` remove unused ports
//! - `POST   /api/cluster/nodes/:id/firewall-sync`   re-apply open/closed ports to the firewall
//! - `PUT    /api/projects/:id/port`                 `{ port }` move an app to another port (empty = next free)
//! - `PUT    /api/projects/:id/extra-ports`          `{ ports: "..." }` extra ports for an app
//! - `GET    /api/cluster/nodes/:id/ips`             the node's IP addresses and the apps on each
//! - `POST   /api/cluster/nodes/:id/ips`             `{ ip, label }` add an IP address
//! - `DELETE /api/cluster/nodes/:id/ips/:ip_id`      remove an IP no app uses
//! - `PUT    /api/projects/:id/ip`                   `{ ip }` serve an app on one IP ("" = every IP)

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

/// Brings the node's firewall in line with the pool for `changed` ports: open and public
/// ports are allowed, everything else loses WebDep's allow rule. Returns a note when the
/// change couldn't be applied yet (offline node, firewall error); the pool change stands and
/// the node shows as out of sync until it is applied.
async fn sync_ports(state: &AppState, node: &crate::cluster::ClusterNode, changed: Vec<u16>) -> Option<String> {
    if changed.is_empty() {
        return None;
    }
    let (allow, deny) = ports::firewall_targets(&state.db, node.id, &changed).await;
    ports::apply_on_node(&state.db, &state.cluster, node, allow, deny).await.err()
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
    let note = sync_ports(&state, &node, added.iter().chain(removed.iter()).copied().collect()).await;
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
    /// Make the ports reachable from the internet (true) or private (false).
    #[serde(default)]
    public: Option<bool>,
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
    let note = sync_ports(&state, &node, added.clone()).await;
    log_audit(&state, &actor.username, "ports.add", Some(&node.name), Some(&format!("Added {} port(s): {}", added.len(), p.ports.trim()))).await;
    pool_response(&state, node_id, note).await
}

/// `PATCH /api/cluster/nodes/:id/ports` with `{ ports, open }` and/or `{ ports, public }`.
pub(crate) async fn set_node_ports_open_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
    Json(p): Json<PortsPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    if p.open.is_none() && p.public.is_none() {
        return Err(bad("Say whether to open, close, or change who can reach the ports."));
    }
    let list = ports::parse_port_list(&p.ports).map_err(bad)?;
    let mut changed = std::collections::BTreeSet::new();
    if let Some(open) = p.open {
        let c = ports::set_open(&state.db, node_id, &list, open).await.map_err(bad)?;
        log_audit(
            &state,
            &actor.username,
            if open { "ports.open" } else { "ports.close" },
            Some(&node.name),
            Some(&format!("{} {} port(s): {}", if open { "Opened" } else { "Closed" }, c.len(), p.ports.trim())),
        )
        .await;
        changed.extend(c);
    }
    if let Some(public) = p.public {
        let c = ports::set_public(&state.db, node_id, &list, public).await.map_err(bad)?;
        log_audit(
            &state,
            &actor.username,
            if public { "ports.public" } else { "ports.private" },
            Some(&node.name),
            Some(&format!("Made {} port(s) {}: {}", c.len(), if public { "public" } else { "private" }, p.ports.trim())),
        )
        .await;
        changed.extend(c);
    }
    let note = sync_ports(&state, &node, changed.into_iter().collect()).await;
    pool_response(&state, node_id, note).await
}

pub(crate) async fn remove_node_port_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((node_id, port)): AxPath<(i64, u16)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    remove_ports_common(&state, &headers, node_id, &[port]).await
}

/// `POST /api/cluster/nodes/:id/ports/remove` with `{ ports: "..." }`.
pub(crate) async fn remove_node_ports_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
    Json(p): Json<PortsPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let list = ports::parse_port_list(&p.ports).map_err(bad)?;
    remove_ports_common(&state, &headers, node_id, &list).await
}

async fn remove_ports_common(state: &AppState, headers: &HeaderMap, node_id: i64, list: &[u16]) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(state, headers).await?;
    let node = load_node(state, node_id).await?;
    let removed = ports::remove_ports(&state.db, node_id, list).await.map_err(bad)?;
    let note = sync_ports(state, &node, removed.clone()).await;
    let shown = removed.iter().map(u16::to_string).collect::<Vec<_>>().join(", ");
    log_audit(state, &actor.username, "ports.remove", Some(&node.name), Some(&format!("Removed {} port(s): {shown}", removed.len()))).await;
    pool_response(state, node_id, note).await
}

pub(crate) async fn sync_node_firewall_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    let (allow, deny) = ports::firewall_plan(&state.db, node_id).await;
    let note = ports::apply_on_node(&state.db, &state.cluster, &node, allow, deny).await.err();
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
            .ok_or_else(|| bad(ports::no_free_port_message(&node.name)))?,
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
    // Saving applied the app's access mode to the new port and dropped the old port's rules;
    // put the old port back to what the pool says (private unless marked public).
    if let Some(old) = current.filter(|_| pooled) {
        let _ = sync_ports(&state, &node, vec![old]).await;
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
    let note = sync_ports(&state, &node, list.clone()).await;
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

async fn database_node(state: &AppState, db: &crate::databases::ManagedDatabase) -> Result<crate::cluster::ClusterNode, ApiError> {
    let node_id = match db.node_id {
        Some(id) => id,
        None => state.cluster.default_local_node_id().await.ok_or_else(|| bad("This database has no node."))?,
    };
    load_node(state, node_id).await
}

async fn load_database(state: &AppState, id: i64) -> Result<crate::databases::ManagedDatabase, ApiError> {
    state
        .databases
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Database not found".to_string()))
}

/// Moves a database to another port on its node. A running database restarts on the new port,
/// and the app it is linked to gets the new connection settings.
pub(crate) async fn set_database_port_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(p): Json<AppPortPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let db = load_database(&state, id).await?;
    let node = database_node(&state, &db).await?;
    let current = db.port;
    let pooled = ports::has_pool(&state.db, node.id).await;
    let target = match (p.port, pooled) {
        (Some(port), true) => {
            ports::check_assignable_for(&state.db, node.id, port, ports::Owner::Database(id)).await.map_err(bad)?;
            port
        }
        (Some(port), false) => crate::db::allocate_global_unique_port(&state.db, Some(port), port, None, Some(id)).await.map_err(bad)?,
        (None, true) => ports::next_free_port(&state.db, node.id, &[current])
            .await
            .map_err(internal)?
            .ok_or_else(|| bad(ports::no_free_port_message(&node.name)))?,
        (None, false) => return Err(bad("Choose a port: this node has no port range yet.")),
    };
    if target == current {
        return Ok(Json(serde_json::json!({ "ok": true, "port": current })));
    }
    if target < ports::MIN_POOL_PORT {
        return Err(bad(format!("Ports below {} are reserved for the system.", ports::MIN_POOL_PORT)));
    }
    state.databases.set_port(id, target).await.map_err(internal)?;
    if pooled {
        let _ = sync_ports(&state, &node, vec![current]).await;
    }
    if let Some(pid) = db.linked_project_id {
        if let Some(project) = state.projects.get(pid).await.map_err(internal)? {
            let fresh = load_database(&state, id).await?;
            super::databases::write_link_env(&state, &project, &fresh).await?;
        }
    }
    log_audit(
        &state,
        &actor.username,
        "database.port",
        Some(&database_tag(id)),
        Some(&format!("Moved from port {current} to {target}")),
    )
    .await;
    state.cache.invalidate_all();
    Ok(Json(serde_json::json!({ "ok": true, "port": target, "linkedAppNeedsRestart": db.linked_project_id.is_some() })))
}

pub(crate) async fn set_database_extra_ports_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(p): Json<PortsPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let db = load_database(&state, id).await?;
    let node = database_node(&state, &db).await?;
    if !ports::has_pool(&state.db, node.id).await {
        return Err(bad("Give this node a port range first."));
    }
    let list = if p.ports.trim().is_empty() { Vec::new() } else { ports::parse_port_list(&p.ports).map_err(bad)? };
    if list.len() > 20 {
        return Err(bad("A database can have up to 20 extra ports."));
    }
    if list.contains(&db.port) {
        return Err(bad("That port is already the database's main port."));
    }
    ports::set_database_extra_ports(&state.db, node.id, id, &list).await.map_err(bad)?;
    let note = sync_ports(&state, &node, list.clone()).await;
    let value = list.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    log_audit(
        &state,
        &actor.username,
        "database.extra_ports",
        Some(&database_tag(id)),
        Some(&if value.is_empty() { "Removed extra ports".to_string() } else { format!("Extra ports: {value}") }),
    )
    .await;
    state.cache.invalidate_all();
    Ok(Json(serde_json::json!({ "ok": true, "extraPorts": list, "firewallNote": note })))
}


// ---------------------------------------------------------------------------
// Node IP addresses and per-app IP / resource limits
// ---------------------------------------------------------------------------

async fn ips_response(state: &AppState, node_id: i64) -> Result<Json<serde_json::Value>, ApiError> {
    let ips = crate::cluster::node_ips::list(&state.db, node_id).await.map_err(internal)?;
    Ok(Json(serde_json::json!({ "ips": ips })))
}

pub(crate) async fn list_node_ips_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_admin(&state, &headers).await?;
    load_node(&state, node_id).await?;
    ips_response(&state, node_id).await
}

#[derive(Deserialize)]
pub(crate) struct AddIpPayload {
    ip: String,
    #[serde(default)]
    label: String,
}

pub(crate) async fn add_node_ip_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
    Json(p): Json<AddIpPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    let ip = crate::cluster::node_ips::add(&state.db, node_id, &p.ip, &p.label).await.map_err(bad)?;
    log_audit(&state, &actor.username, "ip.add", Some(&node.name), Some(&format!("Added IP address {ip}"))).await;
    ips_response(&state, node_id).await
}

pub(crate) async fn remove_node_ip_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((node_id, ip_id)): AxPath<(i64, i64)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let node = load_node(&state, node_id).await?;
    let ip = crate::cluster::node_ips::remove(&state.db, node_id, ip_id).await.map_err(bad)?;
    log_audit(&state, &actor.username, "ip.remove", Some(&node.name), Some(&format!("Removed IP address {ip}"))).await;
    ips_response(&state, node_id).await
}

async fn load_app(state: &AppState, id: i64) -> Result<crate::projects::Project, ApiError> {
    state
        .projects
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "App not found".to_string()))
}

#[derive(Deserialize)]
pub(crate) struct AppIpPayload {
    #[serde(default)]
    ip: String,
}

/// Serves an app's domains on one of its node's IPs (or every IP), then rewrites Caddy.
pub(crate) async fn set_app_ip_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(p): Json<AppIpPayload>,
) -> Result<Json<crate::projects::Project>, ApiError> {
    let actor = verify_admin(&state, &headers).await?;
    let prev = load_app(&state, id).await?;
    let node = app_node(&state, &prev).await?;
    let ip = if p.ip.trim().is_empty() {
        String::new()
    } else {
        crate::cluster::node_ips::normalize_ip(&p.ip).map_err(bad)?
    };
    crate::cluster::node_ips::check_assignable(&state.db, node.id, &ip).await.map_err(bad)?;
    if ip == prev.bind_ip {
        return Ok(Json(prev));
    }
    sqlx::query("UPDATE projects SET bind_ip = $1 WHERE id = $2")
        .bind(&ip)
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;
    let updated = load_app(&state, id).await?;
    super::projects::propagate_project_change(&state, Some(&prev), &updated).await?;
    let shown = if ip.is_empty() { "every IP address".to_string() } else { ip.clone() };
    log_audit_project(&state, Some(id), &actor.username, "app.ip", Some(&updated.name), Some(&format!("Now served on {shown}"))).await;
    state.cache.invalidate_all();
    Ok(Json(updated))
}
