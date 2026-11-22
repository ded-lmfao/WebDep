use std::path::PathBuf;

use axum::{
    body::Body,
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sqlx::Row;

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
    /// `running`, `complete`, or `failed`.
    pub status: String,
    pub error: String,
    #[serde(rename = "checksumSha256")]
    pub checksum: String,
    #[serde(rename = "fileCount")]
    pub file_count: u64,
    pub locked: bool,
    #[serde(rename = "createdBy")]
    pub created_by: String,
    #[serde(rename = "completedAt")]
    pub completed_at: i64,
    /// `archive` (single .tar.gz) or `legacy` (folder copy from earlier versions).
    pub format: String,
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

    let rows = sqlx::query(
        "SELECT id, COALESCE(created_at, 0), COALESCE(size_bytes, 0), COALESCE(note, ''), COALESCE(kind, 'full-snapshot'), COALESCE(path, ''), release_id, COALESCE(storage_mode, 'local'), COALESCE(core_backup_path, ''), source_node_id, status, error, checksum_sha256, file_count, locked, created_by, completed_at, archive_path \
         FROM backups WHERE project_id = $1 AND COALESCE(kind, '') <> 'release-snapshot' ORDER BY id DESC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let backups: Vec<BackupItem> = rows
        .iter()
        .map(|r| {
            let archive_path: String = r.get(17);
            BackupItem {
                id: r.get(0),
                ts: r.get(1),
                size: r.get::<i64, _>(2).max(0) as u64,
                note: r.get(3),
                kind: r.get(4),
                path: r.get(5),
                release_id: r.get(6),
                storage_mode: r.get(7),
                core_backup_path: r.get(8),
                source_node_id: r.get(9),
                status: r.get(10),
                error: r.get(11),
                checksum: r.get(12),
                file_count: r.get::<i64, _>(13).max(0) as u64,
                locked: r.get(14),
                created_by: r.get(15),
                completed_at: r.get(16),
                format: if archive_path.is_empty() { "legacy" } else { "archive" }.to_string(),
            }
        })
        .collect();

    cached_json_response(&state.cache, cache_key, &backups, std::time::Duration::from_secs(15))
        .map_err(|code| (code, "Serialization error".to_string()))
}

#[derive(Deserialize)]
pub(crate) struct CreateBackupPayload {
    pub note: Option<String>,
    pub kind: Option<String>,
    #[serde(default, rename = "offNode")]
    pub off_node: bool,
    /// File or folder names left out of the backup (`cache`, `*.log`).
    #[serde(default)]
    pub ignore: Vec<String>,
}

pub(crate) async fn create_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(payload): Json<CreateBackupPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "backups", Some(id)).await?;
    let backup_limit = crate::app_limits::get(&state.db, crate::app_limits::Kind::Project, id)
        .await
        .ok()
        .flatten()
        .map_or(0, |l| l.backups);
    if backup_limit > 0 {
        let held = crate::backups::count_held(&state.db, id).await;
        if held >= i64::from(backup_limit) {
            return Err((
                StatusCode::CONFLICT,
                format!("{}This app already holds its limit of {backup_limit} backup(s). Delete one first or ask an admin to raise the limit.", crate::api::limits::BACKUP_LIMIT_TAG),
            ));
        }
    }
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

    // Apps on this server are backed up as one archive, built in the background. (A request for
    // an off-node copy keeps the earlier synchronous flow, which returns the finished copy.)
    if !payload.off_node {
        return create_archive_backup(&state, &actor, &p, note, kind, &payload.ignore, remote_nid).await;
    }

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

/// Asks a remote node to roll `p` back to release `target_rel` as deployment `dep_id`.
async fn rollback_on_remote_node(
    state: &AppState,
    nid: i64,
    p: &Project,
    dep_id: i64,
    target_rel: i64,
) -> Result<(), (StatusCode, String)> {
    let rel_info: Option<(String, String)> = sqlx::query_as(
        "SELECT COALESCE(release_dir, ''), COALESCE(sha, '') FROM releases WHERE id = $1 AND project_id = $2",
    )
    .bind(target_rel)
    .bind(p.id)
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
                "project": p,
                "deploymentId": dep_id,
                "releaseId": target_rel,
                "releaseDir": rel_dir,
                "sha": rel_sha,
                "context": ctx,
            }),
            std::time::Duration::from_secs(15),
        )
        .await?;
    Ok(())
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

    let backup_meta: Option<(String, String)> =
        sqlx::query_as("SELECT status, archive_path FROM backups WHERE id = $1 AND project_id = $2")
            .bind(payload.backup_id)
            .bind(id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
    let mut archive_for_cross_node = String::new();
    if let Some((status, archive)) = backup_meta {
        if status != "complete" {
            return Err((StatusCode::CONFLICT, "This backup isn't complete, so it can't be restored.".into()));
        }
        // Restoring in place replaces the app's files; a target node moves the app there instead.
        if payload.target_node_id.is_none() {
            return start_restore(&state, &actor, &p, payload.backup_id).await;
        }
        if !archive.is_empty() {
            archive_for_cross_node = archive;
        }
    }

    // Cross-node restore onto any healthy Web-Node from an off-node tarball
    if let Some(target_nid) = payload.target_node_id {
        if Some(target_nid) != p.node_id {
            crate::cluster::shift::verify_cross_node_shift_rbac(&state, &actor, id, target_nid).await?;
        }
        let (_, _, note, _mode, core_tgz) = backup_row
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Backup #{} not found", payload.backup_id)))?;
        let core_tgz = if core_tgz.trim().is_empty() { archive_for_cross_node } else { core_tgz };
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
            let target_dir = if p.project_kind == "service" {
                PathBuf::from(&target_deploy_dir).join("app")
            } else {
                PathBuf::from(&target_deploy_dir).join("current")
            };
            let _ = tokio::fs::create_dir_all(&target_dir).await;
            let src_files = if p.project_kind == "service" && tmp_unpack.join("files/app").exists() {
                tmp_unpack.join("files/app")
            } else if tmp_unpack.join("files").exists() {
                tmp_unpack.join("files")
            } else if p.project_kind == "service" && tmp_unpack.join("app").exists() {
                tmp_unpack.join("app")
            } else if tmp_unpack.join("current").exists() {
                tmp_unpack.join("current")
            } else {
                tmp_unpack.clone()
            };
            let tc = target_dir.clone();
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

    // No backup row: treat the id as a release id and roll back to it.
    let dep_id = state.deployer.create_deployment(&p, "rollback", &actor.username, "backup-restore").await;
    let target_rel = payload.backup_id;
    if let Some(nid) = remote_node_id {
        rollback_on_remote_node(&state, nid, &p, dep_id, target_rel).await?;
    } else {
        let deployer = state.deployer.clone();
        tokio::spawn(async move {
            deployer.run_rollback(id, dep_id, target_rel).await;
        });
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

pub(crate) async fn delete_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, backup_id)): AxPath<(i64, i64)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "backups", Some(id)).await?;
    let meta: Option<(bool, String, String)> =
        sqlx::query_as("SELECT locked, status, archive_path FROM backups WHERE id = $1 AND project_id = $2")
            .bind(backup_id)
            .bind(id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
    if let Some((locked, status, archive)) = &meta {
        if *locked {
            return Err((StatusCode::CONFLICT, "This backup is locked. Unlock it before deleting.".into()));
        }
        if status == "running" {
            return Err((StatusCode::CONFLICT, "This backup is still being created.".into()));
        }
        if !archive.is_empty() {
            let remote = match state.projects.get(id).await.ok().flatten() {
                Some(p) => state
                    .cluster
                    .should_proxy_to_node(p.node_id)
                    .await
                    .map_err(|e| (StatusCode::BAD_GATEWAY, e))?,
                None => None,
            };
            if let Some(nid) = remote {
                let _ = state
                    .cluster
                    .call_node(nid, "project.backup_delete", serde_json::json!({ "archivePath": archive }), std::time::Duration::from_secs(30))
                    .await;
            } else {
                crate::backups::remove_archive_file(state.cluster.data_dir(), std::path::Path::new(archive));
            }
        }
    }
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


/// Starts a backup of an app on this server and returns at once; the archive is built in the
/// background and the backup row moves from `running` to `complete` (or `failed`).
async fn create_archive_backup(
    state: &AppState,
    actor: &crate::auth::User,
    p: &Project,
    note: String,
    kind: String,
    ignore: &[String],
    remote: Option<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    const MB: u64 = 1024 * 1024;
    let ignore = if kind == "env-only" {
        vec!["*".to_string()]
    } else {
        crate::backups::clean_ignore(ignore).map_err(|e| (StatusCode::BAD_REQUEST, e))?
    };
    let running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backups WHERE project_id = $1 AND status = 'running'")
        .bind(p.id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    if running > 0 {
        return Err((StatusCode::CONFLICT, "A backup of this app is already being created. Wait for it to finish.".into()));
    }
    if crate::backups::is_restoring(state.cluster.data_dir(), p.id) {
        return Err((StatusCode::CONFLICT, "This app is being restored. Wait for the restore to finish before backing it up.".into()));
    }

    let data_dir = state.cluster.data_dir().to_path_buf();
    // An app on another VPS is backed up there (the node checks its own disk space).
    let root = if remote.is_none() {
        let root = crate::api::files::service_root(p)?;
        let probe = root.clone();
        let estimate = tokio::task::spawn_blocking(move || crate::monitor::get_path_disk_size(&probe))
            .await
            .unwrap_or(0);
        if let Some(free) = crate::backups::free_space_bytes(&data_dir) {
            if estimate > free {
                return Err((
                    StatusCode::CONFLICT,
                    format!("Not enough free disk space for this backup ({} MB needed, {} MB free).", estimate / MB, free / MB),
                ));
            }
        }
        Some(root)
    } else {
        None
    };

    let source_node_id = p.node_id.or(state.cluster.default_local_node_id().await);
    let backup_id: i64 = sqlx::query_scalar(
        "INSERT INTO backups(project_id, kind, note, status, created_by, created_at, storage_mode, source_node_id) \
         VALUES($1, $2, $3, 'running', $4, $5, 'local', $6) RETURNING id",
    )
    .bind(p.id)
    .bind(&kind)
    .bind(&note)
    .bind(&actor.username)
    .bind(now())
    .bind(source_node_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Record where the archive will be right away, so the backup shows as the new format while it builds.
    let _ = sqlx::query("UPDATE backups SET archive_path = $2 WHERE id = $1")
        .bind(backup_id)
        .bind(if remote.is_some() {
            crate::backups::PENDING_ARCHIVE.to_string()
        } else {
            crate::backups::archive_path(&data_dir, p.id, backup_id).to_string_lossy().to_string()
        })
        .execute(&state.db)
        .await;

    state.cache.invalidate_project(p.id);
    log_audit_project(state, Some(p.id), &actor.username, "backup.create", Some(&p.name), Some(&format!("Started backup #{backup_id}: {note}"))).await;

    let (state, p, actor_name) = (state.clone(), p.clone(), actor.username.clone());
    tokio::spawn(async move {
        match (remote, root) {
            (Some(nid), _) => run_remote_archive_backup(state, backup_id, p, nid, ignore, kind, actor_name).await,
            (None, Some(root)) => run_archive_backup(state, backup_id, p, root, ignore, actor_name).await,
            (None, None) => {}
        }
    });

    Ok(Json(serde_json::json!({ "ok": true, "backupId": backup_id, "status": "running", "storageMode": "local" })))
}

async fn run_archive_backup(
    state: AppState,
    backup_id: i64,
    p: Project,
    root: PathBuf,
    ignore: Vec<String>,
    actor: String,
) {
    let env = state.projects.export_raw_env(p.id).await.ok();
    let dest = crate::backups::archive_path(state.cluster.data_dir(), p.id, backup_id);
    let part = crate::backups::part_path(&dest);

    let (part_c, ignore_c) = (part.clone(), ignore);
    let packed = tokio::task::spawn_blocking(move || {
        crate::cluster::archive::pack_backup_archive(&root, env.as_deref(), &part_c, &ignore_c)
    })
    .await;
    let built = match packed {
        Ok(Ok(stats)) => tokio::fs::rename(&part, &dest)
            .await
            .map(|()| stats)
            .map_err(|e| format!("Couldn't finish the archive: {e}")),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("The backup task stopped unexpectedly: {e}")),
    };

    let outcome = match built {
        Ok(stats) => crate::backups::finish_ok(&state.db, backup_id, &dest, stats.compressed_bytes, stats.files, &stats.sha256)
            .await
            .map(|()| stats)
            .map_err(|e| format!("Couldn't save the backup record: {e}")),
        Err(e) => Err(e),
    };

    match outcome {
        Ok(stats) => {
            state.logs.add(
                Some(p.id),
                None,
                "backup",
                "info",
                &format!("Backup #{backup_id} complete: {} files, {} KB", stats.files, stats.compressed_bytes / 1024),
            );
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&part).await;
            let _ = tokio::fs::remove_file(&dest).await;
            crate::backups::finish_failed(&state.db, backup_id, &error).await;
            state.logs.add(Some(p.id), None, "backup", "error", &format!("Backup #{backup_id} failed: {error}"));
            log_audit_project(&state, Some(p.id), &actor, "backup.failed", Some(&p.name), Some(&error)).await;
        }
    }
    state.cache.invalidate_project(p.id);
}

// ---------------------------------------------------------------------------
// Restore: replace the app's files with a backup
// ---------------------------------------------------------------------------

/// What a restore needs to know about one finished backup.
struct BackupRow {
    archive_path: String,
    path: String,
    checksum: String,
    file_count: u64,
    kind: String,
    note: String,
}

impl BackupRow {
    fn has_archive(&self) -> bool {
        !self.archive_path.is_empty() && self.archive_path != crate::backups::PENDING_ARCHIVE
    }

    fn source(&self) -> crate::backups::RestoreSource {
        if self.has_archive() {
            crate::backups::RestoreSource::Archive {
                path: PathBuf::from(&self.archive_path),
                sha256: self.checksum.clone(),
                files: self.file_count,
            }
        } else {
            crate::backups::RestoreSource::Dir { path: PathBuf::from(&self.path) }
        }
    }
}

/// Starts restoring a backup in place and returns at once; progress is reported by
/// `GET /api/projects/:id/backups/restore-status`. Works for archive and earlier folder backups,
/// for apps on this server and on other VPSes.
async fn start_restore(
    state: &AppState,
    actor: &crate::auth::User,
    p: &Project,
    backup_id: i64,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let remote = state
        .cluster
        .should_proxy_to_node(p.node_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    let row = sqlx::query(
        "SELECT archive_path, path, checksum_sha256, file_count, kind, note FROM backups WHERE id = $1 AND project_id = $2 AND status = 'complete'",
    )
    .bind(backup_id)
    .bind(p.id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .ok_or((StatusCode::NOT_FOUND, format!("Backup #{backup_id} not found")))?;
    let backup = BackupRow {
        archive_path: row.get(0),
        path: row.get(1),
        checksum: row.get(2),
        file_count: row.get::<i64, _>(3).max(0) as u64,
        kind: row.get(4),
        note: row.get(5),
    };

    if remote.is_none() {
        let location = if backup.has_archive() { &backup.archive_path } else { &backup.path };
        if !tokio::fs::try_exists(location).await.unwrap_or(false) {
            return Err((StatusCode::CONFLICT, "The backup file is missing from this server, so it can't be restored.".into()));
        }
    }
    let building: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backups WHERE project_id = $1 AND status = 'running'")
        .bind(p.id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    if building > 0 {
        return Err((StatusCode::CONFLICT, "A backup of this app is being created. Wait for it to finish, then restore.".into()));
    }
    if state.deployer.has_active_deployment(p.id).await {
        return Err((StatusCode::CONFLICT, format!("A deployment of '{}' is in progress. Wait for it to finish, then restore.", p.name)));
    }
    if !crate::backups::begin_restore(state.cluster.data_dir(), p.id, backup_id) {
        return Err((StatusCode::CONFLICT, "A restore of this app is already running.".into()));
    }

    log_audit_project(state, Some(p.id), &actor.username, "backup.restore", Some(&p.name), Some(&format!("Started restoring backup #{backup_id}"))).await;

    let (state, p, actor_name) = (state.clone(), p.clone(), actor.username.clone());
    tokio::spawn(async move {
        // If the restore task panics, don't leave the app locked as "restoring".
        let (project_id, data_dir) = (p.id, state.cluster.data_dir().to_path_buf());
        let task = tokio::spawn(run_restore(state, p, actor_name, backup_id, backup, remote));
        if let Err(e) = task.await {
            crate::backups::finish_restore(&data_dir, project_id, false, format!("The restore stopped unexpectedly: {e}"));
        }
    });

    Ok(Json(serde_json::json!({ "ok": true, "status": "running", "backupId": backup_id })))
}

async fn run_restore(state: AppState, p: Project, actor: String, backup_id: i64, backup: BackupRow, remote: Option<i64>) {
    match restore_in_place(&state, &p, backup_id, &backup, remote).await {
        Ok(message) => {
            crate::backups::finish_restore(state.cluster.data_dir(), p.id, true, message.clone());
            state.logs.add(Some(p.id), None, "backup", "info", &format!("Restored backup #{backup_id} ({}). {message}", backup.note));
            log_audit_project(&state, Some(p.id), &actor, "backup.restore", Some(&p.name), Some(&format!("Restored backup #{backup_id} ({})", backup.note))).await;
        }
        Err(error) => {
            crate::backups::finish_restore(state.cluster.data_dir(), p.id, false, error.clone());
            state.logs.add(Some(p.id), None, "backup", "error", &format!("Restore of backup #{backup_id} failed: {error}"));
            log_audit_project(&state, Some(p.id), &actor, "backup.restore_failed", Some(&p.name), Some(&error)).await;
        }
    }
    state.cache.invalidate_all();
}

/// The restore itself. The backup is proven good and staged before the app is touched; the swap
/// is two renames with rollback, so the app is never left half restored. For an app on another
/// VPS the node does the file work and Core then replaces the environment variables.
async fn restore_in_place(
    state: &AppState,
    p: &Project,
    backup_id: i64,
    backup: &BackupRow,
    remote: Option<i64>,
) -> Result<String, String> {
    let fresh = state.projects.get(p.id).await.ok().flatten().unwrap_or_else(|| p.clone());
    let was_running = matches!(fresh.app_status.as_str(), "running" | "maintenance");
    let env_only = backup.kind == "env-only";

    let (env, files_replaced): (Option<String>, bool) = if let Some(nid) = remote {
        let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
        let res = state
            .cluster
            .call_node(
                nid,
                "project.backup_restore",
                serde_json::json!({
                    "project": &fresh,
                    "backupId": backup_id,
                    "archivePath": if backup.has_archive() { backup.archive_path.as_str() } else { "" },
                    "backupPath": backup.path,
                    "sha256": backup.checksum,
                    "files": backup.file_count,
                    "envOnly": env_only,
                    "wasRunning": was_running,
                    "context": ctx,
                }),
                std::time::Duration::from_secs(1800),
            )
            .await
            .map_err(|(_, e)| e)?;
        (res["rawEnv"].as_str().map(str::to_string), res["filesReplaced"].as_bool().unwrap_or(true))
    } else {
        let root = crate::api::files::service_root(p).map_err(|(_, e)| e)?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let work = crate::backups::deploy_base(&p.deploy_dir, &cwd);
        let real_root = crate::backups::resolve_real_root(&root);
        let label = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".to_string());
        let stop = {
            let (state, fresh) = (state.clone(), fresh.clone());
            move || async move {
                if was_running {
                    let _ = state.runtime.stop_project(&fresh).await;
                }
            }
        };
        match crate::backups::restore_files(backup.source(), env_only, &work, &real_root, &label, backup_id, stop).await {
            Ok(outcome) => (outcome.env, outcome.files_replaced),
            Err(failure) => {
                if failure.app_stopped && was_running {
                    let env = state.projects.get_env_map(p.id).await;
                    let _ = state.runtime.start_project(&fresh, env, None).await;
                }
                return Err(failure.message);
            }
        }
    };

    // Environment variables come back exactly as they were in the backup.
    let mut notes: Vec<String> = Vec::new();
    if let Some(env) = &env {
        if let Err(e) = state.projects.set_bulk_env(p.id, env, true, true).await {
            notes.push(format!("the environment variables were not restored ({e})"));
        } else if remote.is_some() {
            state.cluster.sync_remote_env_and_keys(&state.projects, &fresh).await;
        }
    }

    // Start the app again if it was running.
    if was_running {
        if let Some(nid) = remote {
            let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
            if let Err((_, e)) = state
                .cluster
                .call_node(nid, "project.start", serde_json::json!({ "project": &fresh, "context": ctx }), std::time::Duration::from_secs(60))
                .await
            {
                notes.push(format!("the app didn't start again ({e})"));
            }
        } else {
            let env = state.projects.get_env_map(p.id).await;
            if let Err(e) = state.runtime.start_project(&fresh, env, None).await {
                notes.push(format!("the app didn't start again ({e})"));
            }
        }
    }

    let mut message = if files_replaced {
        "Restored. Files that weren't in the backup were removed; the previous files are kept for 24 hours.".to_string()
    } else {
        "Restored the environment variables. This backup holds only environment variables, so the app's files were not changed.".to_string()
    };
    if !notes.is_empty() {
        message.push_str(&format!(" Note: {}.", notes.join("; ")));
    }
    Ok(message)
}

pub(crate) async fn restore_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    verify_power(&state, &headers, "backups", Some(id)).await?;
    Ok(Json(match crate::backups::restore_status(state.cluster.data_dir(), id) {
        Some(status) => serde_json::json!(status),
        None => serde_json::json!({ "state": "idle" }),
    }))
}

/// Removes undo copies older than a day (and, at startup, staging folders left by a restore
/// that was cut off).
pub(crate) async fn undo_housekeeping(state: &AppState, at_startup: bool) {
    let Ok(projects) = state.projects.list().await else { return };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for p in projects {
        if crate::backups::is_restoring(state.cluster.data_dir(), p.id) {
            continue;
        }
        let work = crate::backups::deploy_base(&p.deploy_dir, &cwd);
        tokio::task::spawn_blocking(move || {
            if at_startup {
                crate::backups::remove_stale_staging(&work);
            }
            crate::backups::prune_undo_copies(&work, crate::backups::UNDO_KEEP_SECS);
        })
        .await
        .ok();
    }
}

// ---------------------------------------------------------------------------
// Lock and download
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct LockBackupPayload {
    pub locked: bool,
}

/// Locks a backup so it can't be deleted by mistake, or unlocks it.
pub(crate) async fn lock_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, backup_id)): AxPath<(i64, i64)>,
    Json(payload): Json<LockBackupPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "backups", Some(id)).await?;
    let done = sqlx::query("UPDATE backups SET locked = $3 WHERE id = $1 AND project_id = $2")
        .bind(backup_id)
        .bind(id)
        .bind(payload.locked)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, format!("Backup #{backup_id} not found")));
    }
    state.cache.invalidate_project(id);
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        if payload.locked { "backup.lock" } else { "backup.unlock" },
        Some(&format!("Backup #{backup_id}")),
        Some(if payload.locked { "Locked the backup" } else { "Unlocked the backup" }),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true, "locked": payload.locked })))
}

/// Streams a finished backup archive to the browser.
pub(crate) async fn download_backup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath((id, backup_id)): AxPath<(i64, i64)>,
) -> Result<Response, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, "backups", Some(id)).await?;
    let row = sqlx::query("SELECT archive_path, status FROM backups WHERE id = $1 AND project_id = $2")
        .bind(backup_id)
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("Backup #{backup_id} not found")))?;
    let archive: String = row.get(0);
    let status: String = row.get(1);
    if status != "complete" {
        return Err((StatusCode::CONFLICT, "This backup isn't complete yet.".into()));
    }
    if archive.is_empty() {
        return Err((StatusCode::CONFLICT, "Only backups made as a single archive can be downloaded.".into()));
    }
    if let Some(p) = state.projects.get(id).await.ok().flatten() {
        if state
            .cluster
            .should_proxy_to_node(p.node_id)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?
            .is_some()
        {
            return Err((StatusCode::CONFLICT, "This backup is stored on another VPS, so it can't be downloaded from here yet.".into()));
        }
    }

    // The file must be one of WebDep's own backup archives.
    let root = crate::backups::archives_root(state.cluster.data_dir());
    let file_path = tokio::fs::canonicalize(&archive)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "The backup file is missing from this server.".to_string()))?;
    let canon_root = tokio::fs::canonicalize(&root)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "The backup file is missing from this server.".to_string()))?;
    if !file_path.starts_with(&canon_root) {
        return Err((StatusCode::FORBIDDEN, "That file isn't a WebDep backup.".into()));
    }

    let file = tokio::fs::File::open(&file_path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Couldn't open the backup file: {e}")))?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let slug: String = state
        .projects
        .get(id)
        .await
        .ok()
        .flatten()
        .map(|p| p.slug)
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let name = format!("webdep-backup-{}-{backup_id}.tar.gz", if slug.is_empty() { "app" } else { &slug });

    let stream = futures_util::stream::unfold(file, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 64 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });

    log_audit_project(&state, Some(id), &actor.username, "backup.download", Some(&format!("Backup #{backup_id}")), Some("Downloaded the backup archive")).await;
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/gzip")
        .header(axum::http::header::CONTENT_LENGTH, len.to_string())
        .header(axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))
        .body(Body::from_stream(stream))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Builds a backup on the VPS where the app runs, and records where the node saved it.
async fn run_remote_archive_backup(
    state: AppState,
    backup_id: i64,
    p: Project,
    node_id: i64,
    ignore: Vec<String>,
    kind: String,
    actor: String,
) {
    let env = state.projects.export_raw_env(p.id).await.ok();
    let result = state
        .cluster
        .call_node(
            node_id,
            "project.backup_archive",
            serde_json::json!({ "project": &p, "backupId": backup_id, "kind": kind, "rawEnv": env, "ignore": ignore }),
            std::time::Duration::from_secs(3600),
        )
        .await
        .map_err(|(_, e)| e)
        .and_then(|r| {
            let path = r["archivePath"]
                .as_str()
                .ok_or_else(|| "The node didn't report where it saved the backup.".to_string())?
                .to_string();
            Ok((path, r["size"].as_u64().unwrap_or(0), r["files"].as_u64().unwrap_or(0), r["sha256"].as_str().unwrap_or("").to_string()))
        });

    let outcome = match result {
        Ok((path, size, files, sha)) => crate::backups::finish_ok(&state.db, backup_id, std::path::Path::new(&path), size, files, &sha)
            .await
            .map(|()| (files, size))
            .map_err(|e| format!("Couldn't save the backup record: {e}")),
        Err(e) => Err(e),
    };
    match outcome {
        Ok((files, size)) => {
            state.logs.add(Some(p.id), None, "backup", "info", &format!("Backup #{backup_id} complete on its VPS: {files} files, {} KB", size / 1024));
        }
        Err(error) => {
            crate::backups::finish_failed(&state.db, backup_id, &error).await;
            state.logs.add(Some(p.id), None, "backup", "error", &format!("Backup #{backup_id} failed: {error}"));
            log_audit_project(&state, Some(p.id), &actor, "backup.failed", Some(&p.name), Some(&error)).await;
        }
    }
    state.cache.invalidate_project(p.id);
}
