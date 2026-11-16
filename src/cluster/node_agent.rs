use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, Networks, RefreshKind, System};
use tokio::sync::mpsc;

use super::mesh::{
    detect_capabilities, detect_capabilities_with_p2p, generate_wireguard_keypair,
    reconcile_mesh_network, DEFAULT_WG_PORT,
};
use super::models::{
    ClusterWsMessage, EnrollNodeRequest, EnrollNodeResponse, NodeMetrics, NodeStateManifest,
};
use super::node_exec::{execute_rpc, reconcile_placements, start_p2p_server, NodeExecState};
use super::ws::{
    connect_ws, http_base_to_ws_url, http_post_json, normalize_core_http_url, WsMessage,
};

pub const NODE_STATE_FILE: &str = "node-state.json";

#[derive(Debug, Clone)]
pub struct JoinOptions {
    pub core_url: String,
    pub token: String,
    pub name: Option<String>,
    pub region: Option<String>,
    pub public_ip: Option<String>,
    pub wg_port: u16,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn detect_hostname() -> String {
    System::host_name()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "web-node".to_string())
}

pub fn manifest_path(data_dir: &Path) -> PathBuf {
    data_dir.join(NODE_STATE_FILE)
}

pub fn load_manifest(data_dir: &Path) -> Result<Option<NodeStateManifest>, String> {
    let path = manifest_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed reading {}: {e}", path.display()))?;
    let manifest: NodeStateManifest = serde_json::from_str(&raw)
        .map_err(|e| format!("Invalid {}: {e}", path.display()))?;
    Ok(Some(manifest))
}

pub fn save_manifest(data_dir: &Path, manifest: &NodeStateManifest) -> Result<(), String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("Failed creating data dir {}: {e}", data_dir.display()))?;
    let path = manifest_path(data_dir);
    let tmp = data_dir.join(format!("{NODE_STATE_FILE}.tmp"));
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("Failed serializing node manifest: {e}"))?;
    std::fs::write(&tmp, &json)
        .map_err(|e| format!("Failed writing {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("Failed committing {}: {e}", path.display()))?;
    Ok(())
}

pub async fn enroll_with_core(
    data_dir: &Path,
    opts: JoinOptions,
) -> Result<NodeStateManifest, String> {
    let core_url = normalize_core_http_url(&opts.core_url)?;
    let (wg_private_key, wg_pubkey) = generate_wireguard_keypair();
    let name = opts
        .name
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(detect_hostname);

    let wg_port = if opts.wg_port == 0 {
        DEFAULT_WG_PORT
    } else {
        opts.wg_port
    };
    let wg_endpoint = opts
        .public_ip
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|ip| format!("{ip}:{wg_port}"));

    let payload = EnrollNodeRequest {
        token: opts.token.trim().to_string(),
        name,
        region: opts.region.clone(),
        public_ip: opts.public_ip.clone(),
        wg_pubkey: wg_pubkey.clone(),
        wg_endpoint,
        node_version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: detect_capabilities(),
        is_local: false,
    };

    let body = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    let enroll_url = format!("{core_url}/api/cluster/enroll");
    let (status, resp_body) = http_post_json(&enroll_url, None, &body).await?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "Enrollment failed (HTTP {status}): {}",
            resp_body.trim()
        ));
    }

    let resp: EnrollNodeResponse = serde_json::from_str(&resp_body)
        .map_err(|e| format!("Invalid enrollment response from Core: {e}"))?;

    let core_ws_url = if resp.core_ws_url.trim().is_empty() {
        http_base_to_ws_url(&core_url, "/api/cluster/node-ws")?
    } else {
        resp.core_ws_url
    };

    let manifest = NodeStateManifest {
        node_id: resp.node_id,
        name: resp.name,
        region: resp.region,
        core_url,
        core_ws_url,
        mesh_ip: resp.mesh_ip,
        public_ip: opts.public_ip.unwrap_or_default(),
        wg_private_key,
        wg_pubkey,
        wg_listen_port: wg_port,
        node_token: resp.node_token,
        peers: resp.peers,
        workloads: Vec::new(),
        updated_at: now_secs(),
    };

    save_manifest(data_dir, &manifest)?;
    reconcile_mesh_network(&manifest, data_dir).await;
    Ok(manifest)
}

struct MetricsSampler {
    sys: System,
    disks: Disks,
    networks: Networks,
    last_rx: u64,
    last_tx: u64,
    last_ts: std::time::Instant,
}

impl MetricsSampler {
    fn new() -> Self {
        let mut sys = System::new_with_specifics(
            RefreshKind::new()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let disks = Disks::new_with_refreshed_list();
        let networks = Networks::new_with_refreshed_list();
        let (last_rx, last_tx, _) = sum_network_bytes(&networks);
        Self {
            sys,
            disks,
            networks,
            last_rx,
            last_tx,
            last_ts: std::time::Instant::now(),
        }
    }

    fn sample(&mut self) -> NodeMetrics {
        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        self.disks.refresh();
        self.networks.refresh();

        let cpus = self.sys.cpus();
        let cpu_pct = if cpus.is_empty() {
            0.0
        } else {
            let sum: f32 = cpus.iter().map(|c| c.cpu_usage()).sum();
            (sum / cpus.len() as f32).clamp(0.0, 100.0)
        };

        let (disk_used, disk_total) = self
            .disks
            .first()
            .map(|d| {
                let total = d.total_space();
                let avail = d.available_space();
                (total.saturating_sub(avail), total)
            })
            .unwrap_or((0, 0));

        let (rx_now, tx_now, iface_count) = sum_network_bytes(&self.networks);
        let elapsed = self.last_ts.elapsed().as_secs_f64().max(0.1);
        let rx_bytes_sec = ((rx_now.saturating_sub(self.last_rx)) as f64 / elapsed) as u64;
        let tx_bytes_sec = ((tx_now.saturating_sub(self.last_tx)) as f64 / elapsed) as u64;
        self.last_rx = rx_now;
        self.last_tx = tx_now;
        self.last_ts = std::time::Instant::now();

        NodeMetrics {
            cpu_pct,
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
            disk_used,
            disk_total,
            uptime_s: System::uptime(),
            rx_bytes_sec,
            tx_bytes_sec,
            total_rx_bytes: rx_now,
            total_tx_bytes: tx_now,
            interface_count: iface_count,
        }
    }
}

fn sum_network_bytes(networks: &Networks) -> (u64, u64, u32) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let mut count = 0u32;
    for (iface, data) in networks {
        let lower = iface.to_ascii_lowercase();
        if lower == "lo" || lower.starts_with("loopback") {
            continue;
        }
        rx = rx.saturating_add(data.total_received());
        tx = tx.saturating_add(data.total_transmitted());
        count = count.saturating_add(1);
    }
    (rx, tx, count)
}

pub async fn run_node_agent(data_dir: PathBuf) {
    let mut manifest = match load_manifest(&data_dir) {
        Ok(Some(m)) => m,
        Ok(None) => {
            eprintln!(
                "Web-Node Error: {} not found. Enroll first with `webdep --node --join <CORE_URL> --token <TOKEN>` or `--pair-local`.",
                manifest_path(&data_dir).display()
            );
            return;
        }
        Err(e) => {
            eprintln!("Web-Node Error: {e}");
            return;
        }
    };

    reconcile_mesh_network(&manifest, &data_dir).await;
    let exec_state = Arc::new(NodeExecState::new(data_dir.clone(), manifest.clone()));
    // Per-process CPU/RAM for workloads on this node needs periodic samples (demand-driven:
    // near-idle until the panel asks for usage). Without it remote apps report 0 B / 0%.
    exec_state.monitor.start_background_sampler();
    let p2p_bind_host = std::env::var("WEBDEP_NODE_P2P_BIND")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let p2p_pref_port: u16 = std::env::var("WEBDEP_NODE_P2P_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(4601);
    let (p2p_port, _p2p_handle) = match start_p2p_server(exec_state.clone(), &p2p_bind_host, p2p_pref_port).await {
        Ok(res) => {
            println!("Web-Node P2P receiver listening on {}:{}", p2p_bind_host, res.0);
            res
        }
        Err(e) => {
            eprintln!("Web-Node P2P listener warning: {e}");
            (0, tokio::spawn(async {}))
        }
    };
    let mut sampler = MetricsSampler::new();
    let backoff_secs = [1u64, 2, 5, 10, 30];
    let mut attempt: usize = 0;

    loop {
        if let Ok(Some(fresh)) = load_manifest(&data_dir) {
            manifest = fresh.clone();
            *exec_state.manifest.write().await = fresh;
        }

        let ws = match connect_ws(&manifest.core_ws_url, Some(&manifest.node_token)).await {
            Ok(ws) => {
                attempt = 0;
                ws
            }
            Err(e) => {
                let wait = backoff_secs[attempt.min(backoff_secs.len() - 1)];
                attempt = attempt.saturating_add(1);
                eprintln!(
                    "Web-Node WS connect to {} failed: {} (retrying in {}s)",
                    manifest.core_ws_url, e, wait
                );
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
        };

        let (mut reader, mut writer) = ws.into_split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClusterWsMessage>();

        let wg_endpoint = if manifest.public_ip.trim().is_empty() {
            String::new()
        } else {
            format!("{}:{}", manifest.public_ip.trim(), manifest.wg_listen_port)
        };
        let initial_workloads = exec_state.snapshot_workloads().await;
        let register_msg = ClusterWsMessage::Register {
            node_version: env!("CARGO_PKG_VERSION").to_string(),
            wg_pubkey: manifest.wg_pubkey.clone(),
            wg_endpoint,
            public_ip: manifest.public_ip.clone(),
            capabilities: detect_capabilities_with_p2p(if p2p_port > 0 { Some(p2p_port) } else { None }),
            workloads: initial_workloads,
        };
        if let Ok(json) = serde_json::to_string(&register_msg) {
            if let Err(e) = writer.send_text(&json).await {
                eprintln!("Web-Node WS register failed: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        }

        println!(
            "Web-Node '{}' (node #{}, mesh {}) connected to Core ({})",
            manifest.name, manifest.node_id, manifest.mesh_ip, manifest.core_ws_url
        );

        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                Some(outbound) = out_rx.recv() => {
                    if let Ok(json) = serde_json::to_string(&outbound) {
                        if writer.send_text(&json).await.is_err() {
                            break;
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    let workloads = exec_state.snapshot_workloads().await;
                    let msg = ClusterWsMessage::Heartbeat {
                        metrics: sampler.sample(),
                        workloads,
                        ts: now_secs(),
                    };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        if writer.send_text(&json).await.is_err() {
                            break;
                        }
                    }
                }
                frame = reader.recv() => {
                    match frame {
                        Ok(WsMessage::Text(txt)) => {
                            if let Ok(parsed) = serde_json::from_str::<ClusterWsMessage>(&txt) {
                                match parsed {
                                    ClusterWsMessage::PeerSync { mesh_ip, peers, placement_versions } => {
                                        {
                                            let mut guard = exec_state.manifest.write().await;
                                            if !mesh_ip.trim().is_empty() {
                                                guard.mesh_ip = mesh_ip;
                                            }
                                            guard.peers = peers;
                                            guard.updated_at = now_secs();
                                            let _ = save_manifest(&data_dir, &guard);
                                            manifest = guard.clone();
                                        }
                                        reconcile_mesh_network(&manifest, &data_dir).await;
                                        let _ = reconcile_placements(&exec_state, &placement_versions, Some(&out_tx)).await;
                                    }
                                    ClusterWsMessage::RpcRequest { req_id, op, params } => {
                                        let exec_c = exec_state.clone();
                                        let tx_c = out_tx.clone();
                                        tokio::spawn(async move {
                                            let reply = execute_rpc(exec_c, req_id, op, params, tx_c.clone()).await;
                                            let _ = tx_c.send(reply);
                                        });
                                    }
                                    ClusterWsMessage::SetStatus { status } => {
                                        println!("Web-Node status updated by Core: {status}");
                                    }
                                    ClusterWsMessage::Ping { ts } => {
                                        if let Ok(json) = serde_json::to_string(&ClusterWsMessage::Pong { ts }) {
                                            if writer.send_text(&json).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                    ClusterWsMessage::Error { message } => {
                                        eprintln!("Core control error: {message}");
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Ok(WsMessage::Ping(p)) => {
                            if writer.send_pong(&p).await.is_err() {
                                break;
                            }
                        }
                        Ok(WsMessage::Close) | Err(_) => {
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::models::PeerConfig;

    #[test]
    fn manifest_save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "webdep-node-manifest-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let manifest = NodeStateManifest {
            node_id: 7,
            name: "sg-edge-1".to_string(),
            region: "ap-southeast".to_string(),
            core_url: "https://core.example.com".to_string(),
            core_ws_url: "wss://core.example.com/api/cluster/node-ws".to_string(),
            mesh_ip: "10.200.0.4".to_string(),
            public_ip: "198.51.100.9".to_string(),
            wg_private_key: "priv".to_string(),
            wg_pubkey: "pub".to_string(),
            wg_listen_port: 51820,
            node_token: "wdnode_secret".to_string(),
            peers: vec![PeerConfig {
                node_id: 1,
                name: "core".to_string(),
                mesh_ip: "10.200.0.1".to_string(),
                public_ip: "203.0.113.1".to_string(),
                wg_pubkey: "corepub".to_string(),
                wg_endpoint: "203.0.113.1:51820".to_string(),
                status: "online".to_string(),
            }],
            workloads: Vec::new(),
            updated_at: 1700000000,
        };
        save_manifest(&dir, &manifest).unwrap();
        let loaded = load_manifest(&dir).unwrap().unwrap();
        assert_eq!(loaded.node_id, 7);
        assert_eq!(loaded.mesh_ip, "10.200.0.4");
        assert_eq!(loaded.peers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
