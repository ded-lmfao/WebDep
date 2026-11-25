use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::common::*;
use crate::cluster::manager::RemoteWorkloadTelemetry;
use crate::cluster::models::ClusterNode;

#[derive(Serialize)]
pub(crate) struct OverviewProjectItem {
    pub project: Project,
    pub app: OverviewAppStatus,
    pub active: Option<crate::deployer::Deployment>,
    pub behind: Option<u32>,
    pub ahead: Option<u32>,
    #[serde(rename = "lastSuccess")]
    pub last_success: Option<crate::deployer::Deployment>,
}

#[derive(Serialize)]
pub(crate) struct OverviewAppStatus {
    pub running: bool,
    pub pid: Option<u32>,
    #[serde(rename = "startedAt")]
    pub started_at: Option<i64>,
    #[serde(rename = "cpuPct")]
    pub cpu_pct: Option<f32>,
    #[serde(rename = "memoryBytes")]
    pub memory_bytes: Option<u64>,
    #[serde(rename = "memoryFormatted")]
    pub memory_formatted: Option<String>,
    #[serde(rename = "diskBytes")]
    pub disk_bytes: Option<u64>,
    #[serde(rename = "diskFormatted")]
    pub disk_formatted: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ResourceItem {
    pub id: String,
    pub name: String,
    pub kind: String,          // "panel", "website", "service", "database"
    pub status: String,        // "running", "stopped", "error"
    pub pid: Option<u32>,
    pub port: Option<u16>,
    #[serde(rename = "cpuPct")]
    pub cpu_pct: f32,
    #[serde(rename = "memoryBytes")]
    pub memory_bytes: u64,
    #[serde(rename = "memoryFormatted")]
    pub memory_formatted: String,
    #[serde(rename = "memoryPct")]
    pub memory_pct: f32,
    #[serde(rename = "diskBytes")]
    pub disk_bytes: u64,
    #[serde(rename = "diskFormatted")]
    pub disk_formatted: String,
    #[serde(rename = "diskPct")]
    pub disk_pct: f32,
    #[serde(rename = "projectId")]
    pub project_id: Option<i64>,
    #[serde(rename = "databaseId")]
    pub database_id: Option<i64>,
    #[serde(rename = "nodeId")]
    pub node_id: Option<i64>,
    #[serde(rename = "commitSha")]
    pub commit_sha: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "projectType", skip_serializing_if = "Option::is_none")]
    pub project_type: Option<String>,
    #[serde(rename = "startCmd", skip_serializing_if = "Option::is_none")]
    pub start_cmd: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ResourceSummary {
    #[serde(rename = "totalManagedMemory")]
    pub total_managed_memory: u64,
    #[serde(rename = "totalManagedMemoryFormatted")]
    pub total_managed_memory_formatted: String,
    #[serde(rename = "totalManagedCpu")]
    pub total_managed_cpu: f32,
    #[serde(rename = "totalManagedDisk")]
    pub total_managed_disk: u64,
    #[serde(rename = "totalManagedDiskFormatted")]
    pub total_managed_disk_formatted: String,
    #[serde(rename = "panelMemory")]
    pub panel_memory: u64,
    #[serde(rename = "panelMemoryFormatted")]
    pub panel_memory_formatted: String,
    #[serde(rename = "panelCpu")]
    pub panel_cpu: f32,
    #[serde(rename = "panelDisk")]
    pub panel_disk: u64,
    #[serde(rename = "panelDiskFormatted")]
    pub panel_disk_formatted: String,
    #[serde(rename = "systemOverheadMemory")]
    pub system_overhead_memory: u64,
    #[serde(rename = "systemOverheadMemoryFormatted")]
    pub system_overhead_memory_formatted: String,
    #[serde(rename = "systemOverheadCpu")]
    pub system_overhead_cpu: f32,
    #[serde(rename = "runningWorkloadsCount")]
    pub running_workloads_count: usize,
    #[serde(rename = "totalWorkloadsCount")]
    pub total_workloads_count: usize,
}

#[derive(Deserialize, Default)]
pub(crate) struct OverviewQuery {
    #[serde(rename = "nodeId")]
    pub node_id: Option<String>,
}

fn parse_node_filter(raw: Option<&str>) -> Option<i64> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
        .and_then(|s| s.parse::<i64>().ok())
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OverviewNodeSummary {
    pub id: i64,
    pub name: String,
    pub region: String,
    pub status: String,
    pub is_local: bool,
    pub mesh_ip: String,
    pub public_ip: String,
    pub cpu_cores: usize,
    pub cpu_pct: u8,
    pub mem_used: u64,
    pub mem_total: u64,
    pub mem_free: u64,
    pub mem_pct: u8,
    pub disk_used: u64,
    pub disk_total: u64,
    pub disk_free: u64,
    pub disk_pct: u8,
    pub inbound_bps: u64,
    pub outbound_bps: u64,
    pub total_rx_bytes: u64,
    pub total_tx_bytes: u64,
    pub interface_count: usize,
    pub net_history: Vec<crate::monitor::NetworkSample>,
    pub project_count: i64,
    pub website_count: i64,
    pub service_count: i64,
    pub database_count: i64,
    pub display_name: String,
}

#[derive(Serialize)]
pub(crate) struct OverviewResponse {
    pub projects: Vec<OverviewProjectItem>,
    pub resources: Vec<ResourceItem>,
    pub summary: ResourceSummary,
    pub recent: Vec<crate::deployer::Deployment>,
    pub monitor: crate::monitor::SystemMetrics,
    pub network: crate::monitor::NetworkMetrics,
    pub nodes: Vec<OverviewNodeSummary>,
    #[serde(rename = "nodeCount")]
    pub node_count: usize,
    #[serde(rename = "nodeLabel")]
    pub node_label: String,
}

pub(crate) fn format_resource_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

pub(crate) fn resolve_workspace_path(raw: &str, cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = std::path::Path::new(trimmed);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        Some(cwd.join(trimmed.trim_start_matches("./").trim_start_matches(".\\")))
    }
}

pub(crate) fn compute_project_disk_formatted(p: &Project, cwd: &std::path::Path) -> (u64, String) {
    let deploy_dir = resolve_workspace_path(&p.deploy_dir, cwd);
    let deploy_size = deploy_dir
        .as_deref()
        .map(crate::monitor::get_path_disk_size)
        .unwrap_or(0);
    let container_dir = cwd.join("data").join("containers").join(&p.slug);
    let container_size = if deploy_dir.as_ref() != Some(&container_dir) {
        crate::monitor::get_path_disk_size(&container_dir)
    } else {
        0
    };
    let bytes = deploy_size.saturating_add(container_size);
    (bytes, format_resource_bytes(bytes))
}

/// Same total as `compute_project_disk_formatted`, but measured now instead of from the cache.
pub(crate) fn compute_project_disk_bytes_fresh(p: &Project, cwd: &std::path::Path) -> u64 {
    let deploy_dir = resolve_workspace_path(&p.deploy_dir, cwd);
    let deploy_size = deploy_dir.as_deref().map(crate::monitor::get_path_disk_size_fresh).unwrap_or(0);
    let container_dir = cwd.join("data").join("containers").join(&p.slug);
    let container_size = if deploy_dir.as_ref() != Some(&container_dir) {
        crate::monitor::get_path_disk_size_fresh(&container_dir)
    } else {
        0
    };
    deploy_size.saturating_add(container_size)
}

static CACHED_CWD: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
static CACHED_EXE: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

fn cached_cwd() -> &'static std::path::Path {
    CACHED_CWD
        .get_or_init(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
        .as_path()
}

fn cached_exe() -> Option<&'static std::path::Path> {
    CACHED_EXE.get_or_init(|| std::env::current_exe().ok()).as_deref()
}

fn format_usage_metrics(
    is_running: bool,
    raw_cpu: f32,
    mem: u64,
    num_cores: f32,
    total_vps_mem: u64,
) -> (f32, f32, String) {
    if !is_running {
        return (0.0, 0.0, "0 MB".to_string());
    }
    let cpu = ((raw_cpu / num_cores).min(100.0) * 10.0).round() / 10.0;
    let mem_pct = ((mem as f64 / total_vps_mem as f64 * 100.0) as f32 * 10.0).round() / 10.0;
    let mem_fmt = if mem > 0 {
        format_resource_bytes(mem)
    } else {
        "< 1 MB".to_string()
    };
    (cpu, mem_pct, mem_fmt)
}

pub(crate) fn compute_database_disk_formatted_dir(
    data_dir: &str,
    cwd: &std::path::Path,
) -> (u64, String) {
    let bytes = resolve_workspace_path(data_dir, cwd)
        .as_deref()
        .map(crate::monitor::get_path_disk_size)
        .unwrap_or(0);
    (bytes, format_resource_bytes(bytes))
}

pub(crate) fn compute_database_disk_formatted(
    db: &crate::databases::ManagedDatabase,
    cwd: &std::path::Path,
) -> (u64, String) {
    compute_database_disk_formatted_dir(&db.data_dir, cwd)
}

fn row_to_overview_deployment(
    r: &sqlx::postgres::PgRow,
    project_name: &str,
) -> Result<crate::deployer::Deployment, sqlx::Error> {
    use sqlx::Row;
    Ok(crate::deployer::Deployment {
        id: r.try_get(0)?,
        project_id: r.try_get(1)?,
        project_name: Some(project_name.to_string()),
        release_id: r.try_get(2)?,
        from_release_id: r.try_get(3)?,
        kind: r.try_get(4)?,
        status: r.try_get(5)?,
        stage: r.try_get(6)?,
        sha: r.try_get(7)?,
        trigger: r.try_get(8)?,
        actor: r.try_get(9)?,
        error: r.try_get(10)?,
        started_at: r.try_get(11)?,
        finished_at: r.try_get(12)?,
    })
}

async fn load_overview_deployments(
    pool: &crate::db::DbPool,
    projects: &[Project],
) -> (
    std::collections::HashMap<i64, crate::deployer::Deployment>,
    std::collections::HashMap<i64, crate::deployer::Deployment>,
) {
    use sqlx::Row;
    let mut last_success_map = std::collections::HashMap::with_capacity(projects.len());
    let mut active_map = std::collections::HashMap::new();
    if projects.is_empty() {
        return (last_success_map, active_map);
    }

    let proj_names: std::collections::HashMap<i64, &str> =
        projects.iter().map(|p| (p.id, p.name.as_str())).collect();

    if let Ok(rows) = sqlx::query(
        "SELECT id, project_id, release_id, from_release_id, kind, status, stage, sha, trigger, actor, error, started_at, finished_at \
         FROM deployments WHERE status IN ('running', 'pending')",
    )
    .fetch_all(pool)
    .await
    {
        for r in &rows {
            if let Ok(pid) = r.try_get::<i64, _>(1) {
                if let Some(&pname) = proj_names.get(&pid) {
                    if let Ok(dep) = row_to_overview_deployment(r, pname) {
                        active_map
                            .entry(dep.project_id)
                            .and_modify(|existing: &mut crate::deployer::Deployment| {
                                if dep.id > existing.id {
                                    *existing = dep.clone();
                                }
                            })
                            .or_insert(dep);
                    }
                }
            }
        }
    }

    if let Ok(rows) = sqlx::query(
        "SELECT d.id, d.project_id, d.release_id, d.from_release_id, d.kind, d.status, d.stage, d.sha, d.trigger, d.actor, d.error, d.started_at, d.finished_at \
         FROM deployments d \
         INNER JOIN (SELECT project_id, MAX(id) AS max_id FROM deployments WHERE status = 'success' GROUP BY project_id) latest ON latest.max_id = d.id",
    )
    .fetch_all(pool)
    .await
    {
        for r in &rows {
            if let Ok(pid) = r.try_get::<i64, _>(1) {
                if let Some(&pname) = proj_names.get(&pid) {
                    if let Ok(dep) = row_to_overview_deployment(r, pname) {
                        last_success_map.insert(dep.project_id, dep);
                    }
                }
            }
        }
    }

    (last_success_map, active_map)
}

pub(crate) async fn overview_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OverviewQuery>,
) -> Result<Response, StatusCode> {
    let user = current_user(&state, &headers).await.ok_or(StatusCode::UNAUTHORIZED)?;
    state.monitor.touch_activity();
    let selected_node_id = parse_node_filter(query.node_id.as_deref());
    let cache_key = format!(
        "overview:u{}:n{}",
        user.id,
        selected_node_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "all".to_string())
    );

    singleflight_json_bytes(&state.cache, cache_key, std::time::Duration::from_secs(2), &headers, || async {
        compute_overview_bytes(&state, &user, selected_node_id).await
    })
    .await
}

async fn compute_overview_bytes(
    state: &AppState,
    user: &crate::auth::User,
    selected_node_id: Option<i64>,
) -> Result<axum::body::Bytes, StatusCode> {
    let mut projs = state.projects.list().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    projs.retain(|p| !user.disabled && user.can_access_project(p.id));

    let raw_dbs = if state.auth.can_list_databases_for_user(user).await {
        let mut list = state.databases.list_overview_summaries().await.unwrap_or_default();
        list.retain(|db| {
            crate::auth::Auth::user_has_database_power(
                user,
                &["databases", "databases_manage"],
                db.id,
                db.linked_project_id,
            )
        });
        list
    } else {
        Vec::new()
    };

    let (last_success_map, active_map) = if projs.is_empty() {
        (std::collections::HashMap::new(), std::collections::HashMap::new())
    } else {
        load_overview_deployments(&state.db, &projs).await
    };
    let mut recent = state.deployer.list_deployments(None, 25).await;
    recent.retain(|d| !user.disabled && user.can_access_project(d.project_id));

    let nodes = state.cluster.list_nodes().await.unwrap_or_default();
    let remote_telemetry = state.cluster.get_remote_workload_telemetry().await;
    let remote_net_history = state.cluster.get_remote_net_history_map().await;

    let state = state.clone();
    let user = user.clone();
    tokio::task::spawn_blocking(move || {
        compute_overview_bytes_sync(
            &state,
            &user,
            selected_node_id,
            &nodes,
            &remote_telemetry,
            &remote_net_history,
            projs,
            raw_dbs,
            last_success_map,
            active_map,
            &mut recent,
        )
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
}

struct ScopedClusterScope<'a> {
    local_node_id: i64,
    focused_node_id: i64,
    include_local: bool,
    remote_nodes: Vec<&'a ClusterNode>,
    node_count: usize,
    node_label: String,
}

/// Resolves which single host populates the top quick-stat cards (`monitor` / `network`).
/// Never merges distinct VPSes together: when viewing "All nodes", top cards show the Primary
/// Control Node while `OverviewResponse.nodes` renders each VPS separately side-by-side.
fn resolve_cluster_scope<'a>(
    nodes: &'a [ClusterNode],
    selected_node_id: Option<i64>,
) -> ScopedClusterScope<'a> {
    let local_node = nodes.iter().find(|n| n.is_local);
    let local_node_id = local_node.map(|n| n.id).unwrap_or(1);
    let total_nodes = nodes.len().max(1);

    if let Some(target_id) = selected_node_id {
        if let Some(found) = nodes.iter().find(|n| n.id == target_id) {
            if found.is_local {
                return ScopedClusterScope {
                    local_node_id,
                    focused_node_id: found.id,
                    include_local: true,
                    remote_nodes: Vec::new(),
                    node_count: total_nodes,
                    node_label: found.name.clone(),
                };
            }
            return ScopedClusterScope {
                local_node_id,
                focused_node_id: found.id,
                include_local: false,
                remote_nodes: vec![found],
                node_count: total_nodes,
                node_label: found.name.clone(),
            };
        }
    }

    let node_label = local_node
        .map(|n| n.name.clone())
        .unwrap_or_else(|| "Primary".to_string());
    ScopedClusterScope {
        local_node_id,
        focused_node_id: local_node_id,
        include_local: true,
        remote_nodes: Vec::new(),
        node_count: total_nodes,
        node_label,
    }
}

fn build_overview_node_summaries(
    nodes: &[ClusterNode],
    local_monitor: Option<&crate::monitor::SystemMetrics>,
    local_net: Option<&crate::monitor::NetworkMetrics>,
    remote_net_history: &std::collections::HashMap<i64, Vec<crate::monitor::NetworkSample>>,
) -> Vec<OverviewNodeSummary> {
    let mut out: Vec<OverviewNodeSummary> = nodes
        .iter()
        .map(|n| {
            if n.is_local {
                if let Some(lm) = local_monitor {
                    let mem_total = lm.memory.total;
                    let mem_used = lm.memory.used.min(mem_total.max(lm.memory.used));
                    let mem_free = mem_total.saturating_sub(mem_used);
                    let mem_pct = if mem_total > 0 {
                        ((mem_used as f64 / mem_total as f64) * 100.0).clamp(0.0, 100.0).round() as u8
                    } else {
                        lm.memory.pct
                    };
                    let (disk_used, disk_total, disk_free, disk_pct) = lm
                        .disk
                        .as_ref()
                        .map(|d| (d.used, d.total, d.free, d.pct))
                        .unwrap_or((0, 0, 0, 0));
                    return OverviewNodeSummary {
                        id: n.id,
                        name: n.name.clone(),
                        region: n.region.clone(),
                        status: n.status.clone(),
                        is_local: true,
                        mesh_ip: n.mesh_ip.clone(),
                        public_ip: {
                            let raw = n.public_ip.trim();
                            if raw.is_empty() || raw == "127.0.0.1" {
                                std::env::var("PUBLIC_IP")
                                    .ok()
                                    .map(|v| v.trim().to_string())
                                    .filter(|v| !v.is_empty())
                                    .unwrap_or_else(|| n.public_ip.clone())
                            } else {
                                n.public_ip.clone()
                            }
                        },
                        cpu_cores: lm.cpu.cores.max(1),
                        cpu_pct: lm.cpu.pct,
                        mem_used,
                        mem_total,
                        mem_free,
                        mem_pct,
                        disk_used,
                        disk_total,
                        disk_free,
                        disk_pct,
                        inbound_bps: local_net.map(|ln| ln.inbound_bps).unwrap_or(0),
                        outbound_bps: local_net.map(|ln| ln.outbound_bps).unwrap_or(0),
                        total_rx_bytes: local_net.map(|ln| ln.total_rx_bytes).unwrap_or(0),
                        total_tx_bytes: local_net.map(|ln| ln.total_tx_bytes).unwrap_or(0),
                        interface_count: local_net.map(|ln| ln.interface_count).unwrap_or(0),
                        net_history: local_net.map(|ln| ln.history.clone()).unwrap_or_default(),
                        project_count: n.project_count,
                        website_count: n.project_count,
                        service_count: 0,
                        database_count: n.database_count,
                        display_name: n.display_name.clone(),
                    };
                }
            }

            let mem_total = n.metrics.mem_total;
            let mem_used = if mem_total > 0 {
                n.metrics.mem_used.min(mem_total)
            } else {
                n.metrics.mem_used
            };
            let mem_free = mem_total.saturating_sub(mem_used);
            let mem_pct = if mem_total > 0 {
                ((mem_used as f64 / mem_total as f64) * 100.0).clamp(0.0, 100.0).round() as u8
            } else {
                0
            };

            let disk_total = n.metrics.disk_total;
            let disk_used = if disk_total > 0 {
                n.metrics.disk_used.min(disk_total)
            } else {
                n.metrics.disk_used
            };
            let disk_free = disk_total.saturating_sub(disk_used);
            let disk_pct = if disk_total > 0 {
                ((disk_used as f64 / disk_total as f64) * 100.0).clamp(0.0, 100.0).round() as u8
            } else {
                0
            };

            let mut hist = remote_net_history.get(&n.id).cloned().unwrap_or_default();
            if hist.is_empty() && (n.metrics.rx_bytes_sec > 0 || n.metrics.tx_bytes_sec > 0) {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                hist.push(crate::monitor::NetworkSample {
                    ts: now_ms,
                    inbound_bps: n.metrics.rx_bytes_sec,
                    outbound_bps: n.metrics.tx_bytes_sec,
                });
            }

            OverviewNodeSummary {
                id: n.id,
                name: n.name.clone(),
                region: n.region.clone(),
                status: n.status.clone(),
                is_local: n.is_local,
                mesh_ip: n.mesh_ip.clone(),
                public_ip: n.public_ip.clone(),
                cpu_cores: n.capabilities.cpu_cores.max(1),
                cpu_pct: n.metrics.cpu_pct.clamp(0.0, 100.0).round() as u8,
                mem_used,
                mem_total,
                mem_free,
                mem_pct,
                disk_used,
                disk_total,
                disk_free,
                disk_pct,
                inbound_bps: n.metrics.rx_bytes_sec,
                outbound_bps: n.metrics.tx_bytes_sec,
                total_rx_bytes: n.metrics.total_rx_bytes,
                total_tx_bytes: n.metrics.total_tx_bytes,
                interface_count: n.metrics.interface_count.max(1) as usize,
                net_history: hist,
                project_count: n.project_count,
                website_count: n.project_count,
                service_count: 0,
                database_count: n.database_count,
                display_name: n.display_name.clone(),
            }
        })
        .collect();
    out.sort_by_key(|n| (!n.is_local, n.id));
    out
}

fn aggregate_scoped_host_metrics(
    local_monitor: Option<crate::monitor::SystemMetrics>,
    local_net: Option<crate::monitor::NetworkMetrics>,
    remote_nodes: &[&ClusterNode],
    remote_net_history: &std::collections::HashMap<i64, Vec<crate::monitor::NetworkSample>>,
) -> (crate::monitor::SystemMetrics, crate::monitor::NetworkMetrics) {
    let mut total_cores: usize = 0;
    let mut weighted_cpu_sum: f64 = 0.0;
    let mut mem_used: u64 = 0;
    let mut mem_total: u64 = 0;
    let mut disk_used: u64 = 0;
    let mut disk_total: u64 = 0;
    let mut uptime_s: u64 = 0;
    let mut processes = Vec::new();

    if let Some(ref lm) = local_monitor {
        let cores = lm.cpu.cores.max(1);
        total_cores = total_cores.saturating_add(cores);
        weighted_cpu_sum += f64::from(lm.cpu.pct) * (cores as f64);
        mem_used = mem_used.saturating_add(lm.memory.used);
        mem_total = mem_total.saturating_add(lm.memory.total);
        if let Some(ref d) = lm.disk {
            disk_used = disk_used.saturating_add(d.used);
            disk_total = disk_total.saturating_add(d.total);
        }
        uptime_s = uptime_s.max(lm.uptime_s);
        processes = lm.processes.clone();
    }

    for rn in remote_nodes {
        let cores = rn.capabilities.cpu_cores.max(1);
        total_cores = total_cores.saturating_add(cores);
        weighted_cpu_sum += f64::from(rn.metrics.cpu_pct.clamp(0.0, 100.0)) * (cores as f64);

        let r_mem_total = rn.metrics.mem_total;
        mem_total = mem_total.saturating_add(r_mem_total);
        mem_used = mem_used.saturating_add(if r_mem_total > 0 {
            rn.metrics.mem_used.min(r_mem_total)
        } else {
            rn.metrics.mem_used
        });

        let r_disk_total = rn.metrics.disk_total;
        disk_total = disk_total.saturating_add(r_disk_total);
        disk_used = disk_used.saturating_add(if r_disk_total > 0 {
            rn.metrics.disk_used.min(r_disk_total)
        } else {
            rn.metrics.disk_used
        });
        uptime_s = uptime_s.max(rn.metrics.uptime_s);
    }

    let safe_cores = total_cores.max(1);
    let safe_mem_total = mem_total.max(1);
    let cpu_pct = (weighted_cpu_sum / (safe_cores as f64)).clamp(0.0, 100.0).round() as u8;
    let mem_pct = ((mem_used as f64 / safe_mem_total as f64) * 100.0).clamp(0.0, 100.0).round() as u8;

    let disk = if disk_total > 0 {
        let free = disk_total.saturating_sub(disk_used);
        let pct = ((disk_used as f64 / disk_total as f64) * 100.0).clamp(0.0, 100.0).round() as u8;
        Some(crate::monitor::DiskMetrics {
            total: disk_total,
            used: disk_used,
            free,
            pct,
        })
    } else {
        None
    };

    let sys_metrics = crate::monitor::SystemMetrics {
        cpu: crate::monitor::CpuMetrics {
            pct: cpu_pct,
            cores: safe_cores,
        },
        memory: crate::monitor::MemoryMetrics {
            total: mem_total,
            used: mem_used,
            free: mem_total.saturating_sub(mem_used),
            pct: mem_pct,
        },
        disk,
        uptime_s,
        processes,
    };

    let mut inbound_bps: u64 = 0;
    let mut outbound_bps: u64 = 0;
    let mut total_rx_bytes: u64 = 0;
    let mut total_tx_bytes: u64 = 0;
    let mut interface_count: usize = 0;
    let mut history: Vec<crate::monitor::NetworkSample> = Vec::new();

    if let Some(ln) = local_net {
        inbound_bps = inbound_bps.saturating_add(ln.inbound_bps);
        outbound_bps = outbound_bps.saturating_add(ln.outbound_bps);
        total_rx_bytes = total_rx_bytes.saturating_add(ln.total_rx_bytes);
        total_tx_bytes = total_tx_bytes.saturating_add(ln.total_tx_bytes);
        interface_count = interface_count.saturating_add(ln.interface_count);
        history = ln.history;
    }

    for rn in remote_nodes {
        inbound_bps = inbound_bps.saturating_add(rn.metrics.rx_bytes_sec);
        outbound_bps = outbound_bps.saturating_add(rn.metrics.tx_bytes_sec);
        total_rx_bytes = total_rx_bytes.saturating_add(rn.metrics.total_rx_bytes);
        total_tx_bytes = total_tx_bytes.saturating_add(rn.metrics.total_tx_bytes);
        interface_count = interface_count.saturating_add(rn.metrics.interface_count.max(1) as usize);

        if let Some(r_hist) = remote_net_history.get(&rn.id) {
            if history.is_empty() {
                history = r_hist.clone();
            } else {
                for pt in &mut history {
                    if let Some(match_pt) = r_hist
                        .iter()
                        .rev()
                        .find(|s| s.ts <= pt.ts + 5000)
                        .or_else(|| r_hist.first())
                    {
                        pt.inbound_bps = pt.inbound_bps.saturating_add(match_pt.inbound_bps);
                        pt.outbound_bps = pt.outbound_bps.saturating_add(match_pt.outbound_bps);
                    }
                }
            }
        }
    }

    if history.is_empty() && !remote_nodes.is_empty() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        history.push(crate::monitor::NetworkSample {
            ts: now_ms,
            inbound_bps,
            outbound_bps,
        });
    }

    let net_metrics = crate::monitor::NetworkMetrics {
        inbound_bps,
        outbound_bps,
        total_rx_bytes,
        total_tx_bytes,
        interface_count,
        history,
    };

    (sys_metrics, net_metrics)
}

#[allow(clippy::too_many_arguments)]
fn compute_overview_bytes_sync(
    state: &AppState,
    _user: &crate::auth::User,
    selected_node_id: Option<i64>,
    nodes: &[ClusterNode],
    remote_telemetry: &std::collections::HashMap<i64, RemoteWorkloadTelemetry>,
    remote_net_history: &std::collections::HashMap<i64, Vec<crate::monitor::NetworkSample>>,
    projs: Vec<Project>,
    raw_dbs: Vec<crate::databases::DatabaseOverviewSummary>,
    mut last_success_map: std::collections::HashMap<i64, crate::deployer::Deployment>,
    mut active_map: std::collections::HashMap<i64, crate::deployer::Deployment>,
    recent: &mut Vec<crate::deployer::Deployment>,
) -> Result<axum::body::Bytes, StatusCode> {
    let scope = resolve_cluster_scope(nodes, selected_node_id);
    let local_mon_sample = state.monitor.sample();
    let local_net_sample = crate::monitor::get_network_metrics();
    let mut overview_nodes = build_overview_node_summaries(
        nodes,
        Some(&local_mon_sample),
        Some(&local_net_sample),
        remote_net_history,
    );
    let mut node_websites: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut node_services: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let mut node_dbs: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for p in &projs {
        let nid = p.node_id.unwrap_or(scope.local_node_id);
        if p.project_kind == "service" {
            *node_services.entry(nid).or_insert(0) += 1;
        } else {
            *node_websites.entry(nid).or_insert(0) += 1;
        }
    }
    for db in &raw_dbs {
        let nid = db.node_id.unwrap_or(scope.local_node_id);
        *node_dbs.entry(nid).or_insert(0) += 1;
    }
    for node_summary in &mut overview_nodes {
        let websites = node_websites.get(&node_summary.id).copied().unwrap_or(0);
        let services = node_services.get(&node_summary.id).copied().unwrap_or(0);
        let dbs = node_dbs.get(&node_summary.id).copied().unwrap_or(node_summary.database_count);
        node_summary.website_count = websites;
        node_summary.service_count = services;
        node_summary.project_count = websites + services;
        node_summary.database_count = dbs;
    }

    let local_monitor = scope.include_local.then_some(local_mon_sample);
    let local_net = scope.include_local.then_some(local_net_sample);
    let (mut monitor, network) = aggregate_scoped_host_metrics(
        local_monitor,
        local_net,
        &scope.remote_nodes,
        remote_net_history,
    );

    let total_vps_mem = monitor.memory.total.max(1);
    let total_vps_disk = monitor.disk.as_ref().map(|d| d.total).unwrap_or(1).max(1);
    let num_cores = monitor.cpu.cores.max(1) as f32;
    let local_cores = num_cores;
    let total_vps_cpu = monitor.cpu.pct as f32;
    let cwd = cached_cwd();

    let panel_pid = std::process::id();
    let mut pids_to_query = Vec::new();

    let mut proj_effective_pids = std::collections::HashMap::with_capacity(projs.len());
    for p in &projs {
        let is_local_proj = p.node_id.is_none() || p.node_id == Some(scope.local_node_id);
        if !is_local_proj {
            continue;
        }
        if let Some(pid) = p.app_pid {
            if crate::runtime::process::is_pid_alive(pid) {
                proj_effective_pids.insert(p.id, pid);
                pids_to_query.push(pid);
                continue;
            }
        }
        if p.app_status == "running" {
            if let Some(port) = p.port {
                if let Some(&port_pid) = crate::runtime::Runtime::find_pids_on_port_cached(port).first() {
                    if crate::runtime::process::is_pid_alive(port_pid) {
                        proj_effective_pids.insert(p.id, port_pid);
                        pids_to_query.push(port_pid);
                    }
                }
            }
        }
    }

    let db_entries = if !raw_dbs.is_empty() {
        let mut entries = Vec::with_capacity(raw_dbs.len());
        for db in raw_dbs {
            let is_local_db = db.node_id.is_none() || db.node_id == Some(scope.local_node_id);
            let mut d_pid = None;
            if is_local_db {
                if let Some(ref cid) = db.container_id {
                    if let Some(p_str) = cid.strip_prefix("pid-") {
                        d_pid = p_str.parse::<u32>().ok();
                    }
                }
                if d_pid.is_none() {
                    let data_dir = std::path::PathBuf::from(&db.data_dir).join("data");
                    d_pid = crate::databases::read_pg_pid(&data_dir);
                }
                if db.status == "running" {
                    if let Some(pid) = d_pid {
                        pids_to_query.push(pid);
                    }
                }
            }
            entries.push((db, d_pid, is_local_db));
        }
        entries
    } else {
        Vec::new()
    };

    let ((panel_raw_cpu, panel_mem), proc_metrics) = if scope.include_local {
        state.monitor.get_panel_and_tree_metrics(panel_pid, &pids_to_query)
    } else {
        ((0.0, 0), std::collections::HashMap::new())
    };
    let mut resources = Vec::with_capacity(1 + projs.len() + db_entries.len());

    // 1. WebDep Panel daemon self metrics (only when Local Core node is included in scope)
    let (panel_cpu, panel_mem_val, panel_disk) = if scope.include_local {
        let panel_cpu = ((panel_raw_cpu / local_cores).min(100.0) * 10.0).round() / 10.0;
        let panel_mem_pct = ((panel_mem as f64 / total_vps_mem as f64 * 100.0) as f32 * 10.0).round() / 10.0;

        let panel_exe_size = cached_exe().map(crate::monitor::get_path_disk_size).unwrap_or(0);
        let panel_db_size = crate::monitor::get_path_disk_size(&cwd.join("data").join("control-pg"));
        let panel_disk = panel_exe_size.saturating_add(panel_db_size);
        let panel_disk_pct = ((panel_disk as f64 / total_vps_disk as f64 * 100.0) as f32 * 10.0).round() / 10.0;

        resources.push(ResourceItem {
            id: "panel".to_string(),
            name: "WebDep Control Panel".to_string(),
            kind: "panel".to_string(),
            status: "running".to_string(),
            pid: Some(panel_pid),
            port: Some(4600),
            cpu_pct: panel_cpu,
            memory_bytes: panel_mem,
            memory_formatted: format_resource_bytes(panel_mem),
            memory_pct: panel_mem_pct,
            disk_bytes: panel_disk,
            disk_formatted: format_resource_bytes(panel_disk),
            disk_pct: panel_disk_pct,
            project_id: None,
            database_id: None,
            node_id: Some(scope.local_node_id),
            commit_sha: None,
            domain: None,
            project_type: Some("panel".to_string()),
            start_cmd: None,
        });
        (panel_cpu, panel_mem, panel_disk)
    } else {
        (0.0, 0, 0)
    };

    let mut items = Vec::with_capacity(projs.len());

    for p in projs {
        let eff_node_id = p.node_id.unwrap_or(scope.local_node_id);
        let is_local_proj = eff_node_id == scope.local_node_id;
        let in_selected_scope = selected_node_id.is_none() || selected_node_id == Some(eff_node_id);

        let (is_running, active_pid, cpu, mem, mem_pct, mem_fmt, disk_bytes, disk_fmt) = if is_local_proj {
            let verified_pid = proj_effective_pids.get(&p.id).copied();
            let active_pid = verified_pid.or(p.app_pid);
            let is_running = verified_pid.is_some();
            let (raw_cpu, mem) = if let Some(pid) = verified_pid {
                proc_metrics.get(&pid).copied().unwrap_or((0.0, 0))
            } else {
                (0.0, 0)
            };
            let (cpu, mem_pct, mem_fmt) =
                format_usage_metrics(is_running, raw_cpu, mem, local_cores, total_vps_mem);
            let (disk_bytes, disk_fmt) = compute_project_disk_formatted(&p, cwd);
            (is_running, active_pid, cpu, mem, mem_pct, mem_fmt, disk_bytes, disk_fmt)
        } else {
            let telem = remote_telemetry.get(&p.id);
            let is_running = p.app_status == "running";
            let active_pid = telem.and_then(|t| t.pid).or(p.app_pid);
            let (cpu, mem) = if is_running {
                telem.map(|t| (t.cpu_pct, t.memory_bytes)).unwrap_or((0.0, 0))
            } else {
                (0.0, 0)
            };
            let cpu = if is_running { (cpu * 10.0).round() / 10.0 } else { 0.0 };
            let host_mem_total = nodes
                .iter()
                .find(|n| n.id == eff_node_id)
                .map(|n| n.metrics.mem_total)
                .filter(|&v| v > 0)
                .unwrap_or(total_vps_mem);
            let mem_pct = if is_running {
                ((mem as f64 / host_mem_total as f64 * 100.0) as f32 * 10.0).round() / 10.0
            } else {
                0.0
            };
            let mem_fmt = format_resource_bytes(if is_running { mem } else { 0 });
            let disk_bytes = telem.map(|t| t.disk_bytes).unwrap_or(0);
            let disk_fmt = format_resource_bytes(disk_bytes);
            (is_running, active_pid, cpu, mem, mem_pct, mem_fmt, disk_bytes, disk_fmt)
        };

        let host_disk_total = if is_local_proj {
            total_vps_disk
        } else {
            nodes
                .iter()
                .find(|n| n.id == eff_node_id)
                .map(|n| n.metrics.disk_total)
                .filter(|&v| v > 0)
                .unwrap_or(total_vps_disk)
        };
        let disk_pct = ((disk_bytes as f64 / host_disk_total as f64 * 100.0) as f32 * 10.0).round() / 10.0;

        let last_success = last_success_map.remove(&p.id);
        let active = active_map.remove(&p.id);

        let kind = if p.project_kind == "service" { "service" } else { "website" };
        let res_status = if is_running { "running".to_string() } else { p.app_status.clone() };
        let p_node_id = Some(eff_node_id);
        if in_selected_scope {
            resources.push(ResourceItem {
                id: format!("{}-{}", kind, p.id),
                name: p.name.clone(),
                kind: kind.to_string(),
                status: res_status,
                pid: active_pid,
                port: p.port,
                cpu_pct: cpu,
                memory_bytes: mem,
                memory_formatted: mem_fmt.clone(),
                memory_pct: mem_pct,
                disk_bytes,
                disk_formatted: disk_fmt.clone(),
                disk_pct,
                project_id: Some(p.id),
                database_id: None,
                node_id: p_node_id,
                commit_sha: p.deployed_sha.clone(),
                domain: p.domains.first().cloned(),
                project_type: Some(p.project_type.clone()),
                start_cmd: Some(p.start_cmd.clone()),
            });
        }

        items.push(OverviewProjectItem {
            app: OverviewAppStatus {
                running: is_running,
                pid: active_pid,
                started_at: p.app_started_at,
                cpu_pct: if is_running { Some(cpu) } else { None },
                memory_bytes: if is_running { Some(mem) } else { None },
                memory_formatted: if is_running { Some(mem_fmt) } else { None },
                disk_bytes: Some(disk_bytes),
                disk_formatted: Some(disk_fmt),
            },
            project: p,
            active,
            behind: Some(0),
            ahead: Some(0),
            last_success,
        });
    }

    for (db, d_pid, is_local_db) in db_entries {
        let eff_node_id = db.node_id.unwrap_or(scope.local_node_id);
        if selected_node_id.is_some() && selected_node_id != Some(eff_node_id) {
            continue;
        }
        let is_running = db.status == "running";
        let (raw_cpu, mem) = if is_running && is_local_db {
            d_pid.and_then(|pid| proc_metrics.get(&pid).copied()).unwrap_or((0.0, 0))
        } else {
            (0.0, 0)
        };
        let (cpu, mem_pct, mem_fmt) =
            format_usage_metrics(is_running, raw_cpu, mem, local_cores, total_vps_mem);

        let (db_disk, db_disk_fmt) = if is_local_db {
            compute_database_disk_formatted_dir(&db.data_dir, cwd)
        } else {
            (0, format_resource_bytes(0))
        };
        let db_disk_pct = ((db_disk as f64 / total_vps_disk as f64 * 100.0) as f32 * 10.0).round() / 10.0;

        resources.push(ResourceItem {
            id: format!("database-{}", db.id),
            name: db.name,
            kind: "database".to_string(),
            status: db.status,
            pid: d_pid,
            port: Some(db.port),
            cpu_pct: cpu,
            memory_bytes: mem,
            memory_formatted: mem_fmt,
            memory_pct: mem_pct,
            disk_bytes: db_disk,
            disk_formatted: db_disk_fmt,
            disk_pct: db_disk_pct,
            project_id: None,
            database_id: Some(db.id),
            node_id: Some(eff_node_id),
            commit_sha: None,
            domain: if db.domain.is_empty() { None } else { Some(db.domain) },
            project_type: Some("postgres".to_string()),
            start_cmd: None,
        });
    }

    // Sort: running workloads first, descending by memory usage
    resources.sort_by(|a, b| {
        let a_run = a.status == "running";
        let b_run = b.status == "running";
        match (a_run, b_run) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => b.memory_bytes.cmp(&a.memory_bytes),
        }
    });

    // Sum CPU/RAM only for workloads running on the focused VPS (`scope.focused_node_id`)
    // so remote workloads shown in the "All nodes" table never subtract from Primary's RAM/CPU.
    let on_focused = |r: &&ResourceItem| -> bool {
        r.id != "panel" && r.node_id == Some(scope.focused_node_id)
    };
    let focused_managed_cpu: f32 = {
        let s: f32 = resources.iter().filter(on_focused).map(|r| r.cpu_pct).sum();
        (s * 10.0).round() / 10.0
    };
    let sum_all_cpu: f32 = ((panel_cpu + focused_managed_cpu) * 10.0).round() / 10.0;

    let effective_vps_cpu = total_vps_cpu.max(sum_all_cpu).min(100.0);
    monitor.cpu.pct = effective_vps_cpu.round() as u8;

    let final_panel_cpu = resources.iter().find(|r| r.id == "panel").map(|r| r.cpu_pct).unwrap_or(panel_cpu);

    let total_managed_mem: u64 = resources.iter().filter(on_focused).map(|r| r.memory_bytes).sum();
    let total_managed_disk: u64 = resources.iter().filter(on_focused).map(|r| r.disk_bytes).sum();
    let total_managed_cpu: f32 = focused_managed_cpu;

    let total_all_mem = panel_mem_val.saturating_add(total_managed_mem);

    let min_os_overhead = (total_vps_mem as f64 * 0.015).max(64.0 * 1024.0 * 1024.0) as u64;
    let system_overhead = if monitor.memory.used > total_all_mem.saturating_add(min_os_overhead) {
        monitor.memory.used.saturating_sub(total_all_mem)
    } else {
        min_os_overhead
    };
    let effective_used_mem = total_all_mem.saturating_add(system_overhead).min(total_vps_mem);
    monitor.memory.used = effective_used_mem;
    monitor.memory.pct = ((effective_used_mem as f64 / total_vps_mem as f64 * 100.0).round() as u8).min(100);

    let system_overhead_cpu = (((effective_vps_cpu - (final_panel_cpu + total_managed_cpu)).max(0.0)) * 10.0).round() / 10.0;
    let running_count = resources.iter().filter(|r| r.id != "panel" && r.status == "running").count();
    let total_count = resources.iter().filter(|r| r.id != "panel").count();

    let summary = ResourceSummary {
        total_managed_memory: total_managed_mem,
        total_managed_memory_formatted: format_resource_bytes(total_managed_mem),
        total_managed_cpu,
        total_managed_disk,
        total_managed_disk_formatted: format_resource_bytes(total_managed_disk),
        panel_memory: panel_mem_val,
        panel_memory_formatted: format_resource_bytes(panel_mem_val),
        panel_cpu: final_panel_cpu,
        panel_disk,
        panel_disk_formatted: format_resource_bytes(panel_disk),
        system_overhead_memory: system_overhead,
        system_overhead_memory_formatted: format_resource_bytes(system_overhead),
        system_overhead_cpu,
        running_workloads_count: running_count,
        total_workloads_count: total_count,
    };

    recent.retain(|d| !_user.disabled && _user.can_access_project(d.project_id));
    recent.truncate(10);

    let payload = OverviewResponse {
        projects: items,
        resources,
        summary,
        recent: recent.clone(),
        monitor,
        network,
        nodes: overview_nodes,
        node_count: scope.node_count,
        node_label: scope.node_label,
    };

    let json_str = serde_json::to_string(&payload).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::body::Bytes::from(json_str))
}

pub(crate) async fn overview_network_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OverviewQuery>,
) -> Result<Response, StatusCode> {
    let _user = current_user(&state, &headers).await.ok_or(StatusCode::UNAUTHORIZED)?;
    state.monitor.touch_activity();
    let selected_node_id = parse_node_filter(query.node_id.as_deref());
    let nodes = state.cluster.list_nodes().await.unwrap_or_default();
    let remote_net_history = state.cluster.get_remote_net_history_map().await;
    let scope = resolve_cluster_scope(&nodes, selected_node_id);
    let local_net_sample = crate::monitor::get_network_metrics();
    let local_net = scope.include_local.then(|| local_net_sample.clone());
    let (_, net) = aggregate_scoped_host_metrics(
        None,
        local_net,
        &scope.remote_nodes,
        &remote_net_history,
    );

    let per_node: std::collections::HashMap<String, crate::monitor::NetworkMetrics> = nodes
        .iter()
        .map(|n| {
            let nm = if n.is_local {
                local_net_sample.clone()
            } else {
                crate::monitor::NetworkMetrics {
                    inbound_bps: n.metrics.rx_bytes_sec,
                    outbound_bps: n.metrics.tx_bytes_sec,
                    total_rx_bytes: n.metrics.total_rx_bytes,
                    total_tx_bytes: n.metrics.total_tx_bytes,
                    interface_count: n.metrics.interface_count.max(1) as usize,
                    history: remote_net_history.get(&n.id).cloned().unwrap_or_default(),
                }
            };
            (n.id.to_string(), nm)
        })
        .collect();

    let mut val = serde_json::to_value(&net).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            "perNode".to_string(),
            serde_json::to_value(per_node).unwrap_or_else(|_| serde_json::json!({})),
        );
    }

    let mut res = Json(val).into_response();
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    Ok(res)
}

pub(crate) async fn monitor_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let user = verify_power(&state, &headers, "monitor", None).await?;
    let cache_key = format!("monitor:u{}", user.id);

    if let Some(res) = check_api_cache(&state.cache, &cache_key) {
        return Ok(res);
    }

    let sample = state.monitor.sample();
    let mut projs = state.projects.list().await.unwrap_or_default();
    projs.retain(|p| !user.disabled && user.can_access_project(p.id));
    let now_ms = now();

    let proj_metrics: Vec<serde_json::Value> = projs
        .into_iter()
        .map(|p| {
            let running = p.app_status == "running";
            let uptime_ms = if running {
                p.app_started_at.map(|s| (now_ms - s).max(0))
            } else {
                None
            };
            serde_json::json!({
                "id": p.id,
                "name": p.name,
                "slug": p.slug,
                "deployedSha": p.deployed_sha,
                "status": {
                    "running": running,
                    "appStatus": p.app_status,
                    "pid": p.app_pid,
                    "uptimeMs": uptime_ms,
                },
                "disk": {
                    "used": 15 * 1024 * 1024u64
                },
                "deployStatus": {
                    "status": if p.deployed_sha.is_some() { "success" } else { "idle" }
                }
            })
        })
        .collect();

    let hostname = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "webdep-server".to_string());

    let val = serde_json::json!({
        "cpu": sample.cpu,
        "memory": sample.memory,
        "disk": sample.disk,
        "uptime_s": sample.uptime_s,
        "processes": sample.processes,
        "cache": state.cache.stats(),
        "system": {
            "hostname": hostname,
            "platform": format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
            "uptime": sample.uptime_s,
            "cpu": {
                "cores": sample.cpu.cores,
                "pct": sample.cpu.pct,
                "load": [sample.cpu.pct as f64 / 100.0]
            }
        },
        "projects": proj_metrics
    });

    cached_json_response(&state.cache, cache_key, &val, std::time::Duration::from_secs(2))
        .map_err(|c| (c, "Serialization error".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_load_overview_deployments_batch_picks_latest_per_status() {
        let dir = std::env::temp_dir().join(format!("webdep-ov-test-{}", crate::db::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::pg_control::test_db::isolated_pool().await;
        let vault = std::sync::Arc::new(crate::vault::Vault::new(&dir).unwrap());
        let projects_mgr = crate::projects::Projects::new(pool.clone(), vault);

        let mut p1: Project = serde_json::from_str(
            r#"{"name":"Site One","slug":"site-one","repo_url":"https://github.com/o/1"}"#,
        )
        .unwrap();
        p1.deploy_dir = dir.to_string_lossy().to_string();
        let id1 = projects_mgr.create(&p1).await.unwrap();
        let p1 = projects_mgr.get(id1).await.unwrap().unwrap();

        let mut p2: Project = serde_json::from_str(
            r#"{"name":"Site Two","slug":"site-two","repo_url":"https://github.com/o/2"}"#,
        )
        .unwrap();
        p2.deploy_dir = dir.to_string_lossy().to_string();
        let id2 = projects_mgr.create(&p2).await.unwrap();
        let p2 = projects_mgr.get(id2).await.unwrap().unwrap();

        sqlx::query(
            "INSERT INTO deployments (project_id, kind, status, stage, sha, trigger, actor, started_at)
             VALUES ($1, 'deploy', 'success', 'done', 'sha1', 'manual', 'admin', 100)",
        )
        .bind(id1)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO deployments (project_id, kind, status, stage, sha, trigger, actor, started_at)
             VALUES ($1, 'deploy', 'success', 'done', 'sha2', 'manual', 'admin', 200)",
        )
        .bind(id1)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO deployments (project_id, kind, status, stage, sha, trigger, actor, started_at)
             VALUES ($1, 'deploy', 'running', 'build', 'sha3', 'manual', 'admin', 300)",
        )
        .bind(id1)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO deployments (project_id, kind, status, stage, sha, trigger, actor, started_at)
             VALUES ($1, 'deploy', 'failed', 'error', 'sha4', 'manual', 'admin', 400)",
        )
        .bind(id2)
        .execute(&pool)
        .await
        .unwrap();

        let (succ, act) = load_overview_deployments(&pool, &[p1, p2]).await;
        assert_eq!(succ.get(&id1).and_then(|d| d.sha.as_deref()), Some("sha2"));
        assert_eq!(succ.get(&id1).and_then(|d| d.project_name.as_deref()), Some("Site One"));
        assert_eq!(act.get(&id1).and_then(|d| d.sha.as_deref()), Some("sha3"));
        assert!(!succ.contains_key(&id2));
        assert!(!act.contains_key(&id2));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn aggregate_scoped_host_metrics_combines_all_online_nodes_and_scopes_single_node() {
        use crate::cluster::models::{NodeCapabilities, NodeMetrics};

        let make_node = |id: i64, name: &str, is_local: bool, cores: usize, cpu_pct: f32, mem_used_mb: u64, mem_total_mb: u64, disk_used_gb: u64, disk_total_gb: u64| {
            ClusterNode {
                id,
                name: name.to_string(),
                region: "default".to_string(),
                tags: Vec::new(),
                status: "online".to_string(),
                is_local,
                public_ip: String::new(),
                mesh_ip: format!("10.200.0.{id}"),
                wg_pubkey: String::new(),
                wg_endpoint: String::new(),
                node_version: "test".to_string(),
                capabilities: NodeCapabilities {
                    cpu_cores: cores,
                    ..Default::default()
                },
                metrics: NodeMetrics {
                    cpu_pct,
                    mem_used: mem_used_mb * 1024 * 1024,
                    mem_total: mem_total_mb * 1024 * 1024,
                    disk_used: disk_used_gb * 1024 * 1024 * 1024,
                    disk_total: disk_total_gb * 1024 * 1024 * 1024,
                    uptime_s: 120,
                    rx_bytes_sec: 1000 * (id as u64),
                    tx_bytes_sec: 500 * (id as u64),
                    total_rx_bytes: 10_000,
                    total_tx_bytes: 5_000,
                    interface_count: 2,
                },
                last_seen_at: Some(100),
                created_at: 100,
                updated_at: 100,
                project_count: 0,
                database_count: 0,
                display_name: String::new(),
            }
        };

        let n1 = make_node(1, "Primary", true, 4, 10.0, 1000, 4000, 10, 100);
        let n2 = make_node(2, "VPS-2", false, 4, 30.0, 2000, 8000, 20, 200);
        let n3 = make_node(3, "My oracle VPS", false, 4, 20.0, 3000, 12000, 30, 300);
        let nodes = vec![n1, n2, n3];

        let all_scope = resolve_cluster_scope(&nodes, None);
        assert_eq!(all_scope.node_count, 3);
        assert_eq!(all_scope.node_label, "Primary");
        assert!(all_scope.include_local);
        assert!(all_scope.remote_nodes.is_empty());

        let local_mon = crate::monitor::SystemMetrics {
            cpu: crate::monitor::CpuMetrics { pct: 10, cores: 4 },
            memory: crate::monitor::MemoryMetrics {
                total: 4000 * 1024 * 1024,
                used: 1000 * 1024 * 1024,
                free: 3000 * 1024 * 1024,
                pct: 25,
            },
            disk: Some(crate::monitor::DiskMetrics {
                total: 100 * 1024 * 1024 * 1024,
                used: 10 * 1024 * 1024 * 1024,
                free: 90 * 1024 * 1024 * 1024,
                pct: 10,
            }),
            uptime_s: 120,
            processes: Vec::new(),
        };
        let node_cards = build_overview_node_summaries(
            &nodes,
            Some(&local_mon),
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(node_cards.len(), 3);
        assert_eq!(node_cards[0].name, "Primary");
        assert_eq!(node_cards[0].mem_free, 3000 * 1024 * 1024);
        assert_eq!(node_cards[1].name, "VPS-2");
        assert_eq!(node_cards[1].mem_free, 6000 * 1024 * 1024);
        assert_eq!(node_cards[2].name, "My oracle VPS");
        assert_eq!(node_cards[2].mem_free, 9000 * 1024 * 1024);

        let (all_sys, _) = aggregate_scoped_host_metrics(
            Some(local_mon),
            None,
            &all_scope.remote_nodes,
            &std::collections::HashMap::new(),
        );
        // Top cards must show Primary alone (4 cores / 4000 MB), NOT combined across VPSes
        assert_eq!(all_sys.cpu.cores, 4);
        assert_eq!(all_sys.memory.total, 4000 * 1024 * 1024);

        let n2_scope = resolve_cluster_scope(&nodes, Some(2));
        assert_eq!(n2_scope.focused_node_id, 2);
        assert_eq!(n2_scope.node_label, "VPS-2");
        assert!(!n2_scope.include_local);
        let (n2_sys, n2_net) = aggregate_scoped_host_metrics(
            None,
            None,
            &n2_scope.remote_nodes,
            &std::collections::HashMap::new(),
        );
        assert_eq!(n2_sys.cpu.cores, 4);
        assert_eq!(n2_sys.cpu.pct, 30);
        assert_eq!(n2_sys.memory.total, 8000 * 1024 * 1024);
        assert_eq!(n2_net.inbound_bps, 2000);
    }
}

