pub mod binaries;
pub mod security;

pub use binaries::*;
pub use security::*;

use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tracing::{info, warn};

use crate::db::{now, DbPool};
use crate::logs::Logs;
use crate::vault::Vault;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedDatabase {
    pub id: i64,
    pub name: String,
    pub engine: String,
    pub db_name: String,
    pub db_user: String,
    pub db_password: Option<String>,
    pub port: u16,
    pub is_superuser: bool,
    pub access_mode: String,
    pub allowed_ips: String,
    pub domain: String,
    pub status: String,
    pub container_id: Option<String>,
    pub started_at: Option<i64>,
    pub data_dir: String,
    pub linked_project_id: Option<i64>,
    pub linked_project_name: Option<String>,
    pub node_id: Option<i64>,
    pub internal_url: String,
    pub external_url: String,
    pub psql_command: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct DatabaseOverviewSummary {
    pub id: i64,
    pub name: String,
    pub port: u16,
    pub domain: String,
    pub status: String,
    pub container_id: Option<String>,
    pub data_dir: String,
    pub linked_project_id: Option<i64>,
    pub node_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDatabaseInput {
    pub name: String,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(alias = "dbName", alias = "db_name")]
    pub db_name: String,
    #[serde(alias = "dbUser", alias = "db_user")]
    pub db_user: String,
    #[serde(alias = "dbPassword", alias = "db_password")]
    pub db_password: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default, alias = "isSuperuser", alias = "is_superuser")]
    pub is_superuser: Option<bool>,
    #[serde(default, alias = "accessMode", alias = "access_mode")]
    pub access_mode: Option<String>,
    #[serde(default, alias = "allowedIps", alias = "allowed_ips")]
    pub allowed_ips: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default, alias = "linkedProjectId", alias = "linked_project_id")]
    pub linked_project_id: Option<i64>,
    /// Limits chosen at creation; 0 or empty = no limit.
    #[serde(default)]
    pub limits: crate::app_limits::AppLimits,
}

pub fn encode_uri_userinfo_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub fn build_postgres_url(
    db_user: &str,
    raw_pass: &str,
    host: &str,
    port: u16,
    db_name: &str,
    external_ssl: bool,
) -> String {
    let enc_user = encode_uri_userinfo_component(db_user);
    let enc_pass = encode_uri_userinfo_component(raw_pass);
    let enc_db = encode_uri_userinfo_component(db_name);
    if external_ssl {
        format!(
            "postgresql://{}:{}@{}:{}/{}?sslmode=prefer",
            enc_user, enc_pass, host, port, enc_db
        )
    } else {
        format!(
            "postgresql://{}:{}@{}:{}/{}",
            enc_user, enc_pass, host, port, enc_db
        )
    }
}

pub struct Databases {
    db: DbPool,
    vault: Arc<Vault>,
    logs: Logs,
    base_dir: PathBuf,
    public_ip: String,
    /// Set once the cluster exists: databases on other nodes are run through it.
    cluster: std::sync::OnceLock<Arc<crate::cluster::ClusterManager>>,
}

struct RawDatabaseRow {
    id: i64,
    name: String,
    engine: String,
    db_name: String,
    db_user: String,
    enc_pass: String,
    port: u16,
    is_superuser: bool,
    access_mode: String,
    allowed_ips: String,
    status: String,
    container_id: Option<String>,
    data_dir: String,
    linked_project_id: Option<i64>,
    linked_project_name: Option<String>,
    domain: String,
    created_at: i64,
    updated_at: i64,
    started_at_db: Option<i64>,
    node_id: Option<i64>,
}

impl Databases {
    pub fn new(db: DbPool, vault: Arc<Vault>, logs: Logs, data_dir: &Path) -> Self {
        let raw_dir = data_dir.join("databases");
        let _ = std::fs::create_dir_all(&raw_dir);
        let abs_dir = std::fs::canonicalize(&raw_dir).unwrap_or(raw_dir);
        let clean_base = std::path::PathBuf::from(security::to_clean_path_str(&abs_dir));

        let public_ip = std::env::var("PUBLIC_IP")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());

        Self {
            db,
            vault,
            logs,
            base_dir: clean_base,
            public_ip,
            cluster: std::sync::OnceLock::new(),
        }
    }

    /// Where a database's data lives when it runs on this server.
    pub fn local_dir(&self, id: i64) -> PathBuf {
        self.base_dir.join(format!("db-{id}"))
    }

    pub fn attach_cluster(&self, cluster: Arc<crate::cluster::ClusterManager>) {
        let _ = self.cluster.set(cluster);
    }

    /// The worker node a database runs on, or `None` when it runs on this server.
    pub async fn remote_node(&self, node_id: Option<i64>) -> Option<(Arc<crate::cluster::ClusterManager>, i64)> {
        let cluster = self.cluster.get()?.clone();
        let nid = cluster.should_proxy_to_node(node_id).await.ok().flatten()?;
        Some((cluster, nid))
    }

    /// Runs one database action on a worker node.
    pub async fn call_remote(
        &self,
        cluster: &crate::cluster::ClusterManager,
        node_id: i64,
        op: &str,
        params: serde_json::Value,
        secs: u64,
    ) -> Result<serde_json::Value, String> {
        cluster
            .call_node(node_id, op, params, std::time::Duration::from_secs(secs))
            .await
            .map_err(|(_, msg)| msg)
    }

    /// What a worker node needs to start this database (it picks the folder itself).
    pub async fn engine_spec(&self, rec: &ManagedDatabase) -> EngineSpec {
        let limits = crate::app_limits::get(&self.db, crate::app_limits::Kind::Database, rec.id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        EngineSpec {
            id: rec.id,
            port: rec.port,
            db_name: rec.db_name.clone(),
            db_user: rec.db_user.clone(),
            db_password: rec.db_password.clone().unwrap_or_default(),
            is_superuser: rec.is_superuser,
            access_mode: rec.access_mode.clone(),
            allowed_ips: rec.allowed_ips.clone(),
            db_dir: PathBuf::new(),
            cpu: limits.cpu,
            memory_mb: limits.memory_mb,
        }
    }

    /// Where tools on this server reach a database: loopback, or the node's mesh address.
    pub async fn connect_host(&self, rec: &ManagedDatabase) -> Result<String, String> {
        let Some((cluster, nid)) = self.remote_node(rec.node_id).await else {
            return Ok("127.0.0.1".to_string());
        };
        let node = cluster.get_node(nid).await?.ok_or_else(|| format!("Node #{nid} not found"))?;
        if node.mesh_ip.trim().is_empty() {
            return Err(format!("Node '{}' has no mesh address yet, so its databases can't be reached from here.", node.name));
        }
        Ok(node.mesh_ip.trim().to_string())
    }

    async fn mark_running(&self, id: i64, container_id: &str) -> Result<(), String> {
        sqlx::query("UPDATE databases SET status = 'running', container_id = $1, started_at = $2, updated_at = $3 WHERE id = $4")
            .bind(container_id)
            .bind(now())
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn mark_stopped(&self, id: i64) -> Result<(), String> {
        sqlx::query("UPDATE databases SET status = 'stopped', started_at = NULL, updated_at = $1 WHERE id = $2")
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn read_raw_row(r: &sqlx::postgres::PgRow) -> Result<RawDatabaseRow, sqlx::Error> {
        Ok(RawDatabaseRow {
            id: r.try_get(0)?,
            name: r.try_get(1)?,
            engine: r.try_get(2)?,
            db_name: r.try_get(3)?,
            db_user: r.try_get(4)?,
            enc_pass: r.try_get(5)?,
            port: r.try_get::<i32, _>(6)? as u16,
            is_superuser: r.try_get(7)?,
            access_mode: r.try_get(8)?,
            allowed_ips: r.try_get(9)?,
            status: r.try_get(10)?,
            container_id: r.try_get(11)?,
            data_dir: r.try_get(12)?,
            linked_project_id: r.try_get(13)?,
            linked_project_name: r.try_get(14)?,
            domain: r.try_get::<Option<String>, _>(15)?.unwrap_or_default(),
            created_at: r.try_get(16)?,
            updated_at: r.try_get(17)?,
            started_at_db: r.try_get(18)?,
            node_id: r.try_get(19)?,
        })
    }

    fn hydrate_row(&self, row: RawDatabaseRow, include_secrets: bool) -> ManagedDatabase {
        let started_at = if row.status == "running" {
            let pg_data = Path::new(&row.data_dir).join("data");
            row.started_at_db
                .or_else(|| read_pg_started_at(&pg_data))
                .or(Some(row.updated_at))
        } else {
            None
        };

        let host = if !row.domain.trim().is_empty() {
            row.domain.trim()
        } else {
            &self.public_ip
        };
        let (db_password, internal_url, external_url) = if include_secrets {
            let raw_pass = self.vault.decrypt(&row.enc_pass).unwrap_or(row.enc_pass);
            let internal_url = build_postgres_url(&row.db_user, &raw_pass, "127.0.0.1", row.port, &row.db_name, false);
            let external_url = build_postgres_url(&row.db_user, &raw_pass, host, row.port, &row.db_name, true);
            (Some(raw_pass), internal_url, external_url)
        } else {
            (None, String::new(), String::new())
        };
        let psql_command = format!("psql -h {} -p {} -U {} -d {}", host, row.port, row.db_user, row.db_name);

        ManagedDatabase {
            id: row.id,
            name: row.name,
            engine: row.engine,
            db_name: row.db_name,
            db_user: row.db_user,
            db_password,
            port: row.port,
            is_superuser: row.is_superuser,
            access_mode: row.access_mode,
            allowed_ips: row.allowed_ips,
            domain: row.domain,
            status: row.status,
            container_id: row.container_id,
            started_at,
            data_dir: row.data_dir,
            linked_project_id: row.linked_project_id,
            linked_project_name: row.linked_project_name,
            node_id: row.node_id,
            internal_url,
            external_url,
            psql_command,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }

    pub async fn list_with_secrets(&self, include_secrets: bool) -> Result<Vec<ManagedDatabase>, String> {
        self.query_databases(None, include_secrets).await
    }

    pub async fn list_overview_summaries(&self) -> Result<Vec<DatabaseOverviewSummary>, String> {
        let rows = sqlx::query(
            "SELECT d.id, d.name, d.port, d.domain, d.status, d.container_id, d.data_dir, d.linked_project_id, d.node_id \
             FROM databases d ORDER BY d.id DESC",
        )
        .fetch_all(&self.db)
        .await
        .map_err(|e| e.to_string())?;

        rows.into_iter()
            .map(|r| {
                Ok(DatabaseOverviewSummary {
                    id: r.try_get(0).map_err(|e| e.to_string())?,
                    name: r.try_get(1).map_err(|e| e.to_string())?,
                    port: r.try_get::<i32, _>(2).map_err(|e| e.to_string())? as u16,
                    domain: r.try_get::<Option<String>, _>(3).map_err(|e| e.to_string())?.unwrap_or_default(),
                    status: r.try_get(4).map_err(|e| e.to_string())?,
                    container_id: r.try_get(5).map_err(|e| e.to_string())?,
                    data_dir: r.try_get(6).map_err(|e| e.to_string())?,
                    linked_project_id: r.try_get(7).map_err(|e| e.to_string())?,
                    node_id: r.try_get(8).map_err(|e| e.to_string())?,
                })
            })
            .collect()
    }

    pub async fn get(&self, id: i64) -> Result<Option<ManagedDatabase>, String> {
        Ok(self.query_databases(Some(id), true).await?.into_iter().next())
    }

    async fn query_databases(
        &self,
        filter_id: Option<i64>,
        include_secrets: bool,
    ) -> Result<Vec<ManagedDatabase>, String> {
        let base_sql = "SELECT d.id, d.name, d.engine, d.db_name, d.db_user, d.db_password, d.port, \
                               d.is_superuser, d.access_mode, d.allowed_ips, d.status, d.container_id, \
                               d.data_dir, d.linked_project_id, p.name, d.domain, d.created_at, d.updated_at, \
                               d.started_at, d.node_id \
                        FROM databases d \
                        LEFT JOIN projects p ON p.id = d.linked_project_id";

        let rows = if let Some(id) = filter_id {
            sqlx::query(&format!("{} WHERE d.id = $1 LIMIT 1", base_sql))
                .bind(id)
                .fetch_all(&self.db)
                .await
        } else {
            sqlx::query(&format!("{} ORDER BY d.id DESC", base_sql))
                .fetch_all(&self.db)
                .await
        }
        .map_err(|e| e.to_string())?;

        let raw_rows: Vec<RawDatabaseRow> = rows
            .iter()
            .filter_map(|r| Self::read_raw_row(r).ok())
            .collect();

        Ok(raw_rows
            .into_iter()
            .map(|r| self.hydrate_row(r, include_secrets))
            .collect())
    }

    pub async fn get_logs(&self, id: i64) -> Result<String, String> {
        let db_rec = self.get(id).await?.ok_or_else(|| "Database not found".to_string())?;
        if let Some((cluster, nid)) = self.remote_node(db_rec.node_id).await {
            let res = self.call_remote(&cluster, nid, "db.logs", serde_json::json!({ "id": id }), 20).await?;
            let text = res["logs"].as_str().unwrap_or("").trim().to_string();
            return Ok(if text.is_empty() { format!("PostgreSQL engine for '{}' is running on port {}. No errors or warnings recorded.", db_rec.name, db_rec.port) } else { text });
        }
        let db_dir = PathBuf::from(&db_rec.data_dir);
        let log_file = db_dir.join("postgresql.log");
        if log_file.is_file() {
            match tokio::fs::read_to_string(&log_file).await {
                Ok(content) => {
                    let trimmed = content.trim();
                    if trimmed.is_empty() {
                        Ok(format!("PostgreSQL engine for '{}' is running on port {}. No errors or warnings recorded.", db_rec.name, db_rec.port))
                    } else {
                        let lines: Vec<&str> = trimmed.lines().collect();
                        if lines.len() > 300 {
                            Ok(lines[lines.len() - 300..].join("\n"))
                        } else {
                            Ok(trimmed.to_string())
                        }
                    }
                }
                Err(e) => Ok(format!("Failed to read log file: {}", e)),
            }
        } else {
            Ok(format!("No log file exists yet for database #{} ({})", id, db_rec.name))
        }
    }

    pub async fn create(&self, input: CreateDatabaseInput) -> Result<ManagedDatabase, String> {
        let name = input.name.trim();
        if name.is_empty() {
            return Err("Database display name is required".to_string());
        }
        let db_name = input.db_name.trim();
        if db_name.is_empty() {
            return Err("Database name is required".to_string());
        }
        let db_user = input.db_user.trim();
        if db_user.is_empty() {
            return Err("Database username is required".to_string());
        }
        let db_password = input.db_password.trim();
        if db_password.is_empty() {
            return Err("Database password is required".to_string());
        }

        input.limits.validate()?;
        let engine = input.engine.unwrap_or_else(|| "postgres".to_string());
        let access_mode = input.access_mode.unwrap_or_else(|| "local".to_string());
        let allowed_ips = input.allowed_ips.unwrap_or_default().trim().to_string();
        let domain = input.domain.unwrap_or_default().trim().to_string();
        let is_superuser = input.is_superuser.unwrap_or(false);

        // Nodes with a port range hand out their lowest open free port; others use the
        // cluster-wide unique allocation starting at 5432.
        // New databases are created on the local Core node.
        let local_node: Option<i64> = sqlx::query_scalar("SELECT id FROM nodes WHERE is_local ORDER BY id LIMIT 1")
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten();
        let port = match local_node {
            Some(nid) if crate::cluster::ports::has_pool(&self.db, nid).await => match input.port.filter(|p| *p > 0) {
                Some(req) => {
                    crate::cluster::ports::check_assignable(&self.db, nid, req, None).await?;
                    req
                }
                None => crate::cluster::ports::suggest_port(&self.db, nid, 5432).await?.port,
            },
            Some(nid) if input.port.filter(|p| *p > 0).is_none() => crate::cluster::ports::suggest_port(&self.db, nid, 5432).await?.port,
            _ => crate::db::allocate_global_unique_port(&self.db, input.port, 5432, None, None).await?,
        };

        let enc_password = self
            .vault
            .encrypt(db_password)
            .map_err(|e| format!("Vault encryption error: {}", e))?;

        let ts = now();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO databases(name, engine, db_name, db_user, db_password, port, is_superuser, access_mode, allowed_ips, domain, status, data_dir, linked_project_id, created_at, updated_at) \
             VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'stopped', '', $11, $12, $13) RETURNING id",
        )
        .bind(name)
        .bind(&engine)
        .bind(db_name)
        .bind(db_user)
        .bind(&enc_password)
        .bind(port as i32)
        .bind(is_superuser)
        .bind(&access_mode)
        .bind(&allowed_ips)
        .bind(&domain)
        .bind(input.linked_project_id)
        .bind(ts)
        .bind(ts)
        .fetch_one(&self.db)
        .await
        .map_err(|e| e.to_string())?;

        let _ = crate::app_limits::set(&self.db, crate::app_limits::Kind::Database, id, &input.limits).await;

        let db_dir = self.base_dir.join(format!("db-{}", id));
        let data_dir_str = db_dir.to_string_lossy().to_string();
        let _ = tokio::fs::create_dir_all(&db_dir).await;

        let _ = sqlx::query("UPDATE databases SET data_dir = $1 WHERE id = $2")
            .bind(&data_dir_str)
            .bind(id)
            .execute(&self.db)
            .await;

        // Apply network firewall rules (UFW/iptables)
        Self::sync_firewall_rules(port, &access_mode, &allowed_ips).await;

        // Initialize and start process container
        match self
            .init_and_start(
                id,
                port,
                db_name,
                db_user,
                db_password,
                is_superuser,
                &access_mode,
                &allowed_ips,
                &db_dir,
            )
            .await
        {
            Ok(cid) => {
                let started_ts = read_pg_started_at(&db_dir.join("data")).unwrap_or_else(now);
                let _ = sqlx::query(
                    "UPDATE databases SET status = 'running', container_id = $1, started_at = $2, updated_at = $3 WHERE id = $4",
                )
                .bind(cid)
                .bind(started_ts)
                .bind(now())
                .bind(id)
                .execute(&self.db)
                .await;
            }
            Err(err) => {
                warn!("Database process container start failed: {}", err);
                let _ = sqlx::query(
                    "UPDATE databases SET status = 'error', updated_at = $1 WHERE id = $2",
                )
                .bind(now())
                .bind(id)
                .execute(&self.db)
                .await;
                return Err(err);
            }
        }

        self.get(id)
            .await?
            .ok_or_else(|| "Failed to retrieve created database".to_string())
    }

    pub async fn start(&self, id: i64) -> Result<(), String> {
        let db_rec = self.get(id).await?.ok_or_else(|| "Database not found".to_string())?;
        if let Some((cluster, nid)) = self.remote_node(db_rec.node_id).await {
            let spec = self.engine_spec(&db_rec).await;
            let res = self.call_remote(&cluster, nid, "db.start", serde_json::json!({ "spec": spec }), 180).await?;
            let cid = res["containerId"].as_str().unwrap_or("remote").to_string();
            return self.mark_running(id, &cid).await;
        }
        let password = db_rec.db_password.unwrap_or_default();
        let db_dir = PathBuf::from(&db_rec.data_dir);

        Self::sync_firewall_rules(db_rec.port, &db_rec.access_mode, &db_rec.allowed_ips).await;

        // Quick check: if postgres is already running and accepting TCP connections, update status immediately!
        let own_pid = read_pg_pid(&db_dir.join("data")).filter(|p| is_process_alive(*p));
        if own_pid.is_some() && tokio::net::TcpStream::connect(format!("127.0.0.1:{}", db_rec.port)).await.is_ok() {
            let pid = own_pid.unwrap_or_else(std::process::id);
            let started_ts = db_rec
                .started_at
                .or_else(|| read_pg_started_at(&db_dir.join("data")))
                .unwrap_or_else(now);
            sqlx::query(
                "UPDATE databases SET status = 'running', container_id = $1, started_at = $2, updated_at = $3 WHERE id = $4",
            )
            .bind(format!("pid-{}", pid))
            .bind(started_ts)
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;
            return Ok(());
        }

        let cid = self
            .init_and_start(
                id,
                db_rec.port,
                &db_rec.db_name,
                &db_rec.db_user,
                &password,
                db_rec.is_superuser,
                &db_rec.access_mode,
                &db_rec.allowed_ips,
                &db_dir,
            )
            .await?;

        let started_ts = read_pg_started_at(&db_dir.join("data")).unwrap_or_else(now);
        sqlx::query(
            "UPDATE databases SET status = 'running', container_id = $1, started_at = $2, updated_at = $3 WHERE id = $4",
        )
        .bind(cid)
        .bind(started_ts)
        .bind(now())
        .bind(id)
        .execute(&self.db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn stop(&self, id: i64) -> Result<(), String> {
        let db_rec = self.get(id).await?.ok_or_else(|| "Database not found".to_string())?;
        if let Some((cluster, nid)) = self.remote_node(db_rec.node_id).await {
            self.call_remote(&cluster, nid, "db.stop", serde_json::json!({ "id": id, "port": db_rec.port }), 60).await?;
            return self.mark_stopped(id).await;
        }
        stop_engine(&PathBuf::from(&db_rec.data_dir), db_rec.port).await;

        sqlx::query("UPDATE databases SET status = 'stopped', started_at = NULL, updated_at = $1 WHERE id = $2")
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn restart(&self, id: i64) -> Result<(), String> {
        self.stop(id).await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        self.start(id).await
    }

    /// Moves a database to another port: a running one is stopped, its firewall rules follow the
    /// port, and it is started again on the new one.
    pub async fn set_port(&self, id: i64, port: u16) -> Result<(), String> {
        let rec = self.get(id).await?.ok_or_else(|| "Database not found".to_string())?;
        if rec.port == port {
            return Ok(());
        }
        let was_running = rec.status == "running";
        if was_running {
            self.stop(id).await?;
        }
        let remote = self.remote_node(rec.node_id).await;
        if let Some((cluster, nid)) = &remote {
            let params = serde_json::json!({ "id": id, "port": rec.port, "allowedIps": rec.allowed_ips, "removeFirewall": true });
            let _ = self.call_remote(cluster, *nid, "db.stop", params, 60).await;
        } else {
            Self::remove_firewall_rules(rec.port, &rec.allowed_ips).await;
        }
        sqlx::query("UPDATE databases SET port = $1, updated_at = $2 WHERE id = $3")
            .bind(i32::from(port))
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        if remote.is_none() {
            Self::sync_firewall_rules(port, &rec.access_mode, &rec.allowed_ips).await;
        }
        if was_running {
            self.start(id).await?;
        }
        Ok(())
    }

    pub async fn delete(&self, id: i64) -> Result<(), String> {
        let db_rec = self.get(id).await?.ok_or_else(|| "Database not found".to_string())?;
        if let Some((cluster, nid)) = self.remote_node(db_rec.node_id).await {
            let params = serde_json::json!({ "id": id, "port": db_rec.port, "allowedIps": db_rec.allowed_ips });
            self.call_remote(&cluster, nid, "db.delete", params, 120).await?;
            sqlx::query("DELETE FROM databases WHERE id = $1").bind(id).execute(&self.db).await.map_err(|e| e.to_string())?;
            return Ok(());
        }

        // 1. Stop PostgreSQL instance
        let _ = self.stop(id).await;

        // 2. Remove firewall rules
        Self::remove_firewall_rules(db_rec.port, &db_rec.allowed_ips).await;

        // 3. Remove physical data directory
        let db_dir = PathBuf::from(&db_rec.data_dir);
        if db_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&db_dir).await;
        }

        // 4. Delete database row
        sqlx::query("DELETE FROM databases WHERE id = $1")
            .bind(id)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    pub async fn set_linked_project(&self, id: i64, project_id: Option<i64>) -> Result<(), String> {
        sqlx::query("UPDATE databases SET linked_project_id = $1, updated_at = $2 WHERE id = $3")
            .bind(project_id)
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // --- Isolated Process Container Lifecycle ---
    #[allow(clippy::too_many_arguments)]
    async fn init_and_start(
        &self,
        id: i64,
        port: u16,
        db_name: &str,
        db_user: &str,
        db_pass: &str,
        is_superuser: bool,
        access_mode: &str,
        allowed_ips: &str,
        db_dir: &Path,
    ) -> Result<String, String> {
        let limits = crate::app_limits::get(&self.db, crate::app_limits::Kind::Database, id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let spec = EngineSpec {
            id,
            port,
            db_name: db_name.to_string(),
            db_user: db_user.to_string(),
            db_password: db_pass.to_string(),
            is_superuser,
            access_mode: access_mode.to_string(),
            allowed_ips: allowed_ips.to_string(),
            db_dir: db_dir.to_path_buf(),
            cpu: limits.cpu,
            memory_mb: limits.memory_mb,
        };
        let logs = self.logs.clone();
        start_engine(&spec, &move |level, msg| {
            logs.add(None, None, "database", level, msg);
        })
        .await
    }

    // --- Firewall Synchronization (UFW / iptables) ---
    pub async fn sync_firewall_rules(port: u16, access_mode: &str, allowed_ips: &str) {
        security::sync_firewall_rules(port, access_mode, allowed_ips).await;
    }

    pub async fn remove_firewall_rules(port: u16, allowed_ips: &str) {
        security::remove_firewall_rules(port, allowed_ips).await;
    }
}

/// Everything needed to start a database engine, on this server or on a worker node.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSpec {
    pub id: i64,
    pub port: u16,
    pub db_name: String,
    pub db_user: String,
    pub db_password: String,
    pub is_superuser: bool,
    pub access_mode: String,
    pub allowed_ips: String,
    pub db_dir: PathBuf,
    pub cpu: i32,
    pub memory_mb: i32,
}

/// Initializes the data cluster when it is new, applies access rules and starts PostgreSQL.
/// `log` receives (level, message). Returns the container id (`pid-N`).
pub async fn start_engine(spec: &EngineSpec, log: &(dyn Fn(&str, &str) + Send + Sync)) -> Result<String, String> {
    let EngineSpec { id, port, db_dir, .. } = spec;
    let (id, port, db_dir) = (*id, *port, db_dir.as_path());
    let (db_name, db_user, db_pass) = (spec.db_name.as_str(), spec.db_user.as_str(), spec.db_password.as_str());
    let (is_superuser, access_mode, allowed_ips) = (spec.is_superuser, spec.access_mode.as_str(), spec.allowed_ips.as_str());

    let pg_bins = PgBinaries::detect()?;
    let data_dir = db_dir.join("data");
    let log_file = db_dir.join("postgresql.log");

    log("info", &format!("[DB #{}] Preparing isolated sandbox for '{}' on port {}...", id, db_name, port));

    // 1. Initialize cluster if not already initialized
    let is_new = is_new_cluster(&data_dir);
    if is_new {
        // Ensure data directory is empty before running initdb
        if data_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&data_dir).await;
        }
        let _ = tokio::fs::create_dir_all(&data_dir).await;

        let pw_tmp = db_dir.join("pw.tmp");
        tokio::fs::write(&pw_tmp, db_pass)
            .await
            .map_err(|e| format!("Failed to write temporary initdb credential: {}", e))?;

        let pw_str = to_clean_path_str(&pw_tmp);
        let data_str = to_clean_path_str(&data_dir);

        log("info", &format!("[DB #{}] Initializing PostgreSQL data cluster via initdb (SCRAM-SHA-256)...", id));

        ensure_pg_permissions(db_dir, Some(&data_dir));
        let mut init_cmd = make_isolated_pg_command(
            &pg_bins.initdb,
            &[
                "-D",
                &data_str,
                "-U",
                db_user,
                "-A",
                "scram-sha-256",
                "--pwfile",
                &pw_str,
                "-E",
                "UTF8",
                "--no-locale",
            ],
            db_dir,
        );

        let out = init_cmd.output().await.map_err(|e| {
            format!("Failed to execute initdb: {}", e)
        })?;

        let _ = tokio::fs::remove_file(&pw_tmp).await;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            return Err(format!("initdb failed: {} {}", stdout.trim(), stderr.trim()));
        }

        // Append WebDep configuration to postgresql.conf
        let conf_path = data_dir.join("postgresql.conf");
        let extra_conf = format!(
            "\n# --- WebDep Isolated Process Container Settings ---\nlisten_addresses = '*'\nport = {}\nmax_connections = 100\n",
            port
        );
        if let Ok(mut f) = tokio::fs::OpenOptions::new().append(true).open(&conf_path).await {
            use tokio::io::AsyncWriteExt;
            let _ = f.write_all(extra_conf.as_bytes()).await;
        }
    }

    // 2. Configure pg_hba.conf rules
    log("info", &format!("[DB #{}] Applying network firewall & client authentication rules (mode: '{}')...", id, access_mode));
    let hba_content = generate_pg_hba(access_mode, allowed_ips);
    let hba_path = data_dir.join("pg_hba.conf");
    let _ = tokio::fs::write(&hba_path, hba_content).await;

    // 3. Start PostgreSQL using pg_ctl
    log("info", &format!("[DB #{}] Spawning isolated PostgreSQL engine process on port {}...", id, port));
    let data_str = to_clean_path_str(&data_dir);
    let log_str = to_clean_path_str(&log_file);
    // Pin the port in postgresql.conf (passing '-o "-p N"' is mangled by Windows argument quoting)
    {
        let conf_path = data_dir.join("postgresql.conf");
        if let Ok(existing) = tokio::fs::read_to_string(&conf_path).await {
            let mut cleaned: Vec<&str> = existing
                .lines()
                .filter(|l| !l.trim_start().starts_with("port =") && !l.trim_start().starts_with("port="))
                .collect();
            let pin = format!("port = {}", port);
            cleaned.push(&pin);
            let _ = tokio::fs::write(&conf_path, cleaned.join("\n") + "\n").await;
        }
    }

    // Check if there is a stale postmaster.pid from an unexpected termination
    let pid_file = data_dir.join("postmaster.pid");
    if pid_file.exists() {
        if let Some(old_pid) = read_pg_pid(&data_dir) {
            if is_process_alive(old_pid) {
                info!("PostgreSQL for db #{} is already running (PID: {})", id, old_pid);
                return Ok(format!("pid-{}", old_pid));
            }
        }
        let _ = tokio::fs::remove_file(&pid_file).await;
    }

    crate::databases::security::ensure_tcp_only_pg_conf(&data_dir).await;
    ensure_pg_permissions(db_dir, Some(&data_dir));
    rotate_pg_log(&log_file).await;
    let mut start_cmd = make_isolated_pg_command(
        &pg_bins.pg_ctl,
        &[
            "-D",
            &data_str,
            "-l",
            &log_str,
            "-W",
            "start",
        ],
        db_dir,
    );
    start_cmd.stdin(std::process::Stdio::null());
    start_cmd.stdout(std::process::Stdio::null());
    start_cmd.stderr(std::process::Stdio::null());

    // CPU and memory limits: pg_ctl joins the database's cgroup, and the server it
    // starts stays in it (Linux with cgroups v2; elsewhere the database runs unlimited).
    let limits = crate::app_limits::AppLimits { cpu: spec.cpu, memory_mb: spec.memory_mb, ..Default::default() };
    match crate::runtime::limits::prepare(&format!("db-{id}"), limits.cpu, limits.memory_mb) {
        #[cfg(target_os = "linux")]
        Ok(Some(procs)) => {
            // SAFETY: join_in_child only uses async-signal-safe calls.
            unsafe {
                start_cmd.pre_exec(move || crate::runtime::limits::join_in_child(&procs));
            }
        }
        Err(e) => warn!("[DB #{}] Resource limits not applied: {}. The database runs without limits.", id, e),
        _ => {}
    }

    let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
        // Windows: pg_ctl and the server it starts run in a job with the database's limits.
        #[cfg(windows)]
        {
            if limits.cpu > 0 || limits.memory_mb > 0 {
                if let Ok(mut child) = start_cmd.spawn() {
                    if let Some(handle) = child.raw_handle() {
                        if let Err(e) = crate::runtime::limits::apply_to_process(handle, limits.cpu, limits.memory_mb) {
                            warn!("[DB #{}] Resource limits not applied: {}. The database runs without limits.", id, e);
                        }
                    }
                    return child.wait().await;
                }
            }
        }
        start_cmd.status().await
    })
    .await;

    let pid = match wait_for_pg_ready(port, &data_dir, 20000).await {
        Some(p) => p,
        None => {
            let tail = tokio::fs::read_to_string(&log_file)
                .await
                .map(|s| {
                    let lines: Vec<&str> = s.lines().collect();
                    lines[lines.len().saturating_sub(8)..].join(" | ")
                })
                .unwrap_or_default();
            let foreign = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await.is_ok();
            let reason = if foreign {
                format!("Port {} is already used by another program (for example a system PostgreSQL service). Stop it or delete and recreate this database so it gets a free port.", port)
            } else {
                format!("PostgreSQL failed to start on port {}.", port)
            };
            return Err(format!("{} Log: {}", reason, tail));
        }
    };
    log("info", &format!("[DB #{}] PostgreSQL engine running (PID: {}) on port {}. Ready for connections.", id, pid, port));

    // 4. Ensure custom database exists and is owned by db_user (only for newly initialized clusters)
    if is_new && db_name != "postgres" && !pg_database_exists(&pg_bins.psql, port, db_user, db_pass, db_name).await {
        let sql = format!(
            r#"CREATE DATABASE "{}" OWNER "{}";"#,
            escape_pg_ident(db_name),
            escape_pg_ident(db_user)
        );
        let _ = run_psql_command(&pg_bins.psql, port, db_user, db_pass, &sql, 6).await;
    }

    // 5. Restrict superuser privileges if is_superuser is false and cluster is new
    if is_new && !is_superuser {
        let sql = format!(
            r#"ALTER ROLE "{}" NOSUPERUSER NOCREATEDB NOCREATEROLE;"#,
            escape_pg_ident(db_user)
        );
        let _ = run_psql_command(&pg_bins.psql, port, db_user, db_pass, &sql, 4).await;
    }

    info!(
        "Started isolated PostgreSQL process container for db #{} on port {} (PID: {})",
        id, port, pid
    );
    Ok(format!("pid-{}", pid))
}

/// Stops the PostgreSQL server of a database folder, killing leftovers on its port.
pub async fn stop_engine(db_dir: &Path, port: u16) {
    let data_dir = db_dir.join("data");
    if let Ok(pg_bins) = PgBinaries::detect() {
        let data_str = to_clean_path_str(&data_dir);
        let mut stop_cmd = make_isolated_pg_command(&pg_bins.pg_ctl, &["-D", &data_str, "-m", "fast", "stop"], db_dir);
        stop_cmd.stdin(std::process::Stdio::null());
        stop_cmd.stdout(std::process::Stdio::null());
        stop_cmd.stderr(std::process::Stdio::null());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(3), stop_cmd.status()).await;
    }

    let mut pids_to_kill = Vec::new();
    if let Some(pid) = read_pg_pid(&data_dir) {
        pids_to_kill.push(pid);
    }
    for port_pid in crate::runtime::Runtime::find_pids_on_port(port) {
        pids_to_kill.push(port_pid);
    }
    for pid in pids_to_kill {
        // A stale postmaster.pid or another service on the port must never be killed.
        if crate::runtime::tree::pid_command_contains(pid, "postgres") {
            crate::runtime::Runtime::kill_pid_tree(pid).await;
        }
    }
    let pid_file = data_dir.join("postmaster.pid");
    if pid_file.exists() {
        let _ = tokio::fs::remove_file(&pid_file).await;
    }
}

/// The PID of a running engine in `db_dir`, when it answers on `port`.
pub async fn engine_pid(db_dir: &Path, port: u16) -> Option<u32> {
    let pid = read_pg_pid(&db_dir.join("data")).filter(|p| is_process_alive(*p))?;
    tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await.ok()?;
    Some(pid)
}

fn escape_pg_ident(ident: &str) -> String {
    ident.replace('"', "\"\"")
}

fn escape_pg_literal(lit: &str) -> String {
    lit.replace('\'', "''")
}

/// Returns `true` only when `PG_VERSION` is genuinely absent (NotFound), so permission errors
/// on an already-initialized cluster never trigger a destructive re-initialization.
fn is_new_cluster(data_dir: &Path) -> bool {
    matches!(
        std::fs::metadata(data_dir.join("PG_VERSION")),
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

async fn rotate_pg_log(log_file: &Path) {
    if let Ok(meta) = tokio::fs::metadata(log_file).await {
        if meta.len() > 0 {
            let prev = log_file.with_extension("log.prev");
            let _ = tokio::fs::remove_file(&prev).await;
            let _ = tokio::fs::rename(log_file, &prev).await;
        }
    }
}

async fn run_psql_command(
    psql: &Path,
    port: u16,
    db_user: &str,
    db_pass: &str,
    sql: &str,
    timeout_secs: u64,
) -> Option<std::process::Output> {
    let mut psql_cmd = Command::new(psql);
    psql_cmd.args([
        "-h",
        "127.0.0.1",
        "-p",
        &port.to_string(),
        "-U",
        db_user,
        "-d",
        "postgres",
        "-tAc",
        sql,
    ]);
    psql_cmd.stdin(std::process::Stdio::null());
    psql_cmd.env("PGPASSWORD", db_pass);
    tokio::time::timeout(tokio::time::Duration::from_secs(timeout_secs), psql_cmd.output())
        .await
        .ok()
        .and_then(|r| r.ok())
}

async fn pg_database_exists(
    psql: &Path,
    port: u16,
    db_user: &str,
    db_pass: &str,
    db_name: &str,
) -> bool {
    let sql = format!(
        "SELECT 1 FROM pg_database WHERE datname = '{}';",
        escape_pg_literal(db_name)
    );
    match run_psql_command(psql, port, db_user, db_pass, &sql, 4).await {
        Some(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim() == "1",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pg_escaping_and_cluster_detection() {
        assert_eq!(escape_pg_ident(r#"mew"zik"#), r#"mew""zik"#);
        assert_eq!(escape_pg_literal("mew'zik"), "mew''zik");

        let tmp = std::env::temp_dir().join(format!("webdep_pg_new_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(is_new_cluster(&tmp));
        std::fs::write(tmp.join("PG_VERSION"), "18\n").unwrap();
        assert!(!is_new_cluster(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

