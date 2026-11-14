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
    let _user = require_current_user(&state, &headers).await?;
    let nodes = state
        .cluster
        .list_nodes()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(nodes))
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

pub(crate) async fn create_enroll_token_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateEnrollTokenInput>,
) -> Result<(StatusCode, Json<CreateEnrollTokenResponse>), (StatusCode, String)> {
    let admin = verify_main_admin(&state, &headers).await?;
    let (core_http_url, _) = infer_core_urls(&headers, &state);
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
    tokio::spawn(async move {
        let Ok(ws) = rx.await else {
            return;
        };
        let node_id = node.id;
        let (mut reader, mut writer) = ws.into_split();
        let (tx, mut out_rx) = mpsc::unbounded_channel::<ClusterWsMessage>();

        cluster.register_peer_sender(node_id, tx).await;
        let _ = cluster.send_peer_sync_to(node_id).await;

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
                                ..
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
                                cluster.broadcast_peer_sync().await;
                            }
                            ClusterWsMessage::Heartbeat { metrics, .. } => {
                                let _ = cluster.update_node_metrics(node_id, &metrics).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Method;
    use tower::ServiceExt;

    #[tokio::test]
    async fn cluster_api_auth_enrollment_drain_and_delete() {
        let state = crate::api::access::tests_state_helper().await;
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
        assert!(tok_json.join_command.contains("--join http://panel.test:8080"));

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

        // Access overview lists real cluster node
        let ov_res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/access/overview")
                    .header(header::COOKIE, format!("nd_session={admin_sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ov_res.status(), StatusCode::OK);
        let ov_bytes = axum::body::to_bytes(ov_res.into_body(), 1024 * 64)
            .await
            .unwrap();
        let ov_text = String::from_utf8_lossy(&ov_bytes);
        assert!(ov_text.contains("us-west-1"));

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
}
