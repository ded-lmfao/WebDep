//! IP addresses an admin adds to a node (Admin Panel › IP Addresses), and which website or
//! service is served on each. An app with no IP chosen is served on every address.

use std::net::IpAddr;

use serde::Serialize;
use sqlx::Row;

use crate::db::DbPool;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIp {
    pub id: i64,
    pub ip: String,
    pub label: String,
    /// Websites and services served on this address.
    pub apps: Vec<AppRef>,
}

/// Canonical text of a usable address, or a message saying why it can't be used.
pub fn normalize_ip(input: &str) -> Result<String, String> {
    let ip: IpAddr = input
        .trim()
        .parse()
        .map_err(|_| format!("'{}' is not an IP address.", input.trim()))?;
    if ip.is_unspecified() || ip.is_multicast() {
        return Err(format!("{ip} can't be assigned to an app."));
    }
    Ok(ip.to_string())
}

fn clean_label(label: &str) -> String {
    label.trim().chars().filter(|c| !c.is_control()).take(60).collect()
}

pub async fn list(db: &DbPool, node_id: i64) -> Result<Vec<NodeIp>, sqlx::Error> {
    let rows = sqlx::query("SELECT id, ip, label FROM node_ips WHERE node_id = $1 ORDER BY id")
        .bind(node_id)
        .fetch_all(db)
        .await?;
    let apps = sqlx::query("SELECT id, name, bind_ip FROM projects WHERE node_id = $1 AND bind_ip <> '' ORDER BY name")
        .bind(node_id)
        .fetch_all(db)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let ip: String = r.get("ip");
            NodeIp {
                id: r.get("id"),
                label: r.get("label"),
                apps: apps
                    .iter()
                    .filter(|a| a.get::<String, _>("bind_ip") == ip)
                    .map(|a| AppRef { id: a.get("id"), name: a.get("name") })
                    .collect(),
                ip,
            }
        })
        .collect())
}

pub async fn add(db: &DbPool, node_id: i64, ip: &str, label: &str) -> Result<String, String> {
    let ip = normalize_ip(ip)?;
    let done = sqlx::query(
        "INSERT INTO node_ips(node_id, ip, label, created_at) VALUES($1, $2, $3, $4) ON CONFLICT (node_id, ip) DO NOTHING",
    )
    .bind(node_id)
    .bind(&ip)
    .bind(clean_label(label))
    .bind(crate::db::now())
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    if done.rows_affected() == 0 {
        return Err(format!("{ip} is already on this node."));
    }
    Ok(ip)
}

/// Removes an address no app is served on.
pub async fn remove(db: &DbPool, node_id: i64, ip_id: i64) -> Result<String, String> {
    let ip: String = sqlx::query_scalar("SELECT ip FROM node_ips WHERE id = $1 AND node_id = $2")
        .bind(ip_id)
        .bind(node_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("That IP address isn't on this node.")?;
    let users: Vec<String> = sqlx::query_scalar("SELECT name FROM projects WHERE node_id = $1 AND bind_ip = $2 ORDER BY name")
        .bind(node_id)
        .bind(&ip)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
    if !users.is_empty() {
        return Err(format!("{ip} is still used by {}. Move those apps to another IP first.", users.join(", ")));
    }
    sqlx::query("DELETE FROM node_ips WHERE id = $1")
        .bind(ip_id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ip)
}

/// Checks that `ip` (already normalized, or empty for "every address") belongs to the node.
pub async fn check_assignable(db: &DbPool, node_id: i64, ip: &str) -> Result<(), String> {
    if ip.is_empty() {
        return Ok(());
    }
    let known: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM node_ips WHERE node_id = $1 AND ip = $2)")
        .bind(node_id)
        .bind(ip)
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())?;
    if known {
        Ok(())
    } else {
        Err(format!("{ip} isn't one of this node's IP addresses. Add it in Admin Panel › IP Addresses first."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_rejects_addresses() {
        assert_eq!(normalize_ip(" 203.0.113.7 ").unwrap(), "203.0.113.7");
        assert_eq!(normalize_ip("2001:DB8::0001").unwrap(), "2001:db8::1");
        assert!(normalize_ip("0.0.0.0").is_err());
        assert!(normalize_ip("::").is_err());
        assert!(normalize_ip("224.0.0.1").is_err());
        assert!(normalize_ip("example.com").is_err());
        assert!(normalize_ip("203.0.113.7; rm").is_err());
    }
}
