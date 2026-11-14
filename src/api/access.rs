use axum::{
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};

use super::common::*;
use super::logs::{pg_row_to_audit_item, AuditItem};
use crate::auth::access::{
    delete_grant_by_id, get_grant_by_id, list_all_grants, list_resource_grants, list_user_grants,
    upsert_grant, AccessGrant, AccessRevocationEvent, RoleTemplate, UpsertGrantInput, KNOWN_POWERS,
    ROLE_TEMPLATES,
};
use crate::auth::user::User;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessUserSummary {
    #[serde(flatten)]
    pub(crate) user: User,
    pub(crate) active_sessions: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessResourceOption {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) slug: String,
    pub(crate) kind: String,
    pub(crate) linked_project_id: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessNodeOption {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) slug: String,
    pub(crate) status: String,
    pub(crate) is_local: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessOverviewResponse {
    pub(crate) users: Vec<AccessUserSummary>,
    pub(crate) grants: Vec<AccessGrant>,
    pub(crate) projects: Vec<AccessResourceOption>,
    pub(crate) databases: Vec<AccessResourceOption>,
    pub(crate) nodes: Vec<AccessNodeOption>,
    pub(crate) templates: &'static [RoleTemplate],
    pub(crate) known_powers: &'static [&'static str],
    pub(crate) server_time: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MyAccessResponse {
    pub(crate) user: User,
    pub(crate) grants: Vec<AccessGrant>,
    pub(crate) templates: &'static [RoleTemplate],
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GrantsQuery {
    pub(crate) user_id: Option<i64>,
    pub(crate) scope_type: Option<String>,
    pub(crate) scope_id: Option<i64>,
}

fn local_node_option() -> AccessNodeOption {
    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "webcore-local".to_string());
    AccessNodeOption {
        id: 1,
        name: format!("{} (Primary Control Node)", hostname),
        slug: "local".to_string(),
        status: "online".to_string(),
        is_local: true,
    }
}

pub(crate) async fn my_access_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MyAccessResponse>, (StatusCode, String)> {
    let user = require_current_user(&state, &headers).await?;
    let grants = state.auth.active_grants_for_user(user.id).await;
    Ok(Json(MyAccessResponse {
        user,
        grants,
        templates: ROLE_TEMPLATES,
    }))
}

pub(crate) async fn access_overview_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AccessOverviewResponse>, (StatusCode, String)> {
    let _admin = verify_main_admin(&state, &headers).await?;
    state.auth.purge_expired_grants().await;

    let raw_users = state
        .auth
        .list_users()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut users = Vec::with_capacity(raw_users.len());
    for mut u in raw_users {
        u.presence = if u.disabled {
            "offline".to_string()
        } else {
            state.auth.presence_for_user(u.id)
        };
        let active_sessions = state.auth.count_active_sessions(u.id).await;
        users.push(AccessUserSummary {
            user: u,
            active_sessions,
        });
    }

    let grants = list_all_grants(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let projects = state
        .projects
        .list()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(|p| AccessResourceOption {
            id: p.id,
            name: p.name,
            slug: p.slug,
            kind: p.project_kind,
            linked_project_id: None,
        })
        .collect();

    let databases = state
        .databases
        .list_overview_summaries()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .into_iter()
        .map(|d| AccessResourceOption {
            id: d.id,
            name: d.name,
            slug: format!("pg:{}", d.port),
            kind: "database".to_string(),
            linked_project_id: d.linked_project_id,
        })
        .collect();

    Ok(Json(AccessOverviewResponse {
        users,
        grants,
        projects,
        databases,
        nodes: vec![local_node_option()],
        templates: ROLE_TEMPLATES,
        known_powers: KNOWN_POWERS,
        server_time: now(),
    }))
}

pub(crate) async fn list_grants_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GrantsQuery>,
) -> Result<Json<Vec<AccessGrant>>, (StatusCode, String)> {
    let _admin = verify_main_admin(&state, &headers).await?;
    let list = if let Some(uid) = q.user_id {
        list_user_grants(&state.db, uid).await
    } else if let Some(ref st) = q.scope_type {
        list_resource_grants(&state.db, st, q.scope_id).await
    } else {
        list_all_grants(&state.db).await
    }
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(list))
}

pub(crate) async fn upsert_grant_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<UpsertGrantInput>,
) -> Result<Json<AccessGrant>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;
    let target = state
        .auth
        .get_user_by_id(input.user_id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Target user not found.".to_string()))?;
    if target.is_main_admin {
        return Err((
            StatusCode::BAD_REQUEST,
            "Main Admin always holds full access and cannot be scoped down.".to_string(),
        ));
    }

    match input.scope_type.trim().to_ascii_lowercase().as_str() {
        "project" => {
            let pid = input.scope_id.ok_or_else(|| {
                (StatusCode::BAD_REQUEST, "A project scopeId is required.".to_string())
            })?;
            state
                .projects
                .get(pid)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .ok_or_else(|| (StatusCode::NOT_FOUND, "Project not found.".to_string()))?;
        }
        "database" => {
            let did = input.scope_id.ok_or_else(|| {
                (StatusCode::BAD_REQUEST, "A database scopeId is required.".to_string())
            })?;
            state
                .databases
                .get(did)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
                .ok_or_else(|| (StatusCode::NOT_FOUND, "Database not found.".to_string()))?;
        }
        "node" => {
            if input.scope_id != Some(1) {
                return Err((StatusCode::NOT_FOUND, "Node not found.".to_string()));
            }
        }
        _ => {}
    }

    let now_s = now();
    let grant = upsert_grant(&state.db, &input, Some(actor.id), now_s)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    state.cache.invalidate_all();
    state
        .auth
        .notify_revocation(AccessRevocationEvent {
            kind: "grant_updated".to_string(),
            user_id: target.id,
            scope_type: Some(grant.scope_type.clone()),
            scope_id: grant.scope_id,
            reason: format!(
                "Access grant updated by {} ({})",
                actor.username, grant.role_preset
            ),
            ts: now_s,
        })
        .await;

    let scope_desc = match grant.scope_id {
        Some(sid) => format!("{}:{}", grant.scope_type, sid),
        None => grant.scope_type.clone(),
    };
    let detail = format!(
        "preset={} powers=[{}]{}",
        grant.role_preset,
        grant.powers.join(", "),
        grant
            .expires_at
            .map(|e| format!(" expires_at={e}"))
            .unwrap_or_default()
    );
    log_audit_project(
        &state,
        if grant.scope_type == "project" {
            grant.scope_id
        } else {
            None
        },
        &actor.username,
        "access.grant",
        Some(&format!("{} ({})", target.username, scope_desc)),
        Some(&detail),
    )
    .await;

    Ok(Json(grant))
}

pub(crate) async fn delete_grant_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;
    let existing = get_grant_by_id(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Access grant not found.".to_string()))?;

    let removed = delete_grant_by_id(&state.db, id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or(existing);

    let target_name = state
        .auth
        .get_user_by_id(removed.user_id)
        .await
        .map(|u| u.username)
        .unwrap_or_else(|| format!("user#{}", removed.user_id));

    state.cache.invalidate_all();
    state
        .auth
        .notify_revocation(AccessRevocationEvent {
            kind: "grant_revoked".to_string(),
            user_id: removed.user_id,
            scope_type: Some(removed.scope_type.clone()),
            scope_id: removed.scope_id,
            reason: format!("Access grant revoked by {}", actor.username),
            ts: now(),
        })
        .await;

    let scope_desc = match removed.scope_id {
        Some(sid) => format!("{}:{}", removed.scope_type, sid),
        None => removed.scope_type.clone(),
    };
    log_audit_project(
        &state,
        if removed.scope_type == "project" {
            removed.scope_id
        } else {
            None
        },
        &actor.username,
        "access.revoke",
        Some(&format!("{} ({})", target_name, scope_desc)),
        Some(&format!("Revoked preset={}", removed.role_preset)),
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true, "id": removed.id })))
}

pub(crate) async fn revoke_user_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_main_admin(&state, &headers).await?;
    let target = state
        .auth
        .get_user_by_id(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "User not found.".to_string()))?;
    if target.id == actor.id {
        return Err((
            StatusCode::BAD_REQUEST,
            "Use Account -> Revoke Other Sessions for your own active session.".to_string(),
        ));
    }
    let revoked_count = state
        .auth
        .revoke_all_user_sessions(id, &format!("Sessions revoked by {}", actor.username))
        .await;
    state.cache.invalidate_all();

    log_audit(
        &state,
        &actor.username,
        "access.session_revoke",
        Some(&target.username),
        Some(&format!("Force-revoked {} active session(s)", revoked_count)),
    )
    .await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "revokedCount": revoked_count
    })))
}

pub(crate) async fn access_audit_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<AuditItem>>, (StatusCode, String)> {
    let _admin = verify_main_admin(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, project_id, ts, actor, action, target, detail, ip \
         FROM audit \
         WHERE action LIKE 'access.%' OR action LIKE 'user.%' OR action LIKE 'auth.%' \
         ORDER BY ts DESC, id DESC LIMIT 120",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let items = rows
        .iter()
        .filter_map(|r| pg_row_to_audit_item(r, None).ok())
        .collect();
    Ok(Json(items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn test_app_state() -> AppState {
        let base = std::env::temp_dir().join(format!(
            "webdep_api_access_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let db = crate::pg_control::test_db::isolated_pool().await;
        let vault = Arc::new(crate::vault::Vault::new(&base).unwrap());
        let auth = Arc::new(crate::auth::Auth::new(db.clone(), 3600));
        auth.ensure_admin().await;
        let projects = Arc::new(crate::projects::Projects::new(db.clone(), vault.clone()));
        let logs = Arc::new(crate::logs::Logs::new(db.clone()));
        let monitor = Arc::new(crate::monitor::Monitor::new());
        let cache = Arc::new(crate::cache::AppCache::new());
        let caddy = Arc::new(crate::caddy::Caddy::new(&base));
        let runtime = Arc::new(crate::runtime::Runtime::new(db.clone(), (*logs).clone()));
        let deployer = Arc::new(
            crate::deployer::Deployer::new(
                db.clone(),
                (*logs).clone(),
                projects.clone(),
                runtime.clone(),
                cache.clone(),
            )
            .await,
        );
        let databases = Arc::new(crate::databases::Databases::new(
            db.clone(),
            vault,
            (*logs).clone(),
            &base,
        ));
        let updater = Arc::new(crate::updater::Updater::with_db(db.clone()));

        AppState {
            db,
            auth,
            projects,
            logs,
            monitor,
            cache,
            caddy,
            runtime,
            deployer,
            databases,
            updater,
        }
    }

    #[tokio::test]
    async fn test_access_grants_api_lifecycle_and_enforcement() {
        let state = test_app_state().await;
        let users = state.auth.list_users().await.unwrap();
        let admin_user = users.into_iter().find(|u| u.is_main_admin).unwrap();
        let admin_sess = state.auth.create_session(admin_user.id).await.unwrap();

        let dev_id: i64 = sqlx::query_scalar(
            "INSERT INTO users(username, password_hash, permissions, allowed_projects, created_at) \
             VALUES('dev_user', 'hash', '[]', '[]', 1700000000) RETURNING id",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        let dev_sess = state.auth.create_session(dev_id).await.unwrap();

        let proj_id: i64 = sqlx::query_scalar(
            "INSERT INTO projects(name, slug, repo_url, branch, project_type, project_kind, deploy_dir, app_status, created_at, updated_at) \
             VALUES('Frontend App', 'frontend-app', 'https://example.com/app.git', 'main', 'node', 'project', '/tmp/frontend-app', 'stopped', 1700000000, 1700000000) RETURNING id",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();

        let app = crate::api::create_router(state.clone());
        let dev_cookie = format!("nd_session={}", dev_sess.id);
        let admin_cookie = format!("nd_session={}", admin_sess.id);

        // 1. Before grant: dev cannot access project or its logs
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/projects/{proj_id}"))
                    .header("Cookie", &dev_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        let mut rev_rx = state.auth.subscribe_revocations();

        // 2. Main Admin grants dev Frontend Deployer on proj_id
        let body = serde_json::json!({
            "userId": dev_id,
            "scopeType": "project",
            "scopeId": proj_id,
            "rolePreset": "frontend_deployer",
        })
        .to_string();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/access/grants")
                    .header("Cookie", &admin_cookie)
                    .header("X-CSRF-Token", &admin_sess.csrf_token)
                    .header("Content-Type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 64).await.unwrap();
        let created_grant: AccessGrant = serde_json::from_slice(&bytes).unwrap();
        let ev1 = rev_rx.recv().await.unwrap();
        assert_eq!(ev1.user_id, dev_id);
        assert_eq!(ev1.kind, "grant_updated");

        // 3. Dev can now GET project & GET project logs
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/projects/{proj_id}"))
                    .header("Cookie", &dev_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/projects/{proj_id}/app-logs"))
                    .header("Cookie", &dev_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Frontend Deployer does NOT include `env` power
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/projects/{proj_id}/env"))
                    .header("Cookie", &dev_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Start a live SSE log stream, then revoke the grant and verify stream terminates with `event: revoked`
        let sse_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/logs/stream?project={proj_id}&backlog=10"))
                    .header("Cookie", &dev_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sse_res.status(), StatusCode::OK);
        let mut sse_stream = sse_res.into_body().into_data_stream();

        // 4. Main Admin revokes grant -> live broadcast fires and closes SSE stream + blocks API (403)
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/access/grants/{}", created_grant.id))
                    .header("Cookie", &admin_cookie)
                    .header("X-CSRF-Token", &admin_sess.csrf_token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ev2 = rev_rx.recv().await.unwrap();
        assert_eq!(ev2.user_id, dev_id);
        assert_eq!(ev2.kind, "grant_revoked");

        use futures_util::StreamExt;
        let sse_chunk = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            sse_stream.next(),
        )
        .await
        .expect("SSE stream should emit revoked event promptly")
        .expect("SSE frame expected")
        .unwrap();
        let frame_text = String::from_utf8_lossy(&sse_chunk);
        assert!(frame_text.contains("revoked"), "Expected revoked SSE event, got: {frame_text}");
        // Stream terminates immediately after revoked event
        let end = tokio::time::timeout(std::time::Duration::from_secs(2), sse_stream.next())
            .await
            .expect("SSE stream should close immediately after revocation");
        assert!(end.is_none());

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/projects/{proj_id}"))
                    .header("Cookie", &dev_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
}
