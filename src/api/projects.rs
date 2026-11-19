use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::Deserialize;

use super::common::*;

pub mod backups;
pub mod env;
pub mod git_ops;

pub(crate) use backups::*;
pub(crate) use env::*;
pub(crate) use git_ops::*;

#[derive(serde::Serialize)]
pub(crate) struct ProjectResponse {
    #[serde(flatten)]
    pub(crate) project: Project,
    #[serde(rename = "diskFormatted")]
    pub(crate) disk_formatted: String,
}

fn enrich_project_status(p: &mut Project) {
    let pid_alive = p.app_pid.map(crate::runtime::process::is_pid_alive).unwrap_or(false);
    if pid_alive {
        p.app_status = "running".to_string();
    } else if p.app_status == "running" {
        if let Some(port) = p.port {
            if let Some(&port_pid) = crate::runtime::Runtime::find_pids_on_port_cached(port).first() {
                if crate::runtime::process::is_pid_alive(port_pid) {
                    p.app_pid = Some(port_pid);
                }
            }
        }
    }
}

pub(crate) fn to_project_response(mut p: Project, cwd: &std::path::Path) -> ProjectResponse {
    enrich_project_status(&mut p);
    let (_, disk_formatted) = crate::api::system::compute_project_disk_formatted(&p, cwd);
    ProjectResponse {
        project: p,
        disk_formatted,
    }
}

pub(crate) fn to_remote_project_response(
    p: Project,
    remote_disk_bytes: Option<u64>,
) -> ProjectResponse {
    let disk_formatted = crate::api::system::format_resource_bytes(remote_disk_bytes.unwrap_or(0));
    ProjectResponse {
        project: p,
        disk_formatted,
    }
}

async fn resolve_remote_node(
    state: &AppState,
    node_id: Option<i64>,
) -> Result<Option<i64>, (StatusCode, String)> {
    state
        .cluster
        .should_proxy_to_node(node_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
}

static CACHED_CWD: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

fn cached_cwd() -> &'static std::path::Path {
    CACHED_CWD
        .get_or_init(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
        .as_path()
}

pub(crate) async fn list_projects_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let user = require_current_user(&state, &headers).await.map_err(|(c, _)| c)?;
    let cache_key = format!("projects:u{}", user.id);

    singleflight_json_bytes(&state.cache, cache_key, std::time::Duration::from_secs(4), &headers, || async {
        let user = user.clone();
        let all = state
            .projects
            .list()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let remote_node_ids: std::collections::HashSet<i64> = state
            .cluster
            .list_nodes()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|n| !n.is_local)
            .map(|n| n.id)
            .collect();
        let filtered: Vec<Project> = all
            .into_iter()
            .filter(|p| !user.disabled && user.can_access_project(p.id))
            .collect();
        let mut disk_map = std::collections::HashMap::new();
        for p in &filtered {
            if p.node_id.map(|nid| remote_node_ids.contains(&nid)).unwrap_or(false) {
                disk_map.insert(p.id, state.cluster.get_remote_disk_bytes(p.id).await);
            }
        }
        tokio::task::spawn_blocking(move || {
            let cwd = cached_cwd();
            let list: Vec<ProjectResponse> = filtered
                .into_iter()
                .map(|p| {
                    if p.node_id.map(|nid| remote_node_ids.contains(&nid)).unwrap_or(false) {
                        let d = disk_map.get(&p.id).copied().flatten();
                        to_remote_project_response(p, d)
                    } else {
                        to_project_response(p, cwd)
                    }
                })
                .collect();
            let json = serde_json::to_string(&list).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(axum::body::Bytes::from(json))
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    })
    .await
}

pub(crate) async fn get_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<ProjectResponse>, StatusCode> {
    let user = require_current_user(&state, &headers).await.map_err(|(c, _)| c)?;
    if !state.auth.can_access_project_for_user(&user, id).await {
        return Err(StatusCode::FORBIDDEN);
    }
    let p = state
        .projects
        .get(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if resolve_remote_node(&state, p.node_id).await.ok().flatten().is_some() {
        let d = state.cluster.get_remote_disk_bytes(p.id).await;
        Ok(Json(to_remote_project_response(p, d)))
    } else {
        Ok(Json(to_project_response(p, cached_cwd())))
    }
}

fn manage_power_for_kind(kind: &str) -> &'static str {
    if kind == "service" {
        "services_manage"
    } else {
        "projects_manage"
    }
}

pub(crate) async fn create_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut p): Json<Project>,
) -> Result<Json<Project>, (StatusCode, String)> {
    let actor = verify_power(&state, &headers, manage_power_for_kind(&p.project_kind), None).await?;
    let is_service = p.project_kind == "service";
    if p.name.trim().is_empty() || p.slug.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Name and slug are required".into()));
    }
    if p.repo_url.trim().is_empty() {
        p.repo_url = "local".to_string();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if p.deploy_dir.trim().is_empty()
        || p.deploy_dir.starts_with("./")
        || p.deploy_dir.starts_with(".\\")
        || (cfg!(windows) && p.deploy_dir.starts_with("/var/www/") && !std::path::Path::new(&p.deploy_dir).exists())
    {
        let rel = if p.deploy_dir.starts_with("./") || p.deploy_dir.starts_with(".\\") {
            p.deploy_dir.trim_start_matches("./").trim_start_matches(".\\").to_string()
        } else if is_service {
            format!("data/services/{}", p.slug)
        } else {
            format!("data/sites/{}", p.slug)
        };
        let abs = cwd.join(rel);
        let _ = tokio::fs::create_dir_all(&abs).await;
        p.deploy_dir = abs.to_string_lossy().to_string();
    } else {
        let _ = tokio::fs::create_dir_all(&p.deploy_dir).await;
    }

    // Always assign a guaranteed unique port across all workloads (websites, services, databases)
    let allocated_port = {
        let default_start = if is_service { 8000 } else { 3000 };
        crate::db::allocate_global_unique_port(&state.db, p.port, default_start, None, None)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
    };

    if !is_service && p.health_url.contains(":3000") && allocated_port != 3000 {
        p.health_url = p.health_url.replace(":3000", &format!(":{}", allocated_port));
    }
    p.port = Some(allocated_port);

    if p.node_id.is_none() {
        p.node_id = state.cluster.default_local_node_id().await;
    }

    let id = state
        .projects
        .create(&p)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let audit_action = if is_service { "service.create" } else { "website.create" };
    let audit_desc = if is_service {
        format!("Created backend service '{}' (Port {}, Repo: {})", p.name, allocated_port, p.repo_url)
    } else {
        format!("Created website '{}' (Port {}, Repo: {})", p.name, allocated_port, p.repo_url)
    };
    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        audit_action,
        Some(&p.name),
        Some(&audit_desc),
    )
    .await;

    let created = state
        .projects
        .get(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Project not found".into()))?;

    if let Some(node_id) = resolve_remote_node(&state, created.node_id).await? {
        state.cluster.broadcast_peer_sync().await;
        let ctx = state.cluster.build_remote_context(&state.projects, created.id).await;
        state
            .cluster
            .call_node(
                node_id,
                "project.ensure",
                serde_json::json!({ "project": &created, "context": ctx }),
                std::time::Duration::from_secs(15),
            )
            .await?;
        if !is_service || !created.domains.is_empty() {
            state.cluster.sync_remote_caddy(&state.projects, node_id).await;
        }
    } else {
        let _ = state.runtime.ensure_container(&created).await;

        if !is_service || !created.domains.is_empty() {
            if let Ok(all) = state.projects.list().await {
                let _ = state.caddy.write_config(&created, &all);
                let _ = state.caddy.reload();
            }
        }
        if let Some(port) = created.port {
            crate::databases::Databases::sync_firewall_rules(port, &created.access_mode, &created.allowed_ips).await;
        }
    }
    state.cache.invalidate_all();

    Ok(Json(created))
}

pub(crate) fn normalize_domain(d: &str) -> String {
    let cleaned = d
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .trim()
        .to_lowercase();
    if cleaned.is_empty()
        || cleaned.len() > 253
        || cleaned.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '{' | '}' | ';' | '#' | '"' | '\'' | '`' | '\\')
        })
    {
        return String::new();
    }
    cleaned
}

pub(crate) fn sanitize_domains<I: IntoIterator<Item = String>>(domains: I) -> Vec<String> {
    domains
        .into_iter()
        .map(|d| normalize_domain(&d))
        .filter(|d| !d.is_empty())
        .collect()
}

pub(crate) async fn update_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    Json(mut p): Json<Project>,
) -> Result<Json<Project>, (StatusCode, String)> {
    let prev = state.projects.get(id).await.ok().flatten();
    let existing_kind = prev
        .as_ref()
        .map(|old| old.project_kind.as_str())
        .unwrap_or(p.project_kind.as_str());
    let actor = verify_power(&state, &headers, manage_power_for_kind(existing_kind), Some(id)).await?;
    if p.project_kind != existing_kind {
        let _ = verify_power(&state, &headers, manage_power_for_kind(&p.project_kind), Some(id)).await?;
    }
    p.domains = sanitize_domains(p.domains);

    // Ensure non-conflicting, valid port allocation across all workloads
    if let Some(port) = p.port {
        let allocated_port = crate::db::allocate_global_unique_port(&state.db, Some(port), port, Some(id), None)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Port allocation error: {}", e)))?;
        p.port = Some(allocated_port);
    } else if let Some(ref old_p) = prev {
        if let Some(old_port) = old_p.port {
            p.port = Some(old_port);
        } else {
            let default_start = if p.project_kind == "service" { 8000 } else { 3000 };
            let allocated_port = crate::db::allocate_global_unique_port(&state.db, None, default_start, Some(id), None)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            p.port = Some(allocated_port);
        }
    }

    state
        .projects
        .update(id, &p)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    log_audit_project(
        &state,
        Some(id),
        &actor.username,
        if p.project_kind == "service" { "service.update" } else { "website.update" },
        Some(&p.name),
        Some(&if p.project_kind == "service" {
            format!("Updated service settings (Port {:?}, Branch '{}', Start '{}')", p.port, p.branch, p.start_cmd)
        } else {
            format!(
                "Updated website settings (Port {}, Branch '{}', Domains {:?})",
                p.port.unwrap_or(3000),
                p.branch,
                p.domains
            )
        }),
    )
    .await;

    let updated = state
        .projects
        .get(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Project not found".into()))?;

    if let Some(node_id) = resolve_remote_node(&state, updated.node_id).await? {
        if state.cluster.is_node_connected(node_id).await {
            let ctx = state.cluster.build_remote_context(&state.projects, updated.id).await;
            let _ = state
                .cluster
                .call_node(
                    node_id,
                    "project.ensure",
                    serde_json::json!({ "project": &updated, "context": ctx }),
                    std::time::Duration::from_secs(15),
                )
                .await;
            if updated.project_kind != "service" || !updated.domains.is_empty() {
                state.cluster.sync_remote_caddy(&state.projects, node_id).await;
            }
        }
    } else {
        let _ = state.runtime.ensure_container(&updated).await;

        if updated.project_kind != "service" || !updated.domains.is_empty() {
            if let Ok(all) = state.projects.list().await {
                let _ = state.caddy.write_config(&updated, &all);
                let _ = state.caddy.reload();
            }
        }

        // Clean up old firewall rules if port, access mode, or allowed IPs changed
        if let Some(ref old_p) = prev {
            if let Some(old_port) = old_p.port {
                if Some(old_port) != updated.port
                    || old_p.access_mode != updated.access_mode
                    || old_p.allowed_ips != updated.allowed_ips
                {
                    crate::databases::Databases::remove_firewall_rules(old_port, &old_p.allowed_ips).await;
                }
            }
        }

        if let Some(port) = updated.port {
            crate::databases::Databases::sync_firewall_rules(port, &updated.access_mode, &updated.allowed_ips).await;
        }
    }
    state.cache.invalidate_all();

    Ok(Json(updated))
}

pub(crate) async fn delete_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let deleted_proj = state.projects.get(id).await.ok().flatten();
    let kind = deleted_proj
        .as_ref()
        .map(|p| p.project_kind.as_str())
        .unwrap_or("website");
    let actor = verify_power(&state, &headers, manage_power_for_kind(kind), Some(id)).await?;
    if let Some(ref p) = deleted_proj {
        log_audit_project(
            &state,
            Some(id),
            &actor.username,
            if p.project_kind == "service" { "service.delete" } else { "website.delete" },
            Some(&p.name),
            Some(&format!(
                "Deleted {} '{}' ({}) along with its files, container data, and cache",
                if p.project_kind == "service" { "service" } else { "website" },
                p.name, p.slug
            )),
        )
        .await;
        if let Some(node_id) = resolve_remote_node(&state, p.node_id).await? {
            if state.cluster.is_node_connected(node_id).await {
                let remaining = state.projects.list_for_node(node_id).await.unwrap_or_default();
                let _ = state
                    .cluster
                    .call_node(
                        node_id,
                        "project.delete",
                        serde_json::json!({ "project": p, "remainingProjects": remaining }),
                        std::time::Duration::from_secs(20),
                    )
                    .await;
            }
        } else {
            let _ = state.runtime.stop_project(p).await;
            state.runtime.stop_maintenance_server(id);
        }
    }
    state
        .projects
        .delete(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = crate::auth::grants::remove_for_app(&state.db, crate::auth::grants::SCOPE_PROJECT, id).await;
    state.auth.invalidate_sessions();

    if let Some(ref p) = deleted_proj {
        if resolve_remote_node(&state, p.node_id).await?.is_some() {
            state.cluster.broadcast_peer_sync().await;
        } else {
            if let Some(port) = p.port {
                crate::databases::Databases::remove_firewall_rules(port, &p.allowed_ips).await;
            }
            if let Ok(remaining) = state.projects.list().await {
                state.runtime.cleanup_project_files(p, &remaining);
                state.caddy.remove_project_config(&p.slug, &remaining);
            }
        }
    }
    state.cache.invalidate_project(id);
    state.cache.invalidate_all();

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Deployments & Lifecycle Actions
// ---------------------------------------------------------------------------


pub(crate) async fn start_project_with_audit(
    state: &AppState,
    actor: &crate::auth::User,
    p: &crate::projects::Project,
    action: &str,
    action_label: &str,
) -> Result<u32, (StatusCode, String)> {
    let start_res = if let Some(node_id) = resolve_remote_node(state, p.node_id).await? {
        let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
        let val = state
            .cluster
            .call_node(
                node_id,
                "project.start",
                serde_json::json!({ "project": p, "context": ctx }),
                std::time::Duration::from_secs(30),
            )
            .await?;
        let pid = val["pid"].as_u64().unwrap_or(0) as u32;
        let _ = sqlx::query(
            "UPDATE projects SET app_status='running', app_pid=$1, app_started_at=$2 WHERE id=$3",
        )
        .bind(pid as i64)
        .bind(now())
        .bind(p.id)
        .execute(&state.db)
        .await;
        Ok(pid)
    } else {
        let env = state.projects.get_env_map(p.id).await;
        state.runtime.start_project(p, env, None).await.map_err(|e| {
            if e == "NOT_DEPLOYED_YET" {
                (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "'{}' has not been deployed yet. Please click 'Deploy' to build and deploy it first.",
                        p.name
                    ),
                )
            } else {
                (StatusCode::BAD_REQUEST, e)
            }
        })
    };

    let pid = start_res?;
    state.cache.invalidate_all();
    log_audit_project(
        state,
        Some(p.id),
        &actor.username,
        action,
        Some(&p.name),
        Some(&if p.project_kind == "service" {
            format!("{} backend service (PID {})", action_label, pid)
        } else {
            format!(
                "{} website container (PID {}, Port {})",
                action_label,
                pid,
                p.port.unwrap_or(3000)
            )
        }),
    )
    .await;
    Ok(pid)
}

pub(crate) async fn start_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (actor, p) = get_authorized_project(&state, &headers, "process", id).await?;
    let pid = start_project_with_audit(&state, &actor, &p, "container.start", "Started").await?;
    Ok(Json(serde_json::json!({
        "started": true,
        "pid": pid
    })))
}

pub(crate) async fn stop_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (actor, p) = get_authorized_project(&state, &headers, "process", id).await?;

    if let Some(node_id) = resolve_remote_node(&state, p.node_id).await? {
        state
            .cluster
            .call_node(
                node_id,
                "project.stop",
                serde_json::json!({ "project": &p }),
                std::time::Duration::from_secs(20),
            )
            .await?;
        let _ = sqlx::query(
            "UPDATE projects SET app_status='stopped', app_pid=NULL, app_started_at=NULL WHERE id=$1",
        )
        .bind(p.id)
        .execute(&state.db)
        .await;
    } else {
        state
            .runtime
            .stop_project(&p)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }

    state.cache.invalidate_all();
    log_audit_project(
        &state,
        Some(p.id),
        &actor.username,
        "container.stop",
        Some(&p.name),
        Some(&if p.project_kind == "service" {
            "Stopped backend service".to_string()
        } else {
            format!("Stopped website container and freed port {}", p.port.unwrap_or(3000))
        }),
    )
    .await;

    Ok(Json(serde_json::json!({ "stopped": true })))
}

pub(crate) async fn restart_project_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (actor, p) = get_authorized_project(&state, &headers, "process", id).await?;

    if let Some(node_id) = resolve_remote_node(&state, p.node_id).await? {
        let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
        let val = state
            .cluster
            .call_node(
                node_id,
                "project.restart",
                serde_json::json!({ "project": &p, "context": ctx }),
                std::time::Duration::from_secs(30),
            )
            .await?;
        let pid = val["pid"].as_u64().unwrap_or(0) as u32;
        let _ = sqlx::query(
            "UPDATE projects SET app_status='running', app_pid=$1, app_started_at=$2 WHERE id=$3",
        )
        .bind(pid as i64)
        .bind(now())
        .bind(p.id)
        .execute(&state.db)
        .await;
        state.cache.invalidate_all();
        log_audit_project(
            &state,
            Some(p.id),
            &actor.username,
            "container.restart",
            Some(&p.name),
            Some(&format!("Restarted remote workload (PID {pid})")),
        )
        .await;
        return Ok(Json(serde_json::json!({ "started": true, "pid": pid })));
    }

    let _ = state.runtime.stop_project(&p).await;
    let pid = start_project_with_audit(&state, &actor, &p, "container.restart", "Restarted").await?;
    Ok(Json(serde_json::json!({ "started": true, "pid": pid })))
}

#[derive(Deserialize, Default)]
pub(crate) struct MaintenancePayload {
    enabled: Option<bool>,
}

pub(crate) async fn toggle_maintenance_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
    payload: Option<Json<MaintenancePayload>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (actor, p) = get_authorized_project(&state, &headers, "process", id).await?;

    let target_enabled = payload
        .and_then(|Json(b)| b.enabled)
        .unwrap_or(p.app_status != "maintenance");

    if let Some(node_id) = resolve_remote_node(&state, p.node_id).await? {
        let ctx = state.cluster.build_remote_context(&state.projects, p.id).await;
        let val = state
            .cluster
            .call_node(
                node_id,
                "project.maintenance",
                serde_json::json!({ "project": &p, "enabled": target_enabled, "context": ctx }),
                std::time::Duration::from_secs(30),
            )
            .await?;
        let final_status = val["appStatus"]
            .as_str()
            .unwrap_or(if target_enabled { "maintenance" } else { "stopped" })
            .to_string();
        let _ = sqlx::query(
            "UPDATE projects SET app_status=$1, app_pid=NULL, updated_at=$2 WHERE id=$3",
        )
        .bind(&final_status)
        .bind(now())
        .bind(p.id)
        .execute(&state.db)
        .await;
        state.cluster.sync_remote_caddy(&state.projects, node_id).await;
        log_audit_project(
            &state,
            Some(p.id),
            &actor.username,
            if target_enabled {
                "website.maintenance_on"
            } else {
                "website.maintenance_off"
            },
            Some(&p.name),
            Some(&format!("Set remote maintenance={target_enabled} (status {final_status})")),
        )
        .await;
        state.cache.invalidate_all();
        return Ok(Json(serde_json::json!({
            "ok": true,
            "maintenance": target_enabled,
            "app_status": final_status,
        })));
    }

    if target_enabled {
        state
            .runtime
            .set_maintenance_mode(&p, true)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

        if let Ok(all) = state.projects.list().await {
            if let Ok(Some(updated_p)) = state.projects.get(id).await {
                let _ = state.caddy.write_config(&updated_p, &all);
                let _ = state.caddy.reload();
            }
        }

        log_audit_project(
            &state,
            Some(p.id),
            &actor.username,
            "website.maintenance_on",
            Some(&p.name),
            Some(&format!(
                "Enabled Maintenance Mode on port {} (Maintenance page active)",
                p.port.unwrap_or(3000)
            )),
        )
        .await;
        state.cache.invalidate_all();

        Ok(Json(serde_json::json!({
            "ok": true,
            "maintenance": true,
            "app_status": "maintenance"
        })))
    } else {
        let _ = state.runtime.set_maintenance_mode(&p, false).await;
        let env = state.projects.get_env_map(id).await;
        match state.runtime.start_project(&p, env, None).await {
            Ok(pid) => {
                if let Ok(all) = state.projects.list().await {
                    if let Ok(Some(updated_p)) = state.projects.get(id).await {
                        let _ = state.caddy.write_config(&updated_p, &all);
                        let _ = state.caddy.reload();
                    }
                }
                log_audit_project(
                    &state,
                    Some(p.id),
                    &actor.username,
                    "website.maintenance_off",
                    Some(&p.name),
                    Some(&format!(
                        "Disabled Maintenance Mode and resumed website (PID {}, Port {})",
                        pid,
                        p.port.unwrap_or(3000)
                    )),
                )
                .await;
                state.cache.invalidate_all();
                Ok(Json(serde_json::json!({
                    "ok": true,
                    "maintenance": false,
                    "app_status": "running",
                    "pid": pid
                })))
            }
            Err(e) if e == "NOT_DEPLOYED_YET" => {
                let _ = state.runtime.stop_project(&p).await;
                state.cache.invalidate_all();
                Ok(Json(serde_json::json!({
                    "ok": true,
                    "maintenance": false,
                    "app_status": "stopped"
                })))
            }
            Err(e) => Err((StatusCode::BAD_REQUEST, e)),
        }
    }
}

