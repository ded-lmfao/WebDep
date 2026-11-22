use axum::{
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};

use super::common::*;
use super::projects::copy_backup_files;

#[derive(Deserialize, Default)]
pub(crate) struct DeployPayload {
    sha: Option<String>,
}

pub(crate) async fn deploy_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    body_bytes: axum::body::Bytes,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let (actor, p) = get_authorized_project(&state, &headers, "deploy", id).await?;

    if crate::backups::is_restoring(state.cluster.data_dir(), p.id) {
        return Err((
            StatusCode::CONFLICT,
            format!("'{}' is being restored from a backup. Wait for the restore to finish.", p.name),
        ));
    }
    if state.deployer.has_active_deployment(p.id).await {
        return Err((
            StatusCode::CONFLICT,
            format!("Deployment already in progress for '{}'", p.name),
        ));
    }

    let target_sha = serde_json::from_slice::<DeployPayload>(&body_bytes)
        .ok()
        .and_then(|b| b.sha)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let dep_id = state.deployer.create_deployment(&p, "deploy", &actor.username, "manual").await;
    if let Some(ref sha) = target_sha {
        let _ = sqlx::query("UPDATE deployments SET sha = $1 WHERE id = $2")
            .bind(sha)
            .bind(dep_id)
            .execute(&state.db)
            .await;
    }
    // create_deployment() already records the audit entry for this trigger.

    if let Some(node_id) = state
        .cluster
        .should_proxy_to_node(p.node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
    {
        let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
        if let Err(err) = state
            .cluster
            .call_node(
                node_id,
                "project.deploy",
                serde_json::json!({
                    "project": &p,
                    "deploymentId": dep_id,
                    "sha": target_sha,
                    "context": ctx,
                }),
                std::time::Duration::from_secs(20),
            )
            .await
        {
            state
                .deployer
                .finish_deployment(dep_id, "failed", Some(&err.1))
                .await;
            return Err(err);
        }
    } else {
        trigger_background_deploy(state.deployer.clone(), p.id, dep_id);
    }

    Ok(Json(ActionResponse { deployment_id: dep_id }))
}

#[derive(Deserialize)]
pub(crate) struct RollbackPayload {
    #[serde(rename = "releaseId")]
    release_id: i64,
}

pub(crate) async fn rollback_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<RollbackPayload>,
) -> Result<Json<ActionResponse>, (StatusCode, String)> {
    let (actor, p) = get_authorized_project(&state, &headers, "deploy", id).await?;

    let dep_id = state.deployer.create_deployment(&p, "rollback", &actor.username, "manual").await;
    let deployer = state.deployer.clone();
    let target_rel = payload.release_id;
    // create_deployment() already records the audit entry for this trigger.

    if let Some(node_id) = state
        .cluster
        .should_proxy_to_node(p.node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
    {
        let rel_row: Option<(String, String)> = sqlx::query_as(
            "SELECT dir, sha FROM releases WHERE id = $1 AND project_id = $2",
        )
        .bind(target_rel)
        .bind(p.id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        let (rel_dir, rel_sha) = rel_row.unwrap_or_default();
        let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
        if let Err(err) = state
            .cluster
            .call_node(
                node_id,
                "project.rollback",
                serde_json::json!({
                    "project": &p,
                    "deploymentId": dep_id,
                    "releaseId": target_rel,
                    "releaseDir": rel_dir,
                    "sha": rel_sha,
                    "context": ctx,
                }),
                std::time::Duration::from_secs(20),
            )
            .await
        {
            state
                .deployer
                .finish_deployment(dep_id, "failed", Some(&err.1))
                .await;
            return Err(err);
        }
    } else {
        tokio::spawn(async move {
            deployer.run_rollback(p.id, dep_id, target_rel).await;
        });
    }

    Ok(Json(ActionResponse { deployment_id: dep_id }))
}


pub(crate) async fn list_releases_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<Vec<crate::deployer::Release>>, (StatusCode, String)> {
    let _actor = verify_project_access(&state, &headers, id).await?;
    let list = state.deployer.list_releases(id).await;
    Ok(Json(list))
}

#[derive(Deserialize)]
pub(crate) struct CreateReleasePayload {
    source: Option<String>,
    note: Option<String>,
}

pub(crate) async fn create_release_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<CreateReleasePayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (actor, p) = get_authorized_project(&state, &headers, "deploy", id).await?;

    let source = payload.source.unwrap_or_else(|| "deploy".to_string());
    let note = payload.note.unwrap_or_else(|| "Manual Release".to_string());

    if source == "snapshot" {
        let ts = now();
        let rel_dir_str = if let Some(node_id) = state
            .cluster
            .should_proxy_to_node(p.node_id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        {
            let raw_env = state.projects.export_raw_env(id).await.unwrap_or_default();
            let snap = state
                .cluster
                .call_node(
                    node_id,
                    "project.create_snapshot",
                    serde_json::json!({
                        "project": &p,
                        "kind": "full-snapshot",
                        "rawEnv": raw_env,
                    }),
                    std::time::Duration::from_secs(60),
                )
                .await?;
            snap["releaseFilesDir"]
                .as_str()
                .unwrap_or("")
                .to_string()
        } else {
            let base_dir = std::path::PathBuf::from(&p.deploy_dir);
            let rel_dir = base_dir.join("releases").join(format!("rel-snap-{}", ts));
            tokio::fs::create_dir_all(&rel_dir).await.ok();

            let src_dir = if base_dir.join("current").exists() {
                base_dir.join("current")
            } else {
                base_dir.clone()
            };

            if src_dir.exists() {
                copy_backup_files(&src_dir, &rel_dir);
            }
            rel_dir.to_string_lossy().to_string()
        };

        let sha = p.deployed_sha.clone().unwrap_or_else(|| format!("{:x}", ts));
        let short_sha = sha[..7.min(sha.len())].to_string();

        let rel_id = insert_success_release(
            &state.db,
            id,
            &sha,
            &short_sha,
            &format!("Release: {}", note),
            &actor.username,
            &rel_dir_str,
            ts,
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        state.cache.invalidate_project(id);
        log_audit_project(&state, Some(id), &actor.username, "release.create", Some(&p.name), Some(&note)).await;

        Ok(Json(serde_json::json!({
            "ok": true,
            "releaseId": rel_id,
            "message": format!("Release #{} created successfully from current files", rel_id)
        })))
    } else {
        // Trigger a new deployment which builds and creates a live release
        let dep_id = state.deployer.create_deployment(&p, "deploy", &actor.username, "manual").await;
        if let Some(node_id) = state
            .cluster
            .should_proxy_to_node(p.node_id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        {
            let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
            if let Err(err) = state
                .cluster
                .call_node(
                    node_id,
                    "project.deploy",
                    serde_json::json!({
                        "project": &p,
                        "deploymentId": dep_id,
                        "sha": Option::<String>::None,
                        "context": ctx,
                    }),
                    std::time::Duration::from_secs(20),
                )
                .await
            {
                state
                    .deployer
                    .finish_deployment(dep_id, "failed", Some(&err.1))
                    .await;
                return Err(err);
            }
        } else {
            trigger_background_deploy(state.deployer.clone(), p.id, dep_id);
        }

        Ok(Json(serde_json::json!({
            "ok": true,
            "deploymentId": dep_id,
            "message": format!("Release deployment #{} initiated", dep_id)
        })))
    }
}


#[derive(Deserialize)]
pub(crate) struct DeploymentQuery {
    project: Option<i64>,
    limit: Option<usize>,
}

pub(crate) async fn list_deployments_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DeploymentQuery>,
) -> Result<Json<Vec<crate::deployer::Deployment>>, (StatusCode, String)> {
    let user = require_current_user(&state, &headers).await?;
    if let Some(pid) = q.project {
        if !user.can_access_project(pid) {
            return Err((
                StatusCode::FORBIDDEN,
                "Access denied: You are not assigned to this website.".to_string(),
            ));
        }
    }
    let mut list = state.deployer.list_deployments(q.project, q.limit.unwrap_or(200)).await;
    if q.project.is_none() {
        list.retain(|d| user.can_access_project(d.project_id));
    }
    Ok(Json(list))
}

#[derive(Serialize)]
pub(crate) struct DeploymentDetailResponse {
    deployment: crate::deployer::Deployment,
    logs: Vec<crate::logs::LogEntry>,
}

pub(crate) async fn get_deployment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<DeploymentDetailResponse>, (StatusCode, String)> {
    let user = require_current_user(&state, &headers).await?;
    let dep = state
        .deployer
        .get_deployment(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Deployment not found".to_string()))?;
    if !user.can_access_project(dep.project_id) {
        return Err((
            StatusCode::FORBIDDEN,
            "Access denied: You are not assigned to this website.".to_string(),
        ));
    }
    let logs = if user.has_project_power(dep.project_id, &["logs"]) {
        state.logs.list(None, Some(id), None, None, 1000).await
    } else {
        Vec::new()
    };
    Ok(Json(DeploymentDetailResponse {
        deployment: dep,
        logs,
    }))
}

pub(crate) async fn get_deployment_logs_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<Vec<crate::logs::LogEntry>>, (StatusCode, String)> {
    let dep = state
        .deployer
        .get_deployment(id)
        .await
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Deployment not found".to_string()))?;
    let _user = verify_power(&state, &headers, "logs", Some(dep.project_id)).await?;
    let logs = state.logs.list(None, Some(id), None, None, 1000).await;
    Ok(Json(logs))
}


pub(crate) async fn delete_release_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, release_id)): AxPath<(i64, i64)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (actor, p) = get_authorized_project(&state, &headers, "deploy", id).await?;

    if p.current_release_id == Some(release_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Cannot delete the active live release. Roll back to or deploy another release first.".to_string(),
        ));
    }

    let rel_dir: Option<String> = sqlx::query_scalar(
        "SELECT dir FROM releases WHERE id = $1 AND project_id = $2",
    )
    .bind(release_id)
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let remote_node_id = state
        .cluster
        .should_proxy_to_node(p.node_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    if let Some(d_str) = rel_dir {
        if let Some(nid) = remote_node_id {
            if p.project_kind != "service" && (d_str.contains("releases") || d_str.contains("rel-")) {
                let _ = state
                    .cluster
                    .call_node(
                        nid,
                        "project.delete_path",
                        serde_json::json!({ "path": d_str }),
                        std::time::Duration::from_secs(15),
                    )
                    .await;
            }
        } else {
            let path = std::path::Path::new(&d_str);
            if p.project_kind != "service" && path.exists() && (d_str.contains("releases") || d_str.contains("rel-")) {
                let _ = tokio::fs::remove_dir_all(path).await;
            }
        }
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let _ = sqlx::query("UPDATE deployments SET release_id = NULL WHERE release_id = $1")
        .bind(release_id)
        .execute(&mut *tx)
        .await;
    let _ = sqlx::query("UPDATE deployments SET from_release_id = NULL WHERE from_release_id = $1")
        .bind(release_id)
        .execute(&mut *tx)
        .await;
    let _ = sqlx::query("DELETE FROM backups WHERE release_id = $1 AND project_id = $2")
        .bind(release_id)
        .bind(id)
        .execute(&mut *tx)
        .await;
    let _ = sqlx::query("UPDATE backups SET release_id = NULL WHERE release_id = $1")
        .bind(release_id)
        .execute(&mut *tx)
        .await;
    let _ = sqlx::query("UPDATE projects SET current_release_id = NULL WHERE current_release_id = $1")
        .bind(release_id)
        .execute(&mut *tx)
        .await;
    sqlx::query("DELETE FROM releases WHERE id = $1 AND project_id = $2")
        .bind(release_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.cache.invalidate_project(id);
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        "release.delete",
        Some(&p.name),
        Some(&format!("Deleted release #{}", release_id)),
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true })))
}
