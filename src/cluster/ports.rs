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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolPort {
    pub port: u16,
    pub open: bool,
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
        "SELECT id, name, project_kind, COALESCE(assigned_port, port), app_status FROM projects
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
                PortUser { kind: "database".into(), id: r.try_get(0)?, name: r.try_get(1)?, role: "main".into(), running: status == "running" },
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
                },
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
         UNION ALL SELECT port FROM databases UNION ALL SELECT port FROM node_ports WHERE project_id IS NOT NULL",
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
    let rows = sqlx::query("SELECT port, is_open, manual FROM node_ports WHERE node_id = $1 ORDER BY port")
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
            manual: r.try_get(2)?,
            in_pool: true,
            used_by: usage.remove(&port),
        });
    }
    // Apps on ports outside the pool (set before the pool existed): still listed.
    for (port, user) in usage {
        ports.push(PoolPort { port, open: true, manual: false, in_pool: false, used_by: Some(user) });
    }
    ports.sort_by_key(|p| p.port);
    Ok(NodePool {
        node_id,
        port_min: min.and_then(|v| u16::try_from(v).ok()),
        port_max: max.and_then(|v| u16::try_from(v).ok()),
        ports,
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
        "DELETE FROM node_ports WHERE node_id = $1 AND manual = FALSE AND project_id IS NULL
         AND (port < $2 OR port > $3) AND NOT (port = ANY($4)) RETURNING port",
    )
    .bind(node_id)
    .bind(i32::from(min))
    .bind(i32::from(max))
    .bind(usage.keys().map(|p| i32::from(*p)).collect::<Vec<i32>>())
    .fetch_all(&mut *tx)
    .await
    .map_err(err)?;
    sqlx::query("UPDATE nodes SET port_min = $2, port_max = $3 WHERE id = $1")
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
        "DELETE FROM node_ports WHERE node_id = $1 AND port = ANY($2) AND project_id IS NULL RETURNING port",
    )
    .bind(node_id)
    .bind(ports.iter().map(|p| i32::from(*p)).collect::<Vec<i32>>())
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(removed.into_iter().filter_map(|p| u16::try_from(p).ok()).collect())
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

/// Checks that `port` is an open, unused pool port on the node (for a manual choice).
/// `allow_project` lets an app keep a port it already holds.
pub async fn check_assignable(db: &DbPool, node_id: i64, port: u16, allow_project: Option<i64>) -> Result<(), String> {
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
        let own = allow_project.is_some_and(|pid| user.kind != "database" && user.id == pid);
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
    for &p in ports {
        let held: Option<Option<i64>> = sqlx::query_scalar("SELECT project_id FROM node_ports WHERE node_id = $1 AND port = $2")
            .bind(node_id)
            .bind(i32::from(p))
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?;
        if held.flatten() != Some(project_id) {
            check_assignable(db, node_id, p, None).await?;
        }
    }
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("UPDATE node_ports SET project_id = NULL, updated_at = $2 WHERE project_id = $1")
        .bind(project_id)
        .bind(now_secs())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE node_ports SET project_id = $2, updated_at = $4 WHERE node_id = $1 AND port = ANY($3)")
        .bind(node_id)
        .bind(project_id)
        .bind(ports.iter().map(|p| i32::from(*p)).collect::<Vec<i32>>())
        .bind(now_secs())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// Open and closed pool ports of a node (for firewall sync).
pub async fn firewall_plan(db: &DbPool, node_id: i64) -> (Vec<u16>, Vec<u16>) {
    let rows: Vec<(i32, bool)> = sqlx::query_as("SELECT port, is_open FROM node_ports WHERE node_id = $1")
        .bind(node_id)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let mut open = Vec::new();
    let mut closed = Vec::new();
    for (p, is_open) in rows {
        if let Ok(p) = u16::try_from(p) {
            if is_open { open.push(p) } else { closed.push(p) }
        }
    }
    (open, closed)
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

        // Shrinking keeps hand-added ports and drops unused range ports.
        add_ports(&db, node_id, &[9600]).await.unwrap();
        let (_, removed) = set_range(&db, node_id, 9103, 9104).await.unwrap();
        assert!(removed.contains(&9100) && removed.contains(&9102));
        let pool = load_pool(&db, node_id).await.unwrap();
        let listed: Vec<u16> = pool.ports.iter().map(|p| p.port).collect();
        assert_eq!(listed, vec![9103, 9104, 9600]);
    }
}
