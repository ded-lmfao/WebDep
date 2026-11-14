use std::path::PathBuf;

use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::api::common::*;
use crate::cluster::archive::{pack_dir_to_tar_gz, unpack_tar_gz_safe};
use crate::cluster::node_exec::{allocate_free_port_on_node, resolve_target_deploy_dir};

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
    #[serde(rename = "storageMode")]
    pub storage_mode: String,
    #[serde(rename = "coreBackupPath")]
    pub core_backup_path: String,
    #[serde(rename = "sourceNodeId")]
    pub source_node_id: Option<i64>,
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

    let rows = sqlx::query_as::<_, (i64, i64, i64, String, String, String, Option<i64>, String, String, Option<i64>)>(
        "SELECT id, COALESCE(created_at, 0), COALESCE(size_bytes, 0), COALESCE(note, ''), COALESCE(kind, 'full-snapshot'), COALESCE(path, ''), release_id, COALESCE(storage_mode, 'local'), COALESCE(core_backup_path, ''), source_node_id \
         FROM backups WHERE project_id = $1 ORDER BY id DESC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut backups: Vec<BackupItem> = rows
        .into_iter()
        .map(
            |(bid, ts, size, note, kind, path, release_id, storage_mode, core_backup_path, source_node_id)| BackupItem {
                id: bid,
                ts,
                size: size.max(0) as u64,
                note,
                kind,
                path,
                release_id,
                storage_mode,
                core_backup_path,
                source_node_id,
            },
        )
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
                storage_mode: "local".to_string(),
                core_backup_path: String::new(),
                source_node_id: None,
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
    #[serde(default, rename = "offNode")]
    pub off_node: bool,
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
    let remote_nid = state
        .cluster
        .should_proxy_to_node(p.node_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    let source_node_id = p.node_id.or(state.cluster.default_local_node_id().await);

    let (mut total_size, backup_path_str, release_files_dir) = if let Some(nid) = remote_nid {
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

    let mut storage_mode = "local".to_string();
    let mut core_backup_path = String::new();
    if payload.off_node {
        let core_dir = state
            .cluster
            .data_dir()
            .join("offnode-backups")
            .join(format!("project-{}", id));
        let _ = tokio::fs::create_dir_all(&core_dir).await;
        let core_tgz = core_dir.join(format!("backup-{}.tar.gz", ts));

        if let Some(nid) = remote_nid {
            let fetch_res = state
                .cluster
                .call_node(
                    nid,
                    "project.fetch_backup_tar",
                    serde_json::json!({ "project": &p, "backupPath": &backup_path_str }),
                    std::time::Duration::from_secs(180),
                )
                .await?;
            let b64 = fetch_res["tarGzBase64"]
                .as_str()
                .ok_or_else(|| (StatusCode::BAD_GATEWAY, "Node omitted tarGzBase64 payload".to_string()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Invalid snapshot encoding: {e}")))?;
            tokio::fs::write(&core_tgz, &bytes)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed storing off-node snapshot: {e}")))?;
            storage_mode = "off-node".to_string();
            core_backup_path = core_tgz.to_string_lossy().to_string();
        } else {
            let src_b_dir = PathBuf::from(&backup_path_str);
            let tgz_c = core_tgz.clone();
            let packed = tokio::task::spawn_blocking(move || pack_dir_to_tar_gz(&src_b_dir, &tgz_c, &[]))
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            if packed.0 > 0 {
                total_size = packed.0;
            }
            storage_mode = "off-node".to_string();
            core_backup_path = core_tgz.to_string_lossy().to_string();
        }
    }

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
        "INSERT INTO backups(project_id, release_id, kind, path, size_bytes, note, storage_mode, core_backup_path, source_node_id, created_at) \
         VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id",
    )
    .bind(id)
    .bind(rel_id_opt)
    .bind(&kind)
    .bind(&backup_path_str)
    .bind(total_size as i64)
    .bind(&note)
    .bind(&storage_mode)
    .bind(&core_backup_path)
    .bind(source_node_id)
    .bind(ts)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.cache.invalidate_project(id);
    log_audit_project(&state, Some(id), &actor.username, "backup.create", Some(&p.name), Some(&note)).await;
    state.logs.add(
        Some(id),
        None,
        "backup",
        "info",
        &format!(
            "Created {} {} backup #{} / Release #{} ({})",
            storage_mode, kind, backup_id, rel_id, note
        ),
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "backupId": backup_id,
        "releaseId": rel_id,
        "size": total_size,
        "storageMode": storage_mode,
        "coreBackupPath": core_backup_path,
    })))
}

#[derive(Deserialize)]
pub(crate) struct RestoreBackupPayload {
    #[serde(rename = "backupId")]
    pub backup_id: i64,
    #[serde(default, rename = "targetNodeId")]
    pub target_node_id: Option<i64>,
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

    let backup_row: Option<(Option<i64>, String, String, String, String)> = sqlx::query_as(
        "SELECT release_id, COALESCE(path, ''), COALESCE(note, ''), COALESCE(storage_mode, 'local'), COALESCE(core_backup_path, '') FROM backups WHERE id = $1 AND project_id = $2",
    )
    .bind(payload.backup_id)
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    // Cross-node restore onto any healthy Web-Node from an off-node tarball
    if let Some(target_nid) = payload.target_node_id {
        if Some(target_nid) != p.node_id {
            crate::cluster::shift::verify_cross_node_shift_rbac(&state, &actor, id, target_nid).await?;
        }
        let (_, _, note, _mode, core_tgz) = backup_row
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Backup #{} not found", payload.backup_id)))?;
        if core_tgz.trim().is_empty() || !tokio::fs::try_exists(&core_tgz).await.unwrap_or(false) {
            return Err((
                StatusCode::BAD_REQUEST,
                "Cross-node restore requires an off-node backup archive stored on Web-Core".to_string(),
            ));
        }

        let target_node = state
            .cluster
            .get_node(target_nid)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Target node #{target_nid} not found")))?;
        if target_node.status != "online" {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Target node '{}' is '{}' and cannot accept restored workloads", target_node.name, target_node.status),
            ));
        }
        if !target_node.is_local && !state.cluster.is_node_connected(target_nid).await {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Target node '{}' is offline", target_node.name),
            ));
        }

        let used_ports = state.cluster.used_ports_on_node(target_nid).await;
        let (target_port, target_deploy_dir) = if !target_node.is_local {
            let pre = state
                .cluster
                .call_node(
                    target_nid,
                    "shift.preflight",
                    serde_json::json!({ "project": &p, "usedPorts": &used_ports }),
                    std::time::Duration::from_secs(20),
                )
                .await?;
            let tp = pre["allocatedPort"].as_u64().and_then(|v| u16::try_from(v).ok()).unwrap_or(43000);
            let td = pre["targetDeployDir"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    resolve_target_deploy_dir(state.cluster.data_dir(), &p)
                        .to_string_lossy()
                        .to_string()
                });
            (tp, td)
        } else {
            let tp = allocate_free_port_on_node(p.assigned_port.or(p.port), &used_ports)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            let td = resolve_target_deploy_dir(state.cluster.data_dir(), &p)
                .to_string_lossy()
                .to_string();
            (tp, td)
        };

        let mut target_proj = p.clone();
        target_proj.node_id = Some(target_nid);
        target_proj.port = Some(target_port);
        target_proj.assigned_port = Some(target_port);
        target_proj.deploy_dir = target_deploy_dir.clone();

        let restored_env: String = if !target_node.is_local {
            let tgz_bytes = tokio::fs::read(&core_tgz)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed reading off-node archive: {e}")))?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&tgz_bytes);
            let r_res = state
                .cluster
                .call_node(
                    target_nid,
                    "project.restore_backup_tar",
                    serde_json::json!({ "project": &target_proj, "tarGzBase64": b64 }),
                    std::time::Duration::from_secs(180),
                )
                .await?;
            r_res["rawEnv"].as_str().unwrap_or("").to_string()
        } else {
            let tmp_unpack = state
                .cluster
                .data_dir()
                .join("tmp")
                .join(format!("cross-restore-{id}-{}", now()));
            let core_c = PathBuf::from(&core_tgz);
            let tmp_c = tmp_unpack.clone();
            tokio::task::spawn_blocking(move || unpack_tar_gz_safe(&core_c, &tmp_c))
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

            let env_str = tokio::fs::read_to_string(tmp_unpack.join("backup.env"))
                .await
                .unwrap_or_default();
            let target_current = PathBuf::from(&target_deploy_dir).join("current");
            let _ = tokio::fs::create_dir_all(&target_current).await;
            let src_files = if tmp_unpack.join("files").exists() {
                tmp_unpack.join("files")
            } else {
                tmp_unpack.clone()
            };
            let tc = target_current.clone();
            let _ = tokio::task::spawn_blocking(move || copy_backup_files(&src_files, &tc)).await;
            let _ = tokio::fs::remove_dir_all(&tmp_unpack).await;
            env_str
        };

        if !restored_env.trim().is_empty() {
            let _ = state.projects.set_bulk_env(id, &restored_env, true, false).await;
        }
        let _ = state
            .cluster
            .rewrite_linked_db_env_for_node(&state.projects, id, target_nid)
            .await;

        let ts = now();
        sqlx::query(
            "UPDATE projects
             SET node_id = $2,
                 port = $3,
                 assigned_port = $3,
                 deploy_dir = $4,
                 placement_version = placement_version + 1,
                 updated_at = $5
             WHERE id = $1",
        )
        .bind(id)
        .bind(target_nid)
        .bind(i64::from(target_port))
        .bind(&target_deploy_dir)
        .bind(ts)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        state.cache.invalidate_project(id);
        state.cluster.broadcast_peer_sync().await;

        let refreshed = state
            .projects
            .get(id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .unwrap_or(target_proj);

        if !target_node.is_local {
            let ctx = state.cluster.build_remote_context(&state.projects, id).await;
            let _ = state
                .cluster
                .call_node(
                    target_nid,
                    "project.start",
                    serde_json::json!({ "project": &refreshed, "context": ctx }),
                    std::time::Duration::from_secs(45),
                )
                .await;
            state.cluster.sync_remote_caddy(&state.projects, target_nid).await;
        } else {
            let env = state.projects.get_env_map(id).await;
            let _ = state.runtime.start_project(&refreshed, env, None).await;
            let node_projects = state.projects.list_for_node(target_nid).await.unwrap_or_default();
            let _ = state.caddy.write_config(&refreshed, &node_projects);
            let _ = state.caddy.reload();
        }

        log_audit_project(
            &state,
            Some(id),
            &actor.username,
            "backup.restore",
            Some(&p.name),
            Some(&format!("Cross-node restore #{} to node #{}", payload.backup_id, target_nid)),
        )
        .await;
        return Ok(Json(serde_json::json!({
            "ok": true,
            "restoredToNodeId": target_nid,
            "port": target_port,
            "note": note,
        })));
    }

    let remote_node_id = state
        .cluster
        .should_proxy_to_node(p.node_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    if let Some((rel_id, b_path, note, _storage_mode, _core_tgz)) = backup_row {
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
    let paths_row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT path, core_backup_path FROM backups WHERE id = $1 AND project_id = $2",
    )
    .bind(backup_id)
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let b_path = paths_row.as_ref().and_then(|r| r.0.clone());
    let core_path = paths_row.and_then(|r| r.1);
    if let Some(ref c_str) = core_path {
        if !c_str.trim().is_empty() {
            let off_root = state.cluster.data_dir().join("offnode-backups");
            if let (Ok(canon_file), Ok(canon_root)) = (
                tokio::fs::canonicalize(c_str).await,
                tokio::fs::canonicalize(&off_root).await,
            ) {
                if canon_file.starts_with(&canon_root) {
                    let _ = tokio::fs::remove_file(&canon_file).await;
                }
            }
        }
    }

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

