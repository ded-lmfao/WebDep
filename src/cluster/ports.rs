//! Port pools per node.
//!
//! An admin gives each node a port range (and can add single ports later). Every port in
//! the pool is tracked as open or closed; open ports are allowed in the node's firewall and
//! handed out to apps lowest-first. A port is "in use" when it is an app's main port
//! (`projects.port` / `projects.assigned_port`), a database port, or an extra port reserved
//! for an app (`node_ports.project_id`).
//!
//! Nodes without a pool keep the original cluster-wide allocation (`db::allocate_global_unique_port`).

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use sqlx::Row;

use crate::db::DbPool;

pub const MIN_POOL_PORT: u16 = 1024;
/// Largest range accepted in one go (keeps the table and firewall sync small).
pub const MAX_RANGE_SIZE: u32 = 5000;
/// Ports the panel itself or the OS commonly needs; never handed out.
const RESERVED: &[u16] = &[4600, 5431];

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PortUser {
    /// `website`, `service`, or `database`.
    pub kind: String,
    pub id: i64,
    pub name: String,
    /// `main` (the app's own port) or `extra` (reserved additional port).
    pub role: String,
    pub running: bool,
    /// The app's own access mode for its main port: `local`, `public`, or `allowlist`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolPort {
    pub port: u16,
    pub open: bool,
    /// Reachable from the internet (firewall allows it). Private ports are only reachable
    /// from this machine and the mesh, which is all Caddy needs.
    pub public: bool,
    pub manual: bool,
    pub in_pool: bool,
    pub used_by: Option<PortUser>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodePool {
    pub node_id: i64,
    pub port_min: Option<u16>,
    pub port_max: Option<u16>,
    pub ports: Vec<PoolPort>,
    pub firewall: FirewallStatus,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FirewallStatus {
    /// `ok`, `pending` (changes not on the node yet), or `failed`.
    pub state: String,
    pub error: Option<String>,
    pub synced_at: Option<i64>,
}

pub fn validate_range(min: u16, max: u16) -> Result<(), String> {
    if min < MIN_POOL_PORT {
        return Err(format!("Ports below {MIN_POOL_PORT} are reserved for the system."));
    }
    if max < min {
        return Err("The highest port must be at least the lowest port.".into());
    }
    if u32::from(max - min) + 1 > MAX_RANGE_SIZE {
        return Err(format!("A range can hold at most {MAX_RANGE_SIZE} ports."));
    }
    Ok(())
}

/// Parses "8100, 8105-8110" into a sorted list of ports.
pub fn parse_port_list(raw: &str) -> Result<Vec<u16>, String> {
    let mut out = BTreeSet::new();
    for part in raw.split(|c: char| c == ',' || c.is_whitespace()).map(str::trim).filter(|s| !s.is_empty()) {
        let bad = || format!("'{part}' isn't a port or a range like 8100-8110.");
        if let Some((a, b)) = part.split_once('-') {
            let (a, b): (u16, u16) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
            validate_range(a, b)?;
            out.extend(a..=b);
        } else {
            let p: u16 = part.parse().map_err(|_| bad())?;
            validate_range(p, p)?;
            out.insert(p);
        }
        if out.len() as u32 > MAX_RANGE_SIZE {
            return Err(format!("Add at most {MAX_RANGE_SIZE} ports at a time."));
        }
    }
    if out.is_empty() {
        return Err("Enter at least one port.".into());
    }
    Ok(out.into_iter().collect())
}

pub async fn has_pool(db: &DbPool, node_id: i64) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM node_ports WHERE node_id = $1)")
        .bind(node_id)
        .fetch_one(db)
        .await
        .unwrap_or(false)
}

/// Every port in use on a node, with who uses it.
async fn usage_on_node(db: &DbPool, node_id: i64, local_node: bool) -> Result<HashMap<u16, PortUser>, sqlx::Error> {
    let mut used = HashMap::new();
    let projects = sqlx::query(
        "SELECT id, name, project_kind, COALESCE(assigned_port, port), app_status, access_mode FROM projects
         WHERE node_id = $1 OR ($2 AND node_id IS NULL)",
    )
    .bind(node_id)
    .bind(local_node)
    .fetch_all(db)
    .await?;
    for r in projects {
        let port: Option<i32> = r.try_get(3)?;
        if let Some(port) = port.and_then(|p| u16::try_from(p).ok()) {
            let kind: String = r.try_get(2)?;
            let status: String = r.try_get(4)?;
            used.insert(
                port,
                PortUser {
                    kind: if kind == "service" { "service".into() } else { "website".into() },
                    id: r.try_get(0)?,
                    name: r.try_get(1)?,
                    role: "main".into(),
                    running: status == "running" || status == "maintenance",
                    access: r.try_get::<Option<String>, _>(5)?,
                },
            );
        }
    }
    let databases = sqlx::query(
        "SELECT id, name, port, status FROM databases WHERE node_id = $1 OR ($2 AND node_id IS NULL)",
    )
    .bind(node_id)
    .bind(local_node)
    .fetch_all(db)
    .await?;
    for r in databases {
        let port: i32 = r.try_get(2)?;
        if let Ok(port) = u16::try_from(port) {
            let status: String = r.try_get(3)?;
            used.insert(
                port,
                PortUser { kind: "database".into(), id: r.try_get(0)?, name: r.try_get(1)?, role: "main".into(), running: status == "running", access: None },
            );
        }
    }
    let extras = sqlx::query(
        "SELECT np.port, p.id, p.name, p.project_kind, p.app_status FROM node_ports np
         JOIN projects p ON p.id = np.project_id WHERE np.node_id = $1",
    )
    .bind(node_id)
    .fetch_all(db)
    .await?;
    for r in extras {
        let port: i32 = r.try_get(0)?;
        if let Ok(port) = u16::try_from(port) {
            let kind: String = r.try_get(3)?;
            let status: String = r.try_get(4)?;
            used.insert(
                port,
                PortUser {
                    kind: if kind == "service" { "service".into() } else { "website".into() },
                    id: r.try_get(1)?,
                    name: r.try_get(2)?,
                    role: "extra".into(),
                    running: status == "running" || status == "maintenance",
                    access: None,
                },
            );
        }
    }
    let db_extras = sqlx::query(
        "SELECT np.port, d.id, d.name, d.status FROM node_ports np
         JOIN databases d ON d.id = np.database_id WHERE np.node_id = $1",
    )
    .bind(node_id)
    .fetch_all(db)
    .await?;
    for r in db_extras {
        let port: i32 = r.try_get(0)?;
        if let Ok(port) = u16::try_from(port) {
            let status: String = r.try_get(3)?;
            used.insert(
                port,
                PortUser { kind: "database".into(), id: r.try_get(1)?, name: r.try_get(2)?, role: "extra".into(), running: status == "running", access: None },
            );
        }
    }
    Ok(used)
}

/// Ports used by any app or database anywhere (ports stay unique cluster-wide).
async fn used_anywhere(db: &DbPool) -> Result<BTreeSet<u16>, sqlx::Error> {
    let mut set: BTreeSet<u16> = RESERVED.iter().copied().collect();
    let rows: Vec<Option<i32>> = sqlx::query_scalar(
        "SELECT port FROM projects UNION ALL SELECT assigned_port FROM projects
         UNION ALL SELECT port FROM databases UNION ALL SELECT port FROM node_ports WHERE project_id IS NOT NULL OR database_id IS NOT NULL",
    )
    .fetch_all(db)
    .await?;
    set.extend(rows.into_iter().flatten().filter_map(|p| u16::try_from(p).ok()));
    Ok(set)
}

async fn is_local(db: &DbPool, node_id: i64) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT is_local FROM nodes WHERE id = $1")
        .bind(node_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

pub async fn load_pool(db: &DbPool, node_id: i64) -> Result<NodePool, sqlx::Error> {
    let range: Option<(Option<i32>, Option<i32>)> = sqlx::query_as("SELECT port_min, port_max FROM nodes WHERE id = $1")
        .bind(node_id)
        .fetch_optional(db)
        .await?;
    let (min, max) = range.unwrap_or((None, None));
    let mut usage = usage_on_node(db, node_id, is_local(db, node_id).await).await?;
    let rows = sqlx::query("SELECT port, is_open, manual, public FROM node_ports WHERE node_id = $1 ORDER BY port")
        .bind(node_id)
        .fetch_all(db)
        .await?;
    let mut ports = Vec::with_capacity(rows.len());
    for r in rows {
        let port: i32 = r.try_get(0)?;
        let Ok(port) = u16::try_from(port) else { continue };
        ports.push(PoolPort {
            port,
            open: r.try_get(1)?,
            public: r.try_get(3)?,
            manual: r.try_get(2)?,
            in_pool: true,
            used_by: usage.remove(&port),
        });
    }
    // Apps on ports outside the pool (set before the pool existed): still listed.
    for (port, user) in usage {
        ports.push(PoolPort { port, open: true, public: false, manual: false, in_pool: false, used_by: Some(user) });
    }
    ports.sort_by_key(|p| p.port);
    let fw: Option<(String, Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT firewall_state, firewall_error, firewall_synced_at FROM nodes WHERE id = $1")
            .bind(node_id)
            .fetch_optional(db)
            .await?;
    let (state, error, synced_at) = fw.unwrap_or_else(|| ("pending".into(), None, None));
    Ok(NodePool {
        node_id,
        port_min: min.and_then(|v| u16::try_from(v).ok()),
        port_max: max.and_then(|v| u16::try_from(v).ok()),
        ports,
        firewall: FirewallStatus { state, error, synced_at },
    })
}

/// Sets the node's range. New ports start open; range ports that fall outside the new range
/// are dropped unless in use. Returns the ports that were added and removed.
pub async fn set_range(db: &DbPool, node_id: i64, min: u16, max: u16) -> Result<(Vec<u16>, Vec<u16>), String> {
    validate_range(min, max)?;
    let err = |e: sqlx::Error| e.to_string();
    let mut tx = db.begin().await.map_err(err)?;
    let existing: Vec<i32> = sqlx::query_scalar("SELECT port FROM node_ports WHERE node_id = $1")
        .bind(node_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(err)?;
    let existing: BTreeSet<u16> = existing.into_iter().filter_map(|p| u16::try_from(p).ok()).collect();
    let ts = now_secs();
    let mut added = Vec::new();
    for port in min..=max {
        if !existing.contains(&port) && !RESERVED.contains(&port) {
            sqlx::query("INSERT INTO node_ports(node_id, port, is_open, manual, updated_at) VALUES($1, $2, TRUE, FALSE, $3)")
                .bind(node_id)
                .bind(i32::from(port))
                .bind(ts)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
            added.push(port);
        }
    }
    let usage = usage_on_node(db, node_id, is_local(db, node_id).await).await.map_err(err)?;
    let removed: Vec<i32> = sqlx::query_scalar(
        "DELETE FROM node_ports WHERE node_id = $1 AND manual = FALSE AND project_id IS NULL AND database_id IS NULL
         AND (port < $2 OR port > $3) AND NOT (port = ANY($4)) RETURNING port",
    )
    .bind(node_id)
    .bind(i32::from(min))
    .bind(i32::from(max))
    .bind(usage.keys().map(|p| i32::from(*p)).collect::<Vec<i32>>())
    .fetch_all(&mut *tx)
    .await
    .map_err(err)?;
    sqlx::query("UPDATE nodes SET port_min = $2, port_max = $3, firewall_state = CASE WHEN firewall_state = 'ok' THEN 'pending' ELSE firewall_state END WHERE id = $1")
        .bind(node_id)
        .bind(i32::from(min))
        .bind(i32::from(max))
        .execute(&mut *tx)
        .await
        .map_err(err)?;
    tx.commit().await.map_err(err)?;
    Ok((added, removed.into_iter().filter_map(|p| u16::try_from(p).ok()).collect()))
}

/// Adds single ports (open) to a node's pool. Returns the ports that were new.
pub async fn add_ports(db: &DbPool, node_id: i64, ports: &[u16]) -> Result<Vec<u16>, String> {
    let ts = now_secs();
    let mut added = Vec::new();
    for &port in ports {
        if RESERVED.contains(&port) {
            continue;
        }
        let res = sqlx::query(
            "INSERT INTO node_ports(node_id, port, is_open, manual, updated_at) VALUES($1, $2, TRUE, TRUE, $3)
             ON CONFLICT (node_id, port) DO NOTHING",
        )
        .bind(node_id)
        .bind(i32::from(port))
        .bind(ts)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        if res.rows_affected() > 0 {
            added.push(port);
        }
    }
    Ok(added)
}

/// Opens or closes pool ports. Closing a port an app uses is refused.
pub async fn set_open(db: &DbPool, node_id: i64, ports: &[u16], open: bool) -> Result<Vec<u16>, String> {
    if !open {
        let usage = usage_on_node(db, node_id, is_local(db, node_id).await).await.map_err(|e| e.to_string())?;
        if let Some((port, user)) = ports.iter().find_map(|p| usage.get(p).map(|u| (*p, u))) {
            return Err(format!("Port {port} is used by {}. Move it to another port first.", user.name));
        }
    }
    let changed: Vec<i32> = sqlx::query_scalar(
        "UPDATE node_ports SET is_open = $3, updated_at = $4 WHERE node_id = $1 AND port = ANY($2) AND is_open <> $3 RETURNING port",
    )
    .bind(node_id)
    .bind(ports.iter().map(|p| i32::from(*p)).collect::<Vec<i32>>())
    .bind(open)
    .bind(now_secs())
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(changed.into_iter().filter_map(|p| u16::try_from(p).ok()).collect())
}

/// Message the API and the panel use when a node has no free port left.
pub const NO_FREE_PORT: &str = "No free ports available";

pub fn no_free_port_message(node_name: &str) -> String {
    format!("{NO_FREE_PORT} on {node_name}. Add or open more ports in Admin Panel > IP Addresses, then try again.")
}

/// Removes ports from a node's list (range or hand-added). Ports an app uses are refused.
/// Returns the ports that were removed.
pub async fn remove_ports(db: &DbPool, node_id: i64, ports: &[u16]) -> Result<Vec<u16>, String> {
    let usage = usage_on_node(db, node_id, is_local(db, node_id).await).await.map_err(|e| e.to_string())?;
    if let Some((port, user)) = ports.iter().find_map(|p| usage.get(p).map(|u| (*p, u))) {
        return Err(format!("Port {port} is used by {}. Move it to another port first.", user.name));
    }
    let removed: Vec<i32> = sqlx::query_scalar(
        "DELETE FROM node_ports WHERE node_id = $1 AND port = ANY($2) AND project_id IS NULL AND database_id IS NULL RETURNING port",
    )
    .bind(node_id)
    .bind(ports.iter().map(|p| i32::from(*p)).collect::<Vec<i32>>())
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(removed.into_iter().filter_map(|p| u16::try_from(p).ok()).collect())
}

/// (free, open) counts for a node's pool: open ports, and those nothing uses yet.
pub async fn free_and_open_counts(db: &DbPool, node_id: i64) -> Result<(usize, usize), sqlx::Error> {
    let used = used_anywhere(db).await?;
    let open: Vec<i32> = sqlx::query_scalar("SELECT port FROM node_ports WHERE node_id = $1 AND is_open")
        .bind(node_id)
        .fetch_all(db)
        .await?;
    let free = open.iter().filter_map(|p| u16::try_from(*p).ok()).filter(|p| !used.contains(p)).count();
    Ok((free, open.len()))
}

/// Lowest open pool port that nothing uses, skipping `exclude`.
pub async fn next_free_port(db: &DbPool, node_id: i64, exclude: &[u16]) -> Result<Option<u16>, sqlx::Error> {
    let used = used_anywhere(db).await?;
    let open: Vec<i32> = sqlx::query_scalar("SELECT port FROM node_ports WHERE node_id = $1 AND is_open ORDER BY port")
        .bind(node_id)
        .fetch_all(db)
        .await?;
    Ok(open
        .into_iter()
        .filter_map(|p| u16::try_from(p).ok())
        .find(|p| !used.contains(p) && !exclude.contains(p)))
}

/// Who may keep a port it already holds.
#[derive(Debug, Clone, Copy)]
pub enum Owner {
    Nobody,
    Project(i64),
    Database(i64),
}

/// Checks that `port` is an open, unused pool port on the node (for a manual choice).
/// `allow_project` lets an app keep a port it already holds.
pub async fn check_assignable(db: &DbPool, node_id: i64, port: u16, allow_project: Option<i64>) -> Result<(), String> {
    check_assignable_for(db, node_id, port, allow_project.map_or(Owner::Nobody, Owner::Project)).await
}

/// Same as `check_assignable`, for an app or a database that may keep its own port.
pub async fn check_assignable_for(db: &DbPool, node_id: i64, port: u16, owner: Owner) -> Result<(), String> {
    let open: Option<bool> = sqlx::query_scalar("SELECT is_open FROM node_ports WHERE node_id = $1 AND port = $2")
        .bind(node_id)
        .bind(i32::from(port))
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
    match open {
        None => return Err(format!("Port {port} isn't in this node's port list. Add it first.")),
        Some(false) => return Err(format!("Port {port} is closed. Open it first.")),
        Some(true) => {}
    }
    let usage = usage_on_node(db, node_id, is_local(db, node_id).await).await.map_err(|e| e.to_string())?;
    if let Some(user) = usage.get(&port) {
        let own = match owner {
            Owner::Nobody => false,
            Owner::Project(pid) => user.kind != "database" && user.id == pid,
            Owner::Database(did) => user.kind == "database" && user.id == did,
        };
        if !own {
            return Err(format!("Port {port} is already used by {}.", user.name));
        }
    }
    if used_anywhere(db).await.map_err(|e| e.to_string())?.contains(&port) && !usage.contains_key(&port) {
        return Err(format!("Port {port} is already used by an app on another node."));
    }
    Ok(())
}

/// Replaces an app's extra ports. Each must be an open pool port nobody else uses.
pub async fn set_extra_ports(db: &DbPool, node_id: i64, project_id: i64, ports: &[u16]) -> Result<(), String> {
    replace_extra_ports(db, node_id, Owner::Project(project_id), ports).await
}

/// Replaces a database's extra ports.
pub async fn set_database_extra_ports(db: &DbPool, node_id: i64, database_id: i64, ports: &[u16]) -> Result<(), String> {
    replace_extra_ports(db, node_id, Owner::Database(database_id), ports).await
}

async fn replace_extra_ports(db: &DbPool, node_id: i64, owner: Owner, ports: &[u16]) -> Result<(), String> {
    // The column is picked from this fixed pair, never from input.
    let (column, id) = match owner {
        Owner::Project(id) => ("project_id", id),
        Owner::Database(id) => ("database_id", id),
        Owner::Nobody => return Err("No owner for these ports.".into()),
    };
    for &p in ports {
        let held: Option<Option<i64>> = sqlx::query_scalar(&format!("SELECT {column} FROM node_ports WHERE node_id = $1 AND port = $2"))
            .bind(node_id)
            .bind(i32::from(p))
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?;
        if held.flatten() != Some(id) {
            check_assignable_for(db, node_id, p, Owner::Nobody).await?;
        }
    }
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    sqlx::query(&format!("UPDATE node_ports SET {column} = NULL, updated_at = $2 WHERE {column} = $1"))
        .bind(id)
        .bind(now_secs())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query(&format!("UPDATE node_ports SET {column} = $2, updated_at = $4 WHERE node_id = $1 AND port = ANY($3)"))
        .bind(node_id)
        .bind(id)
        .bind(ports.iter().map(|p| i32::from(*p)).collect::<Vec<i32>>())
        .bind(now_secs())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// Marks pool ports public (reachable from the internet) or private. Returns changed ports.
pub async fn set_public(db: &DbPool, node_id: i64, ports: &[u16], public: bool) -> Result<Vec<u16>, String> {
    let changed: Vec<i32> = sqlx::query_scalar(
        "UPDATE node_ports SET public = $3, updated_at = $4 WHERE node_id = $1 AND port = ANY($2) AND public <> $3 RETURNING port",
    )
    .bind(node_id)
    .bind(ports.iter().map(|p| i32::from(*p)).collect::<Vec<i32>>())
    .bind(public)
    .bind(now_secs())
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(changed.into_iter().filter_map(|p| u16::try_from(p).ok()).collect())
}

/// Splits ports into (allow, deny) firewall lists: allowed when open and public; denied
/// otherwise (including ports no longer in the pool).
pub async fn firewall_targets(db: &DbPool, node_id: i64, ports: &[u16]) -> (Vec<u16>, Vec<u16>) {
    let rows: Vec<(i32, bool, bool)> =
        sqlx::query_as("SELECT port, is_open, public FROM node_ports WHERE node_id = $1 AND port = ANY($2)")
            .bind(node_id)
            .bind(ports.iter().map(|p| i32::from(*p)).collect::<Vec<i32>>())
            .fetch_all(db)
            .await
            .unwrap_or_default();
    let allowed: BTreeSet<u16> = rows
        .into_iter()
        .filter(|(_, open, public)| *open && *public)
        .filter_map(|(p, _, _)| u16::try_from(p).ok())
        .collect();
    let deny = ports.iter().copied().filter(|p| !allowed.contains(p)).collect();
    (allowed.into_iter().collect(), deny)
}

/// Every pool port of a node as (allow, deny) for a full firewall sync.
pub async fn firewall_plan(db: &DbPool, node_id: i64) -> (Vec<u16>, Vec<u16>) {
    let rows: Vec<(i32, bool, bool)> = sqlx::query_as("SELECT port, is_open, public FROM node_ports WHERE node_id = $1")
        .bind(node_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let mut allow = Vec::new();
    let mut deny = Vec::new();
    for (p, open, public) in rows {
        if let Ok(p) = u16::try_from(p) {
            if open && public {
                allow.push(p)
            } else {
                deny.push(p)
            }
        }
    }
    (allow, deny)
}

pub async fn record_firewall_state(db: &DbPool, node_id: i64, result: &Result<(), String>) {
    let (state, error) = match result {
        Ok(()) => ("ok", None),
        Err(e) if e.contains("offline") => ("pending", Some(e.as_str())),
        Err(e) => ("failed", Some(e.as_str())),
    };
    let _ = sqlx::query(
        "UPDATE nodes SET firewall_state = $2, firewall_error = $3,
             firewall_synced_at = CASE WHEN $2 = 'ok' THEN $4 ELSE firewall_synced_at END WHERE id = $1",
    )
    .bind(node_id)
    .bind(state)
    .bind(error)
    .bind(now_secs())
    .execute(db)
    .await;
}

/// Applies firewall changes on the node that owns the ports (locally or over RPC) and
/// records the outcome on the node.
pub async fn apply_on_node(
    db: &DbPool,
    cluster: &super::ClusterManager,
    node: &super::ClusterNode,
    allow: Vec<u16>,
    deny: Vec<u16>,
) -> Result<(), String> {
    if allow.is_empty() && deny.is_empty() {
        return Ok(());
    }
    let result = if node.is_local {
        match tokio::task::spawn_blocking(move || crate::runtime::port::sync_firewall_ports(&allow, &deny)).await {
            Ok(r) => r,
            Err(e) => Err(e.to_string()),
        }
    } else if !cluster.is_node_connected(node.id).await {
        Err(format!("{} is offline; its firewall updates automatically when it reconnects.", node.name))
    } else {
        cluster
            .call_node(
                node.id,
                "firewall.ports",
                serde_json::json!({ "open": allow, "close": deny }),
                std::time::Duration::from_secs(120),
            )
            .await
            .map(|_| ())
            .map_err(|(_, msg)| msg)
    };
    record_firewall_state(db, node.id, &result).await;
    result
}

/// Full firewall sync when the node has a pool and its firewall isn't known to be in sync.
pub async fn resync_if_needed(db: &DbPool, cluster: &super::ClusterManager, node: &super::ClusterNode) {
    if !has_pool(db, node.id).await {
        return;
    }
    let state: Option<String> = sqlx::query_scalar("SELECT firewall_state FROM nodes WHERE id = $1")
        .bind(node.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if state.as_deref() == Some("ok") {
        return;
    }
    let (allow, deny) = firewall_plan(db, node.id).await;
    if let Err(e) = apply_on_node(db, cluster, node, allow, deny).await {
        tracing::warn!("Firewall sync for node #{} failed: {e}", node.id);
    }
}

/// Whether an app's port should be opened to the internet when the app starts: always on
/// nodes without a pool (original behaviour), only for public pool ports otherwise.
pub async fn should_open_publicly(db: &DbPool, node_id: Option<i64>, port: u16) -> bool {
    let Some(node_id) = node_id else { return true };
    if !has_pool(db, node_id).await {
        return true;
    }
    sqlx::query_scalar::<_, bool>("SELECT public FROM node_ports WHERE node_id = $1 AND port = $2")
        .bind(node_id)
        .bind(i32::from(port))
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_lists_and_ranges() {
        assert_eq!(parse_port_list("8100, 8103-8105 9000").unwrap(), vec![8100, 8103, 8104, 8105, 9000]);
        assert!(parse_port_list("80").is_err(), "system ports refused");
        assert!(parse_port_list("8105-8100").is_err());
        assert!(parse_port_list("abc").is_err());
        assert!(parse_port_list("").is_err());
        assert!(parse_port_list("10000-20000").is_err(), "too large");
    }

    #[tokio::test]
    async fn a_database_can_hold_extra_ports_and_keeps_them_exclusive() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        let node_id: i64 = sqlx::query_scalar(
            "INSERT INTO nodes(name, region, tags_json, status, is_local, public_ip, mesh_ip, wg_pubkey, wg_endpoint,
                node_token_hash, node_version, capabilities_json, metrics_json, last_seen_at, created_at, updated_at)
             VALUES('db-node', 'default', '[]', 'online', FALSE, '', '10.200.0.8', '', '', 'h', 'v', '{}', '{}', 0, 0, 0)
             RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        set_range(&db, node_id, 9200, 9205).await.unwrap();
        let db_id: i64 = sqlx::query_scalar(
            "INSERT INTO databases(name, db_name, db_user, db_password, port, node_id, created_at, updated_at)
             VALUES('main-db', 'app', 'u', 'p', 9200, $1, 0, 0) RETURNING id",
        )
        .bind(node_id)
        .fetch_one(&db)
        .await
        .unwrap();

        set_database_extra_ports(&db, node_id, db_id, &[9201, 9202]).await.unwrap();
        let rows = crate::app_limits::list_all(&db).await.unwrap();
        let row = rows.iter().find(|r| r.kind == "database" && r.id == db_id).unwrap();
        assert_eq!(row.port, Some(9200));
        assert_eq!(row.extra_ports, vec![9201, 9202]);

        // Held ports are no longer free, but the database may keep its own.
        assert!(check_assignable(&db, node_id, 9201, None).await.is_err());
        check_assignable_for(&db, node_id, 9201, Owner::Database(db_id)).await.unwrap();
        assert_eq!(next_free_port(&db, node_id, &[]).await.unwrap(), Some(9203));
        // Taking the pool away from under a database is refused while it holds ports.
        let (_, removed) = set_range(&db, node_id, 9203, 9205).await.unwrap();
        assert!(!removed.contains(&9201), "ports held by a database stay");

        set_database_extra_ports(&db, node_id, db_id, &[]).await.unwrap();
        assert_eq!(next_free_port(&db, node_id, &[]).await.unwrap(), Some(9201));
    }

    #[tokio::test]
    async fn pool_hands_out_ports_in_order_and_tracks_state() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        let node_id: i64 = sqlx::query_scalar(
            "INSERT INTO nodes(name, region, tags_json, status, is_local, public_ip, mesh_ip, wg_pubkey, wg_endpoint,
                node_token_hash, node_version, capabilities_json, metrics_json, last_seen_at, created_at, updated_at)
             VALUES('pool-node', 'default', '[]', 'online', FALSE, '', '10.200.0.9', '', '', 'h', 'v', '{}', '{}', 0, 0, 0)
             RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(!has_pool(&db, node_id).await);

        set_range(&db, node_id, 9100, 9104).await.unwrap();
        assert!(has_pool(&db, node_id).await);
        assert_eq!(next_free_port(&db, node_id, &[]).await.unwrap(), Some(9100));
        assert_eq!(next_free_port(&db, node_id, &[9100]).await.unwrap(), Some(9101));

        set_open(&db, node_id, &[9100, 9101], false).await.unwrap();
        assert_eq!(next_free_port(&db, node_id, &[]).await.unwrap(), Some(9102), "closed ports are skipped");
        assert!(check_assignable(&db, node_id, 9100, None).await.is_err());
        assert!(check_assignable(&db, node_id, 9500, None).await.is_err(), "not in pool");

        add_ports(&db, node_id, &[9500]).await.unwrap();
        check_assignable(&db, node_id, 9500, None).await.unwrap();
        assert_eq!(remove_ports(&db, node_id, &[9500, 9101]).await.unwrap(), vec![9101, 9500]);
        assert!(check_assignable(&db, node_id, 9101, None).await.is_err(), "removed ports are gone");

        // Open ports stay private until marked public; closed beats public.
        let (allow, deny) = firewall_targets(&db, node_id, &[9103, 9104]).await;
        assert!(allow.is_empty() && deny == vec![9103, 9104]);
        set_public(&db, node_id, &[9103], true).await.unwrap();
        let (allow, _) = firewall_targets(&db, node_id, &[9103, 9104]).await;
        assert_eq!(allow, vec![9103]);
        set_open(&db, node_id, &[9103], false).await.unwrap();
        let (allow, deny) = firewall_targets(&db, node_id, &[9103]).await;
        assert!(allow.is_empty() && deny == vec![9103]);
        set_open(&db, node_id, &[9103], true).await.unwrap();

        // Shrinking keeps hand-added ports and drops unused range ports.
        add_ports(&db, node_id, &[9600]).await.unwrap();
        let (_, removed) = set_range(&db, node_id, 9103, 9104).await.unwrap();
        assert!(removed.contains(&9100) && removed.contains(&9102));
        let pool = load_pool(&db, node_id).await.unwrap();
        let listed: Vec<u16> = pool.ports.iter().map(|p| p.port).collect();
        assert_eq!(listed, vec![9103, 9104, 9600]);
    }
}
