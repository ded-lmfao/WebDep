use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

use crate::api::common::AppState;
use crate::auth::User;
use crate::cluster::archive::pack_project_shift_to_zip;
use crate::cluster::models::{ClusterNode, NodeStateManifest};
use crate::cluster::node_exec::{
    allocate_free_port_on_node, resolve_source_deploy_dir, resolve_target_deploy_dir,
    sanitize_project_folder, start_p2p_server, stream_archive_to_target, NodeExecState,
};
use crate::projects::Project;
use crate::runtime::container::cleanup_project_files_dirs_in_data_dir_checked;
use crate::runtime::Runtime;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMigrationView {
    pub id: i64,
    pub project_id: i64,
    pub project_name: String,
    pub project_slug: String,
    pub from_node_id: Option<i64>,
    pub from_node_name: String,
    pub to_node_id: Option<i64>,
    pub to_node_name: String,
    pub stage: String,
    pub status: String,
    pub from_ip: String,
    pub to_ip: String,
    pub target_port: i64,
    pub domains: Vec<String>,
    pub cutover_mode: String,
    pub dns_verified: bool,
    pub dns_verified_at: Option<i64>,
    pub transfer_bytes: i64,
    pub initiated_by: String,
    pub notes: String,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartShiftPayload {
    pub target_node_id: i64,
    #[serde(default)]
    pub auto_cutover_without_dns: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckDnsResponse {
    pub migration: AppMigrationView,
    pub dns_verified: bool,
    pub expected_ip: String,
    pub domains: Vec<String>,
    pub resolved_ips: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvacuateNodeResult {
    pub node_id: i64,
    pub shifted: Vec<i64>,
    pub awaiting_dns: Vec<i64>,
    pub failed: Vec<(i64, String)>,
}

/// Enforces cross-node shift RBAC: caller must hold project deploy/manage powers AND
/// have access to the destination node (`target_node_id`).
pub async fn verify_cross_node_shift_rbac(
    state: &AppState,
    user: &User,
    project_id: i64,
    target_node_id: i64,
) -> Result<(), (StatusCode, String)> {
    let has_project_power = state
        .auth
        .has_any_power_for_scope(
            user,
            &["deploy", "projects_manage", "services_manage"],
            "project",
            Some(project_id),
        )
        .await;
    if !has_project_power {
        return Err((
            StatusCode::FORBIDDEN,
            "Insufficient project permission to shift workload".to_string(),
        ));
    }
    if user.is_main_admin {
        return Ok(());
    }
    let _ = target_node_id;
    let has_target_node = !user.disabled
        && user.has_any_power(&["nodes_manage", "projects_manage", "deploy", "*"]);
    if !has_target_node {
        return Err((
            StatusCode::FORBIDDEN,
            "Cross-node shift denied: missing deploy/manage permission".to_string(),
        ));
    }
    Ok(())
}

pub async fn list_project_migrations(
    state: &AppState,
    project_id: i64,
) -> Result<Vec<AppMigrationView>, String> {
    let rows = sqlx::query(
        "SELECT m.id, m.project_id, COALESCE(p.name, ''), COALESCE(p.slug, ''),
                m.from_node_id, COALESCE(nf.name, ''), m.to_node_id, COALESCE(nt.name, ''),
                m.stage, m.status, m.from_ip, m.to_ip, m.target_port, m.domains_json,
                m.cutover_mode, m.dns_verified_at, m.transfer_bytes, m.initiated_by,
                m.notes, m.error, m.started_at, m.finished_at
         FROM migrations m
         LEFT JOIN projects p ON p.id = m.project_id
         LEFT JOIN nodes nf ON nf.id = m.from_node_id
         LEFT JOIN nodes nt ON nt.id = m.to_node_id
         WHERE m.project_id = $1
         ORDER BY m.id DESC
         LIMIT 25",
    )
    .bind(project_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| format!("Failed listing migrations: {e}"))?;

    rows.iter().map(row_to_migration_view).collect()
}

pub async fn get_migration(state: &AppState, migration_id: i64) -> Result<Option<AppMigrationView>, String> {
    let row = sqlx::query(
        "SELECT m.id, m.project_id, COALESCE(p.name, ''), COALESCE(p.slug, ''),
                m.from_node_id, COALESCE(nf.name, ''), m.to_node_id, COALESCE(nt.name, ''),
                m.stage, m.status, m.from_ip, m.to_ip, m.target_port, m.domains_json,
                m.cutover_mode, m.dns_verified_at, m.transfer_bytes, m.initiated_by,
                m.notes, m.error, m.started_at, m.finished_at
         FROM migrations m
         LEFT JOIN projects p ON p.id = m.project_id
         LEFT JOIN nodes nf ON nf.id = m.from_node_id
         LEFT JOIN nodes nt ON nt.id = m.to_node_id
         WHERE m.id = $1",
    )
    .bind(migration_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| format!("Failed loading migration #{migration_id}: {e}"))?;

    row.as_ref().map(row_to_migration_view).transpose()
}

fn row_to_migration_view(r: &sqlx::postgres::PgRow) -> Result<AppMigrationView, String> {
    let domains_json: String = r.try_get(13).unwrap_or_else(|_| "[]".to_string());
    let domains: Vec<String> = serde_json::from_str(&domains_json).unwrap_or_default();
    let dns_verified_at: Option<i64> = r.try_get(15).ok().flatten();
    Ok(AppMigrationView {
        id: r.try_get(0).map_err(|e| e.to_string())?,
        project_id: r.try_get(1).map_err(|e| e.to_string())?,
        project_name: r.try_get(2).map_err(|e| e.to_string())?,
        project_slug: r.try_get(3).map_err(|e| e.to_string())?,
        from_node_id: r.try_get(4).ok().flatten(),
        from_node_name: r.try_get(5).map_err(|e| e.to_string())?,
        to_node_id: r.try_get(6).ok().flatten(),
        to_node_name: r.try_get(7).map_err(|e| e.to_string())?,
        stage: r.try_get(8).map_err(|e| e.to_string())?,
        status: r.try_get(9).map_err(|e| e.to_string())?,
        from_ip: r.try_get(10).map_err(|e| e.to_string())?,
        to_ip: r.try_get(11).map_err(|e| e.to_string())?,
        target_port: r.try_get(12).map_err(|e| e.to_string())?,
        domains,
        cutover_mode: r.try_get(14).map_err(|e| e.to_string())?,
        dns_verified: dns_verified_at.is_some(),
        dns_verified_at,
        transfer_bytes: r.try_get(16).map_err(|e| e.to_string())?,
        initiated_by: r.try_get(17).map_err(|e| e.to_string())?,
        notes: r.try_get(18).map_err(|e| e.to_string())?,
        error: r.try_get(19).ok().flatten(),
        started_at: r.try_get(20).map_err(|e| e.to_string())?,
        finished_at: r.try_get(21).ok().flatten(),
    })
}

async fn fail_migration(state: &AppState, migration_id: i64, err: &str) {
    let now = now_secs();
    let _ = sqlx::query(
        "UPDATE migrations SET stage = 'failed', status = 'failed', error = $2, finished_at = $3 WHERE id = $1",
    )
    .bind(migration_id)
    .bind(err)
    .bind(now)
    .execute(&state.db)
    .await;
}

/// Safe 7-step live application shift orchestrator (`Move to Node`).
pub async fn start_project_shift(
    state: &AppState,
    actor_username: &str,
    project_id: i64,
    target_node_id: i64,
    skip_port_probe: bool,
    auto_cutover_without_dns: bool,
) -> Result<AppMigrationView, (StatusCode, String)> {
    let proj = state
        .projects
        .get(project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Website not found".to_string()))?;

    let from_node_id = proj
        .node_id
        .or(state.cluster.default_local_node_id().await)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Source node is not assigned".to_string()))?;

    if from_node_id == target_node_id {
        return Err((
            StatusCode::BAD_REQUEST,
            "Project is already placed on the selected target node".to_string(),
        ));
    }

    let in_flight: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM migrations WHERE project_id = $1 AND status IN ('running', 'pending', 'awaiting_dns') LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(mid) = in_flight {
        return Err((
            StatusCode::CONFLICT,
            format!("Migration #{mid} is already in progress for this project"),
        ));
    }

    let from_node = state
        .cluster
        .get_node(from_node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let to_node = state
        .cluster
        .get_node(target_node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Target node #{target_node_id} not found")))?;

    if to_node.status != "online" {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Target node '{}' is '{}' and cannot accept workloads", to_node.name, to_node.status),
        ));
    }
    if !to_node.is_local && !state.cluster.is_node_connected(to_node.id).await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Target Web-Node '{}' is not actively connected", to_node.name),
        ));
    }

    let from_is_remote = from_node.as_ref().map(|n| !n.is_local).unwrap_or(false);
    if from_is_remote && !state.cluster.is_node_connected(from_node_id).await {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Source Web-Node #{from_node_id} is offline; use Restore from Off-Node Backup instead"),
        ));
    }

    let from_ip = from_node
        .as_ref()
        .map(|n| if !n.public_ip.trim().is_empty() { n.public_ip.clone() } else { n.mesh_ip.clone() })
        .unwrap_or_default();
    let to_ip = if !to_node.public_ip.trim().is_empty() {
        to_node.public_ip.clone()
    } else {
        to_node.mesh_ip.clone()
    };

    let domains_json = serde_json::to_string(&proj.domains).unwrap_or_else(|_| "[]".to_string());
    let started_at = now_secs();
    let cutover_mode = if proj.domains.is_empty() { "immediate" } else { "dns" };

    let migration_id: i64 = sqlx::query_scalar(
        "INSERT INTO migrations(
            project_id, from_node_id, to_node_id, stage, status,
            from_ip, to_ip, target_port, domains_json, cutover_mode,
            initiated_by, started_at
         ) VALUES($1, $2, $3, 'preflight', 'running', $4, $5, 0, $6, $7, $8, $9)
         RETURNING id",
    )
    .bind(project_id)
    .bind(from_node_id)
    .bind(target_node_id)
    .bind(&from_ip)
    .bind(&to_ip)
    .bind(&domains_json)
    .bind(cutover_mode)
    .bind(actor_username)
    .bind(started_at)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed recording migration: {e}")))?;

    // STEP 1: Target Preflight & Port Allocation
    let used_ports = state.cluster.used_ports_on_node(target_node_id).await;
    let (target_port, target_deploy_dir) = if !to_node.is_local {
        let res = match state
            .cluster
            .call_node(
                target_node_id,
                "shift.preflight",
                json!({
                    "project": &proj,
                    "preferredPort": proj.assigned_port.or(proj.port),
                    "usedPorts": &used_ports,
                }),
                Duration::from_secs(20),
            )
            .await
        {
            Ok(v) => v,
            Err((code, msg)) => {
                fail_migration(state, migration_id, &msg).await;
                return Err((code, msg));
            }
        };
        let port = res["allocatedPort"].as_u64().and_then(|v| u16::try_from(v).ok()).unwrap_or(43000);
        let dir = res["targetDeployDir"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                resolve_target_deploy_dir(state.cluster.data_dir(), &proj)
                    .to_string_lossy()
                    .to_string()
            });
        (port, dir)
    } else {
        let port = match allocate_free_port_on_node(proj.assigned_port.or(proj.port), &used_ports) {
            Ok(p) => p,
            Err(e) => {
                fail_migration(state, migration_id, &e).await;
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e));
            }
        };
        let dir = resolve_target_deploy_dir(state.cluster.data_dir(), &proj)
            .to_string_lossy()
            .to_string();
        (port, dir)
    };

    let notes_json = json!({
        "targetDeployDir": &target_deploy_dir,
        "sourceDeployDir": &proj.deploy_dir,
        "fromIp": &from_ip,
        "toIp": &to_ip,
    })
    .to_string();

    let _ = sqlx::query(
        "UPDATE migrations SET stage = 'transferring', target_port = $2, notes = $3 WHERE id = $1",
    )
    .bind(migration_id)
    .bind(i64::from(target_port))
    .bind(&notes_json)
    .execute(&state.db)
    .await;

    // STEP 2: Direct P2P Tarball Transfer + Env/Key Sync
    let _ = state
        .cluster
        .rewrite_linked_db_env_for_node(&state.projects, project_id, target_node_id)
        .await;

    let mut local_p2p_handle: Option<tokio::task::JoinHandle<()>> = None;
    let (token, p2p_urls): (String, Vec<String>) = if !to_node.is_local {
        let prep = match state
            .cluster
            .call_node(
                target_node_id,
                "shift.prepare_receive",
                json!({ "project": &proj, "destDir": &target_deploy_dir }),
                Duration::from_secs(15),
            )
            .await
        {
            Ok(v) => v,
            Err((code, msg)) => {
                fail_migration(state, migration_id, &msg).await;
                return Err((code, msg));
            }
        };
        let tok = prep["token"].as_str().unwrap_or("").to_string();
        let port_num = prep["p2pPort"].as_u64().and_then(|n| u16::try_from(n).ok());
        (tok, crate::cluster::ClusterManager::build_p2p_target_urls(&to_node, port_num))
    } else {
        // Ephemeral P2P receiver on Control Node for direct worker -> core transfer
        let manifest = NodeStateManifest {
            node_id: to_node.id,
            name: to_node.name.clone(),
            region: to_node.region.clone(),
            core_url: String::new(),
            core_ws_url: String::new(),
            mesh_ip: to_node.mesh_ip.clone(),
            public_ip: to_node.public_ip.clone(),
            wg_private_key: String::new(),
            wg_pubkey: String::new(),
            wg_listen_port: 51820,
            node_token: String::new(),
            peers: Vec::new(),
            workloads: Vec::new(),
            updated_at: now_secs(),
        };
        let local_exec = Arc::new(NodeExecState::new(
            state.cluster.data_dir().to_path_buf(),
            manifest,
        ));
        let tok = local_exec
            .issue_p2p_ticket(
                PathBuf::from(&target_deploy_dir),
                proj.id,
                proj.slug.clone(),
                proj.project_kind.clone(),
                target_port,
                8 * 1024 * 1024 * 1024,
                900,
            )
            .await;
        let (bound_port, handle) = match start_p2p_server(local_exec, "0.0.0.0", 0).await {
            Ok(v) => v,
            Err(e) => {
                fail_migration(state, migration_id, &e).await;
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e));
            }
        };
        local_p2p_handle = Some(handle);
        (tok, crate::cluster::ClusterManager::build_p2p_target_urls(&to_node, Some(bound_port)))
    };

    let bytes_transferred: i64 = if from_is_remote {
        let push_res = state
            .cluster
            .call_node(
                from_node_id,
                "shift.push_tar",
                json!({
                    "project": &proj,
                    "token": token,
                    "targetUrls": p2p_urls,
                }),
                Duration::from_secs(600),
            )
            .await;
        if let Some(h) = local_p2p_handle.take() {
            h.abort();
        }
        match push_res {
            Ok(v) => v["bytesTransferred"].as_i64().unwrap_or(0),
            Err((code, msg)) => {
                cleanup_staged_target(state, &to_node, &proj, &target_deploy_dir, Some(target_port)).await;
                fail_migration(state, migration_id, &msg).await;
                return Err((code, msg));
            }
        }
    } else {
        // Local Source -> P2P Target (.zip containing all app files + node_modules + container data)
        let tmp_dir = state.cluster.data_dir().join("tmp");
        let _ = tokio::fs::create_dir_all(&tmp_dir).await;
        let tmp_zip = tmp_dir.join(format!("local-shift-{project_id}-{}.zip", now_secs()));
        let local_data_dir = state.cluster.data_dir();
        let src_dir = resolve_source_deploy_dir(local_data_dir, &proj);
        let safe_slug = sanitize_project_folder(&proj.slug, proj.id);
        let container_dir = local_data_dir.join("containers").join(&safe_slug);
        let legacy_app_data_dir = local_data_dir.join("app-data").join(&safe_slug);
        let zip_c = tmp_zip.clone();
        let pack = tokio::task::spawn_blocking(move || {
            pack_project_shift_to_zip(
                &src_dir,
                Some(&container_dir),
                Some(&legacy_app_data_dir),
                &zip_c,
            )
        })
        .await;
        let comp_bytes = match pack {
            Ok(Ok((comp, _))) => comp as i64,
            Ok(Err(e)) => {
                let _ = tokio::fs::remove_file(&tmp_zip).await;
                if let Some(h) = local_p2p_handle.take() {
                    h.abort();
                }
                fail_migration(state, migration_id, &e).await;
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e));
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_zip).await;
                if let Some(h) = local_p2p_handle.take() {
                    h.abort();
                }
                let msg = e.to_string();
                fail_migration(state, migration_id, &msg).await;
                return Err((StatusCode::INTERNAL_SERVER_ERROR, msg));
            }
        };

        let mut pushed = false;
        let mut last_err = String::from("No reachable target P2P URL");
        for u in &p2p_urls {
            match stream_archive_to_target(u, &token, &tmp_zip).await {
                Ok(_) => {
                    pushed = true;
                    break;
                }
                Err(e) => last_err = format!("{u}: {e}"),
            }
        }
        let _ = tokio::fs::remove_file(&tmp_zip).await;
        if let Some(h) = local_p2p_handle.take() {
            h.abort();
        }
        if !pushed {
            cleanup_staged_target(state, &to_node, &proj, &target_deploy_dir, Some(target_port)).await;
            fail_migration(state, migration_id, &last_err).await;
            return Err((StatusCode::BAD_GATEWAY, last_err));
        }
        comp_bytes
    };

    let _ = sqlx::query(
        "UPDATE migrations SET stage = 'starting', transfer_bytes = $2 WHERE id = $1",
    )
    .bind(migration_id)
    .bind(bytes_transferred)
    .execute(&state.db)
    .await;

    // STEP 3: Target Warm-Start & Health Probe (auto-rollback on failure leaving Source untouched)
    let mut target_proj = proj.clone();
    target_proj.node_id = Some(target_node_id);
    target_proj.port = Some(target_port);
    target_proj.assigned_port = Some(target_port);
    target_proj.deploy_dir = target_deploy_dir.clone();
    let probe_timeout_s = (proj.health_timeout_s.max(30) as u64).clamp(5, 180);

    if !to_node.is_local {
        let ctx = state.cluster.build_remote_context(&state.projects, project_id).await;
        if let Err((code, msg)) = state
            .cluster
            .call_node(
                target_node_id,
                "shift.warm_start",
                json!({
                    "project": &target_proj,
                    "context": ctx,
                    "healthTimeoutMs": probe_timeout_s * 1000,
                    "skipPortProbe": skip_port_probe,
                }),
                Duration::from_secs(probe_timeout_s + 30),
            )
            .await
        {
            cleanup_staged_target(state, &to_node, &target_proj, &target_deploy_dir, Some(target_port)).await;
            fail_migration(state, migration_id, &msg).await;
            return Err((code, msg));
        }
    } else {
        let env = state.projects.get_env_map(project_id).await;
        if let Err(e) = state.runtime.start_project(&target_proj, env, None).await {
            cleanup_staged_target(state, &to_node, &target_proj, &target_deploy_dir, Some(target_port)).await;
            fail_migration(state, migration_id, &e).await;
            return Err((StatusCode::BAD_GATEWAY, e));
        }
        if !skip_port_probe {
            let mut healthy = false;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(probe_timeout_s);
            while tokio::time::Instant::now() < deadline {
                if Runtime::is_port_accepting(target_port).await {
                    healthy = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            if !healthy {
                let msg = format!(
                    "Local target warm-start health probe failed on port {target_port} within {probe_timeout_s}s"
                );
                cleanup_staged_target(state, &to_node, &target_proj, &target_deploy_dir, Some(target_port)).await;
                fail_migration(state, migration_id, &msg).await;
                return Err((StatusCode::BAD_GATEWAY, msg));
            }
        }
    }

    // STEP 4: Stage 1 Pre-Warm HTTP Caddy on Target + DNS Gate (or auto-cutover if no domains or forced)
    if proj.domains.is_empty() || auto_cutover_without_dns {
        return complete_migration_cutover(state, actor_username, migration_id, "immediate").await;
    }

    if !to_node.is_local {
        let mut node_projects = state
            .projects
            .list_for_node(target_node_id)
            .await
            .unwrap_or_default();
        if !node_projects.iter().any(|x| x.id == target_proj.id) {
            node_projects.push(target_proj.clone());
        }
        if let Err((code, msg)) = state
            .cluster
            .call_node(
                target_node_id,
                "caddy.stage_http",
                json!({
                    "project": &target_proj,
                    "projects": node_projects,
                }),
                Duration::from_secs(15),
            )
            .await
        {
            cleanup_staged_target(state, &to_node, &target_proj, &target_deploy_dir, Some(target_port)).await;
            fail_migration(state, migration_id, &msg).await;
            return Err((code, msg));
        }
    } else {
        let mut node_projects = state
            .projects
            .list_for_node(target_node_id)
            .await
            .unwrap_or_default();
        if !node_projects.iter().any(|x| x.id == target_proj.id) {
            node_projects.push(target_proj.clone());
        }
        let _ = state
            .caddy
            .write_staging_http_config(&target_proj, &node_projects);
        let _ = state.caddy.reload();
    }

    let _ = sqlx::query(
        "UPDATE migrations SET stage = 'awaiting_dns', status = 'awaiting_dns' WHERE id = $1",
    )
    .bind(migration_id)
    .execute(&state.db)
    .await;

    state.logs.add(
        Some(project_id),
        None,
        "shift",
        "info",
        &format!(
            "App '{}' warmed on node '{}' (port {}) — awaiting DNS A-record switch ({} -> {})",
            proj.name, to_node.name, target_port, from_ip, to_ip
        ),
    );

    get_migration(state, migration_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Migration not found".to_string()))
}

pub async fn check_migration_dns(
    state: &AppState,
    migration_id: i64,
) -> Result<CheckDnsResponse, (StatusCode, String)> {
    let mig = get_migration(state, migration_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Migration #{migration_id} not found")))?;

    let mut resolved_ips = Vec::new();
    let expected = mig.to_ip.trim().to_string();
    let mut all_match = !mig.domains.is_empty() && !expected.is_empty();

    for domain in &mig.domains {
        let mut domain_matched = false;
        if let Ok(addrs) = tokio::net::lookup_host((domain.as_str(), 80)).await {
            for addr in addrs {
                let ip_str = addr.ip().to_string();
                if !resolved_ips.contains(&ip_str) {
                    resolved_ips.push(ip_str.clone());
                }
                if ip_str == expected || (expected == "127.0.0.1" && addr.ip() == IpAddr::from([127, 0, 0, 1])) {
                    domain_matched = true;
                }
            }
        }
        if !domain_matched {
            all_match = false;
        }
    }

    if all_match {
        let now = now_secs();
        let _ = sqlx::query("UPDATE migrations SET dns_verified_at = $2 WHERE id = $1")
            .bind(migration_id)
            .bind(now)
            .execute(&state.db)
            .await;
    }

    let updated = get_migration(state, migration_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Migration disappeared".to_string()))?;

    Ok(CheckDnsResponse {
        migration: updated,
        dns_verified: all_match,
        expected_ip: expected,
        domains: mig.domains,
        resolved_ips,
    })
}

pub async fn complete_migration_cutover(
    state: &AppState,
    actor_username: &str,
    migration_id: i64,
    cutover_mode: &str,
) -> Result<AppMigrationView, (StatusCode, String)> {
    let mig = get_migration(state, migration_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Migration #{migration_id} not found")))?;

    if !matches!(mig.status.as_str(), "awaiting_dns" | "running") {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Migration #{migration_id} is in status '{}' and cannot cut over", mig.status),
        ));
    }

    let to_node_id = mig
        .to_node_id
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Migration target node missing".to_string()))?;
    let from_node_id = mig.from_node_id;

    let proj = state
        .projects
        .get(mig.project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Project not found".to_string()))?;

    let to_node = state
        .cluster
        .get_node(to_node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Target node #{to_node_id} not found")))?;

    let notes_val: serde_json::Value =
        serde_json::from_str(&mig.notes).unwrap_or_else(|_| json!({}));
    let target_deploy_dir = notes_val["targetDeployDir"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            resolve_target_deploy_dir(state.cluster.data_dir(), &proj)
                .to_string_lossy()
                .to_string()
        });

    let mut target_proj = proj.clone();
    target_proj.node_id = Some(to_node_id);
    target_proj.port = Some(mig.target_port as u16);
    target_proj.assigned_port = Some(mig.target_port as u16);
    target_proj.deploy_dir = target_deploy_dir.clone();

    // Stage 2: Enable full HTTPS Caddy config on Target
    if !to_node.is_local {
        let mut node_projects = state
            .projects
            .list_for_node(to_node_id)
            .await
            .unwrap_or_default();
        if !node_projects.iter().any(|x| x.id == target_proj.id) {
            node_projects.push(target_proj.clone());
        }
        state
            .cluster
            .call_node(
                to_node_id,
                "caddy.enable_https",
                json!({
                    "project": &target_proj,
                    "projects": node_projects,
                }),
                Duration::from_secs(20),
            )
            .await?;
    } else {
        let mut node_projects = state
            .projects
            .list_for_node(to_node_id)
            .await
            .unwrap_or_default();
        if !node_projects.iter().any(|x| x.id == target_proj.id) {
            node_projects.push(target_proj.clone());
        }
        let _ = state.caddy.write_config(&target_proj, &node_projects);
        let _ = state.caddy.reload();
    }

    // Atomic PostgreSQL ownership transfer (`placement_version + 1`)
    let now = now_secs();
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed starting transaction: {e}")))?;

    sqlx::query(
        "UPDATE projects
         SET node_id = $2,
             port = $3,
             assigned_port = $3,
             deploy_dir = $4,
             placement_version = placement_version + 1,
             app_status = 'running',
             updated_at = $5
         WHERE id = $1",
    )
    .bind(proj.id)
    .bind(to_node_id)
    .bind(mig.target_port)
    .bind(&target_deploy_dir)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed committing project placement: {e}")))?;

    sqlx::query(
        "UPDATE migrations
         SET stage = 'completed',
             status = 'completed',
             cutover_mode = $2,
             dns_verified_at = COALESCE(dns_verified_at, $3),
             finished_at = $3
         WHERE id = $1",
    )
    .bind(migration_id)
    .bind(cutover_mode)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed committing migration record: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Commit failed: {e}")))?;

    state.cache.invalidate_project(proj.id);
    state.cluster.broadcast_peer_sync().await;

    // Clean up source ("before") node files, container data, Caddy config & processes after successful move
    if let Some(src_nid) = from_node_id {
        if src_nid != to_node_id {
            if let Ok(Some(src_node)) = state.cluster.get_node(src_nid).await {
                if !src_node.is_local {
                    let remaining = state
                        .projects
                        .list_for_node(src_nid)
                        .await
                        .unwrap_or_default();
                    match state
                        .cluster
                        .call_node(
                            src_nid,
                            "shift.cleanup_source",
                            json!({
                                "project": &proj,
                                "remainingProjects": remaining,
                                "removeFiles": true,
                            }),
                            Duration::from_secs(120),
                        )
                        .await
                    {
                        Ok(res) => {
                            let removed_dirs = res["removedDirs"].as_u64().unwrap_or(0);
                            state.logs.add(
                                Some(proj.id),
                                None,
                                "shift",
                                "info",
                                &format!(
                                    "Cleaned up previous node '{}' ({} directory tree(s) removed) after moving '{}' to '{}'",
                                    src_node.name, removed_dirs, proj.name, to_node.name
                                ),
                            );
                        }
                        Err((_, msg)) => {
                            state.logs.add(
                                Some(proj.id),
                                None,
                                "shift",
                                "warn",
                                &format!(
                                    "App '{}' moved to '{}', but automatic file cleanup on previous node '{}' failed: {}",
                                    proj.name, to_node.name, src_node.name, msg
                                ),
                            );
                        }
                    }
                } else {
                    let _ = state.runtime.stop_project(&proj).await;
                    let remaining = state
                        .projects
                        .list_for_node(src_nid)
                        .await
                        .unwrap_or_default();
                    state.caddy.remove_project_config(&proj.slug, &remaining);
                    let _ = state.caddy.reload();
                    let local_data_dir = state.cluster.data_dir().to_path_buf();
                    let proj_c = proj.clone();
                    let rem_c = remaining;
                    let clean_res = tokio::task::spawn_blocking(move || {
                        cleanup_project_files_dirs_in_data_dir_checked(
                            &local_data_dir,
                            &proj_c,
                            &rem_c,
                        )
                    })
                    .await;
                    match clean_res {
                        Ok(Ok(removed_dirs)) => {
                            state.logs.add(
                                Some(proj.id),
                                None,
                                "shift",
                                "info",
                                &format!(
                                    "Cleaned up previous node '{}' ({} directory tree(s) removed) after moving '{}' to '{}'",
                                    src_node.name, removed_dirs, proj.name, to_node.name
                                ),
                            );
                        }
                        Ok(Err(e)) => {
                            state.logs.add(
                                Some(proj.id),
                                None,
                                "shift",
                                "warn",
                                &format!(
                                    "App '{}' moved to '{}', but local cleanup on previous node '{}' reported: {}",
                                    proj.name, to_node.name, src_node.name, e
                                ),
                            );
                        }
                        Err(e) => {
                            state.logs.add(
                                Some(proj.id),
                                None,
                                "shift",
                                "warn",
                                &format!("Local source cleanup task failed: {e}"),
                            );
                        }
                    }
                }
            }
        }
    }

    state.logs.add(
        Some(proj.id),
        None,
        "shift",
        "info",
        &format!(
            "Completed live shift of '{}' to node '{}' by {}",
            proj.name, to_node.name, actor_username
        ),
    );

    get_migration(state, migration_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Migration not found".to_string()))
}

pub async fn abort_migration(
    state: &AppState,
    actor_username: &str,
    migration_id: i64,
) -> Result<AppMigrationView, (StatusCode, String)> {
    let mig = get_migration(state, migration_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Migration #{migration_id} not found")))?;

    if matches!(mig.status.as_str(), "completed" | "aborted" | "failed") {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Migration #{migration_id} is already '{}'", mig.status),
        ));
    }

    let proj = state
        .projects
        .get(mig.project_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Project not found".to_string()))?;

    let notes_val: serde_json::Value =
        serde_json::from_str(&mig.notes).unwrap_or_else(|_| json!({}));
    let target_deploy_dir = notes_val["targetDeployDir"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            resolve_target_deploy_dir(state.cluster.data_dir(), &proj)
                .to_string_lossy()
                .to_string()
        });

    if let Some(to_nid) = mig.to_node_id {
        if let Ok(Some(to_node)) = state.cluster.get_node(to_nid).await {
            cleanup_staged_target(
                state,
                &to_node,
                &proj,
                &target_deploy_dir,
                u16::try_from(mig.target_port).ok(),
            )
            .await;
        }
    }

    let now = now_secs();
    let _ = sqlx::query(
        "UPDATE migrations SET stage = 'aborted', status = 'aborted', error = $2, finished_at = $3 WHERE id = $1",
    )
    .bind(migration_id)
    .bind(format!("Aborted by {actor_username}"))
    .bind(now)
    .execute(&state.db)
    .await;

    state.logs.add(
        Some(proj.id),
        None,
        "shift",
        "warn",
        &format!("Aborted live shift #{} for '{}' by {} (Source untouched)", migration_id, proj.name, actor_username),
    );

    get_migration(state, migration_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Migration not found".to_string()))
}

async fn cleanup_staged_target(
    state: &AppState,
    to_node: &ClusterNode,
    base_proj: &Project,
    target_deploy_dir: &str,
    target_port: Option<u16>,
) {
    let mut staged = base_proj.clone();
    staged.node_id = Some(to_node.id);
    staged.deploy_dir = target_deploy_dir.to_string();
    if let Some(tp) = target_port {
        staged.port = Some(tp);
    }

    if !to_node.is_local {
        let remaining = state
            .projects
            .list_for_node(to_node.id)
            .await
            .unwrap_or_default();
        let _ = state
            .cluster
            .call_node(
                to_node.id,
                "shift.cleanup",
                json!({
                    "project": &staged,
                    "remainingProjects": remaining,
                    "removeFiles": true,
                }),
                Duration::from_secs(15),
            )
            .await;
    } else {
        let _ = state.runtime.stop_project(&staged).await;
        let remaining = state
            .projects
            .list_for_node(to_node.id)
            .await
            .unwrap_or_default();
        state.caddy.remove_project_config(&staged.slug, &remaining);
        let _ = state.caddy.reload();
        let _ = cleanup_project_files_dirs_in_data_dir_checked(
            state.cluster.data_dir(),
            &staged,
            &remaining,
        );
    }
}

/// Evacuates all applications placed on `source_node_id` onto healthy online destination nodes
/// (marking `source_node_id` draining first).
pub async fn evacuate_node_workloads(
    state: &AppState,
    actor_username: &str,
    source_node_id: i64,
    preferred_target_node_id: Option<i64>,
) -> Result<EvacuateNodeResult, (StatusCode, String)> {
    let _ = state
        .cluster
        .set_node_status(source_node_id, "draining")
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let nodes = state
        .cluster
        .list_nodes()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let mut online_targets: Vec<ClusterNode> = nodes
        .into_iter()
        .filter(|n| n.id != source_node_id && n.status == "online")
        .collect();
    if online_targets.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "No healthy online destination Web-Node available for evacuation".to_string(),
        ));
    }
    if let Some(pref_id) = preferred_target_node_id {
        if let Some(pos) = online_targets.iter().position(|n| n.id == pref_id) {
            let preferred = online_targets.remove(pos);
            online_targets.insert(0, preferred);
        }
    }

    let workloads = state
        .projects
        .list_for_node(source_node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut shifted = Vec::new();
    let mut awaiting_dns = Vec::new();
    let mut failed = Vec::new();

    for (idx, p) in workloads.into_iter().enumerate() {
        let target = &online_targets[idx % online_targets.len()];
        match start_project_shift(state, actor_username, p.id, target.id, false, false).await {
            Ok(mig) => {
                if mig.status == "awaiting_dns" {
                    awaiting_dns.push(p.id);
                } else {
                    shifted.push(p.id);
                }
            }
            Err((_, err)) => {
                failed.push((p.id, err));
            }
        }
    }

    Ok(EvacuateNodeResult {
        node_id: source_node_id,
        shifted,
        awaiting_dns,
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::models::{
        ClusterWsMessage, CreateEnrollTokenInput, EnrollNodeRequest, NodeCapabilities,
    };
    use crate::cluster::node_exec::execute_rpc;
    use axum::body::Body;
    use axum::http::Method;
    use tower::ServiceExt;

    fn sample_shift_project(
        name: &str,
        slug: &str,
        deploy_dir: &str,
        port: u16,
        node_id: i64,
        domains: Vec<String>,
        ssl_email: &str,
    ) -> Project {
        Project {
            id: 0,
            name: name.to_string(),
            slug: slug.to_string(),
            repo_url: "local".to_string(),
            branch: "main".to_string(),
            project_type: "node".to_string(),
            deploy_method: "managed".to_string(),
            deploy_dir: deploy_dir.to_string(),
            port: Some(port),
            install_cmd: String::new(),
            build_cmd: String::new(),
            // Any long-running process works; avoid requiring Node.js on the test machine.
            start_cmd: if cfg!(windows) {
                "node -e \"setTimeout(()=>{},60000)\"".to_string()
            } else {
                "sleep 60".to_string()
            },
            health_url: String::new(),
            health_timeout_s: 30,
            stability_s: 0,
            auto_deploy: false,
            webhook_secret: None,
            current_release_id: None,
            deployed_sha: None,
            last_poll_at: None,
            app_status: "running".to_string(),
            app_pid: None,
            app_started_at: None,
            domains,
            ssl_email: ssl_email.to_string(),
            created_at: 0,
            updated_at: 0,
            project_kind: "website".to_string(),
            access_mode: "public".to_string(),
            allowed_ips: String::new(),
            node_id: Some(node_id),
            placement_version: 1,
            assigned_port: Some(port),
        }
    }

    async fn enroll_and_wire_remote_node(
        state: &AppState,
        name: &str,
        fail_warm_start: bool,
    ) -> (i64, PathBuf, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
        let tok = state
            .cluster
            .create_enroll_token(
                None,
                CreateEnrollTokenInput {
                    label: name.to_string(),
                    region: Some("eu-central".to_string()),
                    tags: None,
                    ttl_seconds: Some(600),
                    ..Default::default()
                },
                "http://127.0.0.1:8080",
            )
            .await
            .unwrap();

        let node_data = std::env::temp_dir().join(format!(
            "webdep-shift-node-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&node_data);

        let enrolled = state
            .cluster
            .enroll_node(
                EnrollNodeRequest {
                    token: tok.token,
                    name: name.to_string(),
                    region: Some("eu-central".to_string()),
                    public_ip: Some("127.0.0.1".to_string()),
                    wg_pubkey: format!("pk_{name}"),
                    wg_endpoint: None,
                    node_version: "2.0.12".to_string(),
                    capabilities: NodeCapabilities::default(),
                    is_local: false,
                },
                "127.0.0.1",
                "ws://127.0.0.1:8080/api/cluster/node-ws",
            )
            .await
            .unwrap();

        let manifest = NodeStateManifest {
            node_id: enrolled.node_id,
            name: name.to_string(),
            region: "eu-central".to_string(),
            core_url: "http://127.0.0.1:8080".to_string(),
            core_ws_url: "ws://127.0.0.1:8080/api/cluster/node-ws".to_string(),
            mesh_ip: enrolled.mesh_ip.clone(),
            public_ip: "127.0.0.1".to_string(),
            wg_private_key: "priv".to_string(),
            wg_pubkey: format!("pk_{name}"),
            wg_listen_port: 51820,
            node_token: enrolled.node_token.clone(),
            peers: enrolled.peers.clone(),
            workloads: Vec::new(),
            updated_at: now_secs(),
        };

        let exec = Arc::new(NodeExecState::new(node_data.clone(), manifest));
        let (p2p_port, p2p_handle) = start_p2p_server(exec.clone(), "127.0.0.1", 0)
            .await
            .unwrap();
        let caps = NodeCapabilities {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cpu_cores: 4,
            has_wireguard: false,
            has_caddy: false,
            has_postgres: false,
            p2p_port: Some(p2p_port),
            ..Default::default()
        };
        state
            .cluster
            .mark_node_online(
                enrolled.node_id,
                "2.0.12",
                &format!("pk_{name}"),
                "",
                "127.0.0.1",
                &caps,
            )
            .await
            .unwrap();

        let (rpc_tx, mut rpc_rx) = tokio::sync::mpsc::unbounded_channel::<ClusterWsMessage>();
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel::<ClusterWsMessage>();
        state
            .cluster
            .register_peer_sender(enrolled.node_id, rpc_tx)
            .await;

        let cluster_c = state.cluster.clone();
        let exec_c = exec.clone();
        let rpc_handle = tokio::spawn(async move {
            while let Some(msg) = rpc_rx.recv().await {
                if let ClusterWsMessage::RpcRequest { req_id, op, params } = msg {
                    if fail_warm_start && op == "shift.warm_start" {
                        cluster_c
                            .resolve_rpc(
                                &req_id,
                                false,
                                502,
                                serde_json::Value::Null,
                                Some("Simulated target health probe failure".to_string()),
                            )
                            .await;
                        continue;
                    }
                    let reply = execute_rpc(exec_c.clone(), req_id.clone(), op, params, out_tx.clone()).await;
                    if let ClusterWsMessage::RpcResponse {
                        ok,
                        status,
                        result,
                        error,
                        ..
                    } = reply
                    {
                        cluster_c
                            .resolve_rpc(&req_id, ok, status, result, error)
                            .await;
                    }
                }
            }
        });

        (enrolled.node_id, node_data, p2p_handle, rpc_handle)
    }

    #[tokio::test]
    async fn cross_node_rbac_and_7_step_shift_abort_failure_and_cutover() {
        let state = crate::api::common::tests_state_helper().await;
        let local_manifest = state
            .cluster
            .pair_local_node("http://127.0.0.1:8080", "ws://127.0.0.1:8080/api/cluster/node-ws", "core-node")
            .await
            .unwrap();

        let (good_target_id, good_data, good_p2p, good_rpc) =
            enroll_and_wire_remote_node(&state, "shift-good", false).await;
        let (bad_target_id, bad_data, bad_p2p, bad_rpc) =
            enroll_and_wire_remote_node(&state, "shift-bad", true).await;

        let proj_dir = state.cluster.data_dir().join("sites/shift-web");
        let proj_id = state
            .projects
            .create(&sample_shift_project(
                "Shift Web",
                "shift-web",
                &proj_dir.to_string_lossy(),
                43101,
                local_manifest.node_id,
                vec!["shift.example.test".to_string()],
                "ops@example.test",
            ))
            .await
            .unwrap();
        let proj = state.projects.get(proj_id).await.unwrap().unwrap();

        let src_data_dir = state.cluster.data_dir().to_path_buf();
        let src_current = PathBuf::from(&proj.deploy_dir).join("current");
        std::fs::create_dir_all(src_current.join("node_modules/pkg")).unwrap();
        std::fs::write(src_current.join("server.js"), b"// live-app-v1").unwrap();
        std::fs::write(src_current.join("node_modules/pkg/mod.js"), b"exports.ok = true;").unwrap();
        std::fs::create_dir_all(src_data_dir.join("containers/shift-web/db")).unwrap();
        std::fs::create_dir_all(src_data_dir.join("containers/shift-web/storage")).unwrap();
        std::fs::write(src_data_dir.join("containers/shift-web/db/app.db"), b"db-bytes").unwrap();
        std::fs::write(src_data_dir.join("containers/shift-web/storage/pic.png"), b"img-bytes").unwrap();
        std::fs::create_dir_all(src_data_dir.join("keys")).unwrap();
        std::fs::write(src_data_dir.join("keys").join(format!("deploy_key_{}", proj.id)), b"k").unwrap();
        state
            .projects
            .set_env_var(proj.id, "APP_SECRET", "shift-secret", true)
            .await
            .unwrap();

        // 1. Cross-node RBAC: user without project assignment is forbidden; assigning the project with deploy allows shift
        let dev_id: i64 = sqlx::query_scalar(
            "INSERT INTO users(username, password_hash, permissions, allowed_projects, created_at)
             VALUES('shift_dev', 'hash', '[\"deploy\",\"projects_manage\"]', '[]', 1700000000) RETURNING id",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        let dev_user = state
            .auth
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .find(|u| u.id == dev_id)
            .unwrap();
        let rbac_err = verify_cross_node_shift_rbac(&state, &dev_user, proj.id, good_target_id)
            .await
            .unwrap_err();
        assert_eq!(rbac_err.0, StatusCode::FORBIDDEN);

        // Assigning project access on user allows cross-node shift
        sqlx::query("UPDATE users SET allowed_projects = $1 WHERE id = $2")
            .bind(format!("[{}]", proj.id))
            .bind(dev_id)
            .execute(&state.db)
            .await
            .unwrap();
        let dev_user_allowed = state
            .auth
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .find(|u| u.id == dev_id)
            .unwrap();
        verify_cross_node_shift_rbac(&state, &dev_user_allowed, proj.id, good_target_id)
            .await
            .unwrap();

        // 2. Warm-start health failure leaves Source untouched & cleans Target
        let fail_res = start_project_shift(&state, "admin", proj.id, bad_target_id, false, false)
            .await
            .unwrap_err();
        assert_eq!(fail_res.0, StatusCode::BAD_GATEWAY);
        assert!(src_current.join("server.js").exists());
        assert!(!bad_data.join("sites/shift-web/current/server.js").exists());

        // 3. Domain shift pauses at `awaiting_dns` (Stage 1 HTTP Caddy), then Abort cleans Target & keeps Source intact
        let staged = start_project_shift(&state, "admin", proj.id, good_target_id, true, false)
            .await
            .unwrap();
        assert_eq!(staged.status, "awaiting_dns");
        assert_eq!(staged.stage, "awaiting_dns");
        assert!(staged.transfer_bytes > 0);
        let stage1_caddy = std::fs::read_to_string(good_data.join("caddy/shift-web.caddy")).unwrap();
        assert!(stage1_caddy.contains("http://shift.example.test"));
        assert!(good_data.join("sites/shift-web/current/server.js").exists());

        let aborted = abort_migration(&state, "admin", staged.id).await.unwrap();
        assert_eq!(aborted.status, "aborted");
        assert!(!good_data.join("sites/shift-web/current/server.js").exists());
        assert!(src_current.join("server.js").exists());

        // 4. Full 7-step shift + Complete Cutover upgrades Caddy to Stage 2 HTTPS, updates placement_version & cleans Source
        let mig2 = start_project_shift(&state, "admin", proj.id, good_target_id, true, false)
            .await
            .unwrap();
        assert_eq!(mig2.status, "awaiting_dns");
        let completed = complete_migration_cutover(&state, "admin", mig2.id, "manual")
            .await
            .unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.stage, "completed");

        let refreshed = state.projects.get(proj.id).await.unwrap().unwrap();
        assert_eq!(refreshed.node_id, Some(good_target_id));
        assert_eq!(refreshed.port, Some(mig2.target_port as u16));
        assert!(refreshed.placement_version >= 2);
        let stage2_caddy = std::fs::read_to_string(good_data.join("caddy/shift-web.caddy")).unwrap();
        assert!(stage2_caddy.contains("tls ops@example.test"));
        assert!(!src_current.exists());
        assert!(!PathBuf::from(&proj.deploy_dir).exists());
        assert!(!src_data_dir.join("containers/shift-web").exists());
        assert!(!src_data_dir.join("keys").join(format!("deploy_key_{}", proj.id)).exists());
        assert_eq!(
            std::fs::read_to_string(good_data.join("sites/shift-web/current/node_modules/pkg/mod.js")).unwrap(),
            "exports.ok = true;"
        );
        assert_eq!(
            std::fs::read_to_string(good_data.join("containers/shift-web/db/app.db")).unwrap(),
            "db-bytes"
        );
        assert_eq!(
            std::fs::read_to_string(good_data.join("containers/shift-web/storage/pic.png")).unwrap(),
            "img-bytes"
        );
        assert_eq!(
            std::fs::read_to_string(good_data.join("sites/shift-web/current/data/app.db")).unwrap(),
            "db-bytes"
        );

        good_p2p.abort();
        good_rpc.abort();
        bad_p2p.abort();
        bad_rpc.abort();
        let _ = std::fs::remove_dir_all(&good_data);
        let _ = std::fs::remove_dir_all(&bad_data);
    }

    #[tokio::test]
    async fn off_node_backup_and_cross_node_restore_moves_app_to_healthy_node() {
        let state = crate::api::common::tests_state_helper().await;
        let local_manifest = state
            .cluster
            .pair_local_node("http://127.0.0.1:8080", "ws://127.0.0.1:8080/api/cluster/node-ws", "core-node")
            .await
            .unwrap();
        let (target_nid, target_data, target_p2p, target_rpc) =
            enroll_and_wire_remote_node(&state, "dr-target", false).await;

        let admin = state
            .auth
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .find(|u| u.is_main_admin)
            .unwrap();
        let sess = state.auth.create_session(admin.id).await.unwrap();

        let dr_dir = state.cluster.data_dir().join("sites/dr-demo");
        let proj_id = state
            .projects
            .create(&sample_shift_project(
                "DR Demo",
                "dr-demo",
                &dr_dir.to_string_lossy(),
                43201,
                local_manifest.node_id,
                vec![],
                "",
            ))
            .await
            .unwrap();
        let proj = state.projects.get(proj_id).await.unwrap().unwrap();

        let src_current = PathBuf::from(&proj.deploy_dir).join("current");
        std::fs::create_dir_all(&src_current).unwrap();
        std::fs::write(src_current.join("app.js"), b"module.exports = 'dr-ok';").unwrap();
        state
            .projects
            .set_env_var(proj.id, "DR_KEY", "preserved-val", true)
            .await
            .unwrap();

        // Rewrite linked DB env test too
        sqlx::query(
            "INSERT INTO databases(name, engine, db_name, db_user, db_password, port, is_superuser, domain, access_mode, allowed_ips, status, container_id, data_dir, linked_project_id, node_id, created_at, updated_at)
             VALUES('shared-pg', 'postgres', 'appdb', 'appuser', 'secret', 55432, FALSE, '', 'mesh', '', 'running', '', '/tmp/db', $1, $2, 1700000000, 1700000000)",
        )
        .bind(proj.id)
        .bind(local_manifest.node_id)
        .execute(&state.db)
        .await
        .unwrap();

        let router = crate::api::create_router(state.clone());
        let create_req = axum::http::Request::builder()
            .method(Method::POST)
            .uri(format!("/api/projects/{}/backup", proj.id))
            .header("Cookie", format!("nd_session={}", sess.id))
            .header("X-CSRF-Token", &sess.csrf_token)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"kind":"full-snapshot","note":"offnode dr","offNode":true}"#))
            .unwrap();
        let create_resp = router.clone().oneshot(create_req).await.unwrap();
        let create_status = create_resp.status();
        let create_body = axum::body::to_bytes(create_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            create_status,
            StatusCode::OK,
            "create_backup failed: {}",
            String::from_utf8_lossy(&create_body)
        );
        let created_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
        assert_eq!(created_json["storageMode"].as_str(), Some("off-node"));
        let backup_id = created_json["backupId"].as_i64().unwrap();
        let core_path = created_json["coreBackupPath"].as_str().unwrap();
        assert!(PathBuf::from(core_path).exists());

        // Simulate source disk loss
        let _ = std::fs::remove_dir_all(&proj.deploy_dir);

        // Cross-node restore onto `target_nid`
        let restore_req = axum::http::Request::builder()
            .method(Method::POST)
            .uri(format!("/api/projects/{}/backups/restore", proj.id))
            .header("Cookie", format!("nd_session={}", sess.id))
            .header("X-CSRF-Token", &sess.csrf_token)
            .header("Content-Type", "application/json")
            .body(Body::from(format!(
                r#"{{"backupId":{backup_id},"targetNodeId":{target_nid}}}"#
            )))
            .unwrap();
        let restore_resp = router.oneshot(restore_req).await.unwrap();
        assert_eq!(restore_resp.status(), StatusCode::OK);

        let restored = state.projects.get(proj.id).await.unwrap().unwrap();
        assert_eq!(restored.node_id, Some(target_nid));
        assert!(restored.placement_version >= 2);
        assert_eq!(
            std::fs::read_to_string(target_data.join("sites/dr-demo/current/app.js")).unwrap(),
            "module.exports = 'dr-ok';"
        );

        let env_after = state.projects.get_env_map(proj.id).await;
        assert_eq!(env_after.get("DR_KEY").map(String::as_str), Some("preserved-val"));
        // Linked DB sits on local_manifest (10.200.0.1), so cross-node restore rewrites PGHOST to mesh IP!
        assert_eq!(
            env_after.get("PGHOST").map(String::as_str),
            Some(local_manifest.mesh_ip.as_str())
        );

        target_p2p.abort();
        target_rpc.abort();
        let _ = std::fs::remove_dir_all(&target_data);
    }
}
