use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Runtime;

static PORT_PIDS_CACHE: OnceLock<Mutex<HashMap<u16, (Instant, Vec<u32>)>>> = OnceLock::new();

/// Whether WebDep may edit the host firewall (UFW / iptables).
///
/// Enabled by default (upstream behaviour). Set `WEBDEP_MANAGE_FIREWALL=false` on hosts where the
/// firewall is managed elsewhere or where other services share the machine, so WebDep never
/// changes which ports are reachable.
pub fn firewall_management_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !std::env::var("WEBDEP_MANAGE_FIREWALL")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
            .unwrap_or(false)
    })
}

/// Opens and closes TCP ports in UFW and iptables (Linux). Used for node port pools:
/// `open` ports accept new connections, `close` ports have WebDep's allow rules removed.
/// Runs synchronously; call from a blocking task. No-op on other systems or when firewall
/// management is turned off.
pub fn sync_firewall_ports(open: &[u16], close: &[u16]) -> Result<(), String> {
    if !firewall_management_enabled() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let run = |args: &[&str]| -> bool {
            let direct = std::process::Command::new(args[0]).args(&args[1..]).stdin(Stdio::null()).output();
            if matches!(direct, Ok(ref o) if o.status.success()) {
                return true;
            }
            std::process::Command::new("sudo")
                .arg("-n")
                .args(args)
                .stdin(Stdio::null())
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        let has_ufw = run(&["ufw", "status"]);
        for p in open {
            let rule = format!("{p}/tcp");
            if has_ufw {
                run(&["ufw", "allow", &rule]);
            }
            let port = p.to_string();
            let check = ["iptables", "-C", "INPUT", "-p", "tcp", "--dport", &port, "-m", "conntrack", "--ctstate", "NEW", "-j", "ACCEPT"];
            if !run(&check) {
                run(&["iptables", "-I", "INPUT", "1", "-p", "tcp", "--dport", &port, "-m", "conntrack", "--ctstate", "NEW", "-j", "ACCEPT"]);
            }
        }
        for p in close {
            let rule = format!("{p}/tcp");
            if has_ufw {
                run(&["ufw", "delete", "allow", &rule]);
            }
            let port = p.to_string();
            let del = ["iptables", "-D", "INPUT", "-p", "tcp", "--dport", &port, "-m", "conntrack", "--ctstate", "NEW", "-j", "ACCEPT"];
            while run(&del) {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (open, close);
        Ok(())
    }
}

impl Runtime {
    /// Automatically open `port/tcp` in UFW and iptables on Linux so newly created servers/websites
    /// are reachable without manual firewall commands.
    pub fn allow_firewall_port(&self, port: u16, project_id: Option<i64>) {
        if port == 0 || !firewall_management_enabled() {
            return;
        }
        if let Ok(mut set) = self.allowed_ports.lock() {
            if !set.insert(port) {
                return;
            }
        }

        #[cfg(unix)]
        {
            let _ = &self.logs;
            let _ = project_id;
            std::thread::spawn(move || {
                let cmd = format!(
                    "(sudo -n ufw allow {p}/tcp >/dev/null 2>&1 || ufw allow {p}/tcp >/dev/null 2>&1 || true) && \
                     (sudo -n iptables -C INPUT -p tcp --dport {p} -m conntrack --ctstate NEW -j ACCEPT >/dev/null 2>&1 || \
                      sudo -n iptables -I INPUT 1 -p tcp --dport {p} -m conntrack --ctstate NEW -j ACCEPT >/dev/null 2>&1 || true)",
                    p = port
                );
                if let Ok(out) = std::process::Command::new("sh").arg("-c").arg(&cmd).output() {
                    if out.status.success() {
                        tracing::info!("Automatically allowed TCP port {}/tcp in UFW & iptables firewall", port);
                    }
                }
            });
        }
        #[cfg(not(unix))]
        {
            let _ = project_id;
        }
    }


    /// Find all OS process IDs currently listening on `port`.
    pub fn find_pids_on_port(port: u16) -> Vec<u32> {
        let mut pids = HashSet::new();
        let self_pid = std::process::id();

        #[cfg(windows)]
        {
            if let Ok(out) = std::process::Command::new("netstat")
                .args(["-ano", "-p", "TCP"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                let port_suffix = format!(":{}", port);
                for line in text.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    // Proto LocalAddress ForeignAddress State PID
                    if parts.len() >= 5 && parts[3].eq_ignore_ascii_case("LISTENING") {
                        let local_addr = parts[1];
                        if local_addr.ends_with(&port_suffix) {
                            if let Ok(pid) = parts[4].parse::<u32>() {
                                if pid > 0 && pid != self_pid {
                                    pids.insert(pid);
                                }
                            }
                        }
                    }
                }
            }
        }

        #[cfg(unix)]
        {
            if let Ok(out) = std::process::Command::new("lsof")
                .args(["-ti", &format!("tcp:{}", port)])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    if let Ok(pid) = line.trim().parse::<u32>() {
                        if pid > 0 && pid != self_pid {
                            pids.insert(pid);
                        }
                    }
                }
            }
        }

        pids.into_iter().collect()
    }

    /// Cached port-to-PID probe (1500ms TTL) for high-frequency read-only telemetry/overview handlers
    /// so polling does not repeatedly spawn `netstat`/`lsof`.
    pub fn find_pids_on_port_cached(port: u16) -> Vec<u32> {
        if port == 0 {
            return Vec::new();
        }
        let ttl = Duration::from_millis(1500);
        let cache = PORT_PIDS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Ok(guard) = cache.lock() {
            if let Some((ts, pids)) = guard.get(&port) {
                if ts.elapsed() < ttl {
                    return pids.clone();
                }
            }
        }
        let pids = Self::find_pids_on_port(port);
        if let Ok(mut guard) = cache.lock() {
            if guard.len() > 128 {
                guard.retain(|_, (ts, _)| ts.elapsed() < ttl);
            }
            guard.insert(port, (Instant::now(), pids.clone()));
        }
        pids
    }



    pub async fn is_port_accepting(port: u16) -> bool {
        tokio::time::timeout(
            Duration::from_millis(150),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
    }

}
