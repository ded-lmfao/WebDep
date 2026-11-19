use axum::{
    extract::{Path as AxPath, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use super::common::*;
use crate::cluster::ws::{accept_ws_upgrade, http_base_to_ws_url, WsMessage};
use crate::cluster::{
    ClusterNode, ClusterWsMessage, CreateEnrollTokenInput, CreateEnrollTokenResponse,
    EnrollNodeRequest, EnrollNodeResponse, NodeEnrollTokenView,
};

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DrainNodeInput {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub draining: Option<bool>,
}

fn infer_core_urls(headers: &HeaderMap, state: &AppState) -> (String, String) {
    let proto = if is_https_request(headers, state.auth.force_secure_cookies) {
        "https"
    } else {
        "http"
    };
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("127.0.0.1:8080");
    let http_url = format!("{proto}://{host}");
    let ws_url = http_base_to_ws_url(&http_url, "/api/cluster/node-ws")
        .unwrap_or_else(|_| format!("ws://{host}/api/cluster/node-ws"));
    (http_url, ws_url)
}

fn extract_remote_ip(req: &Request) -> String {
    if let Some(xff) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(first) = xff.split(',').next().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return first.to_string();
        }
    }
    if let Some(real_ip) = req
        .headers()
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return real_ip.to_string();
    }
    if let Some(info) = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
    {
        return info.0.ip().to_string();
    }
    "127.0.0.1".to_string()
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    auth.strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) async fn list_cluster_nodes_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ClusterNode>>, (StatusCode, String)> {
    let user = require_current_user(&state, &headers).await?;
    let mut nodes = state
        .cluster
        .list_nodes()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    // Every signed-in user needs node names (cards, target-node pickers), but network details
    // are infrastructure secrets: only operators with `system_manage` see them.
    if !user.has_power("system_manage") {
        for n in nodes.iter_mut() {
            redact_node_network_details(n);
        }
    }
    Ok(Json(nodes))
}

async fn resume_running_workloads_on_node(state: &AppState, node_id: i64) {
    let Ok(projects) = state.projects.list_for_node(node_id).await else {
        return;
    };
    for p in projects.into_iter().filter(|p| p.app_status == "running") {
        let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
        match state
            .cluster
            .call_node(
                node_id,
                "project.resume",
                serde_json::json!({ "project": &p, "context": ctx }),
                std::time::Duration::from_secs(60),
            )
            .await
        {
            Ok(v) if v["started"].as_bool() == Some(true) => {
                let pid = v["pid"].as_i64();
                let _ = sqlx::query("UPDATE projects SET app_pid = $1, app_started_at = $2 WHERE id = $3")
                    .bind(pid)
                    .bind(crate::db::now())
                    .bind(p.id)
                    .execute(&state.db)
                    .await;
                state.cache.invalidate_all();
            }
            Ok(_) => {}
            Err((_, e)) => {
                state.logs.add(
                    Some(p.id),
                    None,
                    "system",
                    "warn",
                    &format!("Could not resume '{}' on node #{node_id} after it reconnected: {e}", p.name),
                );
            }
        }
    }
}

fn redact_node_network_details(n: &mut ClusterNode) {
    n.public_ip.clear();
    n.wg_pubkey.clear();
    n.wg_endpoint.clear();
}

pub(crate) async fn delete_enroll_token_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let admin = verify_main_admin(&state, &headers).await?;
    state.cluster.delete_enroll_token(id).await.map_err(|e| {
        let code = if e.contains("not found") { StatusCode::NOT_FOUND } else { StatusCode::INTERNAL_SERVER_ERROR };
        (code, e)
    })?;
    let id_str = id.to_string();
    log_audit_project(&state, None, &admin.username, "cluster.enroll_token.delete", Some(&id_str), Some("Revoked node join token")).await;
    Ok(Json(serde_json::json!({ "ok": true, "deletedId": id })))
}

pub(crate) async fn list_enroll_tokens_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<NodeEnrollTokenView>>, (StatusCode, String)> {
    let _admin = verify_main_admin(&state, &headers).await?;
    let tokens = state
        .cluster
        .list_enroll_tokens()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(tokens))
}

fn extract_host_and_port(headers: &HeaderMap) -> (String, String, u16) {
    let raw = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("127.0.0.1:4600");

    let env_port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(4600);

    if let Some(rest) = raw.strip_prefix('[') {
        if let Some((ip6, after)) = rest.split_once(']') {
            let p = after
                .strip_prefix(':')
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(env_port);
            return (raw.to_string(), ip6.to_string(), p);
        }
    }
    if let Some((h, p_str)) = raw.rsplit_once(':') {
        if let Ok(p) = p_str.parse::<u16>() {
            return (raw.to_string(), h.to_string(), p);
        }
    }
    (raw.to_string(), raw.to_string(), env_port)
}

fn is_loopback_host(h: &str) -> bool {
    let lower = h.trim().to_ascii_lowercase();
    lower == "127.0.0.1" || lower == "localhost" || lower == "::1" || lower == "0.0.0.0"
}

fn detect_lan_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    if !ip.is_loopback() && !ip.is_unspecified() {
        Some(ip.to_string())
    } else {
        None
    }
}

async fn detect_public_ip_cached() -> Option<String> {
    static PUBLIC_IP_CACHE: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    PUBLIC_IP_CACHE
        .get_or_init(|| async {
            if let Ok(env_ip) = std::env::var("PUBLIC_IP") {
                let t = env_ip.trim().to_string();
                if !t.is_empty() && !is_loopback_host(&t) {
                    return Some(t);
                }
            }
            tokio::task::spawn_blocking(|| {
                for svc in ["https://api.ipify.org", "https://ifconfig.me/ip"] {
                    let out = std::process::Command::new("curl")
                        .args(["-fsS", "--max-time", "2", svc])
                        .output()
                        .ok();
                    if let Some(o) = out {
                        if o.status.success() {
                            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                            if !s.is_empty()
                                && s.len() <= 64
                                && !s.contains(char::is_whitespace)
                                && !is_loopback_host(&s)
                            {
                                return Some(s);
                            }
                        }
                    }
                }
                None
            })
            .await
            .ok()
            .flatten()
        })
        .await
        .clone()
}

pub(crate) async fn cluster_panel_info_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _admin = verify_main_admin(&state, &headers).await?;
    let (request_url, _) = infer_core_urls(&headers, &state);
    let (_, req_host, port) = extract_host_and_port(&headers);
    let proto = if is_https_request(&headers, state.auth.force_secure_cookies) {
        "https"
    } else {
        "http"
    };

    let lan_ip = detect_lan_ip().unwrap_or_default();
    let mut public_ip = detect_public_ip_cached().await.unwrap_or_default();
    if public_ip.is_empty() && !is_loopback_host(&req_host) {
        public_ip = req_host.clone();
    }

    let suggested_panel_url = if !is_loopback_host(&req_host) {
        request_url.clone()
    } else if !public_ip.is_empty() {
        format!("{proto}://{public_ip}:{port}")
    } else if !lan_ip.is_empty() {
        format!("{proto}://{lan_ip}:{port}")
    } else {
        request_url.clone()
    };

    let (repo_slug, _) = state.updater.get_repo_and_token().await;

    Ok(Json(serde_json::json!({
        "suggestedPanelUrl": suggested_panel_url,
        "requestUrl": request_url,
        "publicIp": public_ip,
        "lanIp": lan_ip,
        "port": port,
        "repoSlug": repo_slug,
    })))
}

pub(crate) async fn create_enroll_token_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<CreateEnrollTokenInput>,
) -> Result<(StatusCode, Json<CreateEnrollTokenResponse>), (StatusCode, String)> {
    let admin = verify_main_admin(&state, &headers).await?;
    let (inferred_http_url, _) = infer_core_urls(&headers, &state);
    let core_http_url = input
        .core_url
        .as_deref()
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .map(|s| s.to_string())
        .unwrap_or(inferred_http_url);

    if input.repo_slug.as_deref().map(|s| s.trim()).unwrap_or("").is_empty() {
        let (slug, _) = state.updater.get_repo_and_token().await;
        input.repo_slug = Some(slug);
    }

    let created = state
        .cluster
        .create_enroll_token(Some(admin.id), input, &core_http_url)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let tok_id_str = created.id.to_string();
    let detail = format!(
        "Created single-use node join token '{}' (region {})",
        created.label, created.region
    );
    log_audit_project(
        &state,
        None,
        &admin.username,
        "cluster.token.create",
        Some(&tok_id_str),
        Some(&detail),
    )
    .await;
    Ok((StatusCode::CREATED, Json(created)))
}

pub(crate) async fn enroll_node_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<EnrollNodeResponse>, (StatusCode, String)> {
    let (_, core_ws_url) = infer_core_urls(req.headers(), &state);
    let remote_ip = extract_remote_ip(&req);

    let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid request body: {e}")))?;
    let payload: EnrollNodeRequest = serde_json::from_slice(&bytes)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Malformed JSON payload: {e}")))?;

    let enrolled = state
        .cluster
        .enroll_node(payload, &remote_ip, &core_ws_url)
        .await
        .map_err(|e| {
            let status = if e.contains("token") {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, e)
        })?;

    let nid_str = enrolled.node_id.to_string();
    let detail = format!(
        "Enrolled Web-Node '{}' (#{}, region '{}', mesh {})",
        enrolled.name, enrolled.node_id, enrolled.region, enrolled.mesh_ip
    );
    log_audit_project(
        &state,
        None,
        "cluster",
        "cluster.node.enroll",
        Some(&nid_str),
        Some(&detail),
    )
    .await;

    Ok(Json(enrolled))
}

pub(crate) async fn drain_cluster_node_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    body: Option<Json<DrainNodeInput>>,
) -> Result<Json<ClusterNode>, (StatusCode, String)> {
    let admin = verify_main_admin(&state, &headers).await?;
    let input = body.map(|b| b.0).unwrap_or_default();
    let target_status = if let Some(s) = input.status {
        s
    } else if let Some(draining) = input.draining {
        if draining { "draining".to_string() } else { "online".to_string() }
    } else {
        match state.cluster.get_node(id).await {
            Ok(Some(existing)) if existing.status == "draining" => "online".to_string(),
            Ok(Some(_)) => "draining".to_string(),
            Ok(None) => return Err((StatusCode::NOT_FOUND, format!("Node #{id} not found"))),
            Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
        }
    };

    let updated = state
        .cluster
        .set_node_status(id, &target_status)
        .await
        .map_err(|e| {
            let code = if e.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, e)
        })?;

    let id_str = id.to_string();
    let detail = format!("Set Web-Node '{}' status to {}", updated.name, updated.status);
    log_audit_project(
        &state,
        None,
        &admin.username,
        "cluster.node.status",
        Some(&id_str),
        Some(&detail),
    )
    .await;
    Ok(Json(updated))
}

pub(crate) async fn delete_cluster_node_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let admin = verify_main_admin(&state, &headers).await?;
    let existing = state
        .cluster
        .get_node(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Node #{id} not found")))?;

    state.cluster.delete_node(id).await.map_err(|e| {
        let code = if e.contains("still assigned") {
            StatusCode::CONFLICT
        } else if e.contains("not found") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        (code, e)
    })?;

    let id_str = id.to_string();
    let detail = format!("Removed Web-Node '{}' (#{}) from cluster", existing.name, id);
    log_audit_project(
        &state,
        None,
        &admin.username,
        "cluster.node.delete",
        Some(&id_str),
        Some(&detail),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true, "deletedId": id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateNodePayload {
    pub display_name: String,
}

/// Renames a node in the panel (display name only). An empty name restores the node's own name.
pub(crate) async fn update_cluster_node_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<UpdateNodePayload>,
) -> Result<Json<crate::cluster::models::ClusterNode>, (StatusCode, String)> {
    let admin = verify_main_admin(&state, &headers).await?;
    let node = state
        .cluster
        .set_node_display_name(id, &payload.display_name)
        .await
        .map_err(|e| {
            let code = if e.contains("not found") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
            (code, e)
        })?;

    let id_str = id.to_string();
    let detail = if node.display_name.is_empty() {
        format!("Cleared display name of Web-Node '{}' (#{})", node.name, id)
    } else {
        format!("Renamed Web-Node '{}' (#{}) to '{}'", node.name, id, node.display_name)
    };
    log_audit_project(&state, None, &admin.username, "cluster.node.rename", Some(&id_str), Some(&detail)).await;
    Ok(Json(node))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvacuateNodePayload {
    #[serde(default)]
    pub target_node_id: Option<i64>,
}

pub(crate) async fn evacuate_cluster_node_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    payload: Option<Json<EvacuateNodePayload>>,
) -> Result<Json<crate::cluster::shift::EvacuateNodeResult>, (StatusCode, String)> {
    let admin = verify_main_admin(&state, &headers).await?;
    let pref = payload.and_then(|p| p.target_node_id);
    let res = crate::cluster::shift::evacuate_node_workloads(&state, &admin.username, id, pref).await?;
    let id_str = id.to_string();
    log_audit_project(
        &state,
        None,
        &admin.username,
        "cluster.node.evacuate",
        Some(&id_str),
        Some(&format!(
            "Evacuated node #{id}: shifted={}, awaiting_dns={}, failed={}",
            res.shifted.len(),
            res.awaiting_dns.len(),
            res.failed.len()
        )),
    )
    .await;
    Ok(Json(res))
}

pub(crate) async fn move_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<crate::cluster::shift::StartShiftPayload>,
) -> Result<Json<crate::cluster::shift::AppMigrationView>, (StatusCode, String)> {
    let actor = require_current_user(&state, &headers).await?;
    crate::cluster::shift::verify_cross_node_shift_rbac(
        &state,
        &actor,
        id,
        payload.target_node_id,
    )
    .await?;

    let mig = crate::cluster::shift::start_project_shift(
        &state,
        &actor.username,
        id,
        payload.target_node_id,
        false,
        payload.auto_cutover_without_dns.unwrap_or(false),
    )
    .await?;

    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        "project.shift.start",
        Some(&mig.project_name),
        Some(&format!(
            "Started live shift to node #{} (status: {})",
            payload.target_node_id, mig.status
        )),
    )
    .await;
    Ok(Json(mig))
}

pub(crate) async fn list_project_migrations_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<Vec<crate::cluster::shift::AppMigrationView>>, (StatusCode, String)> {
    let _user = verify_any_power(&state, &headers, &["projects_view", "projects_manage", "deploy"], Some(id)).await?;
    let list = crate::cluster::shift::list_project_migrations(&state, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(list))
}

pub(crate) async fn get_migration_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(mid): AxPath<i64>,
) -> Result<Json<crate::cluster::shift::AppMigrationView>, (StatusCode, String)> {
    let mig = crate::cluster::shift::get_migration(&state, mid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Migration #{mid} not found")))?;
    let _user = verify_any_power(
        &state,
        &headers,
        &["projects_view", "projects_manage", "deploy"],
        Some(mig.project_id),
    )
    .await?;
    Ok(Json(mig))
}

pub(crate) async fn check_migration_dns_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(mid): AxPath<i64>,
) -> Result<Json<crate::cluster::shift::CheckDnsResponse>, (StatusCode, String)> {
    let mig = crate::cluster::shift::get_migration(&state, mid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Migration #{mid} not found")))?;
    let _user = verify_any_power(
        &state,
        &headers,
        &["deploy", "projects_manage", "domains"],
        Some(mig.project_id),
    )
    .await?;
    let res = crate::cluster::shift::check_migration_dns(&state, mid).await?;
    Ok(Json(res))
}

pub(crate) async fn complete_migration_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(mid): AxPath<i64>,
) -> Result<Json<crate::cluster::shift::AppMigrationView>, (StatusCode, String)> {
    let actor = require_current_user(&state, &headers).await?;
    let mig = crate::cluster::shift::get_migration(&state, mid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Migration #{mid} not found")))?;
    if let Some(to_nid) = mig.to_node_id {
        crate::cluster::shift::verify_cross_node_shift_rbac(&state, &actor, mig.project_id, to_nid)
            .await?;
    }
    let cutover_mode = if mig.dns_verified { "dns" } else { "manual" };
    let completed =
        crate::cluster::shift::complete_migration_cutover(&state, &actor.username, mid, cutover_mode)
            .await?;
    Ok(Json(completed))
}

pub(crate) async fn abort_migration_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(mid): AxPath<i64>,
) -> Result<Json<crate::cluster::shift::AppMigrationView>, (StatusCode, String)> {
    let actor = require_current_user(&state, &headers).await?;
    let mig = crate::cluster::shift::get_migration(&state, mid)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Migration #{mid} not found")))?;
    let _user = verify_any_power(
        &state,
        &headers,
        &["deploy", "projects_manage", "services_manage"],
        Some(mig.project_id),
    )
    .await?;
    let aborted = crate::cluster::shift::abort_migration(&state, &actor.username, mid).await?;
    Ok(Json(aborted))
}

pub(crate) async fn fleet_update_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Option<Json<crate::updater::TriggerUpdatePayload>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;
    let tag = payload.and_then(|p| p.tag.clone());
    crate::api::system::settings::trigger_fleet_and_core_update(&state, &actor.username, tag).await
}

pub(crate) async fn node_ws_handler(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, (StatusCode, String)> {
    let token = extract_bearer_token(req.headers())
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Missing Bearer node token".to_string()))?;

    let node = state
        .cluster
        .verify_node_token(&token)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Invalid Web-Node credentials".to_string()))?;

    let peer_ip = extract_remote_ip(&req);
    let (resp, rx) = accept_ws_upgrade(req)
        .map_err(|code| (code, "WebSocket upgrade required".to_string()))?;

    let cluster = state.cluster.clone();
    let logs = state.logs.clone();
    let cache = state.cache.clone();
    let resume_state = state.clone();
    let node_is_local = node.is_local;
    tokio::spawn(async move {
        let Ok(ws) = rx.await else {
            return;
        };
        let node_id = node.id;
        let (mut reader, mut writer) = ws.into_split();
        let (tx, mut out_rx) = mpsc::unbounded_channel::<ClusterWsMessage>();

        cluster.register_peer_sender(node_id, tx).await;
        let _ = cluster.send_peer_sync_to(node_id).await;

        // A remote Web-Node that restarted (reboot, crash, upgrade) has lost its app processes while the
        // Core still lists them as running. Ask it to bring them back; it skips apps that are alive.
        // (Apps on the Core's own node are resumed by the Core runtime at startup.)
        if !node_is_local {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                resume_running_workloads_on_node(&resume_state, node_id).await;
            });
        }

        let writer_handle = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Ok(json) = serde_json::to_string(&msg) {
                    if writer.send_text(&json).await.is_err() {
                        break;
                    }
                }
            }
        });

        while let Ok(frame) = reader.recv().await {
            match frame {
                WsMessage::Text(txt) => {
                    if let Ok(msg) = serde_json::from_str::<ClusterWsMessage>(&txt) {
                        match msg {
                            ClusterWsMessage::Register {
                                node_version,
                                wg_pubkey,
                                wg_endpoint,
                                public_ip,
                                capabilities,
                                workloads,
                            } => {
                                let resolved_ip = if public_ip.trim().is_empty() {
                                    peer_ip.as_str()
                                } else {
                                    public_ip.as_str()
                                };
                                let _ = cluster
                                    .mark_node_online(
                                        node_id,
                                        &node_version,
                                        &wg_pubkey,
                                        &wg_endpoint,
                                        resolved_ip,
                                        &capabilities,
                                    )
                                    .await;
                                cluster.apply_remote_workloads_heartbeat(node_id, &workloads).await;
                                cluster.broadcast_peer_sync().await;
                            }
                            ClusterWsMessage::Heartbeat { metrics, workloads, .. } => {
                                let _ = cluster.update_node_metrics(node_id, &metrics).await;
                                cluster.apply_remote_workloads_heartbeat(node_id, &workloads).await;
                            }
                            ClusterWsMessage::RpcResponse {
                                req_id,
                                ok,
                                status,
                                result,
                                error,
                            } => {
                                cluster.resolve_rpc(&req_id, ok, status, result, error).await;
                            }
                            ClusterWsMessage::LogChunk {
                                project_id,
                                deployment_id,
                                channel,
                                level,
                                message,
                                ..
                            } => {
                                logs.add(project_id, deployment_id, &channel, &level, &message);
                            }
                            ClusterWsMessage::DeployStateUpdate { update } => {
                                cluster.apply_remote_deploy_update(&logs, &cache, update).await;
                            }
                            ClusterWsMessage::Ping { ts } => {
                                let _ = cluster.send_peer_sync_to(node_id).await;
                                let _ = ts;
                            }
                            _ => {}
                        }
                    }
                }
                WsMessage::Close => break,
                _ => {}
            }
        }

        cluster.unregister_peer_sender(node_id).await;
        let _ = cluster.mark_node_offline(node_id).await;
        writer_handle.abort();
    });

    Ok(resp)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallModulesInput {
    #[serde(default)]
    pub module_ids: Vec<String>,
}

fn align_node_modules(
    catalog: &[crate::cluster::modules::ModuleCatalogEntry],
    reported: &[crate::cluster::modules::NodeModuleStatus],
) -> Vec<crate::cluster::modules::NodeModuleStatus> {
    catalog
        .iter()
        .map(|entry| {
            reported
                .iter()
                .find(|m| m.id == entry.id)
                .cloned()
                .unwrap_or_else(|| crate::cluster::modules::NodeModuleStatus {
                    id: entry.id.clone(),
                    installed: false,
                    version: String::new(),
                    binary_path: String::new(),
                    details: "Not yet scanned on node".to_string(),
                })
        })
        .collect()
}

fn build_node_modules_view(
    node: &ClusterNode,
    catalog: &[crate::cluster::modules::ModuleCatalogEntry],
) -> serde_json::Value {
    let aligned = align_node_modules(catalog, &node.capabilities.modules);
    let mut required_missing: Vec<String> = Vec::new();
    let mut installed_count = 0usize;
    for entry in catalog {
        if let Some(st) = aligned.iter().find(|m| m.id == entry.id) {
            if st.installed {
                installed_count += 1;
            } else if entry.required {
                required_missing.push(entry.id.clone());
            }
        }
    }
    let pm = if node.capabilities.package_manager.trim().is_empty() {
        if node.capabilities.os == "windows" {
            "manual".to_string()
        } else {
            "apt-get".to_string()
        }
    } else {
        node.capabilities.package_manager.clone()
    };

    serde_json::json!({
        "nodeId": node.id,
        "name": node.name,
        "region": node.region,
        "status": node.status,
        "isLocal": node.is_local,
        "os": node.capabilities.os,
        "arch": node.capabilities.arch,
        "packageManager": pm,
        "installedCount": installed_count,
        "totalCount": catalog.len(),
        "requiredMissing": required_missing,
        "requiredMissingCount": required_missing.len(),
        "allRequiredReady": required_missing.is_empty(),
        "modules": aligned,
        "lastSeenAt": node.last_seen_at,
    })
}

async fn refresh_single_node_caps(
    state: &AppState,
    node: &ClusterNode,
) -> Result<crate::cluster::models::NodeCapabilities, (StatusCode, String)> {
    let updated_caps = if node.is_local {
        let p2p_port = node.capabilities.p2p_port;
        tokio::task::spawn_blocking(move || crate::cluster::mesh::detect_capabilities_with_p2p(p2p_port))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        let rpc_res = state
            .cluster
            .call_node(
                node.id,
                "modules.status",
                serde_json::json!({}),
                std::time::Duration::from_secs(20),
            )
            .await?;
        serde_json::from_value::<crate::cluster::models::NodeCapabilities>(
            rpc_res["capabilities"].clone(),
        )
        .unwrap_or_else(|_| {
            let mut c = node.capabilities.clone();
            if let Some(pm) = rpc_res["packageManager"].as_str() {
                c.package_manager = pm.to_string();
            }
            if let Ok(mods) = serde_json::from_value(rpc_res["modules"].clone()) {
                c.modules = mods;
            }
            c
        })
    };

    state
        .cluster
        .update_node_capabilities(node.id, &updated_caps)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(updated_caps)
}

#[derive(Clone, Copy)]
enum ModuleAction {
    Install,
    Uninstall,
}

impl ModuleAction {
    fn verb(self) -> &'static str {
        match self {
            ModuleAction::Install => "install",
            ModuleAction::Uninstall => "uninstall",
        }
    }

    fn rpc_method(self) -> &'static str {
        match self {
            ModuleAction::Install => "modules.install",
            ModuleAction::Uninstall => "modules.uninstall",
        }
    }

    fn run_local(self, ids: &[String]) -> Vec<crate::cluster::modules::ModuleInstallResult> {
        match self {
            ModuleAction::Install => crate::cluster::modules::install_node_modules_sync(ids).0,
            ModuleAction::Uninstall => crate::cluster::modules::uninstall_node_modules_sync(ids).0,
        }
    }
}

/// Runs a module install/uninstall on one node (locally or over RPC), stores the refreshed
/// capabilities, writes the audit line and returns the node's updated module view.
async fn apply_modules_action(
    state: &AppState,
    actor_username: &str,
    node: &ClusterNode,
    catalog: &[crate::cluster::modules::ModuleCatalogEntry],
    target_ids: &[String],
    action: ModuleAction,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let (results, updated_caps) = if node.is_local {
        let ids_c = target_ids.to_vec();
        let p2p_port = node.capabilities.p2p_port;
        tokio::task::spawn_blocking(move || {
            let results = action.run_local(&ids_c);
            let caps = crate::cluster::mesh::detect_capabilities_with_p2p(p2p_port);
            (results, caps)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        let rpc_res = state
            .cluster
            .call_node(
                node.id,
                action.rpc_method(),
                serde_json::json!({ "moduleIds": target_ids }),
                std::time::Duration::from_secs(600),
            )
            .await?;
        let results: Vec<crate::cluster::modules::ModuleInstallResult> =
            serde_json::from_value(rpc_res["results"].clone()).unwrap_or_default();
        let caps = serde_json::from_value::<crate::cluster::models::NodeCapabilities>(
            rpc_res["capabilities"].clone(),
        )
        .unwrap_or_else(|_| {
            let mut c = node.capabilities.clone();
            if let Some(pm) = rpc_res["packageManager"].as_str() {
                c.package_manager = pm.to_string();
            }
            if let Ok(mods) = serde_json::from_value(rpc_res["modules"].clone()) {
                c.modules = mods;
            }
            c
        });
        (results, caps)
    };

    state
        .cluster
        .update_node_capabilities(node.id, &updated_caps)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let mut refreshed_node = node.clone();
    refreshed_node.capabilities = updated_caps;

    let all_ok = results.iter().all(|r| r.success);
    state.logs.add(
        None,
        None,
        "system",
        if all_ok { "info" } else { "warn" },
        &format!(
            "VPS module {} on '{}' [{}] by {}: {}",
            action.verb(),
            node.name,
            target_ids.join(", "),
            actor_username,
            if all_ok { "completed" } else { "partial/failed" }
        ),
    );

    Ok(serde_json::json!({
        "ok": all_ok,
        "nodeId": node.id,
        "nodeName": node.name,
        "skipped": false,
        "results": results,
        "node": build_node_modules_view(&refreshed_node, catalog),
    }))
}

async fn run_install_on_single_node(
    state: &AppState,
    actor_username: &str,
    node: &ClusterNode,
    catalog: &[crate::cluster::modules::ModuleCatalogEntry],
    requested_ids: &[String],
) -> Result<serde_json::Value, (StatusCode, String)> {
    let mut target_ids: Vec<String> = requested_ids
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if target_ids.is_empty() {
        let aligned = align_node_modules(catalog, &node.capabilities.modules);
        target_ids = catalog
            .iter()
            .filter(|entry| {
                entry.required
                    && !aligned
                        .iter()
                        .find(|m| m.id == entry.id)
                        .map(|m| m.installed)
                        .unwrap_or(false)
            })
            .map(|e| e.id.clone())
            .collect();
    }
    if target_ids.is_empty() {
        return Ok(serde_json::json!({
            "ok": true,
            "nodeId": node.id,
            "nodeName": node.name,
            "skipped": true,
            "results": [],
            "node": build_node_modules_view(node, catalog),
        }));
    }

    apply_modules_action(state, actor_username, node, catalog, &target_ids, ModuleAction::Install).await
}

pub(crate) async fn list_cluster_modules_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Read-only readiness view (no network details) is available to every signed-in user;
    // installing packages stays Main Admin only.
    let _user = require_current_user(&state, &headers).await?;
    let catalog = crate::cluster::modules::module_catalog();
    let mut nodes = state
        .cluster
        .list_nodes()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    for n in &mut nodes {
        if n.is_local && n.capabilities.modules.is_empty() {
            if let Ok(caps) = refresh_single_node_caps(&state, n).await {
                n.capabilities = caps;
            }
        }
    }

    let node_views: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| build_node_modules_view(n, &catalog))
        .collect();

    Ok(Json(serde_json::json!({
        "catalog": catalog,
        "nodes": node_views,
    })))
}

pub(crate) async fn refresh_node_modules_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let _user = require_current_user(&state, &headers).await?;
    let mut node = state
        .cluster
        .get_node(node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Node #{node_id} not found")))?;
    let caps = refresh_single_node_caps(&state, &node).await?;
    node.capabilities = caps;
    let catalog = crate::cluster::modules::module_catalog();
    Ok(Json(serde_json::json!({
        "ok": true,
        "node": build_node_modules_view(&node, &catalog),
    })))
}

/// Loads a node for a module action, refusing remote nodes that are not online.
async fn reachable_node(state: &AppState, node_id: i64) -> Result<ClusterNode, (StatusCode, String)> {
    let node = state
        .cluster
        .get_node(node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Node #{node_id} not found")))?;
    if !node.is_local && !matches!(node.status.as_str(), "online" | "draining") {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Node '{}' is currently '{}'", node.name, node.status),
        ));
    }
    Ok(node)
}

pub(crate) async fn install_node_modules_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
    Json(input): Json<InstallModulesInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;
    let node = reachable_node(&state, node_id).await?;
    let catalog = crate::cluster::modules::module_catalog();
    let res =
        run_install_on_single_node(&state, &actor.username, &node, &catalog, &input.module_ids)
            .await?;
    Ok(Json(res))
}

async fn run_uninstall_on_single_node(
    state: &AppState,
    actor_username: &str,
    node: &ClusterNode,
    catalog: &[crate::cluster::modules::ModuleCatalogEntry],
    requested_ids: &[String],
) -> Result<serde_json::Value, (StatusCode, String)> {
    let target_ids: Vec<String> = requested_ids
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if target_ids.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "No moduleIds specified for uninstall".to_string()));
    }

    apply_modules_action(state, actor_username, node, catalog, &target_ids, ModuleAction::Uninstall).await
}

pub(crate) async fn uninstall_node_modules_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(node_id): AxPath<i64>,
    Json(input): Json<InstallModulesInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;
    let node = reachable_node(&state, node_id).await?;
    let catalog = crate::cluster::modules::module_catalog();
    let res =
        run_uninstall_on_single_node(&state, &actor.username, &node, &catalog, &input.module_ids)
            .await?;
    Ok(Json(res))
}

pub(crate) async fn install_fleet_modules_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<InstallModulesInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;
    let nodes = state
        .cluster
        .list_nodes()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let catalog = crate::cluster::modules::module_catalog();

    let mut per_node = Vec::new();
    for node in nodes {
        if !node.is_local && !matches!(node.status.as_str(), "online" | "draining") {
            per_node.push(serde_json::json!({
                "ok": false,
                "nodeId": node.id,
                "nodeName": node.name,
                "skipped": true,
                "error": format!("Node is '{}' and unreachable", node.status),
                "results": [],
            }));
            continue;
        }
        match run_install_on_single_node(&state, &actor.username, &node, &catalog, &input.module_ids)
            .await
        {
            Ok(v) => per_node.push(v),
            Err((_code, msg)) => per_node.push(serde_json::json!({
                "ok": false,
                "nodeId": node.id,
                "nodeName": node.name,
                "skipped": false,
                "error": msg,
                "results": [],
            })),
        }
    }

    let all_ok = per_node.iter().all(|v| v["ok"].as_bool().unwrap_or(false) || v["skipped"].as_bool().unwrap_or(false));
    Ok(Json(serde_json::json!({
        "ok": all_ok,
        "nodes": per_node,
    })))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use axum::body::Body;
    use axum::http::Method;
    use tower::ServiceExt;

    #[tokio::test]
    async fn cluster_api_auth_enrollment_drain_and_delete() {
        let state = crate::api::common::tests_state_helper().await;
        let users = state.auth.list_users().await.unwrap();
        let admin = users.into_iter().find(|u| u.is_main_admin).unwrap();
        let admin_sess = state.auth.create_session(admin.id).await.unwrap();
        let admin_sid = admin_sess.id;
        let admin_csrf = admin_sess.csrf_token;

        let dev_id: i64 = sqlx::query_scalar(
            "INSERT INTO users(username, password_hash, permissions, allowed_projects, created_at)
             VALUES('cluster_dev', 'hash', '[]', '[]', 1700000000) RETURNING id",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        let dev_sess = state.auth.create_session(dev_id).await.unwrap();
        let dev_sid = dev_sess.id;
        let dev_csrf = dev_sess.csrf_token;

        let router = crate::api::create_router(state.clone());

        // Non-admin cannot generate enrollment token
        let denied = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/cluster/enroll-tokens")
                    .header(header::COOKIE, format!("nd_session={dev_sid}"))
                    .header("x-csrf-token", &dev_csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"label":"node-a"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        // Main admin creates enrollment token
        let tok_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/cluster/enroll-tokens")
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .header("x-csrf-token", &admin_csrf)
                    .header(header::HOST, "panel.test:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"label":"us-west-1","region":"us-west"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tok_res.status(), StatusCode::CREATED);
        let tok_body = axum::body::to_bytes(tok_res.into_body(), 1024 * 64)
            .await
            .unwrap();
        let tok_json: CreateEnrollTokenResponse = serde_json::from_slice(&tok_body).unwrap();
        assert!(tok_json.join_command.contains("--node --join http://panel.test:8080"));
        assert!(tok_json.fresh_vps_command.starts_with("curl -fsSL https://raw.githubusercontent.com/"));
        assert!(tok_json.fresh_vps_command.contains("/install.sh | sudo bash -s -- --node --join "));
        assert!(tok_json.service_command.contains("sudo bash start.sh --service --node"));

        // Panel info + full join command with detected/custom Core URL & VPS 2 IP
        let info_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/cluster/panel-info")
                    .header("Cookie", format!("nd_session={admin_sid}"))
                    .header(header::HOST, "203.0.113.10:4600")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(info_res.status(), StatusCode::OK);
        let info_body = axum::body::to_bytes(info_res.into_body(), 1024 * 64).await.unwrap();
        let info_json: serde_json::Value = serde_json::from_slice(&info_body).unwrap();
        assert_eq!(info_json["suggestedPanelUrl"], "http://203.0.113.10:4600");

        let full_tok_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/cluster/enroll-tokens")
                    .header("Cookie", format!("nd_session={admin_sid}"))
                    .header("X-CSRF-Token", &admin_csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "label": "vps-2",
                            "region": "mumbai-IND",
                            "coreUrl": "http://203.0.113.10:4600",
                            "targetPublicIp": "198.51.100.77"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(full_tok_res.status(), StatusCode::CREATED);
        let full_tok: CreateEnrollTokenResponse = serde_json::from_slice(
            &axum::body::to_bytes(full_tok_res.into_body(), 64 * 1024).await.unwrap(),
        )
        .unwrap();
        assert!(full_tok.fresh_vps_command.contains("--join http://203.0.113.10:4600"));
        assert!(full_tok.fresh_vps_command.contains("--public-ip \"198.51.100.77\""));
        assert!(full_tok.fresh_vps_command.contains("--name \"vps-2\""));
        assert!(full_tok.fresh_vps_command.contains("--region \"mumbai-IND\""));

        // Node enrolls with single-use token (no cookie needed)
        let enroll_payload = serde_json::json!({
            "token": tok_json.token,
            "name": "us-west-1",
            "wgPubkey": "pubKeyTest==",
            "nodeVersion": "0.2.2"
        });
        let enroll_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/cluster/enroll")
                    .header(header::HOST, "panel.test:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(enroll_payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(enroll_res.status(), StatusCode::OK);
        let enroll_body = axum::body::to_bytes(enroll_res.into_body(), 1024 * 64)
            .await
            .unwrap();
        let enrolled: EnrollNodeResponse = serde_json::from_slice(&enroll_body).unwrap();
        assert_eq!(enrolled.mesh_ip, "10.200.0.1");
        assert_eq!(enrolled.region, "us-west");

        // Cluster nodes listing includes enrolled node
        let nodes_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/cluster/nodes")
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(nodes_res.status(), StatusCode::OK);
        let nodes_bytes = axum::body::to_bytes(nodes_res.into_body(), 1024 * 64)
            .await
            .unwrap();
        let nodes_text = String::from_utf8_lossy(&nodes_bytes);
        assert!(nodes_text.contains("us-west-1"));

        // Drain node & delete node
        let drain_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/cluster/nodes/{}/drain", enrolled.node_id))
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .header("x-csrf-token", &admin_csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(drain_res.status(), StatusCode::OK);

        let del_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/cluster/nodes/{}", enrolled.node_id))
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .header("x-csrf-token", &admin_csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_res.status(), StatusCode::OK);

        // Unauthorized WS rejected cleanly
        let ws_denied = router
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/cluster/node-ws")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ws_denied.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn phase4_ws_rpc_streaming_and_remote_http_proxy() {
        let state = crate::api::common::tests_state_helper().await;
        let users = state.auth.list_users().await.unwrap();
        let admin = users.into_iter().find(|u| u.is_main_admin).unwrap();
        let admin_sess = state.auth.create_session(admin.id).await.unwrap();
        let admin_sid = admin_sess.id;
        let admin_csrf = admin_sess.csrf_token;
        let router = crate::api::create_router(state.clone());

        // Create restricted file-only user (no "env" power) to verify Core RBAC before remote proxy
        let file_user_id: i64 = sqlx::query_scalar(
            "INSERT INTO users(username, password_hash, permissions, allowed_projects, created_at)
             VALUES('remote_file_user', 'hash', '[\"files\",\"files_write\"]', '[\"*\"]', 1700000000)
             RETURNING id",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        crate::auth::grants::replace_from_legacy_json(&state.db, file_user_id, r#"["files","files_write"]"#, r#"["*"]"#, None)
            .await
            .unwrap();
        let file_sess = state.auth.create_session(file_user_id).await.unwrap();

        // 1. Enroll remote node
        let tok_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/cluster/enroll-tokens")
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .header("x-csrf-token", &admin_csrf)
                    .header(header::HOST, "panel.test:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"label":"phase4-worker","region":"eu-central"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tok_res.status(), StatusCode::CREATED);
        let tok_body = axum::body::to_bytes(tok_res.into_body(), 64 * 1024).await.unwrap();
        let tok_json: CreateEnrollTokenResponse = serde_json::from_slice(&tok_body).unwrap();

        let enroll_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/cluster/enroll")
                    .header(header::HOST, "panel.test:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "token": tok_json.token,
                            "name": "phase4-worker",
                            "wgPubkey": "pubKeyPhase4==",
                            "nodeVersion": "0.2.3"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(enroll_res.status(), StatusCode::OK);
        let enroll_body = axum::body::to_bytes(enroll_res.into_body(), 64 * 1024).await.unwrap();
        let enrolled: EnrollNodeResponse = serde_json::from_slice(&enroll_body).unwrap();

        // 2. Offline node RPC fails with 503 Service Unavailable
        let offline_err = state
            .cluster
            .call_node(
                enrolled.node_id,
                "files.list",
                serde_json::json!({}),
                std::time::Duration::from_secs(2),
            )
            .await;
        assert!(matches!(offline_err, Err((StatusCode::SERVICE_UNAVAILABLE, _))));

        // Deterministic RPC timeout test via synthetic unresponsive peer channel
        let (dead_tx, _dead_rx) = tokio::sync::mpsc::unbounded_channel();
        state.cluster.register_peer_sender(999_999, dead_tx).await;
        let timeout_err = state
            .cluster
            .call_node(
                999_999,
                "slow.op",
                serde_json::json!({}),
                std::time::Duration::from_millis(50),
            )
            .await;
        state.cluster.unregister_peer_sender(999_999).await;
        assert!(matches!(timeout_err, Err((StatusCode::GATEWAY_TIMEOUT, _))));

        // 3. Serve router & connect real Web-Node WebSocket + NodeExecState executor
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let temp_root = std::env::temp_dir().join(format!("webdep-phase4-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_root);
        let worker_data_dir = temp_root.join("worker-data");
        let remote_site_dir = temp_root.join("remote-site");
        std::fs::create_dir_all(&worker_data_dir).unwrap();

        let ws_url = format!("ws://{addr}/api/cluster/node-ws");
        let ws = crate::cluster::ws::connect_ws(&ws_url, Some(&enrolled.node_token))
            .await
            .expect("WS connect_ws should succeed");
        let (mut ws_reader, mut ws_writer) = ws.into_split();

        let reg = ClusterWsMessage::Register {
            node_version: "0.2.3".to_string(),
            wg_pubkey: "pubKeyPhase4==".to_string(),
            wg_endpoint: String::new(),
            public_ip: "127.0.0.1".to_string(),
            capabilities: crate::cluster::models::NodeCapabilities::default(),
            workloads: Vec::new(),
        };
        ws_writer
            .send_text(&serde_json::to_string(&reg).unwrap())
            .await
            .unwrap();

        let manifest = crate::cluster::models::NodeStateManifest {
            node_id: enrolled.node_id,
            name: "phase4-worker".to_string(),
            region: "eu-central".to_string(),
            core_url: format!("http://{addr}"),
            core_ws_url: format!("ws://{addr}/api/cluster/node-ws"),
            mesh_ip: enrolled.mesh_ip.clone(),
            public_ip: "127.0.0.1".to_string(),
            wg_private_key: String::new(),
            wg_pubkey: "pubKeyPhase4==".to_string(),
            wg_listen_port: 51820,
            node_token: enrolled.node_token.clone(),
            peers: Vec::new(),
            workloads: Vec::new(),
            updated_at: 0,
        };
        let exec_state = Arc::new(crate::cluster::node_exec::NodeExecState::new(
            worker_data_dir.clone(),
            manifest,
        ));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ClusterWsMessage>();

        let exec_c = exec_state.clone();
        let out_tx_c = out_tx.clone();
        let agent_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = out_rx.recv() => {
                        if let Ok(txt) = serde_json::to_string(&msg) {
                            if ws_writer.send_text(&txt).await.is_err() {
                                break;
                            }
                        }
                    }
                    frame = ws_reader.recv() => {
                        match frame {
                            Ok(crate::cluster::ws::WsMessage::Text(txt)) => {
                                if let Ok(parsed) = serde_json::from_str::<ClusterWsMessage>(&txt) {
                                    match parsed {
                                        ClusterWsMessage::PeerSync { placement_versions, .. } => {
                                            let _ = crate::cluster::node_exec::reconcile_placements(
                                                &exec_c,
                                                &placement_versions,
                                                Some(&out_tx_c),
                                            ).await;
                                        }
                                        ClusterWsMessage::RpcRequest { req_id, op, params } => {
                                            let ec = exec_c.clone();
                                            let tx = out_tx_c.clone();
                                            tokio::spawn(async move {
                                                let reply = crate::cluster::node_exec::execute_rpc(
                                                    ec, req_id, op, params, tx.clone()
                                                ).await;
                                                let _ = tx.send(reply);
                                            });
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Ok(crate::cluster::ws::WsMessage::Close) | Err(_) => break,
                            _ => {}
                        }
                    }
                }
            }
        });

        // Wait up to 2s for Core to register the peer sender
        for _ in 0..40 {
            if state.cluster.is_node_connected(enrolled.node_id).await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(state.cluster.is_node_connected(enrolled.node_id).await);

        // 4. Create project assigned to remote node -> triggers PeerSync + project.ensure RPC
        let unique_slug = format!("phase4-remote-{}", crate::db::now());
        let create_body = serde_json::json!({
            "name": "Phase 4 Remote App",
            "slug": unique_slug,
            "repo_url": "local",
            "branch": "main",
            "deploy_dir": remote_site_dir.to_string_lossy().to_string(),
            "start_cmd": "node server.js",
            "port": 49123,
            "node_id": enrolled.node_id
        });
        let app_router = crate::api::create_router(state.clone());
        let cr = app_router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/projects")
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .header("x-csrf-token", &admin_csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(create_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cr.status(), StatusCode::OK);
        let cr_bytes = axum::body::to_bytes(cr.into_body(), 64 * 1024).await.unwrap();
        let cr_json: serde_json::Value = serde_json::from_slice(&cr_bytes).unwrap();
        let pid = cr_json["id"].as_i64().unwrap();
        assert_eq!(cr_json["node_id"].as_i64(), Some(enrolled.node_id));

        // 5. Remote File Write & Read proxy via Core HTTP API -> WS RPC -> NodeExecState
        let fw = app_router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/api/projects/{pid}/files/content"))
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .header("x-csrf-token", &admin_csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "path": "src/app.js", "content": "remote-hello" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fw.status(), StatusCode::OK);

        let fr = app_router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/projects/{pid}/files/content?path=src/app.js"))
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fr.status(), StatusCode::OK);
        let fr_bytes = axum::body::to_bytes(fr.into_body(), 64 * 1024).await.unwrap();
        let fr_json: serde_json::Value = serde_json::from_slice(&fr_bytes).unwrap();
        assert_eq!(fr_json["content"].as_str(), Some("remote-hello"));

        // Core RBAC rejects .env write BEFORE proxying to remote Web-Node
        let env_denied = app_router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/api/projects/{pid}/files/content"))
                    .header(header::COOKIE, format!("nd_session={}", file_sess.id))
                    .header("x-csrf-token", &file_sess.csrf_token)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "path": ".env", "content": "SECRET=1" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(env_denied.status(), StatusCode::FORBIDDEN);
        assert!(!remote_site_dir.join(".env").exists());

        // 6. Streaming LogChunk over WS persists & reaches Core Logs
        out_tx
            .send(ClusterWsMessage::LogChunk {
                project_id: Some(pid),
                deployment_id: None,
                channel: "deploy".to_string(),
                level: "info".to_string(),
                message: "Phase4 remote streamed log".to_string(),
                ts: crate::db::now(),
            })
            .unwrap();
        let mut found_log = false;
        for _ in 0..30 {
            let cnt: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM logs WHERE project_id = $1 AND message = 'Phase4 remote streamed log'",
            )
            .bind(pid)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);
            if cnt > 0 {
                found_log = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(found_log);

        // 7. Streaming DeployStateUpdate updates PG deployments, releases, and project sha
        let proj = state.projects.get(pid).await.unwrap().unwrap();
        let dep_id = state
            .deployer
            .create_deployment(&proj, "manual", "admin", "remote-test")
            .await;
        let rel_dir = remote_site_dir.join("releases").join("rel-abc1234");
        out_tx
            .send(ClusterWsMessage::DeployStateUpdate {
                update: crate::cluster::models::DeployStateUpdate {
                    deployment_id: dep_id,
                    project_id: pid,
                    kind: Some("manual".to_string()),
                    stage: Some("done".to_string()),
                    status: Some("success".to_string()),
                    sha: Some("abc1234567890abcdef".to_string()),
                    short_sha: Some("abc1234".to_string()),
                    release_dir: Some(rel_dir.to_string_lossy().to_string()),
                    commit_message: Some("Deployed via remote worker".to_string()),
                    commit_author: Some("admin".to_string()),
                    deployed_sha: Some("abc1234567890abcdef".to_string()),
                    app_pid: Some(42424),
                    app_status: Some("running".to_string()),
                    ..Default::default()
                },
            })
            .unwrap();
        let mut deploy_done = false;
        for _ in 0..30 {
            let st: Option<String> = sqlx::query_scalar("SELECT status FROM deployments WHERE id = $1")
                .bind(dep_id)
                .fetch_optional(&state.db)
                .await
                .unwrap_or(None);
            if st.as_deref() == Some("success") {
                deploy_done = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(deploy_done);

        // 8. Heartbeat workload telemetry syncs status, pid, and remote disk_bytes
        out_tx
            .send(ClusterWsMessage::Heartbeat {
                metrics: crate::cluster::models::NodeMetrics::default(),
                workloads: vec![crate::cluster::models::NodeWorkloadPlacement {
                    project_id: pid,
                    slug: proj.slug.clone(),
                    placement_version: 1,
                    port: Some(49123),
                    app_status: "running".to_string(),
                    pid: Some(42424),
                    cpu_pct: 0.0,
                    memory_bytes: 0,
                    disk_bytes: 20480,
                    deploy_dir: proj.deploy_dir.clone(),
                }],
                ts: crate::db::now(),
            })
            .unwrap();
        let mut hb_synced = false;
        for _ in 0..30 {
            if state.cluster.get_remote_disk_bytes(pid).await == Some(20480) {
                hb_synced = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(hb_synced);

        let gp = app_router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/projects/{pid}"))
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(gp.status(), StatusCode::OK);
        let gp_bytes = axum::body::to_bytes(gp.into_body(), 64 * 1024).await.unwrap();
        let gp_json: serde_json::Value = serde_json::from_slice(&gp_bytes).unwrap();
        assert_eq!(gp_json["deployed_sha"].as_str(), Some("abc1234567890abcdef"));
        assert_eq!(gp_json["app_status"].as_str(), Some("running"));
        assert_eq!(gp_json["diskFormatted"].as_str(), Some("20 KB"));

        // Cleanup project, WS client, server, and temp dir
        let dp = app_router
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/projects/{pid}"))
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .header("x-csrf-token", &admin_csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(dp.status(), StatusCode::OK);

        agent_task.abort();
        server_task.abort();
        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[tokio::test]
    async fn cluster_modules_api_lists_refreshes_and_rbac_protects_install() {
        let state = crate::api::common::tests_state_helper().await;
        let local_manifest = state
            .cluster
            .pair_local_node(
                "http://127.0.0.1:8080",
                "ws://127.0.0.1:8080/api/cluster/node-ws",
                "core-modules",
            )
            .await
            .unwrap();

        let users = state.auth.list_users().await.unwrap();
        let admin = users.into_iter().find(|u| u.is_main_admin).unwrap();
        let admin_sess = state.auth.create_session(admin.id).await.unwrap();
        let admin_sid = admin_sess.id;
        let admin_csrf = admin_sess.csrf_token;

        let dev_id: i64 = sqlx::query_scalar(
            "INSERT INTO users(username, password_hash, permissions, allowed_projects, created_at)
             VALUES('mod_viewer', 'hash', '[]', '[]', 1700000000) RETURNING id",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        let dev_sess = state.auth.create_session(dev_id).await.unwrap();
        let dev_sid = dev_sess.id;
        let dev_csrf = dev_sess.csrf_token;

        let router = crate::api::create_router(state.clone());

        // 1. Authenticated user can view catalog & per-VPS module readiness
        let list_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/cluster/modules")
                    .header(header::COOKIE, format!("nd_session={dev_sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list_res.status(), StatusCode::OK);
        let list_bytes = axum::body::to_bytes(list_res.into_body(), 128 * 1024).await.unwrap();
        let list_json: serde_json::Value = serde_json::from_slice(&list_bytes).unwrap();
        let cat = list_json["catalog"].as_array().unwrap();
        assert!(cat.iter().any(|m| m["id"] == "unrar" && m["required"] == true));
        assert!(cat.iter().any(|m| m["id"] == "python" && m["required"] == true));
        assert!(cat.iter().any(|m| m["id"] == "node" && m["required"] == true));
        assert!(cat.iter().any(|m| m["id"] == "java"));
        let nodes = list_json["nodes"].as_array().unwrap();
        assert!(nodes.iter().any(|n| n["nodeId"] == local_manifest.node_id));

        // 2. Non-main-admin cannot trigger package installs
        let denied = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/cluster/nodes/{}/modules/install", local_manifest.node_id))
                    .header(header::COOKIE, format!("nd_session={dev_sid}"))
                    .header("x-csrf-token", &dev_csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"moduleIds":["unrar"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let denied_fleet = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/cluster/modules/install-fleet")
                    .header(header::COOKIE, format!("nd_session={dev_sid}"))
                    .header("x-csrf-token", &dev_csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"moduleIds":["unrar"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_fleet.status(), StatusCode::FORBIDDEN);

        // 3. Main admin can refresh node module scan
        let ref_res = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/cluster/nodes/{}/modules/refresh", local_manifest.node_id))
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .header("x-csrf-token", &admin_csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ref_res.status(), StatusCode::OK);
        let ref_bytes = axum::body::to_bytes(ref_res.into_body(), 128 * 1024).await.unwrap();
        let ref_json: serde_json::Value = serde_json::from_slice(&ref_bytes).unwrap();
        assert_eq!(ref_json["ok"], true);
        assert!(ref_json["node"]["modules"].as_array().unwrap().len() == cat.len());
    }
}
