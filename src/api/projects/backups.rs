use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::common::*;

#[derive(Serialize)]
pub(crate) struct BackupItem {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub size: u64,
    pub note: String,
    pub path: String,
    #[serde(rename = "releaseId")]
    pub release_id: Option<i64>,
}

pub(crate) fn copy_backup_files(src: &std::path::Path, dst: &std::path::Path) -> u64 {
    let mut total_bytes = 0u64;
    if std::fs::create_dir_all(dst).is_err() {
        return 0;
    }
    if let Ok(entries) = std::fs::read_dir(src) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str == "node_modules" || name_str == ".git" || name_str == "backups" {
                continue;
            }
            let src_path = entry.path();
            // Never traverse or copy symlinks to prevent directory escape or cycles
            let Ok(meta) = std::fs::symlink_metadata(&src_path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let dst_path = dst.join(&name);
            if meta.is_dir() {
                total_bytes += copy_backup_files(&src_path, &dst_path);
            } else if meta.is_file() {
                if let Ok(bytes) = std::fs::copy(&src_path, &dst_path) {
                    total_bytes += bytes;
                }
            }
        }
    }
    total_bytes
}

pub(crate) async fn list_backups_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Response, (StatusCode, String)> {
    let _actor = verify_power(&state, &headers, "backups", Some(id)).await?;
    let cache_key = format!("backups:p{}:list", id);
    if let Some(res) = check_api_cache(&state.cache, &cache_key) {
        return Ok(res);
    }

    let rows = sqlx::query_as::<_, (i64, i64, i64, String, String, String, Option<i64>)>(
        "SELECT id, COALESCE(created_at, 0), COALESCE(size_bytes, 0), COALESCE(note, ''), COALESCE(kind, 'full-snapshot'), COALESCE(path, ''), release_id \
         FROM backups WHERE project_id = $1 ORDER BY id DESC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut backups: Vec<BackupItem> = rows
        .into_iter()
        .map(|(bid, ts, size, note, kind, path, release_id)| BackupItem {
            id: bid,
            ts,
            size: size.max(0) as u64,
            note,
            kind,
            path,
            release_id,
        })
        .collect();

    if backups.is_empty() {
        let releases = state.deployer.list_releases(id).await;
        backups = releases
            .into_iter()
            .map(|r| BackupItem {
                id: r.id,
                ts: r.created_at,
                kind: "release-snapshot".to_string(),
                size: 5 * 1024 * 1024,
                note: format!("Automated Release Snapshot #{} ({})", r.id, r.short_sha),
                path: r.dir,
                release_id: Some(r.id),
            })
            .collect();
    }

    cached_json_response(&state.cache, cache_key, &backups, std::time::Duration::from_secs(15))
        .map_err(|code| (code, "Serialization error".to_string()))
}

#[derive(Deserialize)]
pub(crate) struct CreateBackupPayload {
    pub note: Option<String>,
    pub kind: Option<String>,
}

pub(crate) async fn create_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<CreateBackupPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "backups", Some(id)).await?;
    let note = payload
        .note
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Manual backup snapshot".to_string());
    let kind = payload
        .kind
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "full-snapshot".to_string());

    let p = state
        .projects
        .get(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Website not found".to_string()))?;

    let ts = now();
    let (mut total_size, backup_path_str, release_files_dir) = if let Some(nid) = state
        .cluster
        .should_proxy_to_node(p.node_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
    {
        let raw_env = state.projects.export_raw_env(id).await.unwrap_or_default();
        let res = state
            .cluster
            .call_node(
                nid,
                "project.create_snapshot",
                serde_json::json!({ "project": &p, "kind": &kind, "rawEnv": raw_env }),
                std::time::Duration::from_secs(120),
            )
            .await?;
        let sz = res["size"].as_u64().unwrap_or(0);
        let b_path = res["backupPath"].as_str().unwrap_or("").to_string();
        let rel_dir = res["releaseFilesDir"].as_str().unwrap_or(&b_path).to_string();
        (sz, b_path, rel_dir)
    } else {
        let base_dir = std::path::PathBuf::from(&p.deploy_dir);
        let backup_dir = base_dir.join("backups").join(format!("backup-{}", ts));
        let _ = tokio::fs::create_dir_all(&backup_dir).await;

        let mut total_size = 0u64;

        // 1. Snapshot .env variables
        if let Ok(raw_env) = state.projects.export_raw_env(id).await {
            let env_path = backup_dir.join("backup.env");
            if tokio::fs::write(&env_path, &raw_env).await.is_ok() {
                total_size += raw_env.len() as u64;
            }
        }

        // 2. Snapshot website release files & database files (excluding node_modules/.git)
        if kind != "env-only" {
            let src_dir = if base_dir.join("current").exists() {
                base_dir.join("current")
            } else if base_dir.join("repo").exists() {
                base_dir.join("repo")
            } else {
                base_dir.clone()
            };
            if src_dir.exists() {
                let dst_dir = backup_dir.join("files");
                total_size += tokio::task::spawn_blocking(move || copy_backup_files(&src_dir, &dst_dir))
                    .await
                    .unwrap_or(0);
            }
        }

        let b_str = backup_dir.to_string_lossy().to_string();
        let rel_dir = if backup_dir.join("files").exists() {
            backup_dir.join("files").to_string_lossy().to_string()
        } else {
            b_str.clone()
        };
        (total_size, b_str, rel_dir)
    };

    if total_size == 0 {
        total_size = 4096;
    }

    // Also register an immutable release in `releases` so it appears in Releases & Rollbacks tabs!
    let sha = p.deployed_sha.clone().unwrap_or_else(|| format!("{:x}", ts));
    let short_sha = sha[..7.min(sha.len())].to_string();

    let rel_id_opt = insert_success_release(
        &state.db,
        id,
        &sha,
        &short_sha,
        &format!("Snapshot: {}", note),
        &actor.username,
        &release_files_dir,
        ts,
    )
    .await
    .ok();
    let rel_id = rel_id_opt.unwrap_or(0);

    let backup_id: i64 = sqlx::query_scalar(
        "INSERT INTO backups(project_id, release_id, kind, path, size_bytes, note, created_at) \
         VALUES($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(id)
    .bind(rel_id_opt)
    .bind(&kind)
    .bind(&backup_path_str)
    .bind(total_size as i64)
    .bind(&note)
    .bind(ts)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    log_audit_project(&state, Some(id), &actor.username, "backup.create", Some(&p.name), Some(&note)).await;
    state.logs.add(
        Some(id),
        None,
        "backup",
        "info",
        &format!("Created {} backup #{} / Release #{} ({})", kind, backup_id, rel_id, note),
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "backupId": backup_id,
        "releaseId": rel_id,
        "size": total_size
    })))
}

#[derive(Deserialize)]
pub(crate) struct RestoreBackupPayload {
    #[serde(rename = "backupId")]
    pub backup_id: i64,
}

pub(crate) async fn restore_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<RestoreBackupPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "backups", Some(id)).await?;
    let p = state
        .projects
        .get(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Website not found".into()))?;

    let backup_row: Option<(Option<i64>, String, String)> = sqlx::query_as(
        "SELECT release_id, COALESCE(path, ''), COALESCE(note, '') FROM backups WHERE id = $1 AND project_id = $2",
    )
    .bind(payload.backup_id)
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

    if let Some((rel_id, b_path, note)) = backup_row {
        if let Some(nid) = remote_node_id {
            let res = state
                .cluster
                .call_node(
                    nid,
                    "project.restore_backup",
                    serde_json::json!({ "project": &p, "backupPath": b_path }),
                    std::time::Duration::from_secs(120),
                )
                .await?;
            let raw_env = res["rawEnv"].as_str().unwrap_or("");
            if !raw_env.trim().is_empty() {
                let _ = state.projects.set_bulk_env(id, raw_env, true, false).await;
                state.cluster.sync_remote_env_and_keys(&state.projects, &p).await;
            }
            if let Some(target_rel) = rel_id {
                let dep_id = state.deployer.create_deployment(&p, "rollback", &actor.username, "backup-restore").await;
                let rel_info: Option<(String, String)> = sqlx::query_as(
                    "SELECT COALESCE(release_dir, ''), COALESCE(sha, '') FROM releases WHERE id = $1 AND project_id = $2",
                )
                .bind(target_rel)
                .bind(id)
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten();
                let (rel_dir, rel_sha) = rel_info.unwrap_or_default();
                let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
                state
                    .cluster
                    .call_node(
                        nid,
                        "project.rollback",
                        serde_json::json!({
                            "project": &p,
                            "deploymentId": dep_id,
                            "releaseId": target_rel,
                            "releaseDir": rel_dir,
                            "sha": rel_sha,
                            "context": ctx,
                        }),
                        std::time::Duration::from_secs(15),
                    )
                    .await?;
            } else {
                let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
                let _ = state
                    .cluster
                    .call_node(
                        nid,
                        "project.restart",
                        serde_json::json!({ "project": &p, "context": ctx }),
                        std::time::Duration::from_secs(60),
                    )
                    .await;
            }
        } else {
            let b_dir = std::path::PathBuf::from(&b_path);
            // Restore .env if present in snapshot
            let env_file = b_dir.join("backup.env");
            if let Ok(raw_env) = tokio::fs::read_to_string(&env_file).await {
                if !raw_env.trim().is_empty() {
                    let _ = state.projects.set_bulk_env(id, &raw_env, true, false).await;
                }
            }
            // Restore files if present in snapshot
            let files_dir = b_dir.join("files");
            let target_current = std::path::PathBuf::from(&p.deploy_dir).join("current");
            if files_dir.exists() && target_current.exists() {
                let _ = copy_backup_files(&files_dir, &target_current);
            }
            if let Some(target_rel) = rel_id {
                let dep_id = state.deployer.create_deployment(&p, "rollback", &actor.username, "backup-restore").await;
                let deployer = state.deployer.clone();
                tokio::spawn(async move {
                    deployer.run_rollback(id, dep_id, target_rel).await;
                });
            } else {
                let _ = state.runtime.stop_project(&p).await;
                let env = state.projects.get_env_map(id).await;
                let _ = state.runtime.start_project(&p, env, None).await;
            }
        }
        log_audit_project(&state, Some(id), &actor.username, "backup.restore", Some(&p.name), Some(&note)).await;
        state.logs.add(
            Some(id),
            None,
            "backup",
            "info",
            &format!("Restored backup #{} ({}) for {}", payload.backup_id, note, p.name),
        );
    } else {
        // Fallback: treat backup_id as release_id
        let dep_id = state.deployer.create_deployment(&p, "rollback", &actor.username, "backup-restore").await;
        let target_rel = payload.backup_id;
        if let Some(nid) = remote_node_id {
            let rel_info: Option<(String, String)> = sqlx::query_as(
                "SELECT COALESCE(release_dir, ''), COALESCE(sha, '') FROM releases WHERE id = $1 AND project_id = $2",
            )
            .bind(target_rel)
            .bind(id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
            let (rel_dir, rel_sha) = rel_info.unwrap_or_default();
            let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
            state
                .cluster
                .call_node(
                    nid,
                    "project.rollback",
                    serde_json::json!({
                        "project": &p,
                        "deploymentId": dep_id,
                        "releaseId": target_rel,
                        "releaseDir": rel_dir,
                        "sha": rel_sha,
                        "context": ctx,
                    }),
                    std::time::Duration::from_secs(15),
                )
                .await?;
        } else {
            let deployer = state.deployer.clone();
            tokio::spawn(async move {
                deployer.run_rollback(id, dep_id, target_rel).await;
            });
        }
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) async fn delete_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, backup_id)): AxPath<(i64, i64)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "backups", Some(id)).await?;
    let b_path: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT path FROM backups WHERE id = $1 AND project_id = $2",
    )
    .bind(backup_id)
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .flatten();

    let p_opt = state.projects.get(id).await.ok().flatten();
    if let Some(p_str) = b_path {
        if let Some(ref p) = p_opt {
            if let Some(nid) = state
                .cluster
                .should_proxy_to_node(p.node_id)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
            {
                let _ = state
                    .cluster
                    .call_node(
                        nid,
                        "project.delete_path",
                        serde_json::json!({ "project": p, "path": p_str }),
                        std::time::Duration::from_secs(15),
                    )
                    .await;
            } else {
                let path = std::path::Path::new(&p_str);
                let backups_root = std::path::Path::new(&p.deploy_dir).join("backups");
                let releases_root = std::path::Path::new(&p.deploy_dir).join("releases");
                if let Ok(canon_path) = tokio::fs::canonicalize(path).await {
                    let inside_backups = tokio::fs::canonicalize(&backups_root)
                        .await
                        .map(|r| canon_path.starts_with(&r) && canon_path != r)
                        .unwrap_or(false);
                    let inside_releases = tokio::fs::canonicalize(&releases_root)
                        .await
                        .map(|r| canon_path.starts_with(&r) && canon_path != r)
                        .unwrap_or(false);
                    if inside_backups || inside_releases {
                        let _ = tokio::fs::remove_dir_all(&canon_path).await;
                    }
                }
            }
        }
    }

    sqlx::query("DELETE FROM backups WHERE id = $1 AND project_id = $2")
        .bind(backup_id)
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let p_name = p_opt
        .map(|p| p.name)
        .unwrap_or_else(|| format!("Website #{}", id));
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        "backup.delete",
        Some(&p_name),
        Some(&format!("Deleted backup #{}", backup_id)),
    )
    .await;
    state.cache.invalidate_project(id);
    Ok(Json(serde_json::json!({ "ok": true })))
}

