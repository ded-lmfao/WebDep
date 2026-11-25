use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::api::common::*;
use crate::projects::{EnvVarInput, EnvVarView};

pub(crate) async fn get_env_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<Vec<EnvVarView>>, (StatusCode, String)> {
    let _actor = verify_power(&state, &headers, "env", Some(id)).await?;
    let list = state
        .projects
        .get_env_vars(id)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error".to_string()))?;
    Ok(Json(list))
}

#[derive(Deserialize)]
pub(crate) struct SetEnvPayload {
    pub key: String,
    pub value: String,
    pub is_secret: Option<bool>,
    pub previous_key: Option<String>,
}

pub(crate) async fn set_env_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<SetEnvPayload>,
) -> Result<StatusCode, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "env", Some(id)).await?;
    let p_name = state
        .projects
        .get(id)
        .await
        .ok()
        .flatten()
        .map(|p| p.name)
        .unwrap_or_else(|| format!("Website #{}", id));
    state
        .projects
        .set_env_var_with_rename(
            id,
            &payload.key,
            &payload.value,
            payload.is_secret.unwrap_or(false),
            payload.previous_key.as_deref(),
        )
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    if let Ok(Some(p)) = state.projects.get(id).await {
        state.cluster.sync_remote_env_and_keys(&state.projects, &p).await;
    }
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        "env.set",
        Some(&p_name),
        Some(&format!("Saved environment variable '{}'", payload.key)),
    )
    .await;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
pub(crate) struct BulkEnvPayload {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub items: Option<Vec<EnvVarInput>>,
    pub is_secret: Option<bool>,
    pub replace_all: Option<bool>,
}

#[derive(Serialize)]
pub(crate) struct BulkEnvResponse {
    pub count: usize,
}

pub(crate) async fn bulk_set_env_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<BulkEnvPayload>,
) -> Result<Json<BulkEnvResponse>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "env", Some(id)).await?;
    let p_name = state
        .projects
        .get(id)
        .await
        .ok()
        .flatten()
        .map(|p| p.name)
        .unwrap_or_else(|| format!("Website #{}", id));
    let replace_all = payload.replace_all.unwrap_or(false);
    let count = if let Some(ref items) = payload.items {
        state
            .projects
            .set_batch_env_items(id, items, replace_all)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    } else {
        state
            .projects
            .set_bulk_env(
                id,
                &payload.content,
                payload.is_secret.unwrap_or(true),
                replace_all,
            )
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    };
    if let Ok(Some(p)) = state.projects.get(id).await {
        state.cluster.sync_remote_env_and_keys(&state.projects, &p).await;
    }
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        "env.bulk_import",
        Some(&p_name),
        Some(&format!("Saved {} environment variables", count)),
    )
    .await;
    Ok(Json(BulkEnvResponse { count }))
}

#[derive(Serialize)]
pub(crate) struct RawEnvResponse {
    pub content: String,
}

pub(crate) async fn get_raw_env_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<RawEnvResponse>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "env", Some(id)).await?;
    let content = state
        .projects
        .export_raw_env(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    log_env_read(&state, id, &actor.username, "env.export", "Exported all environment variables").await;
    Ok(Json(RawEnvResponse { content }))
}

#[derive(Deserialize)]
pub(crate) struct RevealEnvPayload {
    /// One variable; without it, every variable of the app (for the editors).
    pub key: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct RevealEnvResponse {
    pub values: HashMap<String, String>,
}

/// The only way to read secret values in the panel, and every call is recorded.
pub(crate) async fn reveal_env_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<RevealEnvPayload>,
) -> Result<Json<RevealEnvResponse>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "env", Some(id)).await?;
    let (values, detail) = match payload.key {
        Some(key) => {
            let value = state
                .projects
                .reveal_env_var(id, &key)
                .await
                .ok_or((StatusCode::NOT_FOUND, "Variable not found".to_string()))?;
            let detail = format!("Revealed variable '{key}'");
            (HashMap::from([(key, value)]), detail)
        }
        None => (state.projects.get_env_map(id).await, "Revealed all environment variables".to_string()),
    };
    log_env_read(&state, id, &actor.username, "env.reveal", &detail).await;
    Ok(Json(RevealEnvResponse { values }))
}

async fn log_env_read(state: &AppState, id: i64, actor: &str, action: &str, detail: &str) {
    let name = state
        .projects
        .get(id)
        .await
        .ok()
        .flatten()
        .map(|p| p.name)
        .unwrap_or_else(|| format!("Website #{}", id));
    log_audit_project(state, Some(id), actor, action, Some(&name), Some(detail)).await;
}

pub(crate) async fn delete_env_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, key)): AxPath<(i64, String)>,
) -> Result<StatusCode, StatusCode> {
    let actor = verify_power(&state, &headers, "env", Some(id))
        .await
        .map_err(|(c, _)| c)?;
    let p_name = state
        .projects
        .get(id)
        .await
        .ok()
        .flatten()
        .map(|p| p.name)
        .unwrap_or_else(|| format!("Website #{}", id));
    state
        .projects
        .delete_env_var(id, &key)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if let Ok(Some(p)) = state.projects.get(id).await {
        state.cluster.sync_remote_env_and_keys(&state.projects, &p).await;
    }
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        "env.delete",
        Some(&p_name),
        Some(&format!("Deleted environment variable '{}'", key)),
    )
    .await;
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use serde_json::json;
    use tower::ServiceExt;

    #[tokio::test]
    async fn secrets_are_masked_and_every_plaintext_read_is_recorded() {
        let state = crate::api::common::tests_state_helper().await;
        let router = crate::api::create_router(state.clone());
        let admin = state.auth.create_session(1).await.unwrap();
        let call = |method: Method, uri: String, body: serde_json::Value| {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("nd_session={}", admin.id))
                .header("x-csrf-token", &admin.csrf_token)
                .body(Body::from(body.to_string()))
                .unwrap();
            let router = router.clone();
            async move {
                let res = router.oneshot(req).await.unwrap();
                let status = res.status();
                let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
                (status, String::from_utf8_lossy(&bytes).into_owned())
            }
        };

        let (_, body) = call(Method::POST, "/api/projects".into(), json!({"name": "Env", "slug": "env-mask", "repo_url": ""})).await;
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"].as_i64().unwrap();
        for (key, value, is_secret) in [("TOKEN", "hunter2-token", true), ("MODE", "public-mode", false)] {
            let (st, _) = call(Method::PUT, format!("/api/projects/{id}/env"), json!({"key": key, "value": value, "is_secret": is_secret})).await;
            assert_eq!(st, StatusCode::OK);
        }

        let (st, list) = call(Method::GET, format!("/api/projects/{id}/env"), json!({})).await;
        assert_eq!(st, StatusCode::OK);
        assert!(list.contains("public-mode") && list.contains("********"));
        assert!(!list.contains("hunter2-token"), "the list must never carry a secret: {list}");

        for body in [json!({"key": "TOKEN"}), json!({})] {
            let (st, revealed) = call(Method::POST, format!("/api/projects/{id}/env/reveal"), body).await;
            assert_eq!(st, StatusCode::OK);
            assert!(revealed.contains("hunter2-token"));
        }
        let (st, _) = call(Method::GET, format!("/api/projects/{id}/env/raw"), json!({})).await;
        assert_eq!(st, StatusCode::OK);

        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit WHERE project_id = $1 AND action IN ('env.reveal', 'env.export') ORDER BY id",
        )
        .bind(id)
        .fetch_all(&state.db)
        .await
        .unwrap();
        assert_eq!(actions, ["env.reveal", "env.reveal", "env.export"]);
    }
}
