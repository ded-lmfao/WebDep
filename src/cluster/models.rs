use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterNode {
    pub id: i64,
    pub name: String,
    pub region: String,
    pub tags: Vec<String>,
    pub status: String,
    pub is_local: bool,
    pub public_ip: String,
    pub mesh_ip: String,
    pub wg_pubkey: String,
    pub wg_endpoint: String,
    pub node_version: String,
    pub capabilities: NodeCapabilities,
    pub metrics: NodeMetrics,
    pub last_seen_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub project_count: i64,
    #[serde(default)]
    pub database_count: i64,
}

impl ClusterNode {
    pub fn from_pg_row(r: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        let tags_json: String = r.try_get(3)?;
        let caps_json: String = r.try_get(11)?;
        let metrics_json: String = r.try_get(12)?;
        Ok(Self {
            id: r.try_get(0)?,
            name: r.try_get(1)?,
            region: r.try_get(2)?,
            tags: serde_json::from_str(&tags_json).unwrap_or_default(),
            status: r.try_get(4)?,
            is_local: r.try_get(5)?,
            public_ip: r.try_get(6)?,
            mesh_ip: r.try_get(7)?,
            wg_pubkey: r.try_get(8)?,
            wg_endpoint: r.try_get(9)?,
            node_version: r.try_get(10)?,
            capabilities: serde_json::from_str(&caps_json).unwrap_or_default(),
            metrics: serde_json::from_str(&metrics_json).unwrap_or_default(),
            last_seen_at: r.try_get(13)?,
            created_at: r.try_get(14)?,
            updated_at: r.try_get(15)?,
            project_count: r.try_get::<Option<i64>, _>(16).ok().flatten().unwrap_or(0),
            database_count: r.try_get::<Option<i64>, _>(17).ok().flatten().unwrap_or(0),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeCapabilities {
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub cpu_cores: usize,
    #[serde(default)]
    pub has_wireguard: bool,
    #[serde(default)]
    pub has_caddy: bool,
    #[serde(default)]
    pub has_postgres: bool,
    #[serde(default)]
    pub p2p_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeMetrics {
    #[serde(default)]
    pub cpu_pct: f32,
    #[serde(default)]
    pub mem_used: u64,
    #[serde(default)]
    pub mem_total: u64,
    #[serde(default)]
    pub disk_used: u64,
    #[serde(default)]
    pub disk_total: u64,
    #[serde(default)]
    pub uptime_s: u64,
    #[serde(default)]
    pub rx_bytes_sec: u64,
    #[serde(default)]
    pub tx_bytes_sec: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeEnrollTokenView {
    pub id: i64,
    pub label: String,
    pub region: String,
    pub tags: Vec<String>,
    pub created_by: Option<i64>,
    pub expires_at: i64,
    pub used_at: Option<i64>,
    pub used_node_id: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeerConfig {
    pub node_id: i64,
    pub name: String,
    pub mesh_ip: String,
    pub public_ip: String,
    pub wg_pubkey: String,
    pub wg_endpoint: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeWorkloadPlacement {
    pub project_id: i64,
    pub slug: String,
    pub placement_version: i64,
    pub port: Option<u16>,
    pub app_status: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub disk_bytes: u64,
    #[serde(default)]
    pub deploy_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeployStateUpdate {
    pub deployment_id: i64,
    pub project_id: i64,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default)]
    pub release_id: Option<i64>,
    #[serde(default)]
    pub release_dir: Option<String>,
    #[serde(default)]
    pub short_sha: Option<String>,
    #[serde(default)]
    pub commit_message: Option<String>,
    #[serde(default)]
    pub commit_author: Option<String>,
    #[serde(default)]
    pub committed_at: Option<i64>,
    #[serde(default)]
    pub deployed_sha: Option<String>,
    #[serde(default)]
    pub app_status: Option<String>,
    #[serde(default)]
    pub app_pid: Option<u32>,
    #[serde(default)]
    pub last_poll_at: Option<i64>,
    #[serde(default)]
    pub backup_note: Option<String>,
    #[serde(default)]
    pub backup_size_bytes: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStateManifest {
    pub node_id: i64,
    pub name: String,
    pub region: String,
    pub core_url: String,
    pub core_ws_url: String,
    pub mesh_ip: String,
    pub public_ip: String,
    pub wg_private_key: String,
    pub wg_pubkey: String,
    #[serde(default = "default_wg_port")]
    pub wg_listen_port: u16,
    pub node_token: String,
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    #[serde(default)]
    pub workloads: Vec<NodeWorkloadPlacement>,
    pub updated_at: i64,
}

fn default_wg_port() -> u16 {
    51820
}

fn default_ok_status() -> u16 {
    200
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClusterWsMessage {
    Register {
        node_version: String,
        wg_pubkey: String,
        wg_endpoint: String,
        public_ip: String,
        capabilities: NodeCapabilities,
        workloads: Vec<NodeWorkloadPlacement>,
    },
    Heartbeat {
        metrics: NodeMetrics,
        workloads: Vec<NodeWorkloadPlacement>,
        ts: i64,
    },
    PeerSync {
        mesh_ip: String,
        peers: Vec<PeerConfig>,
        placement_versions: HashMap<i64, i64>,
    },
    SetStatus {
        status: String,
    },
    RpcRequest {
        req_id: String,
        op: String,
        params: serde_json::Value,
    },
    RpcResponse {
        req_id: String,
        ok: bool,
        #[serde(default = "default_ok_status")]
        status: u16,
        #[serde(default)]
        result: serde_json::Value,
        #[serde(default)]
        error: Option<String>,
    },
    LogChunk {
        #[serde(default)]
        project_id: Option<i64>,
        #[serde(default)]
        deployment_id: Option<i64>,
        channel: String,
        level: String,
        message: String,
        ts: i64,
    },
    DeployStateUpdate {
        update: DeployStateUpdate,
    },
    Ping {
        ts: i64,
    },
    Pong {
        ts: i64,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateEnrollTokenInput {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateEnrollTokenResponse {
    pub id: i64,
    pub token: String,
    pub label: String,
    pub region: String,
    pub expires_at: i64,
    pub join_command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollNodeRequest {
    pub token: String,
    pub name: String,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub public_ip: Option<String>,
    #[serde(default)]
    pub wg_pubkey: String,
    #[serde(default)]
    pub wg_endpoint: Option<String>,
    #[serde(default)]
    pub node_version: String,
    #[serde(default)]
    pub capabilities: NodeCapabilities,
    #[serde(default)]
    pub is_local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollNodeResponse {
    pub node_id: i64,
    pub name: String,
    pub region: String,
    pub mesh_ip: String,
    pub node_token: String,
    pub core_ws_url: String,
    pub peers: Vec<PeerConfig>,
}
