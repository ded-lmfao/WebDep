use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::cache::AppCache;
use crate::db::DbPool;
use crate::git::Git;
use crate::logs::Logs;
use crate::projects::{Project, Projects};

use super::mesh::{detect_capabilities, generate_wireguard_keypair, next_available_mesh_ip, DEFAULT_WG_PORT};
use super::models::{
    ClusterNode, ClusterWsMessage, CreateEnrollTokenInput, CreateEnrollTokenResponse,
    DeployStateUpdate, EnrollNodeRequest, EnrollNodeResponse, NodeCapabilities, NodeEnrollTokenView,
    NodeMetrics, NodeStateManifest, NodeWorkloadPlacement, PeerConfig,
};
use super::node_agent::{load_manifest, save_manifest};

const SELECT_NODE_COLUMNS: &str = "
    n.id, n.name, n.region, n.tags_json, n.status, n.is_local,
    n.public_ip, n.mesh_ip, n.wg_pubkey, n.wg_endpoint, n.node_version,
    n.capabilities_json, n.metrics_json, n.last_seen_at, n.created_at, n.updated_at,
    (SELECT COUNT(*) FROM projects p WHERE p.node_id = n.id) AS project_count,
    (SELECT COUNT(*) FROM databases d WHERE d.node_id = n.id) AS database_count
";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.trim().as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_secret(prefix: &str) -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("{prefix}{}", hex::encode(bytes))
}

#[derive(Debug, Clone, Default)]
pub struct RemoteWorkloadTelemetry {
    pub pid: Option<u32>,
    pub cpu_pct: f32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct RpcReply {
    pub ok: bool,
    pub status: u16,
    pub result: serde_json::Value,
    pub error: Option<String>,
}

pub struct ClusterManager {
    db: DbPool,
    data_dir: PathBuf,
    peers_tx: RwLock<HashMap<i64, mpsc::UnboundedSender<ClusterWsMessage>>>,
    pending_rpc: RwLock<HashMap<String, oneshot::Sender<RpcReply>>>,
    rpc_seq: AtomicU64,
    remote_disk_bytes: RwLock<HashMap<i64, u64>>,
    remote_workload_telemetry: RwLock<HashMap<i64, RemoteWorkloadTelemetry>>,
    remote_net_history: RwLock<HashMap<i64, Vec<crate::monitor::NetworkSample>>>,
}

impl ClusterManager {
    pub fn new(db: DbPool, data_dir: PathBuf) -> Self {
        Self {
            db,
            data_dir,
            peers_tx: RwLock::new(HashMap::new()),
            pending_rpc: RwLock::new(HashMap::new()),
            rpc_seq: AtomicU64::new(1),
            remote_disk_bytes: RwLock::new(HashMap::new()),
            remote_workload_telemetry: RwLock::new(HashMap::new()),
            remote_net_history: RwLock::new(HashMap::new()),
        }
    }

    pub async fn create_enroll_token(
        &self,
        created_by: Option<i64>,
        input: CreateEnrollTokenInput,
        core_public_url: &str,
    ) -> Result<CreateEnrollTokenResponse, String> {
        let now = now_secs();
        let ttl = input.ttl_seconds.unwrap_or(900).clamp(60, 86_400);
        let expires_at = now + ttl;
        let region = input
            .region
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("default")
            .to_string();
        let label = input.label.trim().to_string();
        let tags = input.tags.unwrap_or_default();
        let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string());

        let raw_token = generate_secret("wdjoin_");
        let token_hash = hash_token(&raw_token);

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO node_enroll_tokens(token_hash, label, region, tags_json, created_by, expires_at, created_at)
             VALUES($1, $2, $3, $4, $5, $6, $7)
             RETURNING id",
        )
        .bind(&token_hash)
        .bind(&label)
        .bind(&region)
        .bind(&tags_json)
        .bind(created_by)
        .bind(expires_at)
        .bind(now)
        .fetch_one(&self.db)
        .await
        .map_err(|e| format!("Failed creating enrollment token: {e}"))?;

        let base_url = core_public_url
            .trim()
            .trim_end_matches('/')
            .chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '`' | '$' | ';' | '&' | '|' | '<' | '>'))
            .collect::<String>();
        let clean_target_ip = input
            .target_public_ip
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-'))
                    .collect::<String>()
            })
            .unwrap_or_default();
        let clean_slug = input
            .repo_slug
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
                    .collect::<String>()
            })
            .filter(|s| s.contains('/'))
            .unwrap_or_else(|| "ded-lmfao/WebDep".to_string());

        let sanitize_quoted = |val: &str| -> String {
            val.chars()
                .filter(|c| !matches!(c, '"' | '\'' | '`' | '$' | ';' | '&' | '|' | '\n' | '\r' | '<' | '>'))
                .collect()
        };

        let mut args = format!("--node --join {base_url} --token {raw_token}");
        if !label.is_empty() {
            let safe_name = sanitize_quoted(&label);
            args.push_str(&format!(" --name \"{safe_name}\""));
        }
        if region != "default" {
            let safe_region = sanitize_quoted(&region);
            args.push_str(&format!(" --region \"{safe_region}\""));
        }
        if !clean_target_ip.is_empty() {
            args.push_str(&format!(" --public-ip \"{clean_target_ip}\""));
        }

        let join_command = format!("webdep {args}");
        let service_command = format!("sudo bash start.sh --service {args}");
        // Workers must run the same code line as the Core: a Core built from a feature branch
        // clones that branch (main has no Web-Node mode on older releases).
        let build_branch = env!("WEBDEP_GIT_BRANCH").trim();
        let branch_flag = if !build_branch.is_empty()
            && !matches!(build_branch, "HEAD" | "main" | "master")
            && build_branch.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
        {
            format!("-b {build_branch} ")
        } else {
            String::new()
        };
        let fresh_vps_command = format!(
            "git clone {branch_flag}https://github.com/{clean_slug}.git webdep && cd webdep && {service_command}"
        );

        Ok(CreateEnrollTokenResponse {
            id,
            token: raw_token,
            label,
            region,
            expires_at,
            core_url: base_url,
            target_public_ip: clean_target_ip,
            join_command,
            service_command,
            fresh_vps_command,
        })
    }

    pub async fn list_enroll_tokens(&self) -> Result<Vec<NodeEnrollTokenView>, String> {
        let rows = sqlx::query(
            "SELECT id, label, region, tags_json, created_by, expires_at, used_at, used_node_id, created_at
             FROM node_enroll_tokens
             ORDER BY id DESC
             LIMIT 30",
        )
        .fetch_all(&self.db)
        .await
        .map_err(|e| format!("Failed listing enrollment tokens: {e}"))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let tags_json: String = r.try_get(3).map_err(|e| e.to_string())?;
            out.push(NodeEnrollTokenView {
                id: r.try_get(0).map_err(|e| e.to_string())?,
                label: r.try_get(1).map_err(|e| e.to_string())?,
                region: r.try_get(2).map_err(|e| e.to_string())?,
                tags: serde_json::from_str(&tags_json).unwrap_or_default(),
                created_by: r.try_get(4).map_err(|e| e.to_string())?,
                expires_at: r.try_get(5).map_err(|e| e.to_string())?,
                used_at: r.try_get(6).map_err(|e| e.to_string())?,
                used_node_id: r.try_get(7).map_err(|e| e.to_string())?,
                created_at: r.try_get(8).map_err(|e| e.to_string())?,
            });
        }
        Ok(out)
    }

    pub async fn enroll_node(
        &self,
        req: EnrollNodeRequest,
        remote_ip: &str,
        core_ws_url: &str,
    ) -> Result<EnrollNodeResponse, String> {
        let raw_token = req.token.trim();
        let node_name = req.name.trim();
        if raw_token.is_empty() {
            return Err("Enrollment token is required".to_string());
        }
        if node_name.is_empty() {
            return Err("Node name is required".to_string());
        }

        let now = now_secs();
        let token_hash = hash_token(raw_token);

        let mut tx = self
            .db
            .begin()
            .await
            .map_err(|e| format!("Database transaction error: {e}"))?;

        let tok_row = sqlx::query(
            "SELECT id, label, region, tags_json, expires_at, used_at
             FROM node_enroll_tokens
             WHERE token_hash = $1
             FOR UPDATE",
        )
        .bind(&token_hash)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("Failed checking enrollment token: {e}"))?
        .ok_or_else(|| "Invalid enrollment token".to_string())?;

        let token_id: i64 = tok_row.try_get(0).map_err(|e| e.to_string())?;
        let token_region: String = tok_row.try_get(2).map_err(|e| e.to_string())?;
        let token_tags_json: String = tok_row.try_get(3).map_err(|e| e.to_string())?;
        let expires_at: i64 = tok_row.try_get(4).map_err(|e| e.to_string())?;
        let used_at: Option<i64> = tok_row.try_get(5).map_err(|e| e.to_string())?;

        if used_at.is_some() {
            return Err("Enrollment token has already been used".to_string());
        }
        if expires_at <= now {
            return Err("Enrollment token has expired".to_string());
        }

        let existing_ips: Vec<String> = sqlx::query_scalar("SELECT mesh_ip FROM nodes")
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| format!("Failed querying mesh IPs: {e}"))?;
        let mesh_ip = next_available_mesh_ip(&existing_ips);

        let region = req
            .region
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(if token_region.is_empty() {
                "default"
            } else {
                token_region.as_str()
            })
            .to_string();

        let public_ip = req
            .public_ip
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(if remote_ip.trim().is_empty() {
                "127.0.0.1"
            } else {
                remote_ip.trim()
            })
            .to_string();

        let wg_endpoint = req
            .wg_endpoint
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{public_ip}:{DEFAULT_WG_PORT}"));

        let node_token = generate_secret("wdnode_");
        let auth_token_hash = hash_token(&node_token);
        let caps_json = serde_json::to_string(&req.capabilities).unwrap_or_else(|_| "{}".to_string());
        let metrics_json =
            serde_json::to_string(&NodeMetrics::default()).unwrap_or_else(|_| "{}".to_string());

        let node_id: i64 = sqlx::query_scalar(
            "INSERT INTO nodes(
                name, region, tags_json, status, is_local, public_ip, mesh_ip,
                wg_pubkey, wg_endpoint, node_token_hash, node_version,
                capabilities_json, metrics_json, last_seen_at, created_at, updated_at
             ) VALUES($1, $2, $3, 'online', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $14)
             RETURNING id",
        )
        .bind(node_name)
        .bind(&region)
        .bind(&token_tags_json)
        .bind(req.is_local)
        .bind(&public_ip)
        .bind(&mesh_ip)
        .bind(req.wg_pubkey.trim())
        .bind(&wg_endpoint)
        .bind(&auth_token_hash)
        .bind(req.node_version.trim())
        .bind(&caps_json)
        .bind(&metrics_json)
        .bind(now)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| format!("Failed creating node record (name may already exist): {e}"))?;

        sqlx::query("UPDATE node_enroll_tokens SET used_at = $1, used_node_id = $2 WHERE id = $3")
            .bind(now)
            .bind(node_id)
            .bind(token_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("Failed consuming enrollment token: {e}"))?;

        tx.commit()
            .await
            .map_err(|e| format!("Failed committing node enrollment: {e}"))?;

        let peers = self.list_peer_configs().await?;
        self.broadcast_peer_sync().await;

        Ok(EnrollNodeResponse {
            node_id,
            name: node_name.to_string(),
            region,
            mesh_ip,
            node_token,
            core_ws_url: core_ws_url.to_string(),
            peers,
        })
    }

    pub async fn ensure_default_local_workloads_assigned(&self, node_id: i64) -> Result<(), String> {
        sqlx::query("UPDATE projects SET node_id = $1 WHERE node_id IS NULL")
            .bind(node_id)
            .execute(&self.db)
            .await
            .map_err(|e| format!("Failed assigning unassigned projects to local node: {e}"))?;
        sqlx::query("UPDATE databases SET node_id = $1 WHERE node_id IS NULL")
            .bind(node_id)
            .execute(&self.db)
            .await
            .map_err(|e| format!("Failed assigning unassigned databases to local node: {e}"))?;
        Ok(())
    }

    pub async fn pair_local_node(
        &self,
        core_url: &str,
        core_ws_url: &str,
        hostname: &str,
    ) -> Result<NodeStateManifest, String> {
        let now = now_secs();
        let existing_local: Option<ClusterNode> = {
            let query_str = format!("SELECT {SELECT_NODE_COLUMNS} FROM nodes n WHERE n.is_local = TRUE LIMIT 1");
            sqlx::query(&query_str)
                .fetch_optional(&self.db)
                .await
                .map_err(|e| format!("Failed checking local node: {e}"))?
                .map(|r| ClusterNode::from_pg_row(&r))
                .transpose()
                .map_err(|e| format!("Failed parsing local node row: {e}"))?
        };

        let existing_manifest = load_manifest(&self.data_dir)?;
        let peers = self.list_peer_configs().await?;

        if let (Some(node), Some(mut manifest)) = (&existing_local, existing_manifest) {
            if manifest.node_id == node.id {
                manifest.core_url = core_url.to_string();
                manifest.core_ws_url = core_ws_url.to_string();
                manifest.mesh_ip = node.mesh_ip.clone();
                manifest.peers = peers;
                manifest.updated_at = now;
                save_manifest(&self.data_dir, &manifest)?;
                self.ensure_default_local_workloads_assigned(node.id).await?;
                return Ok(manifest);
            }
        }

        let (wg_private_key, wg_pubkey) = generate_wireguard_keypair();
        let node_token = generate_secret("wdnode_");
        let auth_token_hash = hash_token(&node_token);
        let caps = detect_capabilities();
        let caps_json = serde_json::to_string(&caps).unwrap_or_else(|_| "{}".to_string());

        let node_id = if let Some(node) = existing_local {
            sqlx::query(
                "UPDATE nodes
                 SET wg_pubkey = $1, node_token_hash = $2, capabilities_json = $3,
                     node_version = $4, updated_at = $5
                 WHERE id = $6",
            )
            .bind(&wg_pubkey)
            .bind(&auth_token_hash)
            .bind(&caps_json)
            .bind(env!("CARGO_PKG_VERSION"))
            .bind(now)
            .bind(node.id)
            .execute(&self.db)
            .await
            .map_err(|e| format!("Failed updating local node credentials: {e}"))?;
            node.id
        } else {
            let existing_ips: Vec<String> = sqlx::query_scalar("SELECT mesh_ip FROM nodes")
                .fetch_all(&self.db)
                .await
                .map_err(|e| format!("Failed reading mesh IPs: {e}"))?;
            let mesh_ip = next_available_mesh_ip(&existing_ips);
            let safe_name = {
                let trimmed = hostname.trim();
                if trimmed.is_empty() {
                    "webcore-local".to_string()
                } else {
                    trimmed.to_string()
                }
            };
            let metrics_json =
                serde_json::to_string(&NodeMetrics::default()).unwrap_or_else(|_| "{}".to_string());
            sqlx::query_scalar(
                "INSERT INTO nodes(
                    name, region, tags_json, status, is_local, public_ip, mesh_ip,
                    wg_pubkey, wg_endpoint, node_token_hash, node_version,
                    capabilities_json, metrics_json, last_seen_at, created_at, updated_at
                 ) VALUES($1, 'primary', '[\"control-plane\"]', 'online', TRUE, '127.0.0.1', $2, $3, $4, $5, $6, $7, $8, $9, $9, $9)
                 RETURNING id",
            )
            .bind(&safe_name)
            .bind(&mesh_ip)
            .bind(&wg_pubkey)
            .bind(format!("127.0.0.1:{DEFAULT_WG_PORT}"))
            .bind(&auth_token_hash)
            .bind(env!("CARGO_PKG_VERSION"))
            .bind(&caps_json)
            .bind(&metrics_json)
            .bind(now)
            .fetch_one(&self.db)
            .await
            .map_err(|e| format!("Failed registering local control node: {e}"))?
        };

        self.ensure_default_local_workloads_assigned(node_id).await?;
        let refreshed = self
            .get_node(node_id)
            .await?
            .ok_or_else(|| "Local node disappeared after registration".to_string())?;
        let all_peers = self.list_peer_configs().await?;

        let manifest = NodeStateManifest {
            node_id: refreshed.id,
            name: refreshed.name,
            region: refreshed.region,
            core_url: core_url.to_string(),
            core_ws_url: core_ws_url.to_string(),
            mesh_ip: refreshed.mesh_ip,
            public_ip: refreshed.public_ip,
            wg_private_key,
            wg_pubkey,
            wg_listen_port: DEFAULT_WG_PORT,
            node_token,
            peers: all_peers,
            workloads: Vec::new(),
            updated_at: now,
        };
        save_manifest(&self.data_dir, &manifest)?;
        Ok(manifest)
    }

    pub async fn list_nodes(&self) -> Result<Vec<ClusterNode>, String> {
        let sql = format!(
            "SELECT {SELECT_NODE_COLUMNS} FROM nodes n ORDER BY n.is_local DESC, n.id ASC"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.db)
            .await
            .map_err(|e| format!("Failed listing nodes: {e}"))?;
        let mut nodes = Vec::with_capacity(rows.len());
        for r in rows {
            nodes.push(ClusterNode::from_pg_row(&r).map_err(|e| e.to_string())?);
        }
        Ok(nodes)
    }

    pub async fn get_node(&self, node_id: i64) -> Result<Option<ClusterNode>, String> {
        let sql = format!("SELECT {SELECT_NODE_COLUMNS} FROM nodes n WHERE n.id = $1");
        let row = sqlx::query(&sql)
            .bind(node_id)
            .fetch_optional(&self.db)
            .await
            .map_err(|e| format!("Failed reading node #{node_id}: {e}"))?;
        row.map(|r| ClusterNode::from_pg_row(&r).map_err(|e| e.to_string()))
            .transpose()
    }

    pub async fn verify_node_token(&self, raw_token: &str) -> Result<Option<ClusterNode>, String> {
        let trimmed = raw_token.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let token_hash = hash_token(trimmed);
        let sql = format!("SELECT {SELECT_NODE_COLUMNS} FROM nodes n WHERE n.node_token_hash = $1");
        let row = sqlx::query(&sql)
            .bind(&token_hash)
            .fetch_optional(&self.db)
            .await
            .map_err(|e| format!("Failed verifying node token: {e}"))?;
        row.map(|r| ClusterNode::from_pg_row(&r).map_err(|e| e.to_string()))
            .transpose()
    }

    pub async fn mark_node_online(
        &self,
        node_id: i64,
        node_version: &str,
        wg_pubkey: &str,
        wg_endpoint: &str,
        public_ip: &str,
        capabilities: &NodeCapabilities,
    ) -> Result<(), String> {
        let now = now_secs();
        let caps_json = serde_json::to_string(capabilities).unwrap_or_else(|_| "{}".to_string());
        sqlx::query(
            "UPDATE nodes
             SET status = CASE WHEN status = 'draining' THEN 'draining' ELSE 'online' END,
                 node_version = CASE WHEN $2 <> '' THEN $2 ELSE node_version END,
                 wg_pubkey = CASE WHEN $3 <> '' THEN $3 ELSE wg_pubkey END,
                 wg_endpoint = CASE WHEN $4 <> '' THEN $4 ELSE wg_endpoint END,
                 public_ip = CASE WHEN $5 <> '' THEN $5 ELSE public_ip END,
                 capabilities_json = $6,
                 last_seen_at = $7,
                 updated_at = $7
             WHERE id = $1",
        )
        .bind(node_id)
        .bind(node_version.trim())
        .bind(wg_pubkey.trim())
        .bind(wg_endpoint.trim())
        .bind(public_ip.trim())
        .bind(&caps_json)
        .bind(now)
        .execute(&self.db)
        .await
        .map_err(|e| format!("Failed marking node #{node_id} online: {e}"))?;
        Ok(())
    }

    pub async fn update_node_capabilities(
        &self,
        node_id: i64,
        capabilities: &NodeCapabilities,
    ) -> Result<(), String> {
        let now = now_secs();
        let caps_json = serde_json::to_string(capabilities).unwrap_or_else(|_| "{}".to_string());
        sqlx::query(
            "UPDATE nodes
             SET capabilities_json = $2,
                 updated_at = $3
             WHERE id = $1",
        )
        .bind(node_id)
        .bind(&caps_json)
        .bind(now)
        .execute(&self.db)
        .await
        .map_err(|e| format!("Failed updating node #{node_id} capabilities: {e}"))?;
        Ok(())
    }

    pub async fn update_node_metrics(&self, node_id: i64, metrics: &NodeMetrics) -> Result<(), String> {
        let now = now_secs();
        let metrics_json = serde_json::to_string(metrics).unwrap_or_else(|_| "{}".to_string());
        sqlx::query(
            "UPDATE nodes
             SET metrics_json = $2,
                 status = CASE WHEN status = 'draining' THEN 'draining' ELSE 'online' END,
                 last_seen_at = $3,
                 updated_at = $3
             WHERE id = $1",
        )
        .bind(node_id)
        .bind(&metrics_json)
        .bind(now)
        .execute(&self.db)
        .await
        .map_err(|e| format!("Failed updating node #{node_id} metrics: {e}"))?;

        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(now * 1000);
        let mut hist_guard = self.remote_net_history.write().await;
        let entry = hist_guard.entry(node_id).or_default();
        if entry.len() >= 60 {
            let excess = entry.len() - 59;
            entry.drain(0..excess);
        }
        entry.push(crate::monitor::NetworkSample {
            ts: ts_ms,
            inbound_bps: metrics.rx_bytes_sec,
            outbound_bps: metrics.tx_bytes_sec,
        });
        Ok(())
    }

    pub async fn mark_node_offline(&self, node_id: i64) -> Result<(), String> {
        let now = now_secs();
        sqlx::query(
            "UPDATE nodes
             SET status = CASE WHEN status = 'draining' THEN 'draining' ELSE 'offline' END,
                 updated_at = $2
             WHERE id = $1",
        )
        .bind(node_id)
        .bind(now)
        .execute(&self.db)
        .await
        .map_err(|e| format!("Failed marking node #{node_id} offline: {e}"))?;
        Ok(())
    }

    pub async fn set_node_status(&self, node_id: i64, status: &str) -> Result<ClusterNode, String> {
        let normalized = match status.trim().to_ascii_lowercase().as_str() {
            "online" => "online",
            "draining" => "draining",
            other => return Err(format!("Unsupported node status '{other}'")),
        };
        let now = now_secs();
        let res = sqlx::query("UPDATE nodes SET status = $2, updated_at = $3 WHERE id = $1")
            .bind(node_id)
            .bind(normalized)
            .bind(now)
            .execute(&self.db)
            .await
            .map_err(|e| format!("Failed updating node status: {e}"))?;
        if res.rows_affected() == 0 {
            return Err(format!("Node #{node_id} not found"));
        }

        if let Some(tx) = self.peers_tx.read().await.get(&node_id).cloned() {
            let _ = tx.send(ClusterWsMessage::SetStatus {
                status: normalized.to_string(),
            });
        }
        self.broadcast_peer_sync().await;
        self.get_node(node_id)
            .await?
            .ok_or_else(|| format!("Node #{node_id} not found"))
    }

    pub async fn delete_node(&self, node_id: i64) -> Result<(), String> {
        let proj_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE node_id = $1")
                .bind(node_id)
                .fetch_one(&self.db)
                .await
                .map_err(|e| format!("Failed checking projects on node #{node_id}: {e}"))?;
        let db_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM databases WHERE node_id = $1")
                .bind(node_id)
                .fetch_one(&self.db)
                .await
                .map_err(|e| format!("Failed checking databases on node #{node_id}: {e}"))?;
        if proj_count > 0 || db_count > 0 {
            return Err(format!(
                "Cannot remove node #{node_id}: {proj_count} project(s) and {db_count} database(s) are still assigned to it"
            ));
        }

        let res = sqlx::query("DELETE FROM nodes WHERE id = $1")
            .bind(node_id)
            .execute(&self.db)
            .await
            .map_err(|e| format!("Failed removing node #{node_id}: {e}"))?;
        if res.rows_affected() == 0 {
            return Err(format!("Node #{node_id} not found"));
        }

        self.unregister_peer_sender(node_id).await;
        self.broadcast_peer_sync().await;
        Ok(())
    }

    pub async fn list_peer_configs(&self) -> Result<Vec<PeerConfig>, String> {
        let rows = sqlx::query(
            "SELECT id, name, mesh_ip, public_ip, wg_pubkey, wg_endpoint, status
             FROM nodes
             ORDER BY id ASC",
        )
        .fetch_all(&self.db)
        .await
        .map_err(|e| format!("Failed listing peer configs: {e}"))?;

        let mut peers = Vec::with_capacity(rows.len());
        for r in rows {
            peers.push(PeerConfig {
                node_id: r.try_get(0).map_err(|e| e.to_string())?,
                name: r.try_get(1).map_err(|e| e.to_string())?,
                mesh_ip: r.try_get(2).map_err(|e| e.to_string())?,
                public_ip: r.try_get(3).map_err(|e| e.to_string())?,
                wg_pubkey: r.try_get(4).map_err(|e| e.to_string())?,
                wg_endpoint: r.try_get(5).map_err(|e| e.to_string())?,
                status: r.try_get(6).map_err(|e| e.to_string())?,
            });
        }
        Ok(peers)
    }

    async fn placement_versions_for_node(&self, node_id: i64) -> Result<HashMap<i64, i64>, String> {
        let rows = sqlx::query(
            "SELECT id, placement_version FROM projects WHERE node_id = $1",
        )
        .bind(node_id)
        .fetch_all(&self.db)
        .await
        .map_err(|e| format!("Failed reading placement versions: {e}"))?;
        let mut map = HashMap::with_capacity(rows.len());
        for r in rows {
            let pid: i64 = r.try_get(0).map_err(|e| e.to_string())?;
            let ver: i64 = r.try_get(1).map_err(|e| e.to_string())?;
            map.insert(pid, ver);
        }
        Ok(map)
    }

    pub async fn register_peer_sender(
        &self,
        node_id: i64,
        tx: mpsc::UnboundedSender<ClusterWsMessage>,
    ) {
        self.peers_tx.write().await.insert(node_id, tx);
    }

    pub async fn unregister_peer_sender(&self, node_id: i64) {
        self.peers_tx.write().await.remove(&node_id);
    }

    pub async fn send_peer_sync_to(&self, node_id: i64) -> Result<(), String> {
        let Some(node) = self.get_node(node_id).await? else {
            return Ok(());
        };
        let peers = self.list_peer_configs().await?;
        let placement_versions = self.placement_versions_for_node(node_id).await?;
        if let Some(tx) = self.peers_tx.read().await.get(&node_id).cloned() {
            let _ = tx.send(ClusterWsMessage::PeerSync {
                mesh_ip: node.mesh_ip,
                peers,
                placement_versions,
            });
        }
        Ok(())
    }

    pub async fn broadcast_peer_sync(&self) {
        let Ok(peers) = self.list_peer_configs().await else {
            return;
        };
        let senders: Vec<(i64, mpsc::UnboundedSender<ClusterWsMessage>)> = self
            .peers_tx
            .read()
            .await
            .iter()
            .map(|(&k, v)| (k, v.clone()))
            .collect();
        for (node_id, tx) in senders {
            let mesh_ip = peers
                .iter()
                .find(|p| p.node_id == node_id)
                .map(|p| p.mesh_ip.clone())
                .unwrap_or_default();
            let placement_versions = self
                .placement_versions_for_node(node_id)
                .await
                .unwrap_or_default();
            let _ = tx.send(ClusterWsMessage::PeerSync {
                mesh_ip,
                peers: peers.clone(),
                placement_versions,
            });
        }
    }

    pub fn start_staleness_reaper(self: &Arc<Self>) {
        let mgr = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                let cutoff = now_secs() - 45;
                let _ = sqlx::query(
                    "UPDATE nodes
                     SET status = 'offline', updated_at = $2
                     WHERE status = 'online'
                       AND is_local = FALSE
                       AND last_seen_at IS NOT NULL
                       AND last_seen_at < $1",
                )
                .bind(cutoff)
                .bind(now_secs())
                .execute(&mgr.db)
                .await;
            }
        });
    }

    pub async fn default_local_node_id(&self) -> Option<i64> {
        sqlx::query_scalar::<_, i64>("SELECT id FROM nodes WHERE is_local = TRUE ORDER BY id ASC LIMIT 1")
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten()
    }

    pub async fn should_proxy_to_node(&self, node_id: Option<i64>) -> Result<Option<i64>, String> {
        let Some(nid) = node_id else {
            return Ok(None);
        };
        let is_local: Option<bool> = sqlx::query_scalar("SELECT is_local FROM nodes WHERE id = $1")
            .bind(nid)
            .fetch_optional(&self.db)
            .await
            .map_err(|e| format!("Failed checking target node #{nid}: {e}"))?;
        match is_local {
            Some(false) => Ok(Some(nid)),
            Some(true) | None => Ok(None),
        }
    }

    pub async fn is_node_connected(&self, node_id: i64) -> bool {
        self.peers_tx.read().await.contains_key(&node_id)
    }

    pub async fn get_remote_disk_bytes(&self, project_id: i64) -> Option<u64> {
        self.remote_disk_bytes.read().await.get(&project_id).copied()
    }

    pub async fn get_remote_workload_telemetry(&self) -> HashMap<i64, RemoteWorkloadTelemetry> {
        self.remote_workload_telemetry.read().await.clone()
    }

    pub async fn get_remote_net_history_map(
        &self,
    ) -> HashMap<i64, Vec<crate::monitor::NetworkSample>> {
        self.remote_net_history.read().await.clone()
    }

    pub async fn call_node(
        &self,
        node_id: i64,
        op: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, (StatusCode, String)> {
        let tx = self
            .peers_tx
            .read()
            .await
            .get(&node_id)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Web-Node #{node_id} is currently offline or not connected"),
                )
            })?;

        let seq = self.rpc_seq.fetch_add(1, Ordering::Relaxed);
        let req_id = format!("rpc-{}-{}", now_secs(), seq);
        let (resp_tx, resp_rx) = oneshot::channel::<RpcReply>();
        self.pending_rpc.write().await.insert(req_id.clone(), resp_tx);

        if tx
            .send(ClusterWsMessage::RpcRequest {
                req_id: req_id.clone(),
                op: op.to_string(),
                params,
            })
            .is_err()
        {
            self.pending_rpc.write().await.remove(&req_id);
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Failed sending '{op}' to Web-Node #{node_id}"),
            ));
        }

        let reply = match tokio::time::timeout(timeout, resp_rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => {
                self.pending_rpc.write().await.remove(&req_id);
                return Err((
                    StatusCode::BAD_GATEWAY,
                    format!("Web-Node #{node_id} closed connection during '{op}'"),
                ));
            }
            Err(_) => {
                self.pending_rpc.write().await.remove(&req_id);
                return Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("Timed out waiting for Web-Node #{node_id} to complete '{op}'"),
                ));
            }
        };

        if reply.ok {
            Ok(reply.result)
        } else {
            let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
            Err((
                status,
                reply
                    .error
                    .unwrap_or_else(|| format!("Remote '{op}' failed on Web-Node #{node_id}")),
            ))
        }
    }

    pub async fn resolve_rpc(
        &self,
        req_id: &str,
        ok: bool,
        status: u16,
        result: serde_json::Value,
        error: Option<String>,
    ) {
        if let Some(tx) = self.pending_rpc.write().await.remove(req_id) {
            let _ = tx.send(RpcReply {
                ok,
                status,
                result,
                error,
            });
        }
    }

    pub async fn apply_remote_workloads_heartbeat(
        &self,
        node_id: i64,
        workloads: &[NodeWorkloadPlacement],
    ) {
        if workloads.is_empty() {
            return;
        }
        let mut disk_guard = self.remote_disk_bytes.write().await;
        let mut telem_guard = self.remote_workload_telemetry.write().await;
        for w in workloads {
            if w.disk_bytes > 0 {
                disk_guard.insert(w.project_id, w.disk_bytes);
            }
            let prev_disk = telem_guard
                .get(&w.project_id)
                .map(|t| t.disk_bytes)
                .unwrap_or(0);
            telem_guard.insert(
                w.project_id,
                RemoteWorkloadTelemetry {
                    pid: w.pid,
                    cpu_pct: w.cpu_pct,
                    memory_bytes: w.memory_bytes,
                    disk_bytes: if w.disk_bytes > 0 { w.disk_bytes } else { prev_disk },
                },
            );
            let pid_i64 = w.pid.map(|p| p as i64);
            let normalized_status = match w.app_status.as_str() {
                "running" | "stopped" | "maintenance" | "idle" => w.app_status.as_str(),
                _ => continue,
            };
            let _ = sqlx::query(
                "UPDATE projects
                 SET app_status = $1,
                     app_pid = $2
                 WHERE id = $3 AND node_id = $4",
            )
            .bind(normalized_status)
            .bind(pid_i64)
            .bind(w.project_id)
            .bind(node_id)
            .execute(&self.db)
            .await;
        }
    }

    pub async fn apply_remote_deploy_update(
        &self,
        logs: &Logs,
        cache: &AppCache,
        upd: DeployStateUpdate,
    ) {
        // Control-plane timestamps (deployments, releases, projects) are Unix milliseconds,
        // matching crate::db::now() used by local deployments.
        let now = crate::db::now();
        if let Some(ref stage) = upd.stage {
            let _ = sqlx::query("UPDATE deployments SET stage = $1 WHERE id = $2")
                .bind(stage)
                .bind(upd.deployment_id)
                .execute(&self.db)
                .await;
        }

        if let Some(poll_ts) = upd.last_poll_at {
            // Web-Nodes report seconds; store milliseconds like local deployments do.
            let poll_ms = if poll_ts < 100_000_000_000 { poll_ts * 1000 } else { poll_ts };
            let _ = sqlx::query("UPDATE projects SET last_poll_at = $1 WHERE id = $2")
                .bind(poll_ms)
                .bind(upd.project_id)
                .execute(&self.db)
                .await;
        }

        // Create or update remote release entry once checkout/dir is known
        let mut resolved_release_id = upd.release_id;
        if let (Some(ref dir), Some(ref sha)) = (&upd.release_dir, &upd.sha) {
            let existing: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM releases WHERE deployment_id = $1 ORDER BY id DESC LIMIT 1",
            )
            .bind(upd.deployment_id)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten();

            let short_sha = upd
                .short_sha
                .clone()
                .unwrap_or_else(|| sha[..7.min(sha.len())].to_string());
            let rel_status = if upd.status.as_deref() == Some("success") {
                "succeeded"
            } else if upd.status.as_deref() == Some("failed") {
                "failed"
            } else {
                "pulled"
            };
            let build_status = if upd.status.as_deref() == Some("success") {
                "success"
            } else if upd.status.as_deref() == Some("failed") {
                "failed"
            } else {
                "pending"
            };
            let health_status = if upd.status.as_deref() == Some("success") {
                "ok"
            } else if upd.status.as_deref() == Some("failed") {
                "failed"
            } else {
                "pending"
            };

            let rid = if let Some(eid) = existing {
                let _ = sqlx::query(
                    "UPDATE releases
                     SET dir = $1, status = $2, build_status = $3, health_status = $4,
                         deployed_at = CASE WHEN $2 = 'succeeded' THEN $5 ELSE deployed_at END
                     WHERE id = $6",
                )
                .bind(dir)
                .bind(rel_status)
                .bind(build_status)
                .bind(health_status)
                .bind(now)
                .bind(eid)
                .execute(&self.db)
                .await;
                eid
            } else {
                sqlx::query_scalar::<_, i64>(
                    "INSERT INTO releases(project_id, deployment_id, sha, short_sha, message, author, committed_at, dir, status, build_status, health_status, created_at, deployed_at)
                     VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, CASE WHEN $9 = 'succeeded' THEN $12 ELSE NULL END)
                     RETURNING id",
                )
                .bind(upd.project_id)
                .bind(upd.deployment_id)
                .bind(sha)
                .bind(&short_sha)
                .bind(upd.commit_message.as_deref().unwrap_or("Deploy"))
                .bind(upd.commit_author.as_deref().unwrap_or("System"))
                .bind(upd.committed_at.unwrap_or(now))
                .bind(dir)
                .bind(rel_status)
                .bind(build_status)
                .bind(health_status)
                .bind(now)
                .fetch_one(&self.db)
                .await
                .unwrap_or(0)
            };
            if rid > 0 {
                resolved_release_id = Some(rid);
                let _ = sqlx::query(
                    "UPDATE deployments SET release_id = $1, sha = $2 WHERE id = $3",
                )
                .bind(rid)
                .bind(sha)
                .bind(upd.deployment_id)
                .execute(&self.db)
                .await;
            }
        } else if upd.sha.is_some() || upd.release_id.is_some() {
            let _ = sqlx::query(
                "UPDATE deployments
                 SET sha = COALESCE($1, sha),
                     release_id = COALESCE($2, release_id)
                 WHERE id = $3",
            )
            .bind(upd.sha.as_deref())
            .bind(upd.release_id)
            .bind(upd.deployment_id)
            .execute(&self.db)
            .await;
        }

        if let Some(ref app_status) = upd.app_status {
            let pid_i64 = upd.app_pid.map(|p| p as i64);
            let _ = sqlx::query(
                "UPDATE projects
                 SET app_status = $1,
                     app_pid = $2,
                     app_started_at = CASE WHEN $1 = 'running' THEN $3 ELSE app_started_at END
                 WHERE id = $4",
            )
            .bind(app_status)
            .bind(pid_i64)
            .bind(now)
            .bind(upd.project_id)
            .execute(&self.db)
            .await;
        }

        if let Some(ref deployed_sha) = upd.deployed_sha {
            let _ = sqlx::query(
                "UPDATE projects
                 SET current_release_id = COALESCE($1, current_release_id),
                     deployed_sha = $2,
                     updated_at = $3
                 WHERE id = $4",
            )
            .bind(resolved_release_id)
            .bind(deployed_sha)
            .bind(now)
            .bind(upd.project_id)
            .execute(&self.db)
            .await;
        }

        if let (Some(sz), Some(ref rdir)) = (upd.backup_size_bytes, &upd.release_dir) {
            if let Some(rid) = resolved_release_id {
                let note = upd
                    .backup_note
                    .clone()
                    .unwrap_or_else(|| format!("Auto-snapshot for Release #{rid}"));
                let _ = sqlx::query(
                    "INSERT INTO backups(project_id, release_id, path, size_bytes, note, created_at)
                     VALUES($1, $2, $3, $4, $5, $6)",
                )
                .bind(upd.project_id)
                .bind(rid)
                .bind(rdir)
                .bind(sz)
                .bind(&note)
                .bind(now)
                .execute(&self.db)
                .await;
            }
        }

        if let Some(ref status) = upd.status {
            if status == "success" || status == "failed" {
                let stage_val = if status == "success" { "completed" } else { "failed" };
                let _ = sqlx::query(
                    "UPDATE deployments SET status = $1, stage = $2, error = $3, finished_at = $4 WHERE id = $5",
                )
                .bind(status)
                .bind(stage_val)
                .bind(upd.error.as_deref())
                .bind(now)
                .bind(upd.deployment_id)
                .execute(&self.db)
                .await;
                if let Some(ref err) = upd.error {
                    logs.add(
                        Some(upd.project_id),
                        Some(upd.deployment_id),
                        "deployment",
                        "error",
                        err,
                    );
                }
            }
        }

        cache.invalidate_project(upd.project_id);
    }

    pub async fn build_remote_context(
        &self,
        projects: &Projects,
        project_id: i64,
    ) -> serde_json::Value {
        let env_map = projects.get_env_map(project_id).await;
        let deploy_keypair = projects.get_deploy_keypair(project_id).await;
        let github_tokens = Git::read_github_tokens(&self.db).await;
        serde_json::json!({
            "env": env_map,
            "deployKeyPublic": deploy_keypair.as_ref().map(|(pub_k, _)| pub_k),
            "deployKeyPrivate": deploy_keypair.as_ref().map(|(_, priv_k)| priv_k),
            "githubTokens": github_tokens,
        })
    }

    pub async fn sync_remote_env_and_keys(&self, projects: &Projects, p: &Project) {
        let Ok(Some(node_id)) = self.should_proxy_to_node(p.node_id).await else {
            return;
        };
        if !self.is_node_connected(node_id).await {
            return;
        }
        let ctx = self.build_remote_context(projects, p.id).await;
        let _ = self
            .call_node(
                node_id,
                "env.sync",
                serde_json::json!({
                    "project": p,
                    "context": ctx,
                }),
                Duration::from_secs(15),
            )
            .await;
    }

    pub async fn sync_remote_caddy(&self, projects: &Projects, node_id: i64) {
        let Ok(Some(target_node_id)) = self.should_proxy_to_node(Some(node_id)).await else {
            return;
        };
        if !self.is_node_connected(target_node_id).await {
            return;
        }
        let node_projects = projects.list_for_node(target_node_id).await.unwrap_or_default();
        let _ = self
            .call_node(
                target_node_id,
                "caddy.apply",
                serde_json::json!({
                    "projects": node_projects,
                }),
                Duration::from_secs(15),
            )
            .await;
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub async fn used_ports_on_node(&self, node_id: i64) -> Vec<u16> {
        let mut ports = Vec::new();
        if let Ok(rows) = sqlx::query(
            "SELECT port, assigned_port FROM projects WHERE node_id = $1",
        )
        .bind(node_id)
        .fetch_all(&self.db)
        .await
        {
            for r in rows {
                if let Ok(Some(p)) = r.try_get::<Option<i64>, _>(0) {
                    if let Ok(u) = u16::try_from(p) {
                        ports.push(u);
                    }
                }
                if let Ok(Some(ap)) = r.try_get::<Option<i64>, _>(1) {
                    if let Ok(u) = u16::try_from(ap) {
                        ports.push(u);
                    }
                }
            }
        }
        if let Ok(db_rows) = sqlx::query("SELECT port FROM databases WHERE node_id = $1")
            .bind(node_id)
            .fetch_all(&self.db)
            .await
        {
            for r in db_rows {
                if let Ok(p) = r.try_get::<i64, _>(0) {
                    if let Ok(u) = u16::try_from(p) {
                        ports.push(u);
                    }
                }
            }
        }
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// Ensures any managed PostgreSQL database linked to `project_id` remains reachable
    /// after shifting onto `target_node_id` (rewrites `DATABASE_URL` / `PGHOST` to `127.0.0.1`
    /// when co-located or `db_node.mesh_ip` across the WireGuard mesh).
    pub async fn rewrite_linked_db_env_for_node(
        &self,
        projects: &Projects,
        project_id: i64,
        target_node_id: i64,
    ) -> Result<bool, String> {
        let db_row = sqlx::query(
            "SELECT db_user, db_password, port, db_name, node_id
             FROM databases
             WHERE linked_project_id = $1
             ORDER BY id ASC
             LIMIT 1",
        )
        .bind(project_id)
        .fetch_optional(&self.db)
        .await
        .map_err(|e| format!("Failed checking linked database: {e}"))?;

        let Some(row) = db_row else {
            return Ok(false);
        };

        let db_user: String = row.try_get(0).map_err(|e| e.to_string())?;
        let db_password: String = row.try_get(1).map_err(|e| e.to_string())?;
        let port: i32 = row.try_get(2).map_err(|e| e.to_string())?;
        let db_name: String = row.try_get(3).map_err(|e| e.to_string())?;
        let db_node_id: Option<i64> = row.try_get(4).ok().flatten();

        let target_host = match db_node_id {
            Some(dnid) if dnid != target_node_id => self
                .get_node(dnid)
                .await?
                .map(|n| n.mesh_ip)
                .filter(|ip| !ip.trim().is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            _ => "127.0.0.1".to_string(),
        };

        let port_u16 = u16::try_from(port).unwrap_or(5432);
        let url = crate::databases::build_postgres_url(
            &db_user,
            &db_password,
            &target_host,
            port_u16,
            &db_name,
            false,
        );
        let _ = projects.set_env_var(project_id, "DATABASE_URL", &url, true).await;
        let _ = projects.set_env_var(project_id, "PGHOST", &target_host, false).await;
        Ok(true)
    }

    pub fn build_p2p_target_urls(node: &ClusterNode, port_override: Option<u16>) -> Vec<String> {
        let port = port_override
            .filter(|&p| p > 0)
            .or(node.capabilities.p2p_port)
            .unwrap_or(4601);
        let mut urls = Vec::new();
        let mesh = node.mesh_ip.trim();
        let pub_ip = node.public_ip.trim();
        if !pub_ip.is_empty() && (pub_ip == "127.0.0.1" || pub_ip == "::1") {
            urls.push(format!("http://{pub_ip}:{port}"));
        }
        if !mesh.is_empty() {
            urls.push(format!("http://{mesh}:{port}"));
        }
        if !pub_ip.is_empty() && pub_ip != "127.0.0.1" && pub_ip != "::1" {
            urls.push(format!("http://{pub_ip}:{port}"));
        }
        if urls.is_empty() {
            urls.push(format!("http://127.0.0.1:{port}"));
        }
        urls.dedup();
        urls
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pg_control::test_db;

    #[tokio::test]
    async fn enroll_drain_delete_and_pair_local_lifecycle() {
        let db = test_db::isolated_pool().await;
        let data_dir = std::env::temp_dir().join(format!(
            "webdep-cluster-mgr-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&data_dir);

        let mgr = ClusterManager::new(db.clone(), data_dir.clone());

        // 1. Pair local control node
        let local_manifest = mgr
            .pair_local_node("http://127.0.0.1:8080", "ws://127.0.0.1:8080/api/cluster/node-ws", "ctrl-node")
            .await
            .unwrap();
        assert_eq!(local_manifest.mesh_ip, "10.200.0.1");
        assert!(local_manifest.node_token.starts_with("wdnode_"));

        // 2. Create single-use enrollment token & enroll remote node
        let tok = mgr
            .create_enroll_token(
                None,
                CreateEnrollTokenInput {
                    label: "eu-worker-1".to_string(),
                    region: Some("eu-central".to_string()),
                    tags: Some(vec!["gpu".to_string()]),
                    ttl_seconds: Some(600),
                    ..Default::default()
                },
                "https://panel.example.com",
            )
            .await
            .unwrap();
        assert!(tok.join_command.contains("--node --join https://panel.example.com"));

        let enrolled = mgr
            .enroll_node(
                EnrollNodeRequest {
                    token: tok.token.clone(),
                    name: "eu-worker-1".to_string(),
                    region: None,
                    public_ip: Some("203.0.113.20".to_string()),
                    wg_pubkey: "abcPubKey123=".to_string(),
                    wg_endpoint: None,
                    node_version: "0.2.2".to_string(),
                    capabilities: NodeCapabilities::default(),
                    is_local: false,
                },
                "203.0.113.20",
                "wss://panel.example.com/api/cluster/node-ws",
            )
            .await
            .unwrap();
        assert_eq!(enrolled.mesh_ip, "10.200.0.2");
        assert_eq!(enrolled.region, "eu-central");
        assert_eq!(enrolled.peers.len(), 2);

        // Token cannot be reused
        let reuse_err = mgr
            .enroll_node(
                EnrollNodeRequest {
                    token: tok.token,
                    name: "duplicate-worker".to_string(),
                    region: None,
                    public_ip: None,
                    wg_pubkey: "def".to_string(),
                    wg_endpoint: None,
                    node_version: "0.2.2".to_string(),
                    capabilities: NodeCapabilities::default(),
                    is_local: false,
                },
                "203.0.113.21",
                "wss://panel.example.com/api/cluster/node-ws",
            )
            .await
            .unwrap_err();
        assert!(reuse_err.contains("already been used"));

        // Verify token auth lookup
        let verified = mgr.verify_node_token(&enrolled.node_token).await.unwrap();
        assert_eq!(verified.as_ref().map(|n| n.id), Some(enrolled.node_id));

        // Drain -> online -> delete
        let drained = mgr.set_node_status(enrolled.node_id, "draining").await.unwrap();
        assert_eq!(drained.status, "draining");
        let back = mgr.set_node_status(enrolled.node_id, "online").await.unwrap();
        assert_eq!(back.status, "online");

        mgr.delete_node(enrolled.node_id).await.unwrap();
        assert!(mgr.get_node(enrolled.node_id).await.unwrap().is_none());

        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
