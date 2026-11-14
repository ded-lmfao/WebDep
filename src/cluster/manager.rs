use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::{mpsc, RwLock};

use crate::db::DbPool;

use super::mesh::{detect_capabilities, generate_wireguard_keypair, next_available_mesh_ip, DEFAULT_WG_PORT};
use super::models::{
    ClusterNode, ClusterWsMessage, CreateEnrollTokenInput, CreateEnrollTokenResponse,
    EnrollNodeRequest, EnrollNodeResponse, NodeCapabilities, NodeEnrollTokenView, NodeMetrics,
    NodeStateManifest, PeerConfig,
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

pub struct ClusterManager {
    db: DbPool,
    data_dir: PathBuf,
    peers_tx: RwLock<HashMap<i64, mpsc::UnboundedSender<ClusterWsMessage>>>,
}

impl ClusterManager {
    pub fn new(db: DbPool, data_dir: PathBuf) -> Self {
        Self {
            db,
            data_dir,
            peers_tx: RwLock::new(HashMap::new()),
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

        let base_url = core_public_url.trim().trim_end_matches('/');
        let mut join_command = format!("webdep --node --join {base_url} --token {raw_token}");
        if !label.is_empty() {
            let safe_name = label.replace('"', "");
            join_command.push_str(&format!(" --name \"{safe_name}\""));
        }
        if region != "default" {
            let safe_region = region.replace('"', "");
            join_command.push_str(&format!(" --region \"{safe_region}\""));
        }

        Ok(CreateEnrollTokenResponse {
            id,
            token: raw_token,
            label,
            region,
            expires_at,
            join_command,
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
