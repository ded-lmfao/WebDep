use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as AxPath, State},
    http::StatusCode,
    routing::{get, put},
    Json, Router,
};
use base64::Engine;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::api::files::{ensure_ancestor_within_root, ensure_existing_within_root, safe_join, service_root};
use crate::api::projects::backups::copy_backup_files;
use crate::api::system::app_usage::{
    collect_project_seed_pids_pure, compute_entity_usage_with_monitor, EntityUsageInput,
};
use crate::caddy::Caddy;
use crate::cluster::archive::{
    pack_dir_to_tar_gz, pack_project_shift_to_zip, unpack_archive_auto_safe,
};
use crate::cluster::mesh::detect_capabilities_with_p2p;
use crate::deployer::builder::{merge_copy_dir, run_shell_cmd_with_logger, switch_current};
use crate::git::{CommitInfo, Git, StoredGithubToken};
use crate::monitor::{get_path_disk_size, Monitor};
use crate::projects::Project;
use crate::runtime::container::{
    build_container_env_pure, cleanup_project_files_dirs_in_data_dir,
    cleanup_project_files_dirs_in_data_dir_checked, ensure_container_in_data_dir,
    link_work_dir_to_container_dir, link_work_dir_to_db_dir, WebsiteContainer,
};
use crate::runtime::maintenance::{start_maintenance_server_in_map, stop_maintenance_server_in_map};
use crate::runtime::process::spawn_project_process_pure;
use crate::runtime::tree::{is_pid_alive, kill_project_processes_pure};
use crate::runtime::Runtime;

use super::models::{ClusterWsMessage, DeployStateUpdate, NodeStateManifest, NodeWorkloadPlacement};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteContextPayload {
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub deploy_key_public: Option<String>,
    #[serde(default)]
    pub deploy_key_private: Option<String>,
    #[serde(default)]
    pub github_tokens: Vec<StoredGithubToken>,
}

#[derive(Debug, Clone)]
pub struct P2pTransferTicket {
    pub dest_dir: PathBuf,
    pub project_slug: String,
    pub project_kind: String,
    pub project_id: i64,
    #[allow(dead_code)]
    pub port: u16,
    pub expires_at: i64,
    pub max_bytes: u64,
}

pub struct NodeExecState {
    pub data_dir: PathBuf,
    pub processes: Arc<Mutex<HashMap<i64, Child>>>,
    pub project_locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    pub stop_seq: Arc<Mutex<HashMap<i64, u64>>>,
    pub maintenance_shutdowns: Arc<Mutex<HashMap<i64, oneshot::Sender<()>>>>,
    pub env_cache: RwLock<HashMap<i64, HashMap<String, String>>>,
    pub deploy_queue: Arc<tokio::sync::Semaphore>,
    pub monitor: Monitor,
    pub manifest: RwLock<NodeStateManifest>,
    pub p2p_port: AtomicU16,
    pub p2p_tokens: RwLock<HashMap<String, P2pTransferTicket>>,
}

impl NodeExecState {
    pub fn new(data_dir: PathBuf, manifest: NodeStateManifest) -> Self {
        Self {
            data_dir,
            processes: Arc::new(Mutex::new(HashMap::new())),
            project_locks: Mutex::new(HashMap::new()),
            stop_seq: Arc::new(Mutex::new(HashMap::new())),
            maintenance_shutdowns: Arc::new(Mutex::new(HashMap::new())),
            env_cache: RwLock::new(HashMap::new()),
            deploy_queue: Arc::new(tokio::sync::Semaphore::new(1)),
            monitor: Monitor::new(),
            manifest: RwLock::new(manifest),
            p2p_port: AtomicU16::new(0),
            p2p_tokens: RwLock::new(HashMap::new()),
        }
    }

    pub async fn issue_p2p_ticket(
        &self,
        dest_dir: PathBuf,
        project_id: i64,
        project_slug: String,
        project_kind: String,
        port: u16,
        max_bytes: u64,
        ttl_secs: i64,
    ) -> String {
        let mut raw = [0u8; 24];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut raw);
        let token = format!("p2p_{}", hex::encode(raw));
        let now = now_secs();
        let ticket = P2pTransferTicket {
            dest_dir,
            project_slug,
            project_kind,
            project_id,
            port,
            expires_at: now + ttl_secs.clamp(30, 3600),
            max_bytes,
        };
        let mut guard = self.p2p_tokens.write().await;
        guard.retain(|_, v| v.expires_at > now);
        guard.insert(token.clone(), ticket);
        token
    }

    pub async fn peek_p2p_ticket(&self, token: &str) -> Option<P2pTransferTicket> {
        let now = now_secs();
        let guard = self.p2p_tokens.read().await;
        let ticket = guard.get(token)?.clone();
        if ticket.expires_at < now {
            None
        } else {
            Some(ticket)
        }
    }

    pub async fn remove_p2p_ticket(&self, token: &str) {
        self.p2p_tokens.write().await.remove(token);
    }

    fn get_project_lock(&self, project_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.project_locks.lock().unwrap();
        map.entry(project_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn get_stop_seq(&self, project_id: i64) -> u64 {
        let map = self.stop_seq.lock().unwrap();
        *map.get(&project_id).unwrap_or(&0)
    }

    fn bump_stop_seq(&self, project_id: i64) -> u64 {
        let mut map = self.stop_seq.lock().unwrap();
        let entry = map.entry(project_id).or_insert(0);
        *entry += 1;
        *entry
    }

    pub async fn snapshot_workloads(&self) -> Vec<NodeWorkloadPlacement> {
        let mut list = self.manifest.read().await.workloads.clone();
        let tracked_pids: HashMap<i64, Option<u32>> = {
            let mut out = HashMap::new();
            if let Ok(mut guard) = self.processes.lock() {
                for (&pid_proj, child) in guard.iter_mut() {
                    let alive = match child.try_wait() {
                        Ok(None) => child.id().filter(|&p| p > 0 && is_pid_alive(p)),
                        _ => None,
                    };
                    out.insert(pid_proj, alive);
                }
            }
            out
        };

        for w in &mut list {
            let in_mem_pid = tracked_pids.get(&w.project_id).copied().flatten();
            let port_pid = if in_mem_pid.is_none() {
                w.port
                    .and_then(|pt| Runtime::find_pids_on_port(pt).into_iter().next())
            } else {
                None
            };
            let existing_alive = w.pid.filter(|&p| p > 0 && is_pid_alive(p));
            let active_pid = in_mem_pid.or(port_pid).or(existing_alive);
            w.pid = active_pid;

            if w.app_status != "maintenance" {
                w.app_status = if active_pid.is_some() {
                    "running".to_string()
                } else {
                    "stopped".to_string()
                };
            }

            if !w.deploy_dir.trim().is_empty() {
                let dpath = Path::new(&w.deploy_dir);
                if dpath.exists() {
                    w.disk_bytes = get_path_disk_size(dpath);
                }
            }
        }
        list
    }

    pub async fn upsert_workload_from_project(&self, p: &Project, status_override: Option<&str>, pid: Option<u32>) {
        let mut guard = self.manifest.write().await;
        if let Some(existing) = guard.workloads.iter_mut().find(|w| w.project_id == p.id) {
            existing.slug = p.slug.clone();
            existing.placement_version = p.placement_version;
            existing.port = p.port;
            existing.deploy_dir = p.deploy_dir.clone();
            if let Some(st) = status_override {
                existing.app_status = st.to_string();
            }
            existing.pid = pid;
        } else {
            guard.workloads.push(NodeWorkloadPlacement {
                project_id: p.id,
                slug: p.slug.clone(),
                placement_version: p.placement_version,
                port: p.port,
                app_status: status_override
                    .unwrap_or(p.app_status.as_str())
                    .to_string(),
                pid,
                disk_bytes: 0,
                deploy_dir: p.deploy_dir.clone(),
            });
        }
        guard.updated_at = now_secs();
        let _ = super::node_agent::save_manifest(&self.data_dir, &guard);
    }

    pub async fn remove_workload(&self, project_id: i64) {
        let mut guard = self.manifest.write().await;
        guard.workloads.retain(|w| w.project_id != project_id);
        guard.updated_at = now_secs();
        let _ = super::node_agent::save_manifest(&self.data_dir, &guard);
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn send_log(
    ws_tx: &mpsc::UnboundedSender<ClusterWsMessage>,
    project_id: Option<i64>,
    deployment_id: Option<i64>,
    channel: &str,
    level: &str,
    message: &str,
) {
    let _ = ws_tx.send(ClusterWsMessage::LogChunk {
        project_id,
        deployment_id,
        channel: channel.to_string(),
        level: level.to_string(),
        message: message.to_string(),
        ts: now_secs() * 1000,
    });
}

fn apply_remote_context(data_dir: &Path, project_id: i64, ctx: &RemoteContextPayload) {
    if !ctx.github_tokens.is_empty() {
        Git::set_cached_github_tokens(ctx.github_tokens.clone());
    }
    if ctx.deploy_key_public.is_some() || ctx.deploy_key_private.is_some() {
        let _ = Git::write_project_deploy_key(
            data_dir,
            project_id,
            ctx.deploy_key_public.as_deref().unwrap_or(""),
            ctx.deploy_key_private.as_deref().unwrap_or(""),
        );
    }
}

async fn apply_and_cache_context(
    exec: &Arc<NodeExecState>,
    p: &Project,
    ctx: RemoteContextPayload,
) {
    apply_remote_context(&exec.data_dir, p.id, &ctx);
    let container = ensure_container_in_data_dir(&exec.data_dir, p, p.port.unwrap_or(3000));
    if !ctx.env.is_empty() {
        exec.env_cache.write().await.insert(p.id, ctx.env.clone());
    }
    if p.project_kind != "service" && !ctx.env.is_empty() {
        let (_iso, dotenv_body) = build_container_env_pure(p, &ctx.env, &container);
        let current_dir = Path::new(&p.deploy_dir).join("current");
        if current_dir.is_dir() {
            let _ = tokio::fs::write(current_dir.join(".env"), dotenv_body).await;
        }
    }
}

fn ok_resp(req_id: String, result: Value) -> ClusterWsMessage {
    ClusterWsMessage::RpcResponse {
        req_id,
        ok: true,
        status: 200,
        result,
        error: None,
    }
}

fn err_resp(req_id: String, status: u16, msg: impl Into<String>) -> ClusterWsMessage {
    ClusterWsMessage::RpcResponse {
        req_id,
        ok: false,
        status,
        result: Value::Null,
        error: Some(msg.into()),
    }
}

/// Reconcile local workload placements against Core's authoritative `placement_versions` map.
/// Stops any stale or reassigned workloads whose `placement_version` is older than Core's
/// or no longer assigned to this Web-Node.
pub async fn reconcile_placements(
    exec: &Arc<NodeExecState>,
    placement_versions: &HashMap<i64, i64>,
    ws_tx: Option<&mpsc::UnboundedSender<ClusterWsMessage>>,
) -> Vec<i64> {
    let mut stopped_ids = Vec::new();
    let stale_items: Vec<NodeWorkloadPlacement> = {
        let guard = exec.manifest.read().await;
        guard
            .workloads
            .iter()
            .filter(|w| match placement_versions.get(&w.project_id) {
                None => true,
                Some(&core_ver) => w.placement_version < core_ver,
            })
            .cloned()
            .collect()
    };

    for stale in stale_items {
        let old_child = exec
            .processes
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&stale.project_id));
        stop_maintenance_server_in_map(&exec.maintenance_shutdowns, stale.project_id);
        let extra = stale.pid.into_iter().collect::<Vec<_>>();
        let killed = kill_project_processes_pure(
            stale.project_id,
            &stale.slug,
            &stale.deploy_dir,
            stale.port,
            &extra,
            old_child,
        )
        .await;
        if killed || stale.app_status == "running" {
            if let Some(tx) = ws_tx {
                send_log(
                    tx,
                    Some(stale.project_id),
                    None,
                    "system",
                    "warn",
                    &format!(
                        "Split-brain guard: stopped stale workload '{}' (local placement v{}) after PeerSync reconciliation",
                        stale.slug, stale.placement_version
                    ),
                );
            }
        }
        stopped_ids.push(stale.project_id);
    }

    if !stopped_ids.is_empty() {
        let mut guard = exec.manifest.write().await;
        guard
            .workloads
            .retain(|w| !stopped_ids.contains(&w.project_id));
        guard.updated_at = now_secs();
        let _ = super::node_agent::save_manifest(&exec.data_dir, &guard);
    }

    stopped_ids
}

async fn stop_project_local(exec: &Arc<NodeExecState>, p: &Project, container: &WebsiteContainer) -> bool {
    exec.bump_stop_seq(p.id);
    let proj_lock = exec.get_project_lock(p.id);
    let _guard = proj_lock.lock().await;
    stop_maintenance_server_in_map(&exec.maintenance_shutdowns, p.id);
    let old_child = exec.processes.lock().ok().and_then(|mut m| m.remove(&p.id));
    let extra = p.app_pid.into_iter().collect::<Vec<_>>();
    let killed = kill_project_processes_pure(
        p.id,
        &p.slug,
        &p.deploy_dir,
        Some(p.port.unwrap_or(container.port)),
        &extra,
        old_child,
    )
    .await;
    exec.upsert_workload_from_project(p, Some("stopped"), None).await;
    killed
}

async fn stop_project_processes_for_cleanup(exec: &Arc<NodeExecState>, p: &Project) -> bool {
    exec.bump_stop_seq(p.id);
    let proj_lock = exec.get_project_lock(p.id);
    let _guard = proj_lock.lock().await;
    stop_maintenance_server_in_map(&exec.maintenance_shutdowns, p.id);
    let old_child = exec.processes.lock().ok().and_then(|mut m| m.remove(&p.id));
    let extra = p.app_pid.into_iter().collect::<Vec<_>>();
    let resolved_dir = resolve_source_deploy_dir(&exec.data_dir, p);
    let deploy_str = resolved_dir.to_string_lossy();
    kill_project_processes_pure(
        p.id,
        &p.slug,
        &deploy_str,
        p.port,
        &extra,
        old_child,
    )
    .await
}

async fn start_project_local(
    exec: &Arc<NodeExecState>,
    p: &Project,
    env_vars: HashMap<String, String>,
    release_dir: Option<&Path>,
    ws_tx: &mpsc::UnboundedSender<ClusterWsMessage>,
) -> Result<u32, String> {
    let proj_lock = exec.get_project_lock(p.id);
    let _guard = proj_lock.lock().await;

    stop_maintenance_server_in_map(&exec.maintenance_shutdowns, p.id);
    let old_child = exec.processes.lock().ok().and_then(|mut m| m.remove(&p.id));
    let extra = p.app_pid.into_iter().collect::<Vec<_>>();
    let container = ensure_container_in_data_dir(&exec.data_dir, p, p.port.unwrap_or(3000));
    let _ = kill_project_processes_pure(
        p.id,
        &p.slug,
        &p.deploy_dir,
        Some(container.port),
        &extra,
        old_child,
    )
    .await;

    let tx_clone = ws_tx.clone();
    let proj_id = p.id;
    let log_cb: Arc<dyn Fn(&str, &str, &str) + Send + Sync> = Arc::new(move |ch, lvl, msg| {
        send_log(&tx_clone, Some(proj_id), None, ch, lvl, msg);
    });

    let (final_pid, child, work_dir, final_cmd) =
        match spawn_project_process_pure(p, &container, env_vars, release_dir, log_cb).await {
            Ok(res) => res,
            Err(e) => {
                exec.upsert_workload_from_project(p, Some("stopped"), None).await;
                return Err(e);
            }
        };
    let pid = child.id().unwrap_or(final_pid);

    if let Ok(mut map) = exec.processes.lock() {
        map.insert(p.id, child);
    }
    exec.upsert_workload_from_project(p, Some("running"), Some(final_pid))
        .await;

    send_log(
        ws_tx,
        Some(p.id),
        None,
        "system",
        "info",
        &format!(
            "Started '{}' on Web-Node (PID: {}, Port: {}, Dir: {}) cmd='{}'",
            p.name,
            pid,
            container.port,
            work_dir.display(),
            final_cmd
        ),
    );
    Ok(final_pid)
}

async fn run_remote_deploy_task(
    exec: Arc<NodeExecState>,
    p: Project,
    dep_id: i64,
    requested_sha: Option<String>,
    ctx: RemoteContextPayload,
    ws_tx: mpsc::UnboundedSender<ClusterWsMessage>,
) {
    let emit_state = |upd: DeployStateUpdate| {
        let _ = ws_tx.send(ClusterWsMessage::DeployStateUpdate { update: upd });
    };

    let _permit = match exec.deploy_queue.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return,
    };

    apply_and_cache_context(&exec, &p, ctx.clone()).await;
    let start_stop_seq = exec.get_stop_seq(p.id);
    let container = ensure_container_in_data_dir(&exec.data_dir, &p, p.port.unwrap_or(3000));
    send_log(
        &ws_tx,
        Some(p.id),
        Some(dep_id),
        "deployment",
        "info",
        &format!(
            "Starting remote deployment #{} for {} in container '{}' (Port: {})",
            dep_id, p.name, container.container_id, container.port
        ),
    );

    emit_state(DeployStateUpdate {
        deployment_id: dep_id,
        project_id: p.id,
        stage: Some("fetching".to_string()),
        ..Default::default()
    });

    let repo_dir = Path::new(&p.deploy_dir).join("repo");
    let fetch_res = tokio::task::spawn_blocking({
        let url = p.repo_url.clone();
        let rd = repo_dir.clone();
        let br = p.branch.clone();
        move || Git::clone_or_fetch(&url, &rd, &br)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));

    if let Err(e) = fetch_res {
        send_log(&ws_tx, Some(p.id), Some(dep_id), "github", "error", &format!("Git fetch failed: {e}"));
        emit_state(DeployStateUpdate {
            deployment_id: dep_id,
            project_id: p.id,
            status: Some("failed".to_string()),
            error: Some(e),
            ..Default::default()
        });
        return;
    }

    let poll_ts = now_secs();
    let head = match requested_sha.filter(|s| !s.trim().is_empty()) {
        Some(s) => s,
        None => match Git::resolve_head(&repo_dir) {
            Ok(h) => h,
            Err(e) => {
                emit_state(DeployStateUpdate {
                    deployment_id: dep_id,
                    project_id: p.id,
                    status: Some("failed".to_string()),
                    last_poll_at: Some(poll_ts),
                    error: Some(e),
                    ..Default::default()
                });
                return;
            }
        },
    };

    emit_state(DeployStateUpdate {
        deployment_id: dep_id,
        project_id: p.id,
        stage: Some("pulling".to_string()),
        last_poll_at: Some(poll_ts),
        ..Default::default()
    });

    let is_service = p.project_kind == "service";
    let mut release_dir = Path::new(&p.deploy_dir)
        .join("releases")
        .join(format!("rel-{dep_id}"));
    if let Err(e) = Git::checkout(&repo_dir, &head, &release_dir) {
        send_log(&ws_tx, Some(p.id), Some(dep_id), "deployment", "error", &format!("Checkout error: {e}"));
        emit_state(DeployStateUpdate {
            deployment_id: dep_id,
            project_id: p.id,
            status: Some("failed".to_string()),
            error: Some(e),
            ..Default::default()
        });
        return;
    }

    if is_service {
        let app_dir = Path::new(&p.deploy_dir).join("app");
        if let Err(e) = merge_copy_dir(&release_dir, &app_dir) {
            emit_state(DeployStateUpdate {
                deployment_id: dep_id,
                project_id: p.id,
                status: Some("failed".to_string()),
                error: Some(e),
                ..Default::default()
            });
            return;
        }
        let _ = tokio::fs::remove_dir_all(&release_dir).await;
        release_dir = app_dir;
    }

    let commit_info = Git::get_commit_info(&repo_dir, &head).unwrap_or(CommitInfo {
        sha: head.clone(),
        short_sha: head[..7.min(head.len())].to_string(),
        author: "System".to_string(),
        date: now_secs(),
        message: "Deploy".to_string(),
    });

    emit_state(DeployStateUpdate {
        deployment_id: dep_id,
        project_id: p.id,
        sha: Some(head.clone()),
        release_dir: Some(release_dir.to_string_lossy().to_string()),
        short_sha: Some(commit_info.short_sha.clone()),
        commit_message: Some(commit_info.message.clone()),
        commit_author: Some(commit_info.author.clone()),
        committed_at: Some(commit_info.date),
        ..Default::default()
    });

    if !is_service {
        link_work_dir_to_container_dir(&release_dir, &container);
        let (_iso, dotenv_body) = build_container_env_pure(&p, &ctx.env, &container);
        let _ = tokio::fs::write(release_dir.join(".env"), dotenv_body).await;
    }

    let has_pkg_json = release_dir.join("package.json").exists();
    let effective_install = if !p.install_cmd.trim().is_empty() {
        p.install_cmd.clone()
    } else if has_pkg_json {
        "npm install".to_string()
    } else {
        String::new()
    };

    let tx_build = ws_tx.clone();
    let proj_id = p.id;
    let build_logger = move |ch: &str, lvl: &str, line: &str| {
        send_log(&tx_build, Some(proj_id), Some(dep_id), ch, lvl, line);
    };

    if !effective_install.trim().is_empty() {
        emit_state(DeployStateUpdate {
            deployment_id: dep_id,
            project_id: p.id,
            stage: Some("installing".to_string()),
            ..Default::default()
        });
        send_log(&ws_tx, Some(p.id), Some(dep_id), "build", "info", &format!("Running install: {effective_install}"));
        if let Err(e) = run_shell_cmd_with_logger(&effective_install, &release_dir, build_logger.clone()).await {
            let fallback_res = if effective_install.contains("npm ci") && has_pkg_json {
                send_log(&ws_tx, Some(p.id), Some(dep_id), "build", "warn", "npm ci failed, falling back to npm install...");
                run_shell_cmd_with_logger("npm install", &release_dir, build_logger.clone()).await
            } else {
                Err(e)
            };
            if let Err(e2) = fallback_res {
                send_log(&ws_tx, Some(p.id), Some(dep_id), "build", "error", &format!("Install failed: {e2}"));
                emit_state(DeployStateUpdate {
                    deployment_id: dep_id,
                    project_id: p.id,
                    status: Some("failed".to_string()),
                    error: Some(e2),
                    ..Default::default()
                });
                return;
            }
        }
    }

    let effective_build = if !p.build_cmd.trim().is_empty() {
        p.build_cmd.clone()
    } else {
        tokio::fs::read_to_string(release_dir.join("package.json"))
            .await
            .ok()
            .filter(|txt| txt.contains("\"build\""))
            .map(|_| "npm run build".to_string())
            .unwrap_or_default()
    };

    if !effective_build.trim().is_empty() {
        emit_state(DeployStateUpdate {
            deployment_id: dep_id,
            project_id: p.id,
            stage: Some("building".to_string()),
            ..Default::default()
        });
        send_log(&ws_tx, Some(p.id), Some(dep_id), "build", "info", &format!("Running build: {effective_build}"));
        if let Err(e) = run_shell_cmd_with_logger(&effective_build, &release_dir, build_logger).await {
            emit_state(DeployStateUpdate {
                deployment_id: dep_id,
                project_id: p.id,
                status: Some("failed".to_string()),
                error: Some(format!("Build failed: {e}")),
                ..Default::default()
            });
            return;
        }
    }

    emit_state(DeployStateUpdate {
        deployment_id: dep_id,
        project_id: p.id,
        stage: Some("deploying".to_string()),
        ..Default::default()
    });
    let current_symlink = Path::new(&p.deploy_dir).join("current");
    switch_current(&current_symlink, &release_dir);

    if exec.get_stop_seq(p.id) != start_stop_seq {
        emit_state(DeployStateUpdate {
            deployment_id: dep_id,
            project_id: p.id,
            status: Some("success".to_string()),
            deployed_sha: Some(head),
            ..Default::default()
        });
        return;
    }

    emit_state(DeployStateUpdate {
        deployment_id: dep_id,
        project_id: p.id,
        stage: Some("health_checking".to_string()),
        ..Default::default()
    });

    let pid = match start_project_local(&exec, &p, ctx.env.clone(), Some(&release_dir), &ws_tx).await {
        Ok(pid) => pid,
        Err(e) => {
            emit_state(DeployStateUpdate {
                deployment_id: dep_id,
                project_id: p.id,
                status: Some("failed".to_string()),
                app_status: Some("stopped".to_string()),
                error: Some(format!("Start failed: {e}")),
                ..Default::default()
            });
            return;
        }
    };

    if !p.health_url.trim().is_empty() {
        let port_str = container.port.to_string();
        let health_url = p.health_url.replace("{{PORT}}", &port_str);
        if let Err(e) = crate::runtime::supervisor::health_check_url(&health_url, p.health_timeout_s.max(5) as u64).await {
            emit_state(DeployStateUpdate {
                deployment_id: dep_id,
                project_id: p.id,
                status: Some("failed".to_string()),
                error: Some(format!("Health check failed: {e}")),
                ..Default::default()
            });
            return;
        }
    }

    if p.stability_s > 0 {
        tokio::time::sleep(Duration::from_secs(p.stability_s as u64)).await;
    }

    send_log(
        &ws_tx,
        Some(p.id),
        Some(dep_id),
        "deployment",
        "info",
        "Remote deployment completed successfully! Version is live.",
    );
    emit_state(DeployStateUpdate {
        deployment_id: dep_id,
        project_id: p.id,
        status: Some("success".to_string()),
        sha: Some(head.clone()),
        release_dir: Some(release_dir.to_string_lossy().to_string()),
        deployed_sha: Some(head),
        app_status: Some("running".to_string()),
        app_pid: Some(pid),
        backup_note: Some(format!("Auto-snapshot ({})", commit_info.short_sha)),
        backup_size_bytes: Some(5 * 1024 * 1024),
        ..Default::default()
    });
}

async fn run_remote_rollback_task(
    exec: Arc<NodeExecState>,
    p: Project,
    dep_id: i64,
    target_release_id: i64,
    target_release_dir: String,
    target_sha: String,
    ctx: RemoteContextPayload,
    ws_tx: mpsc::UnboundedSender<ClusterWsMessage>,
) {
    let emit_state = |upd: DeployStateUpdate| {
        let _ = ws_tx.send(ClusterWsMessage::DeployStateUpdate { update: upd });
    };

    let _permit = match exec.deploy_queue.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return,
    };

    apply_and_cache_context(&exec, &p, ctx.clone()).await;
    let release_dir = PathBuf::from(&target_release_dir);
    if !release_dir.exists() {
        run_remote_deploy_task(exec, p, dep_id, Some(target_sha), ctx, ws_tx).await;
        return;
    }

    emit_state(DeployStateUpdate {
        deployment_id: dep_id,
        project_id: p.id,
        release_id: Some(target_release_id),
        stage: Some("deploying".to_string()),
        ..Default::default()
    });

    let current_symlink = Path::new(&p.deploy_dir).join("current");
    switch_current(&current_symlink, &release_dir);

    emit_state(DeployStateUpdate {
        deployment_id: dep_id,
        project_id: p.id,
        release_id: Some(target_release_id),
        stage: Some("health_checking".to_string()),
        ..Default::default()
    });

    match start_project_local(&exec, &p, ctx.env, Some(&release_dir), &ws_tx).await {
        Ok(pid) => {
            send_log(
                &ws_tx,
                Some(p.id),
                Some(dep_id),
                "rollback",
                "info",
                &format!("Remote rollback to Release #{target_release_id} succeeded!"),
            );
            emit_state(DeployStateUpdate {
                deployment_id: dep_id,
                project_id: p.id,
                release_id: Some(target_release_id),
                status: Some("success".to_string()),
                deployed_sha: Some(target_sha),
                app_status: Some("running".to_string()),
                app_pid: Some(pid),
                ..Default::default()
            });
        }
        Err(e) => {
            emit_state(DeployStateUpdate {
                deployment_id: dep_id,
                project_id: p.id,
                status: Some("failed".to_string()),
                app_status: Some("stopped".to_string()),
                error: Some(format!("Rollback start failed: {e}")),
                ..Default::default()
            });
        }
    }
}

pub async fn execute_rpc(
    exec: Arc<NodeExecState>,
    req_id: String,
    op: String,
    params: Value,
    ws_tx: mpsc::UnboundedSender<ClusterWsMessage>,
) -> ClusterWsMessage {
    match op.as_str() {
        "project.ensure" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            let _ = tokio::fs::create_dir_all(&p.deploy_dir).await;
            ensure_container_in_data_dir(&exec.data_dir, &p, p.port.unwrap_or(3000));
            apply_and_cache_context(&exec, &p, ctx).await;
            exec.upsert_workload_from_project(&p, Some(&p.app_status), p.app_pid)
                .await;
            ok_resp(req_id, json!({ "ok": true }))
        }
        "project.delete" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let remaining: Vec<Project> =
                serde_json::from_value(params["remainingProjects"].clone()).unwrap_or_default();
            let container = ensure_container_in_data_dir(&exec.data_dir, &p, p.port.unwrap_or(3000));
            stop_project_local(&exec, &p, &container).await;
            cleanup_project_files_dirs_in_data_dir(&exec.data_dir, &p, &remaining);
            let caddy = Caddy::new(&exec.data_dir);
            caddy.remove_project_config(&p.slug, &remaining);
            exec.remove_workload(p.id).await;
            exec.env_cache.write().await.remove(&p.id);
            ok_resp(req_id, json!({ "ok": true }))
        }
        "project.start" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            apply_and_cache_context(&exec, &p, ctx.clone()).await;
            match start_project_local(&exec, &p, ctx.env, None, &ws_tx).await {
                Ok(pid) => ok_resp(req_id, json!({ "ok": true, "pid": pid })),
                Err(e) => {
                    let status = if e == "NOT_DEPLOYED_YET" { 409 } else { 500 };
                    err_resp(req_id, status, e)
                }
            }
        }
        "project.stop" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let container = ensure_container_in_data_dir(&exec.data_dir, &p, p.port.unwrap_or(3000));
            stop_project_local(&exec, &p, &container).await;
            send_log(&ws_tx, Some(p.id), None, "system", "info", &format!("Stopped '{}' on Web-Node", p.name));
            ok_resp(req_id, json!({ "ok": true }))
        }
        "project.restart" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            apply_and_cache_context(&exec, &p, ctx.clone()).await;
            let container = ensure_container_in_data_dir(&exec.data_dir, &p, p.port.unwrap_or(3000));
            stop_project_local(&exec, &p, &container).await;
            match start_project_local(&exec, &p, ctx.env, None, &ws_tx).await {
                Ok(pid) => ok_resp(req_id, json!({ "ok": true, "pid": pid })),
                Err(e) => err_resp(req_id, 500, e),
            }
        }
        "project.maintenance" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let enabled = params["enabled"].as_bool().unwrap_or(false);
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            apply_and_cache_context(&exec, &p, ctx.clone()).await;
            let container = ensure_container_in_data_dir(&exec.data_dir, &p, p.port.unwrap_or(3000));
            if enabled {
                stop_project_local(&exec, &p, &container).await;
                if let Some(port) = p.port {
                    start_maintenance_server_in_map(
                        &exec.maintenance_shutdowns,
                        p.id,
                        &p.name,
                        &p.slug,
                        port,
                        "Scheduled Maintenance • Developers Working",
                    );
                }
                exec.upsert_workload_from_project(&p, Some("maintenance"), None)
                    .await;
                send_log(
                    &ws_tx,
                    Some(p.id),
                    None,
                    "system",
                    "info",
                    &format!("Enabled Maintenance Mode for '{}' on Web-Node (Port {})", p.name, p.port.unwrap_or(3000)),
                );
                ok_resp(req_id, json!({ "appStatus": "maintenance" }))
            } else {
                stop_maintenance_server_in_map(&exec.maintenance_shutdowns, p.id);
                let _ = start_project_local(&exec, &p, ctx.env, None, &ws_tx).await;
                let final_status = exec
                    .manifest
                    .read()
                    .await
                    .workloads
                    .iter()
                    .find(|w| w.project_id == p.id)
                    .map(|w| w.app_status.clone())
                    .unwrap_or_else(|| "stopped".to_string());
                ok_resp(req_id, json!({ "appStatus": final_status }))
            }
        }
        "project.deploy" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let dep_id = params["deploymentId"].as_i64().unwrap_or(0);
            let requested_sha = params["sha"].as_str().map(|s| s.to_string());
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            let exec_c = exec.clone();
            let tx_c = ws_tx.clone();
            tokio::spawn(async move {
                run_remote_deploy_task(exec_c, p, dep_id, requested_sha, ctx, tx_c).await;
            });
            ok_resp(req_id, json!({ "queued": true, "deploymentId": dep_id }))
        }
        "project.rollback" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let dep_id = params["deploymentId"].as_i64().unwrap_or(0);
            let target_rel_id = params["releaseId"].as_i64().unwrap_or(0);
            let target_rel_dir = params["releaseDir"].as_str().unwrap_or("").to_string();
            let target_sha = params["sha"].as_str().unwrap_or("").to_string();
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            let exec_c = exec.clone();
            let tx_c = ws_tx.clone();
            tokio::spawn(async move {
                run_remote_rollback_task(
                    exec_c,
                    p,
                    dep_id,
                    target_rel_id,
                    target_rel_dir,
                    target_sha,
                    ctx,
                    tx_c,
                )
                .await;
            });
            ok_resp(req_id, json!({ "queued": true, "deploymentId": dep_id }))
        }
        "project.create_snapshot" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let kind = params["kind"].as_str().unwrap_or("full-snapshot");
            let raw_env = params["rawEnv"].as_str().unwrap_or("");
            let ts = now_secs();
            let base_dir = PathBuf::from(&p.deploy_dir);
            let backup_dir = base_dir.join("backups").join(format!("backup-{ts}"));
            let _ = tokio::fs::create_dir_all(&backup_dir).await;
            let mut total_size = 0u64;
            if !raw_env.is_empty() && tokio::fs::write(backup_dir.join("backup.env"), raw_env).await.is_ok() {
                total_size += raw_env.len() as u64;
            }
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
            if total_size == 0 {
                total_size = 4096;
            }
            let release_files_dir = if backup_dir.join("files").exists() {
                backup_dir.join("files").to_string_lossy().to_string()
            } else {
                backup_dir.to_string_lossy().to_string()
            };
            ok_resp(
                req_id,
                json!({
                    "size": total_size,
                    "backupPath": backup_dir.to_string_lossy().to_string(),
                    "releaseFilesDir": release_files_dir,
                    "ts": ts,
                }),
            )
        }
        "project.restore_backup" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let backup_path = params["backupPath"].as_str().unwrap_or("");
            let b_dir = PathBuf::from(backup_path);
            let raw_env = tokio::fs::read_to_string(b_dir.join("backup.env"))
                .await
                .unwrap_or_default();
            let files_dir = b_dir.join("files");
            let target_current = PathBuf::from(&p.deploy_dir).join("current");
            if files_dir.exists() && target_current.exists() {
                let f_c = files_dir.clone();
                let t_c = target_current.clone();
                let _ = tokio::task::spawn_blocking(move || copy_backup_files(&f_c, &t_c)).await;
            }
            ok_resp(req_id, json!({ "ok": true, "rawEnv": raw_env }))
        }
        "project.delete_path" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let target_str = params["path"].as_str().unwrap_or("");
            let path = Path::new(target_str);
            let backups_root = Path::new(&p.deploy_dir).join("backups");
            let releases_root = Path::new(&p.deploy_dir).join("releases");
            if let Ok(canon_path) = tokio::fs::canonicalize(path).await {
                let in_b = tokio::fs::canonicalize(&backups_root)
                    .await
                    .map(|r| canon_path.starts_with(&r) && canon_path != r)
                    .unwrap_or(false);
                let in_r = tokio::fs::canonicalize(&releases_root)
                    .await
                    .map(|r| canon_path.starts_with(&r) && canon_path != r)
                    .unwrap_or(false);
                if in_b || in_r {
                    let _ = tokio::fs::remove_dir_all(&canon_path).await;
                }
            }
            ok_resp(req_id, json!({ "ok": true }))
        }
        "git.deploy_key_sync" | "env.sync" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            apply_and_cache_context(&exec, &p, ctx).await;
            ok_resp(req_id, json!({ "ok": true }))
        }
        "git.commits" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            apply_remote_context(&exec.data_dir, p.id, &ctx);
            let pull = params["pull"].as_bool().unwrap_or(false);
            let limit = params["limit"].as_u64().unwrap_or(20).clamp(1, 50) as usize;
            let repo_dir = PathBuf::from(&p.deploy_dir).join("repo");
            let branch = p.branch.clone();
            let repo_url = p.repo_url.clone();
            let commits = tokio::task::spawn_blocking(move || {
                if !repo_dir.join(".git").exists() || pull {
                    let _ = Git::clone_or_fetch(&repo_url, &repo_dir, &branch);
                }
                Git::list_commits(&repo_dir, &branch, limit)
            })
            .await
            .unwrap_or_default();
            ok_resp(req_id, json!({ "commits": commits }))
        }
        "git.check" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            apply_remote_context(&exec.data_dir, p.id, &ctx);
            let repo_dir = PathBuf::from(&p.deploy_dir).join("repo");
            let branch = p.branch.clone();
            let repo_url = p.repo_url.clone();
            let head_res = tokio::task::spawn_blocking(move || {
                Git::clone_or_fetch(&repo_url, &repo_dir, &branch)?;
                Git::resolve_head(&repo_dir)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            match head_res {
                Ok(head) => ok_resp(req_id, json!({ "head": head })),
                Err(e) => err_resp(req_id, 500, e),
            }
        }
        "git.fetch" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            apply_remote_context(&exec.data_dir, p.id, &ctx);
            let repo_dir = PathBuf::from(&p.deploy_dir).join("repo");
            let branch = p.branch.clone();
            let repo_url = p.repo_url.clone();
            let head_res = tokio::task::spawn_blocking(move || {
                Git::clone_or_fetch(&repo_url, &repo_dir, &branch)?;
                Git::resolve_head(&repo_dir)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            match head_res {
                Ok(head) => ok_resp(req_id, json!({ "head": head, "fetched": true })),
                Err(e) => err_resp(req_id, 500, e),
            }
        }
        "git.detect" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload = serde_json::from_value(params["context"].clone()).unwrap_or_default();
            apply_remote_context(&exec.data_dir, p.id, &ctx);
            let repo_dir = PathBuf::from(&p.deploy_dir).join("repo");
            if !repo_dir.exists() {
                let u = p.repo_url.clone();
                let rd = repo_dir.clone();
                let b = p.branch.clone();
                let _ = tokio::task::spawn_blocking(move || Git::clone_or_fetch(&u, &rd, &b)).await;
            }
            let pkg_json = repo_dir.join("package.json");
            if pkg_json.exists() {
                if let Ok(content) = tokio::fs::read_to_string(&pkg_json).await {
                    let has_build = content.contains("\"build\"");
                    let has_server_js = repo_dir.join("server.js").exists();
                    let has_vite = content.contains("\"vite\"");
                    let start_cmd = if has_server_js {
                        "node server.js".to_string()
                    } else if has_vite {
                        "npx vite preview --port {{PORT}} --host 127.0.0.1".to_string()
                    } else {
                        "npm start".to_string()
                    };
                    return ok_resp(
                        req_id,
                        json!({
                            "type": if has_vite && !has_server_js { "static" } else { "node" },
                            "install": "npm ci",
                            "build": if has_build { "npm run build" } else { "" },
                            "start": start_cmd,
                        }),
                    );
                }
            }
            ok_resp(
                req_id,
                json!({
                    "type": p.project_type,
                    "install": p.install_cmd,
                    "build": p.build_cmd,
                    "start": p.start_cmd,
                }),
            )
        }
        "caddy.apply" => {
            let projects: Vec<Project> =
                serde_json::from_value(params["projects"].clone()).unwrap_or_default();
            let caddy = Caddy::new(&exec.data_dir);
            for p in &projects {
                if let Err(e) = caddy.write_config(p, &projects) {
                    return err_resp(req_id, 500, format!("Caddy write failed: {e}"));
                }
            }
            if let Err(e) = caddy.reload() {
                return err_resp(req_id, 500, format!("Caddy reload failed: {e}"));
            }
            ok_resp(req_id, json!({ "ok": true, "reloaded": true }))
        }
        "files.list" => handle_files_list(req_id, params).await,
        "files.read" => handle_files_read(req_id, params).await,
        "files.write" => handle_files_write(req_id, params).await,
        "files.mkdir" => handle_files_mkdir(req_id, params).await,
        "files.delete" => handle_files_delete(req_id, params).await,
        "files.rename" => handle_files_rename(req_id, params).await,
        "files.download" => handle_files_download(req_id, params).await,
        "files.upload" => handle_files_upload(req_id, params).await,
        "metrics.project_usage" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let exec_c = exec.clone();
            let res = tokio::task::spawn_blocking(move || {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let deploy_dir = Path::new(&p.deploy_dir);
                let tracked = exec_c
                    .processes
                    .lock()
                    .ok()
                    .and_then(|m| m.get(&p.id).and_then(|c| c.id()));
                let seeds = collect_project_seed_pids_pure(
                    tracked,
                    p.id,
                    &p.slug,
                    &p.app_status,
                    deploy_dir,
                    p.port,
                    p.app_pid.map(i64::from),
                    &cwd,
                );
                let mut disk_dirs = vec![deploy_dir.to_path_buf()];
                let ctr_dir = cwd.join("data").join("containers").join(&p.slug);
                if !ctr_dir.starts_with(deploy_dir) {
                    disk_dirs.push(ctr_dir);
                }
                let runtime_running = tracked.map(is_pid_alive).unwrap_or(false);
                compute_entity_usage_with_monitor(
                    &exec_c.monitor,
                    EntityUsageInput {
                        tracker_key: p.id,
                        public_id: p.id,
                        entity_kind: &p.project_kind,
                        status_str: &p.app_status,
                        port: p.port,
                        seed_pids: seeds,
                        runtime_running,
                        disk_dirs,
                    },
                )
            })
            .await;
            match res {
                Ok(usage) => ok_resp(req_id, serde_json::to_value(usage).unwrap_or(Value::Null)),
                Err(e) => err_resp(req_id, 500, e.to_string()),
            }
        }
        "node.allocate_free_port" => {
            let preferred = params["preferredPort"].as_u64().and_then(|v| u16::try_from(v).ok());
            let used: Vec<u16> = params["usedPorts"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                        .collect()
                })
                .unwrap_or_default();
            match allocate_free_port_on_node(preferred, &used) {
                Ok(port) => ok_resp(req_id, json!({ "port": port })),
                Err(e) => err_resp(req_id, 500, e),
            }
        }
        "shift.preflight" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let preferred = params["preferredPort"]
                .as_u64()
                .and_then(|v| u16::try_from(v).ok())
                .or(p.assigned_port.or(p.port));
            let used: Vec<u16> = params["usedPorts"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                        .collect()
                })
                .unwrap_or_default();
            let allocated_port = match allocate_free_port_on_node(preferred, &used) {
                Ok(port) => port,
                Err(e) => return err_resp(req_id, 500, e),
            };
            let caps = detect_capabilities_with_p2p(
                match exec.p2p_port.load(Ordering::Relaxed) {
                    0 => None,
                    v => Some(v),
                },
            );
            let mut sys = sysinfo::System::new();
            sys.refresh_memory();
            let avail_mem = sys.available_memory();
            let total_mem = sys.total_memory();
            let target_deploy_dir = resolve_target_deploy_dir(&exec.data_dir, &p);
            ok_resp(
                req_id,
                json!({
                    "ok": true,
                    "allocatedPort": allocated_port,
                    "targetDeployDir": target_deploy_dir.to_string_lossy().to_string(),
                    "freeMemBytes": avail_mem,
                    "totalMemBytes": total_mem,
                    "capabilities": caps,
                }),
            )
        }
        "shift.prepare_receive" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let dest_dir = params["destDir"]
                .as_str()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| resolve_target_deploy_dir(&exec.data_dir, &p));
            let max_bytes = params["maxBytes"].as_u64().unwrap_or(8 * 1024 * 1024 * 1024);
            let ttl_secs = params["ttlSecs"].as_i64().unwrap_or(900);
            let token = exec
                .issue_p2p_ticket(
                    dest_dir.clone(),
                    p.id,
                    p.slug.clone(),
                    p.project_kind.clone(),
                    p.port.unwrap_or(3000),
                    max_bytes,
                    ttl_secs,
                )
                .await;
            let p2p_port = exec.p2p_port.load(Ordering::Relaxed);
            ok_resp(
                req_id,
                json!({
                    "token": token,
                    "p2pPort": p2p_port,
                    "targetDeployDir": dest_dir.to_string_lossy().to_string(),
                }),
            )
        }
        "shift.push_tar" | "shift.push_zip" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let token = params["token"].as_str().unwrap_or("").trim().to_string();
            if token.is_empty() {
                return err_resp(req_id, 400, "Missing P2P transfer token");
            }
            let target_urls: Vec<String> = params["targetUrls"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if target_urls.is_empty() {
                return err_resp(req_id, 400, "Missing targetUrls for P2P transfer");
            }

            let tmp_dir = exec.data_dir.join("tmp");
            let _ = tokio::fs::create_dir_all(&tmp_dir).await;
            let tmp_zip = tmp_dir.join(format!("shift-push-{}-{}.zip", p.id, now_secs()));
            let deploy_dir = resolve_source_deploy_dir(&exec.data_dir, &p);
            let safe_slug = sanitize_project_folder(&p.slug, p.id);
            let container_dir = exec.data_dir.join("containers").join(&safe_slug);
            let legacy_app_data_dir = exec.data_dir.join("app-data").join(&safe_slug);
            let zip_pack = tmp_zip.clone();

            let pack_res = tokio::task::spawn_blocking(move || {
                pack_project_shift_to_zip(
                    &deploy_dir,
                    Some(&container_dir),
                    Some(&legacy_app_data_dir),
                    &zip_pack,
                )
            })
            .await;

            let (compressed_bytes, raw_bytes) = match pack_res {
                Ok(Ok(sizes)) => sizes,
                Ok(Err(e)) => {
                    let _ = tokio::fs::remove_file(&tmp_zip).await;
                    return err_resp(req_id, 500, format!("Failed packing .zip archive: {e}"));
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp_zip).await;
                    return err_resp(req_id, 500, format!("Pack worker panicked: {e}"));
                }
            };

            let mut last_err = String::from("No reachable P2P endpoint");
            let mut transfer_result = None;
            for base_url in &target_urls {
                match stream_archive_to_target(base_url, &token, &tmp_zip).await {
                    Ok(extracted_bytes) => {
                        transfer_result = Some((base_url.clone(), extracted_bytes));
                        break;
                    }
                    Err(e) => {
                        last_err = format!("{base_url}: {e}");
                    }
                }
            }
            let _ = tokio::fs::remove_file(&tmp_zip).await;

            match transfer_result {
                Some((used_url, extracted_bytes)) => {
                    send_log(
                        &ws_tx,
                        Some(p.id),
                        None,
                        "shift",
                        "info",
                        &format!(
                            "Streamed {} bytes (.zip, {} raw bytes) directly to Target via {}",
                            compressed_bytes, raw_bytes, used_url
                        ),
                    );
                    ok_resp(
                        req_id,
                        json!({
                            "ok": true,
                            "bytesTransferred": compressed_bytes,
                            "rawBytes": raw_bytes,
                            "extractedBytes": extracted_bytes,
                            "usedUrl": used_url,
                        }),
                    )
                }
                None => err_resp(req_id, 502, format!("Direct P2P transfer failed: {last_err}")),
            }
        }
        "shift.warm_start" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let ctx: RemoteContextPayload =
                serde_json::from_value(params["context"].clone()).unwrap_or_default();
            let health_timeout_ms = params["healthTimeoutMs"].as_u64().unwrap_or(5000);
            let skip_port_probe = params["skipPortProbe"].as_bool().unwrap_or(false);
            let _ = tokio::fs::create_dir_all(&p.deploy_dir).await;
            let container = ensure_container_in_data_dir(&exec.data_dir, &p, p.port.unwrap_or(3000));
            apply_and_cache_context(&exec, &p, ctx.clone()).await;

            match start_project_local(&exec, &p, ctx.env, None, &ws_tx).await {
                Ok(pid) => {
                    if !skip_port_probe {
                        if let Some(port) = p.port {
                            let deadline = tokio::time::Instant::now()
                                + Duration::from_millis(health_timeout_ms.clamp(250, 30_000));
                            let mut healthy = false;
                            while tokio::time::Instant::now() < deadline {
                                if Runtime::is_port_accepting(port).await {
                                    healthy = true;
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(120)).await;
                            }
                            if !healthy {
                                stop_project_local(&exec, &p, &container).await;
                                return err_resp(
                                    req_id,
                                    502,
                                    format!(
                                        "Target warm-start health probe failed: port {port} did not accept connections"
                                    ),
                                );
                            }
                        }
                    }
                    ok_resp(req_id, json!({ "ok": true, "pid": pid, "port": p.port }))
                }
                Err(e) => {
                    stop_project_local(&exec, &p, &container).await;
                    err_resp(req_id, 502, format!("Target warm-start failed: {e}"))
                }
            }
        }
        "shift.cleanup" | "shift.cleanup_source" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let remaining: Vec<Project> =
                serde_json::from_value(params["remainingProjects"].clone()).unwrap_or_default();
            let remove_files = params["removeFiles"].as_bool().unwrap_or(true);
            stop_project_processes_for_cleanup(&exec, &p).await;
            let caddy = Caddy::new(&exec.data_dir);
            caddy.remove_project_config(&p.slug, &remaining);
            let _ = caddy.reload();

            let mut removed_dirs = 0usize;
            if remove_files {
                let data_dir_c = exec.data_dir.clone();
                let mut src_proj = p.clone();
                let resolved_src = resolve_source_deploy_dir(&data_dir_c, &src_proj);
                src_proj.deploy_dir = resolved_src.to_string_lossy().to_string();
                let rem_c = remaining.clone();
                let orig_proj = p.clone();
                let cleanup_res = tokio::task::spawn_blocking(move || {
                    let mut count =
                        cleanup_project_files_dirs_in_data_dir_checked(&data_dir_c, &src_proj, &rem_c)?;
                    if orig_proj.deploy_dir != src_proj.deploy_dir {
                        count += cleanup_project_files_dirs_in_data_dir_checked(
                            &data_dir_c,
                            &orig_proj,
                            &rem_c,
                        )?;
                    }
                    Ok::<usize, String>(count)
                })
                .await;
                match cleanup_res {
                    Ok(Ok(count)) => {
                        removed_dirs = count;
                    }
                    Ok(Err(e)) => {
                        exec.remove_workload(p.id).await;
                        exec.env_cache.write().await.remove(&p.id);
                        return err_resp(req_id, 500, format!("Source cleanup failed: {e}"));
                    }
                    Err(e) => {
                        exec.remove_workload(p.id).await;
                        exec.env_cache.write().await.remove(&p.id);
                        return err_resp(req_id, 500, format!("Source cleanup worker panicked: {e}"));
                    }
                }
            }
            exec.remove_workload(p.id).await;
            exec.env_cache.write().await.remove(&p.id);
            send_log(
                &ws_tx,
                Some(p.id),
                None,
                "shift",
                "info",
                &format!(
                    "Cleaned up old node workload & {} directory tree(s) for '{}'",
                    removed_dirs, p.slug
                ),
            );
            ok_resp(req_id, json!({ "ok": true, "removedDirs": removed_dirs }))
        }
        "caddy.stage_http" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let mut projects: Vec<Project> =
                serde_json::from_value(params["projects"].clone()).unwrap_or_default();
            if !projects.iter().any(|item| item.id == p.id) {
                projects.push(p.clone());
            }
            let caddy = Caddy::new(&exec.data_dir);
            if let Err(e) = caddy.write_staging_http_config(&p, &projects) {
                return err_resp(req_id, 500, format!("Caddy stage HTTP write failed: {e}"));
            }
            let _ = caddy.reload();
            ok_resp(req_id, json!({ "ok": true, "mode": "http-staging" }))
        }
        "caddy.enable_https" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let mut projects: Vec<Project> =
                serde_json::from_value(params["projects"].clone()).unwrap_or_default();
            if !projects.iter().any(|item| item.id == p.id) {
                projects.push(p.clone());
            }
            let caddy = Caddy::new(&exec.data_dir);
            if let Err(e) = caddy.write_config(&p, &projects) {
                return err_resp(req_id, 500, format!("Caddy enable HTTPS write failed: {e}"));
            }
            if let Err(e) = caddy.reload() {
                return err_resp(req_id, 500, format!("Caddy reload failed: {e}"));
            }
            ok_resp(req_id, json!({ "ok": true, "mode": "auto-https" }))
        }
        "node.self_update" => {
            let repo_slug = params["repoSlug"]
                .as_str()
                .unwrap_or("abhishek737/web-dep")
                .trim()
                .to_string();
            let tag = params["tag"].as_str().map(|s| s.to_string());
            let token = params["githubToken"].as_str().map(|s| s.to_string());
            let restart = params["restart"].as_bool().unwrap_or(true);

            let res = tokio::task::spawn_blocking(move || {
                crate::updater::perform_binary_swap_sync(
                    &repo_slug,
                    tag.as_deref(),
                    token.as_deref(),
                    false,
                    None,
                )
            })
            .await;

            match res {
                Ok(Ok(new_tag)) => {
                    if restart {
                        crate::updater::restart_node_service_delayed(800);
                    }
                    ok_resp(
                        req_id,
                        json!({ "ok": true, "tag": new_tag, "restarting": restart }),
                    )
                }
                Ok(Err(e)) => err_resp(req_id, 500, e),
                Err(e) => err_resp(req_id, 500, format!("Self-update worker failed: {e}")),
            }
        }
        "project.fetch_backup_tar" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let backup_path = params["backupPath"].as_str().unwrap_or("").trim().to_string();
            let b_dir = if backup_path.is_empty() {
                PathBuf::from(&p.deploy_dir)
            } else {
                PathBuf::from(&backup_path)
            };
            if !b_dir.exists() {
                return err_resp(req_id, 404, "Backup snapshot path does not exist on node");
            }
            let tmp_tgz = exec
                .data_dir
                .join("tmp")
                .join(format!("backup-fetch-{}-{}.tar.gz", p.id, now_secs()));
            let tgz_c = tmp_tgz.clone();
            let res = tokio::task::spawn_blocking(move || pack_dir_to_tar_gz(&b_dir, &tgz_c, &[])).await;
            let (comp, raw) = match res {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return err_resp(req_id, 500, e),
                Err(e) => return err_resp(req_id, 500, e.to_string()),
            };
            let bytes = match tokio::fs::read(&tmp_tgz).await {
                Ok(b) => b,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp_tgz).await;
                    return err_resp(req_id, 500, e.to_string());
                }
            };
            let _ = tokio::fs::remove_file(&tmp_tgz).await;
            if bytes.len() > 32 * 1024 * 1024 {
                return err_resp(
                    req_id,
                    413,
                    "Off-node RPC snapshot payload exceeds 32 MiB; configure P2P upload streaming",
                );
            }
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            ok_resp(
                req_id,
                json!({
                    "ok": true,
                    "size": comp,
                    "rawSize": raw,
                    "tarGzBase64": b64,
                }),
            )
        }
        "project.restore_backup_tar" => {
            let Ok(p) = serde_json::from_value::<Project>(params["project"].clone()) else {
                return err_resp(req_id, 400, "Invalid project payload");
            };
            let b64 = params["tarGzBase64"].as_str().unwrap_or("");
            if b64.is_empty() {
                return err_resp(req_id, 400, "Missing tarGzBase64 archive payload");
            }
            let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => return err_resp(req_id, 400, format!("Invalid base64 archive: {e}")),
            };
            let tmp_dir = exec
                .data_dir
                .join("tmp")
                .join(format!("restore-unpack-{}-{}", p.id, now_secs()));
            let tmp_tgz = tmp_dir.with_extension("tar.gz");
            if let Some(parent) = tmp_tgz.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Err(e) = tokio::fs::write(&tmp_tgz, &bytes).await {
                return err_resp(req_id, 500, format!("Failed writing restore archive: {e}"));
            }
            let tgz_c = tmp_tgz.clone();
            let dir_c = tmp_dir.clone();
            let unpack_res =
                tokio::task::spawn_blocking(move || unpack_archive_auto_safe(&tgz_c, &dir_c)).await;
            let _ = tokio::fs::remove_file(&tmp_tgz).await;
            if let Ok(Err(e)) = unpack_res {
                let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
                return err_resp(req_id, 400, e);
            } else if let Err(e) = unpack_res {
                let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
                return err_resp(req_id, 500, e.to_string());
            }

            let raw_env = tokio::fs::read_to_string(tmp_dir.join("backup.env"))
                .await
                .unwrap_or_default();
            let deploy_dir = PathBuf::from(&p.deploy_dir);
            let target_dir = if p.project_kind == "service" {
                deploy_dir.join("app")
            } else {
                deploy_dir.join("current")
            };
            let _ = tokio::fs::create_dir_all(&target_dir).await;
            let src_files = if p.project_kind == "service" && tmp_dir.join("files/app").exists() {
                tmp_dir.join("files/app")
            } else if tmp_dir.join("files").exists() {
                tmp_dir.join("files")
            } else if p.project_kind == "service" && tmp_dir.join("app").exists() {
                tmp_dir.join("app")
            } else if tmp_dir.join("current").exists() {
                tmp_dir.join("current")
            } else {
                tmp_dir.clone()
            };
            let tgt_c = target_dir.clone();
            let _ = tokio::task::spawn_blocking(move || copy_backup_files(&src_files, &tgt_c)).await;
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

            ok_resp(
                req_id,
                json!({
                    "ok": true,
                    "rawEnv": raw_env,
                    "targetDeployDir": deploy_dir.to_string_lossy().to_string(),
                }),
            )
        }
        "modules.status" => {
            let p2p_port = exec.p2p_port.load(Ordering::Relaxed);
            let port_opt = if p2p_port > 0 { Some(p2p_port) } else { None };
            let caps = tokio::task::spawn_blocking(move || detect_capabilities_with_p2p(port_opt))
                .await
                .map_err(|e| e.to_string());
            match caps {
                Ok(c) => ok_resp(
                    req_id,
                    json!({
                        "ok": true,
                        "packageManager": c.package_manager,
                        "modules": c.modules,
                        "capabilities": c,
                    }),
                ),
                Err(e) => err_resp(req_id, 500, format!("Module detection failed: {e}")),
            }
        }
        "modules.install" => {
            let mut module_ids: Vec<String> = params["moduleIds"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_ascii_lowercase()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if module_ids.is_empty() {
                if let Some(single) = params["moduleId"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    module_ids.push(single.to_ascii_lowercase());
                }
            }
            if module_ids.is_empty() {
                return err_resp(req_id, 400, "Missing moduleIds for installation");
            }
            let p2p_port = exec.p2p_port.load(Ordering::Relaxed);
            let port_opt = if p2p_port > 0 { Some(p2p_port) } else { None };
            let ids_log = module_ids.join(", ");
            send_log(
                &ws_tx,
                None,
                None,
                "system",
                "info",
                &format!("Installing VPS modules [{ids_log}]..."),
            );
            let res = tokio::task::spawn_blocking(move || {
                let (results, _snap) = crate::cluster::modules::install_node_modules_sync(&module_ids);
                let caps = detect_capabilities_with_p2p(port_opt);
                (results, caps)
            })
            .await;
            match res {
                Ok((results, caps)) => {
                    let all_ok = results.iter().all(|r| r.success);
                    ok_resp(
                        req_id,
                        json!({
                            "ok": all_ok,
                            "results": results,
                            "packageManager": caps.package_manager,
                            "modules": caps.modules,
                            "capabilities": caps,
                        }),
                    )
                }
                Err(e) => err_resp(req_id, 500, format!("Module install worker panicked: {e}")),
            }
        }
        other => err_resp(req_id, 404, format!("Unknown remote RPC op '{other}'")),
    }
}

fn parse_project_root(params: &Value) -> Result<(Project, PathBuf), (u16, String)> {
    let p: Project = serde_json::from_value(params["project"].clone())
        .map_err(|_| (400, "Invalid project payload".to_string()))?;
    let root = service_root(&p).map_err(|(c, m)| (c.as_u16(), m))?;
    Ok((p, root))
}

async fn handle_files_list(req_id: String, params: Value) -> ClusterWsMessage {
    let (_, root) = match parse_project_root(&params) {
        Ok(v) => v,
        Err((s, m)) => return err_resp(req_id, s, m),
    };
    let rel_path = params["path"].as_str().unwrap_or("");
    match crate::api::files::list_entries_in_root(&root, rel_path).await {
        Ok(axum::Json(v)) => ok_resp(req_id, v),
        Err((c, m)) => err_resp(req_id, c.as_u16(), m),
    }
}

async fn handle_files_read(req_id: String, params: Value) -> ClusterWsMessage {
    let (_, root) = match parse_project_root(&params) {
        Ok(v) => v,
        Err((s, m)) => return err_resp(req_id, s, m),
    };
    let rel_path = params["path"].as_str().unwrap_or("");
    let file = match safe_join(&root, rel_path) {
        Ok(f) => f,
        Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
    };
    if let Err((c, m)) = ensure_existing_within_root(&root, &file).await {
        return err_resp(req_id, c.as_u16(), m);
    }
    let meta = match tokio::fs::metadata(&file).await {
        Ok(m) => m,
        Err(e) => return err_resp(req_id, 404, e.to_string()),
    };
    if !meta.is_file() {
        return err_resp(req_id, 400, "Not a file");
    }
    if meta.len() > 2 * 1024 * 1024 {
        return err_resp(req_id, 400, "File is larger than 2 MB and cannot be edited here");
    }
    let bytes = match tokio::fs::read(&file).await {
        Ok(b) => b,
        Err(e) => return err_resp(req_id, 500, e.to_string()),
    };
    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return err_resp(req_id, 400, "Binary file cannot be displayed"),
    };
    ok_resp(req_id, json!({ "path": rel_path, "content": content }))
}

async fn handle_files_write(req_id: String, params: Value) -> ClusterWsMessage {
    let (_, root) = match parse_project_root(&params) {
        Ok(v) => v,
        Err((s, m)) => return err_resp(req_id, s, m),
    };
    let rel_path = params["path"].as_str().unwrap_or("");
    let content = params["content"].as_str().unwrap_or("");
    let file = match safe_join(&root, rel_path) {
        Ok(f) => f,
        Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
    };
    if file == root {
        return err_resp(req_id, 400, "Invalid file path");
    }
    if let Err((c, m)) = ensure_ancestor_within_root(&root, &file).await {
        return err_resp(req_id, c.as_u16(), m);
    }
    if let Some(parent) = file.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match tokio::fs::write(&file, content.as_bytes()).await {
        Ok(()) => ok_resp(req_id, json!({ "ok": true })),
        Err(e) => err_resp(req_id, 500, e.to_string()),
    }
}

async fn handle_files_mkdir(req_id: String, params: Value) -> ClusterWsMessage {
    let (_, root) = match parse_project_root(&params) {
        Ok(v) => v,
        Err((s, m)) => return err_resp(req_id, s, m),
    };
    let rel_path = params["path"].as_str().unwrap_or("");
    let dir = match safe_join(&root, rel_path) {
        Ok(d) => d,
        Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
    };
    if let Err((c, m)) = ensure_ancestor_within_root(&root, &dir).await {
        return err_resp(req_id, c.as_u16(), m);
    }
    match tokio::fs::create_dir_all(&dir).await {
        Ok(()) => ok_resp(req_id, json!({ "ok": true })),
        Err(e) => err_resp(req_id, 500, e.to_string()),
    }
}

async fn handle_files_delete(req_id: String, params: Value) -> ClusterWsMessage {
    let (_, root) = match parse_project_root(&params) {
        Ok(v) => v,
        Err((s, m)) => return err_resp(req_id, s, m),
    };
    let rel_path = params["path"].as_str().unwrap_or("");
    let target = match safe_join(&root, rel_path) {
        Ok(t) => t,
        Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
    };
    if target == root {
        return err_resp(req_id, 400, "Cannot delete the workspace root");
    }
    if let Some(parent) = target.parent() {
        if let Err((c, m)) = ensure_existing_within_root(&root, parent).await {
            return err_resp(req_id, c.as_u16(), m);
        }
    }
    let meta = match tokio::fs::symlink_metadata(&target).await {
        Ok(m) => m,
        Err(e) => return err_resp(req_id, 404, e.to_string()),
    };
    let res = if meta.file_type().is_symlink() {
        tokio::fs::remove_file(&target).await
    } else if meta.is_dir() {
        if let Err((c, m)) = ensure_existing_within_root(&root, &target).await {
            return err_resp(req_id, c.as_u16(), m);
        }
        tokio::fs::remove_dir_all(&target).await
    } else {
        if let Err((c, m)) = ensure_existing_within_root(&root, &target).await {
            return err_resp(req_id, c.as_u16(), m);
        }
        tokio::fs::remove_file(&target).await
    };
    match res {
        Ok(()) => ok_resp(req_id, json!({ "ok": true })),
        Err(e) => err_resp(req_id, 500, e.to_string()),
    }
}

async fn handle_files_rename(req_id: String, params: Value) -> ClusterWsMessage {
    let (_, root) = match parse_project_root(&params) {
        Ok(v) => v,
        Err((s, m)) => return err_resp(req_id, s, m),
    };
    let old_path = params["oldPath"].as_str().unwrap_or("");
    let new_path = params["newPath"].as_str().unwrap_or("");
    let old_target = match safe_join(&root, old_path) {
        Ok(t) => t,
        Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
    };
    let new_target = match safe_join(&root, new_path) {
        Ok(t) => t,
        Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
    };
    if old_target == root || new_target == root {
        return err_resp(req_id, 400, "Invalid path");
    }
    if let Some(parent) = old_target.parent() {
        if let Err((c, m)) = ensure_existing_within_root(&root, parent).await {
            return err_resp(req_id, c.as_u16(), m);
        }
    }
    if let Err((c, m)) = ensure_ancestor_within_root(&root, &new_target).await {
        return err_resp(req_id, c.as_u16(), m);
    }
    if let Some(parent) = new_target.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match tokio::fs::rename(&old_target, &new_target).await {
        Ok(()) => ok_resp(req_id, json!({ "ok": true })),
        Err(e) => err_resp(req_id, 500, e.to_string()),
    }
}

async fn handle_files_download(req_id: String, params: Value) -> ClusterWsMessage {
    let (_, root) = match parse_project_root(&params) {
        Ok(v) => v,
        Err((s, m)) => return err_resp(req_id, s, m),
    };
    let rel_path = params["path"].as_str().unwrap_or("");
    let file = match safe_join(&root, rel_path) {
        Ok(f) => f,
        Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
    };
    if let Err((c, m)) = ensure_existing_within_root(&root, &file).await {
        return err_resp(req_id, c.as_u16(), m);
    }
    let meta = match tokio::fs::metadata(&file).await {
        Ok(m) => m,
        Err(e) => return err_resp(req_id, 404, e.to_string()),
    };
    if !meta.is_file() {
        return err_resp(req_id, 400, "Not a file");
    }
    let file_name = Path::new(rel_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download")
        .to_string();
    let bytes = match tokio::fs::read(&file).await {
        Ok(b) => b,
        Err(e) => return err_resp(req_id, 500, e.to_string()),
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    ok_resp(
        req_id,
        json!({
            "fileName": file_name,
            "contentBase64": encoded,
        }),
    )
}

async fn handle_files_upload(req_id: String, params: Value) -> ClusterWsMessage {
    let (_, root) = match parse_project_root(&params) {
        Ok(v) => v,
        Err((s, m)) => return err_resp(req_id, s, m),
    };
    let rel_path = params["path"].as_str().unwrap_or("");
    let target_dir = match safe_join(&root, rel_path) {
        Ok(d) => d,
        Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
    };
    if let Err((c, m)) = ensure_ancestor_within_root(&root, &target_dir).await {
        return err_resp(req_id, c.as_u16(), m);
    }
    let _ = tokio::fs::create_dir_all(&target_dir).await;
    let mut uploaded = Vec::new();
    if let Some(files) = params["files"].as_array() {
        for item in files {
            let Some(name) = item["name"].as_str() else {
                continue;
            };
            let normalized = name.replace('\\', "/");
            let target_file = match safe_join(&target_dir, &normalized) {
                Ok(tf) => tf,
                Err((c, m)) => return err_resp(req_id, c.as_u16(), m),
            };
            if let Err((c, m)) = ensure_ancestor_within_root(&root, &target_file).await {
                return err_resp(req_id, c.as_u16(), m);
            }
            if let Some(parent) = target_file.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let b64 = item["contentBase64"].as_str().unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => return err_resp(req_id, 400, format!("Invalid base64 upload: {e}")),
            };
            if let Err(e) = tokio::fs::write(&target_file, &bytes).await {
                return err_resp(req_id, 500, e.to_string());
            }
            uploaded.push(normalized);
        }
    }
    ok_resp(req_id, json!({ "ok": true, "uploaded": uploaded }))
}

pub fn sanitize_project_folder(slug: &str, project_id: i64) -> String {
    let clean_slug: String = slug
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if clean_slug.is_empty() {
        format!("project-{}", project_id)
    } else {
        clean_slug
    }
}

pub fn resolve_source_deploy_dir(data_dir: &Path, p: &Project) -> PathBuf {
    let trimmed = p.deploy_dir.trim();
    let raw = PathBuf::from(trimmed);
    if !trimmed.is_empty() && raw.exists() {
        return raw;
    }
    let norm = trimmed.replace('\\', "/");
    let stripped = norm
        .strip_prefix("./data/")
        .or_else(|| norm.strip_prefix("data/"))
        .map(PathBuf::from);
    if let Some(rel) = stripped {
        let candidate = data_dir.join(rel);
        if candidate.exists() {
            return candidate;
        }
    }
    let folder = sanitize_project_folder(&p.slug, p.id);
    let kind_dir = if p.project_kind == "service" {
        "services"
    } else {
        "sites"
    };
    let fallback = data_dir.join(kind_dir).join(folder);
    if fallback.exists() || trimmed.is_empty() {
        fallback
    } else {
        raw
    }
}

pub fn resolve_target_deploy_dir(data_dir: &Path, p: &Project) -> PathBuf {
    let folder = sanitize_project_folder(&p.slug, p.id);
    if p.project_kind == "service" {
        data_dir.join("services").join(folder)
    } else {
        data_dir.join("sites").join(folder)
    }
}

fn remove_path_any(path: &Path) -> Result<(), String> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        if std::fs::remove_file(path).is_ok() {
            return Ok(());
        }
        if std::fs::remove_dir(path).is_ok() {
            return Ok(());
        }
    }
    if meta.is_dir() {
        #[cfg(windows)]
        {
            // Also remove Windows junctions cleanly without following targets
            if std::fs::remove_dir(path).is_ok() {
                return Ok(());
            }
        }
        std::fs::remove_dir_all(path)
            .map_err(|e| format!("Failed removing '{}': {e}", path.display()))
    } else {
        std::fs::remove_file(path)
            .map_err(|e| format!("Failed removing '{}': {e}", path.display()))
    }
}

fn copy_path_preserving_symlinks(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(src)
        .map_err(|e| format!("Failed reading '{}': {e}", src.display()))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)
            .map_err(|e| format!("Failed reading symlink '{}': {e}", src.display()))?;
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, dst)
                .map_err(|e| format!("Failed creating symlink '{}': {e}", dst.display()))?;
        }
        #[cfg(windows)]
        {
            if src.is_dir() {
                std::os::windows::fs::symlink_dir(&target, dst)
                    .map_err(|e| format!("Failed creating dir symlink '{}': {e}", dst.display()))?;
            } else {
                std::os::windows::fs::symlink_file(&target, dst)
                    .map_err(|e| format!("Failed creating file symlink '{}': {e}", dst.display()))?;
            }
        }
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dst)
            .map_err(|e| format!("Failed creating dir '{}': {e}", dst.display()))?;
        for entry in std::fs::read_dir(src)
            .map_err(|e| format!("Failed reading dir '{}': {e}", src.display()))?
            .flatten()
        {
            copy_path_preserving_symlinks(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::copy(src, dst)
        .map_err(|e| format!("Failed copying '{}': {e}", src.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.permissions().mode()));
    }
    Ok(())
}

fn replace_path_from_stage(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating parent '{}': {e}", parent.display()))?;
    }
    remove_path_any(dst)?;
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_path_preserving_symlinks(src, dst)?;
            let _ = remove_path_any(src);
            Ok(())
        }
    }
}

fn install_shift_unpacked_tree(
    data_dir: &Path,
    dest_dir: &Path,
    unpacked_dir: &Path,
    project_slug: &str,
    project_kind: &str,
    project_id: i64,
) -> Result<(), String> {
    let safe_slug = sanitize_project_folder(project_slug, project_id);
    let container_stage = unpacked_dir.join("__webdep_container__");
    if container_stage.is_dir() {
        let target_container_dir = data_dir.join("containers").join(&safe_slug);
        std::fs::create_dir_all(&target_container_dir)
            .map_err(|e| format!("Failed creating container dir: {e}"))?;
        for entry in std::fs::read_dir(&container_stage)
            .map_err(|e| format!("Failed listing unpacked container dir: {e}"))?
            .flatten()
        {
            let dst = target_container_dir.join(entry.file_name());
            replace_path_from_stage(&entry.path(), &dst)?;
        }
        let _ = std::fs::remove_dir_all(&container_stage);
    }

    // Legacy compatibility: older archives may carry containers/<slug> under unpacked root
    let legacy_container_stage = unpacked_dir.join("containers").join(&safe_slug);
    if legacy_container_stage.is_dir() {
        let target_container_dir = data_dir.join("containers").join(&safe_slug);
        std::fs::create_dir_all(&target_container_dir)
            .map_err(|e| format!("Failed creating legacy container dir: {e}"))?;
        for entry in std::fs::read_dir(&legacy_container_stage)
            .map_err(|e| format!("Failed listing legacy container dir: {e}"))?
            .flatten()
        {
            let dst = target_container_dir.join(entry.file_name());
            replace_path_from_stage(&entry.path(), &dst)?;
        }
        let _ = std::fs::remove_dir_all(&legacy_container_stage);
        if let Ok(mut rem) = std::fs::read_dir(unpacked_dir.join("containers")) {
            if rem.next().is_none() {
                let _ = std::fs::remove_dir(unpacked_dir.join("containers"));
            }
        }
    }

    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed creating target deploy_dir '{}': {e}", dest_dir.display()))?;
    for entry in std::fs::read_dir(unpacked_dir)
        .map_err(|e| format!("Failed listing unpacked root: {e}"))?
        .flatten()
    {
        let dst = dest_dir.join(entry.file_name());
        replace_path_from_stage(&entry.path(), &dst)?;
    }

    if project_kind != "service" {
        let target_db_dir = data_dir.join("containers").join(&safe_slug).join("db");
        let current_dir = dest_dir.join("current");
        let app_dir = dest_dir.join("app");
        if current_dir.is_dir() {
            link_work_dir_to_db_dir(&current_dir, &target_db_dir);
        } else if app_dir.is_dir() {
            link_work_dir_to_db_dir(&app_dir, &target_db_dir);
        }
    }
    Ok(())
}

pub fn allocate_free_port_on_node(preferred: Option<u16>, used_ports: &[u16]) -> Result<u16, String> {
    let blocked: HashSet<u16> = used_ports.iter().copied().collect();
    if let Some(pref) = preferred {
        if pref >= 1024 && !blocked.contains(&pref) {
            if std::net::TcpListener::bind(("127.0.0.1", pref)).is_ok() {
                return Ok(pref);
            }
        }
    }
    for port in 43000u16..=44999u16 {
        if blocked.contains(&port) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) {
        if let Ok(addr) = listener.local_addr() {
            let port = addr.port();
            if !blocked.contains(&port) {
                return Ok(port);
            }
        }
    }
    Err("No free TCP port available on target Web-Node".to_string())
}

pub async fn start_p2p_server(
    exec: Arc<NodeExecState>,
    bind_host: &str,
    preferred_port: u16,
) -> Result<(u16, tokio::task::JoinHandle<()>), String> {
    let addr: SocketAddr = format!("{bind_host}:{preferred_port}")
        .parse()
        .map_err(|e| format!("Invalid P2P bind address: {e}"))?;
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(_) => {
            let fallback: SocketAddr = format!("{bind_host}:0")
                .parse()
                .map_err(|e| format!("Invalid fallback P2P bind address: {e}"))?;
            TcpListener::bind(fallback)
                .await
                .map_err(|e| format!("Failed binding P2P listener: {e}"))?
        }
    };
    let bound_port = listener
        .local_addr()
        .map_err(|e| format!("Failed reading P2P local_addr: {e}"))?
        .port();
    exec.p2p_port.store(bound_port, Ordering::Relaxed);

    let router = Router::new()
        .route("/p2p/health", get(|| async { "ok" }))
        .route(
            "/p2p/shift/:token",
            put(p2p_receive_shift_tar).post(p2p_receive_shift_tar),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(exec);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            eprintln!("Web-Node P2P HTTP receiver exited: {e}");
        }
    });

    Ok((bound_port, handle))
}

async fn p2p_receive_shift_tar(
    State(exec): State<Arc<NodeExecState>>,
    AxPath(token): AxPath<String>,
    body: Body,
) -> Result<Json<Value>, (StatusCode, String)> {
    let token_clean = token.trim().to_string();
    let ticket = exec
        .peek_p2p_ticket(&token_clean)
        .await
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Invalid or expired P2P shift token".to_string()))?;

    let tmp_dir = exec.data_dir.join("tmp");
    let _ = tokio::fs::create_dir_all(&tmp_dir).await;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_archive = tmp_dir.join(format!("recv-{stamp}.archive"));
    let unpack_stage = tmp_dir.join(format!("unpack-{stamp}"));
    let mut file = tokio::fs::File::create(&tmp_archive)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed creating staging archive: {e}")))?;

    let mut received_bytes = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.map_err(|e| {
            let _ = std::fs::remove_file(&tmp_archive);
            (StatusCode::BAD_REQUEST, format!("Stream read failed: {e}"))
        })?;
        received_bytes = received_bytes.saturating_add(chunk.len() as u64);
        if received_bytes > ticket.max_bytes {
            let _ = tokio::fs::remove_file(&tmp_archive).await;
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "Transfer exceeded ticket limit".to_string()));
        }
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&tmp_archive).await;
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Disk write failed: {e}")));
        }
    }
    let _ = file.flush().await;
    drop(file);

    let archive_c = tmp_archive.clone();
    let stage_c = unpack_stage.clone();
    let data_dir_c = exec.data_dir.clone();
    let ticket_c = ticket.clone();
    let extracted_res = tokio::task::spawn_blocking(move || {
        let extracted = unpack_archive_auto_safe(&archive_c, &stage_c)?;
        install_shift_unpacked_tree(
            &data_dir_c,
            &ticket_c.dest_dir,
            &stage_c,
            &ticket_c.project_slug,
            &ticket_c.project_kind,
            ticket_c.project_id,
        )?;
        Ok::<u64, String>(extracted)
    })
    .await
    .map_err(|e| {
        let _ = std::fs::remove_file(&tmp_archive);
        let _ = std::fs::remove_dir_all(&unpack_stage);
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Unpack worker failed: {e}"))
    })?;

    let _ = tokio::fs::remove_file(&tmp_archive).await;
    let _ = tokio::fs::remove_dir_all(&unpack_stage).await;

    let extracted_bytes = extracted_res.map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    exec.remove_p2p_ticket(&token_clean).await;
    Ok(Json(json!({
        "ok": true,
        "bytesTransferred": received_bytes,
        "extractedBytes": extracted_bytes,
        "destDir": ticket.dest_dir.to_string_lossy().to_string(),
    })))
}

pub async fn stream_archive_to_target(
    base_url: &str,
    token: &str,
    archive_path: &Path,
) -> Result<u64, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let host_port = trimmed
        .strip_prefix("http://")
        .ok_or_else(|| format!("Unsupported P2P URL scheme in '{base_url}' (expected http://)"))?;
    let (host_hdr, path_prefix) = match host_port.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (host_port, String::new()),
    };

    let meta = tokio::fs::metadata(archive_path)
        .await
        .map_err(|e| format!("Failed reading staged archive: {e}"))?;
    let total_len = meta.len();

    let mut stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(host_hdr))
        .await
        .map_err(|_| format!("Timed out connecting to P2P peer {host_hdr}"))?
        .map_err(|e| format!("TCP connect to {host_hdr} failed: {e}"))?;

    let content_type = if archive_path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("zip"))
        .unwrap_or(false)
    {
        "application/zip"
    } else {
        "application/gzip"
    };
    let req_path = format!("{path_prefix}/p2p/shift/{token}");
    let headers = format!(
        "PUT {req_path} HTTP/1.1\r\nHost: {host_hdr}\r\nContent-Type: {content_type}\r\nContent-Length: {total_len}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(|e| format!("Failed writing P2P HTTP request headers: {e}"))?;

    let mut file = tokio::fs::File::open(archive_path)
        .await
        .map_err(|e| format!("Failed opening archive for streaming: {e}"))?;
    tokio::io::copy(&mut file, &mut stream)
        .await
        .map_err(|e| format!("Failed streaming archive payload: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("Failed flushing P2P stream: {e}"))?;

    let mut reader = TokioBufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(|e| format!("Failed reading P2P HTTP status: {e}"))?;
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("Failed reading P2P HTTP response headers: {e}"))?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut body_bytes = Vec::new();
    let _ = reader.read_to_end(&mut body_bytes).await;
    let body_text = String::from_utf8_lossy(&body_bytes).to_string();
    if !(200..300).contains(&status_code) {
        return Err(format!("Peer returned HTTP {status_code}: {body_text}"));
    }
    let parsed: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
    Ok(parsed["extractedBytes"].as_u64().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::models::{NodeStateManifest, NodeWorkloadPlacement};
    use crate::projects::models::*;

    fn temp_dir(prefix: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&p);
        p
    }

    fn dummy_project(id: i64, slug: &str, deploy_dir: &Path) -> Project {
        Project {
            id,
            name: slug.to_string(),
            slug: slug.to_string(),
            repo_url: String::new(),
            branch: "main".to_string(),
            project_type: "node".to_string(),
            deploy_method: "managed".to_string(),
            deploy_dir: deploy_dir.to_string_lossy().to_string(),
            port: Some(4123),
            install_cmd: String::new(),
            build_cmd: String::new(),
            start_cmd: String::new(),
            health_url: String::new(),
            health_timeout_s: 30,
            stability_s: 0,
            auto_deploy: false,
            webhook_secret: None,
            current_release_id: None,
            deployed_sha: None,
            last_poll_at: None,
            app_status: "stopped".to_string(),
            app_pid: None,
            app_started_at: None,
            domains: Vec::new(),
            ssl_email: String::new(),
            created_at: 0,
            updated_at: 0,
            project_kind: "web".to_string(),
            access_mode: "local".to_string(),
            allowed_ips: String::new(),
            node_id: Some(2),
            placement_version: 1,
            assigned_port: Some(4123),
        }
    }

    fn test_manifest(workloads: Vec<NodeWorkloadPlacement>) -> NodeStateManifest {
        NodeStateManifest {
            node_id: 2,
            name: "worker-2".to_string(),
            region: "eu".to_string(),
            core_url: "http://127.0.0.1:8080".to_string(),
            core_ws_url: "ws://127.0.0.1:8080/api/cluster/node-ws".to_string(),
            mesh_ip: "10.200.0.2".to_string(),
            public_ip: "127.0.0.1".to_string(),
            wg_private_key: "priv".to_string(),
            wg_pubkey: "pub".to_string(),
            wg_listen_port: 51820,
            node_token: "wdnode_test".to_string(),
            peers: Vec::new(),
            workloads,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn reconcile_placements_stops_stale_and_unassigned_workloads() {
        let data_dir = temp_dir("webdep-reconcile");
        let manifest = test_manifest(vec![
            NodeWorkloadPlacement {
                project_id: 101,
                slug: "keep-app".to_string(),
                placement_version: 3,
                port: Some(43001),
                app_status: "stopped".to_string(),
                pid: None,
                disk_bytes: 0,
                deploy_dir: data_dir.join("sites/keep-app").to_string_lossy().to_string(),
            },
            NodeWorkloadPlacement {
                project_id: 102,
                slug: "stale-app".to_string(),
                placement_version: 1,
                port: Some(43002),
                app_status: "running".to_string(),
                pid: None,
                disk_bytes: 0,
                deploy_dir: data_dir.join("sites/stale-app").to_string_lossy().to_string(),
            },
            NodeWorkloadPlacement {
                project_id: 103,
                slug: "moved-away".to_string(),
                placement_version: 2,
                port: Some(43003),
                app_status: "running".to_string(),
                pid: None,
                disk_bytes: 0,
                deploy_dir: data_dir.join("sites/moved-away").to_string_lossy().to_string(),
            },
        ]);

        let exec = Arc::new(NodeExecState::new(data_dir.clone(), manifest));
        let mut core_versions = HashMap::new();
        core_versions.insert(101, 3); // current -> keep
        core_versions.insert(102, 2); // Core moved to v2 -> local v1 is stale -> stop & prune!
        // 103 absent from Core -> moved off node -> stop & prune!

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stopped = reconcile_placements(&exec, &core_versions, Some(&tx)).await;
        assert_eq!(stopped.len(), 2);
        assert!(stopped.contains(&102));
        assert!(stopped.contains(&103));
        assert!(!stopped.contains(&101));

        let remaining = &exec.manifest.read().await.workloads;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].project_id, 101);

        let mut log_count = 0;
        while let Ok(msg) = rx.try_recv() {
            if let ClusterWsMessage::LogChunk { project_id, .. } = msg {
                assert!(project_id == Some(102) || project_id == Some(103));
                log_count += 1;
            }
        }
        assert_eq!(log_count, 2);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn execute_rpc_files_env_and_deploy_key_sync_without_postgres() {
        let data_dir = temp_dir("webdep-node-exec");
        let site_dir = data_dir.join("sites/remote-demo");
        let current_dir = site_dir.join("current");
        std::fs::create_dir_all(&current_dir).unwrap();
        let proj = dummy_project(42, "remote-demo", &site_dir);
        let exec = Arc::new(NodeExecState::new(data_dir.clone(), test_manifest(Vec::new())));
        let (tx, _rx) = mpsc::unbounded_channel();

        // 1. env.sync + deploy_key_sync writes keypair + cached env without PG
        let sync_res = execute_rpc(
            exec.clone(),
            "r1".to_string(),
            "env.sync".to_string(),
            json!({
                "project": proj,
                "context": {
                    "env": { "SECRET_FOO": "bar123" },
                    "deployKeyPublic": "ssh-ed25519 AAAA_TEST_KEY webdep",
                    "deployKeyPrivate": "PRIVATE-KEY-CONTENT"
                }
            }),
            tx.clone(),
        )
        .await;
        assert!(matches!(sync_res, ClusterWsMessage::RpcResponse { ok: true, .. }));
        assert_eq!(
            std::fs::read_to_string(data_dir.join("keys/deploy_key_42")).unwrap(),
            "PRIVATE-KEY-CONTENT"
        );
        let dotenv_on_disk = std::fs::read_to_string(current_dir.join(".env")).unwrap();
        assert!(dotenv_on_disk.contains("SECRET_FOO=bar123"));

        // 2. files.write + files.read + files.list + files.delete
        let w = execute_rpc(
            exec.clone(),
            "r2".to_string(),
            "files.write".to_string(),
            json!({ "project": proj, "path": "hello.txt", "content": "world" }),
            tx.clone(),
        )
        .await;
        assert!(matches!(w, ClusterWsMessage::RpcResponse { ok: true, .. }));

        let r = execute_rpc(
            exec.clone(),
            "r3".to_string(),
            "files.read".to_string(),
            json!({ "project": proj, "path": "hello.txt" }),
            tx.clone(),
        )
        .await;
        match r {
            ClusterWsMessage::RpcResponse { ok: true, result, .. } => {
                assert_eq!(result["content"].as_str(), Some("world"));
            }
            other => panic!("unexpected rpc response: {other:?}"),
        }

        // Reject traversal escape
        let esc = execute_rpc(
            exec.clone(),
            "r4".to_string(),
            "files.read".to_string(),
            json!({ "project": proj, "path": "../../etc/passwd" }),
            tx,
        )
        .await;
        assert!(matches!(esc, ClusterWsMessage::RpcResponse { ok: false, status: 400, .. }));

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn p2p_shift_streams_zip_with_node_modules_and_container_data_between_nodes() {
        let src_data = temp_dir("webdep-p2p-src");
        let dst_data = temp_dir("webdep-p2p-dst");
        let src_site = src_data.join("sites/p2p-app");
        std::fs::create_dir_all(src_site.join("current/node_modules/pkg")).unwrap();
        std::fs::create_dir_all(src_site.join("backups/backup-1")).unwrap();
        std::fs::create_dir_all(src_site.join("repo/.git")).unwrap();
        std::fs::create_dir_all(src_data.join("containers/p2p-app/db")).unwrap();
        std::fs::create_dir_all(src_data.join("containers/p2p-app/storage")).unwrap();
        std::fs::create_dir_all(src_data.join("caddy")).unwrap();
        std::fs::create_dir_all(src_data.join("keys")).unwrap();
        std::fs::write(src_site.join("current/app.js"), b"console.log('shifted');").unwrap();
        std::fs::write(src_site.join("current/node_modules/pkg/index.js"), b"module.exports = 42;").unwrap();
        std::fs::write(src_site.join("backups/backup-1/note.txt"), b"snap").unwrap();
        std::fs::write(src_site.join("repo/.git/pack.idx"), b"readonly-git-idx").unwrap();
        if let Ok(mut perms) = std::fs::metadata(src_site.join("repo/.git/pack.idx")).map(|m| m.permissions()) {
            perms.set_readonly(true);
            let _ = std::fs::set_permissions(src_site.join("repo/.git/pack.idx"), perms);
        }
        std::fs::write(src_data.join("containers/p2p-app/db/app.db"), b"SQLite-data").unwrap();
        std::fs::write(src_data.join("containers/p2p-app/storage/avatar.png"), b"PNGDATA").unwrap();
        std::fs::write(src_data.join("caddy/p2p-app.caddy"), b":80 {}").unwrap();
        std::fs::write(src_data.join("keys/deploy_key_99"), b"priv").unwrap();

        let src_exec = Arc::new(NodeExecState::new(src_data.clone(), test_manifest(Vec::new())));
        let dst_exec = Arc::new(NodeExecState::new(dst_data.clone(), test_manifest(Vec::new())));
        let (p2p_port, p2p_handle) = start_p2p_server(dst_exec.clone(), "127.0.0.1", 0)
            .await
            .unwrap();
        assert!(p2p_port > 0);

        let proj = dummy_project(99, "p2p-app", &src_site);
        let (tx, _rx) = mpsc::unbounded_channel();

        // 1. Preflight allocates free port & computes target deploy_dir
        let pre = execute_rpc(
            dst_exec.clone(),
            "pre-1".to_string(),
            "shift.preflight".to_string(),
            json!({ "project": proj, "usedPorts": [] }),
            tx.clone(),
        )
        .await;
        let allocated_port = match pre {
            ClusterWsMessage::RpcResponse { ok: true, result, .. } => {
                assert_eq!(result["capabilities"]["p2pPort"].as_u64(), Some(p2p_port as u64));
                result["allocatedPort"].as_u64().unwrap() as u16
            }
            other => panic!("unexpected preflight reply: {other:?}"),
        };
        assert!(allocated_port >= 1024);

        // 2. Target prepares single-use receive ticket
        let prep = execute_rpc(
            dst_exec.clone(),
            "prep-1".to_string(),
            "shift.prepare_receive".to_string(),
            json!({ "project": proj }),
            tx.clone(),
        )
        .await;
        let (token, target_dir) = match prep {
            ClusterWsMessage::RpcResponse { ok: true, result, .. } => (
                result["token"].as_str().unwrap().to_string(),
                PathBuf::from(result["targetDeployDir"].as_str().unwrap()),
            ),
            other => panic!("unexpected prepare_receive reply: {other:?}"),
        };

        // 3. Source packs and streams .zip directly to Target P2P HTTP receiver
        let push = execute_rpc(
            src_exec.clone(),
            "push-1".to_string(),
            "shift.push_tar".to_string(),
            json!({
                "project": proj,
                "token": token,
                "targetUrls": [format!("http://127.0.0.1:{p2p_port}")],
            }),
            tx.clone(),
        )
        .await;
        match push {
            ClusterWsMessage::RpcResponse { ok: true, result, .. } => {
                assert!(result["bytesTransferred"].as_u64().unwrap() > 0);
                assert!(result["extractedBytes"].as_u64().unwrap() > 0);
            }
            other => panic!("unexpected push reply: {other:?}"),
        }

        // Verify target extracted current/, node_modules/, backups/, and container DB + storage
        assert_eq!(
            std::fs::read_to_string(target_dir.join("current/app.js")).unwrap(),
            "console.log('shifted');"
        );
        assert_eq!(
            std::fs::read_to_string(target_dir.join("current/node_modules/pkg/index.js")).unwrap(),
            "module.exports = 42;"
        );
        assert_eq!(
            std::fs::read_to_string(target_dir.join("backups/backup-1/note.txt")).unwrap(),
            "snap"
        );
        assert_eq!(
            std::fs::read_to_string(dst_data.join("containers/p2p-app/db/app.db")).unwrap(),
            "SQLite-data"
        );
        assert_eq!(
            std::fs::read_to_string(dst_data.join("containers/p2p-app/storage/avatar.png")).unwrap(),
            "PNGDATA"
        );
        assert_eq!(
            std::fs::read_to_string(target_dir.join("current/data/app.db")).unwrap(),
            "SQLite-data"
        );

        // 4. Clean up source ("before") node after successful transfer & verify all old files removed
        let clean = execute_rpc(
            src_exec.clone(),
            "clean-1".to_string(),
            "shift.cleanup_source".to_string(),
            json!({
                "project": proj,
                "remainingProjects": [],
                "removeFiles": true,
            }),
            tx,
        )
        .await;
        match clean {
            ClusterWsMessage::RpcResponse { ok: true, result, .. } => {
                assert!(result["removedDirs"].as_u64().unwrap_or(0) >= 2);
            }
            other => panic!("unexpected cleanup reply: {other:?}"),
        }
        assert!(!src_site.exists(), "source site dir should be removed");
        assert!(
            !src_data.join("containers/p2p-app").exists(),
            "source container dir should be removed without recreating empty dirs"
        );
        assert!(!src_data.join("caddy/p2p-app.caddy").exists());
        assert!(!src_data.join("keys/deploy_key_99").exists());
        // Target files remain intact
        assert!(target_dir.join("current/app.js").exists());
        assert!(dst_data.join("containers/p2p-app/db/app.db").exists());

        p2p_handle.abort();
        let _ = std::fs::remove_dir_all(&src_data);
        let _ = std::fs::remove_dir_all(&dst_data);
    }

    #[test]
    fn install_shift_zip_restores_all_files_node_modules_and_container_db_offline() {
        let src_data = temp_dir("webdep-inst-src");
        let dst_data = temp_dir("webdep-inst-dst");
        let src_site = src_data.join("sites/my-app");
        std::fs::create_dir_all(src_site.join("current/node_modules/dep")).unwrap();
        std::fs::create_dir_all(src_data.join("containers/my-app/db")).unwrap();
        std::fs::create_dir_all(src_data.join("containers/my-app/storage")).unwrap();
        std::fs::write(src_site.join("current/index.js"), b"ok").unwrap();
        std::fs::write(src_site.join("current/node_modules/dep/lib.js"), b"dep").unwrap();
        std::fs::write(src_data.join("containers/my-app/db/main.sqlite"), b"sqlite").unwrap();
        std::fs::write(src_data.join("containers/my-app/storage/doc.pdf"), b"pdf").unwrap();

        let zip_path = src_data.join("shift.zip");
        pack_project_shift_to_zip(
            &src_site,
            Some(&src_data.join("containers/my-app")),
            None,
            &zip_path,
        )
        .unwrap();

        let stage = dst_data.join("stage");
        unpack_archive_auto_safe(&zip_path, &stage).unwrap();
        let dst_site = dst_data.join("sites/my-app");
        install_shift_unpacked_tree(&dst_data, &dst_site, &stage, "my-app", "website", 7).unwrap();

        assert_eq!(std::fs::read_to_string(dst_site.join("current/index.js")).unwrap(), "ok");
        assert_eq!(
            std::fs::read_to_string(dst_site.join("current/node_modules/dep/lib.js")).unwrap(),
            "dep"
        );
        assert_eq!(
            std::fs::read_to_string(dst_data.join("containers/my-app/db/main.sqlite")).unwrap(),
            "sqlite"
        );
        assert_eq!(
            std::fs::read_to_string(dst_data.join("containers/my-app/storage/doc.pdf")).unwrap(),
            "pdf"
        );
        assert_eq!(
            std::fs::read_to_string(dst_site.join("current/data/main.sqlite")).unwrap(),
            "sqlite"
        );

        let _ = std::fs::remove_dir_all(&src_data);
        let _ = std::fs::remove_dir_all(&dst_data);
    }
}
