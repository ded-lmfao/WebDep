use std::path::Path;
#[cfg(unix)]
use tokio::process::Command;

/// Parse and validate an IPv4/IPv6 address or CIDR notation so malformed/injected rules cannot enter pg_hba.conf or ufw.
pub fn parse_valid_ip_or_cidr(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((ip_str, mask_str)) = s.split_once('/') {
        let addr: std::net::IpAddr = ip_str.parse().ok()?;
        let mask: u8 = mask_str.parse().ok()?;
        let max_mask = if addr.is_ipv4() { 32 } else { 128 };
        if mask > max_mask {
            return None;
        }
        Some(format!("{}/{}", addr, mask))
    } else {
        let addr: std::net::IpAddr = s.parse().ok()?;
        let mask = if addr.is_ipv4() { 32 } else { 128 };
        Some(format!("{}/{}", addr, mask))
    }
}

/// Subnet of the WireGuard mesh between nodes.
#[cfg_attr(not(unix), allow(dead_code))]
pub const MESH_SUBNET: &str = "10.200.0.0/16";

pub fn generate_pg_hba(access_mode: &str, allowed_ips: &str) -> String {
    let mut hba = String::new();
    hba.push_str("# PostgreSQL Client Authentication Configuration File (WebDep Managed)\n");
    hba.push_str("# TYPE  DATABASE        USER            ADDRESS                 METHOD\n");
    // Unix-socket ("local") rules are not supported on Windows and make PostgreSQL reject pg_hba.conf
    if !cfg!(windows) {
        hba.push_str("local   all             all                                     trust\n");
    }
    hba.push_str("host    all             all             127.0.0.1/32            scram-sha-256\n");
    hba.push_str("host    all             all             ::1/128                 scram-sha-256\n");
    // Allow encrypted Web-Node WireGuard P2P mesh subnet for cross-node DB connections
    hba.push_str("host    all             all             10.200.0.0/16           scram-sha-256\n");

    if access_mode == "public" {
        hba.push_str("host    all             all             0.0.0.0/0               scram-sha-256\n");
        hba.push_str("host    all             all             ::/0                    scram-sha-256\n");
    } else if access_mode == "allowlist" {
        for raw in allowed_ips.split([',', '\n', ';']) {
            if let Some(cidr) = parse_valid_ip_or_cidr(raw) {
                hba.push_str(&format!("host    all             all             {:<23} scram-sha-256\n", cidr));
            }
        }
    }

    hba
}

pub fn to_clean_path_str(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        stripped.to_string()
    } else {
        s
    }
}

pub fn read_pg_pid(data_dir: &Path) -> Option<u32> {
    let pid_file = data_dir.join("postmaster.pid");
    if let Ok(content) = std::fs::read_to_string(pid_file) {
        if let Some(first_line) = content.lines().next() {
            if let Ok(pid) = first_line.trim().parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

/// Reads PostgreSQL's start timestamp from line 3 of `postmaster.pid` (Unix seconds) in milliseconds.
pub fn read_pg_started_at(data_dir: &Path) -> Option<i64> {
    let pid_file = data_dir.join("postmaster.pid");
    if let Ok(content) = std::fs::read_to_string(pid_file) {
        if let Some(third_line) = content.lines().nth(2) {
            if let Ok(secs) = third_line.trim().parse::<i64>() {
                if secs > 0 {
                    return Some(secs * 1000);
                }
            }
        }
    }
    None
}

const TCP_ONLY_SETTING: &str = "unix_socket_directories = ''";

/// WebDep connects to every PostgreSQL it runs over TCP (127.0.0.1), so turn off the Unix socket.
/// Its default folder (e.g. /var/run/postgresql) belongs to the `postgres` user, and a server
/// started as any other user dies with "could not create lock file ... Permission denied".
/// Idempotent; run before each start so clusters created by older versions get it too.
pub async fn ensure_tcp_only_pg_conf(pgdata_dir: &Path) {
    let conf_path = pgdata_dir.join("postgresql.conf");
    let Ok(existing) = tokio::fs::read_to_string(&conf_path).await else {
        return;
    };
    if existing.lines().any(|l| l.trim() == TCP_ONLY_SETTING) {
        return;
    }
    let mut updated = existing;
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("# WebDep: TCP only, no Unix socket (its default folder may not be writable)\n");
    updated.push_str(TCP_ONLY_SETTING);
    updated.push('\n');
    let _ = tokio::fs::write(&conf_path, updated).await;
}

pub async fn wait_for_pg_ready(port: u16, data_dir: &Path, max_wait_ms: u64) -> Option<u32> {
    let start = std::time::Instant::now();
    let addr = format!("127.0.0.1:{}", port);
    while start.elapsed().as_millis() < max_wait_ms as u128 {
        if let Some(pid) = read_pg_pid(data_dir) {
            if is_process_alive(pid) && tokio::net::TcpStream::connect(&addr).await.is_ok() {
                return Some(pid);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    None
}

pub fn is_process_alive(pid: u32) -> bool {
    crate::runtime::tree::is_pid_alive(pid)
}

pub async fn sync_firewall_rules(port: u16, access_mode: &str, allowed_ips: &str) {
    if !crate::runtime::port::firewall_management_enabled() {
        return;
    }
    #[cfg(unix)]
    {
        // The WireGuard mesh is private and encrypted: other nodes (and the core) may always reach
        // the database over it, whatever the public access mode is.
        let _ = Command::new("sudo")
            .args(["ufw", "allow", "proto", "tcp", "from", MESH_SUBNET, "to", "any", "port", &port.to_string()])
            .output()
            .await;
        if access_mode == "local" {
            let _ = Command::new("sudo").args(["ufw", "delete", "allow", &format!("{}/tcp", port)]).output().await;
        } else if access_mode == "public" {
            let _ = Command::new("sudo").args(["ufw", "allow", &format!("{}/tcp", port)]).output().await;
        } else if access_mode == "allowlist" {
            let _ = Command::new("sudo").args(["ufw", "delete", "allow", &format!("{}/tcp", port)]).output().await;
            for raw in allowed_ips.split(|c| c == ',' || c == '\n' || c == ';') {
                if let Some(cidr) = parse_valid_ip_or_cidr(raw) {
                    let _ = Command::new("sudo")
                        .args(["ufw", "allow", "proto", "tcp", "from", &cidr, "to", "any", "port", &port.to_string()])
                        .output()
                        .await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (port, access_mode, allowed_ips);
    }
}

pub async fn remove_firewall_rules(port: u16, allowed_ips: &str) {
    if !crate::runtime::port::firewall_management_enabled() {
        return;
    }
    #[cfg(unix)]
    {
        let _ = Command::new("sudo").args(["ufw", "delete", "allow", &format!("{}/tcp", port)]).output().await;
        let _ = Command::new("sudo")
            .args(["ufw", "delete", "allow", "proto", "tcp", "from", MESH_SUBNET, "to", "any", "port", &port.to_string()])
            .output()
            .await;
        for raw in allowed_ips.split(|c| c == ',' || c == '\n' || c == ';') {
            if let Some(cidr) = parse_valid_ip_or_cidr(raw) {
                let _ = Command::new("sudo")
                    .args(["ufw", "delete", "allow", "proto", "tcp", "from", &cidr, "to", "any", "port", &port.to_string()])
                    .output()
                    .await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (port, allowed_ips);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tcp_only_conf_is_appended_once() {
        let dir = std::env::temp_dir().join(format!("webdep-tcp-only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("postgresql.conf");
        std::fs::write(&conf, "port = 5432").unwrap();

        ensure_tcp_only_pg_conf(&dir).await;
        ensure_tcp_only_pg_conf(&dir).await;

        let text = std::fs::read_to_string(&conf).unwrap();
        assert!(text.starts_with("port = 5432\n"));
        assert_eq!(text.lines().filter(|l| l.trim() == TCP_ONLY_SETTING).count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_ip_allowlist_validation() {
        assert_eq!(parse_valid_ip_or_cidr("192.168.1.10"), Some("192.168.1.10/32".into()));
        assert_eq!(parse_valid_ip_or_cidr("10.0.0.0/8"), Some("10.0.0.0/8".into()));
        assert_eq!(parse_valid_ip_or_cidr("2001:db8::/32"), Some("2001:db8::/32".into()));
        // Reject pg_hba / ufw directive injection
        assert_eq!(parse_valid_ip_or_cidr("0.0.0.0/0 trust"), None);
        assert_eq!(parse_valid_ip_or_cidr("127.0.0.1/99"), None);
        assert_eq!(parse_valid_ip_or_cidr("all"), None);
    }
}
