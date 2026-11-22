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

/// Which firewall tools a host has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct FirewallTools {
    pub ufw: bool,
    pub iptables: bool,
}

/// The commands that allow `allow` ports and remove WebDep's allow rules for `deny` ports.
/// Pure (no I/O) so it can be tested anywhere. Each iptables delete is listed once; the
/// executor repeats it until the rule is gone.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn plan_firewall_commands(tools: FirewallTools, allow: &[u16], deny: &[u16]) -> Vec<Vec<String>> {
    let mut cmds = Vec::new();
    let rule = |p: u16| format!("{p}/tcp");
    let ipt = |op: &str, p: u16| -> Vec<String> {
        let mut v: Vec<String> = vec!["iptables".into(), op.into()];
        if op == "-I" {
            v.extend(["INPUT".to_string(), "1".to_string()]);
        } else {
            v.push("INPUT".into());
        }
        v.extend(
            ["-p", "tcp", "--dport", &p.to_string(), "-m", "conntrack", "--ctstate", "NEW", "-j", "ACCEPT"]
                .iter()
                .map(|s| s.to_string()),
        );
        v
    };
    for &p in allow {
        if tools.ufw {
            cmds.push(vec!["ufw".into(), "allow".into(), rule(p)]);
        } else if tools.iptables {
            cmds.push(ipt("-I", p));
        }
    }
    for &p in deny {
        if tools.ufw {
            cmds.push(vec!["ufw".into(), "delete".into(), "allow".into(), rule(p)]);
        }
        if tools.iptables {
            cmds.push(ipt("-D", p));
        }
    }
    cmds
}

/// Runs a command directly, then through `sudo -n` (non-root installs need passwordless
/// sudo for ufw/iptables). Returns (success, stderr).
#[cfg(unix)]
fn run_privileged(args: &[String]) -> (bool, String) {
    let attempt = |sudo: bool| {
        let mut cmd = if sudo {
            let mut c = std::process::Command::new("sudo");
            c.arg("-n").args(args);
            c
        } else {
            let mut c = std::process::Command::new(&args[0]);
            c.args(&args[1..]);
            c
        };
        cmd.stdin(Stdio::null()).output()
    };
    match attempt(false) {
        Ok(o) if o.status.success() => (true, String::new()),
        _ => match attempt(true) {
            Ok(o) if o.status.success() => (true, String::new()),
            Ok(o) => (false, String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Err(e) => (false, e.to_string()),
        },
    }
}

#[cfg(unix)]
pub fn detect_firewall_tools() -> FirewallTools {
    let ufw = run_privileged(&["ufw".into(), "status".into()]).0;
    let iptables = run_privileged(&["iptables".into(), "-S".into(), "INPUT".into()]).0;
    FirewallTools { ufw, iptables }
}

/// Allows `allow` ports and removes WebDep's allow rules for `deny` ports (Linux: UFW, else
/// iptables). Runs synchronously; call from a blocking task. Reports why it failed instead of
/// failing silently. No-op on other systems or when firewall management is turned off.
pub fn sync_firewall_ports(allow: &[u16], deny: &[u16]) -> Result<(), String> {
    if !firewall_management_enabled() || (allow.is_empty() && deny.is_empty()) {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let tools = detect_firewall_tools();
        if !tools.ufw && !tools.iptables {
            return Err("Couldn't change the firewall: neither ufw nor iptables could be run. Run WebDep as root or allow passwordless sudo for ufw and iptables.".into());
        }
        let mut failures = Vec::new();
        for cmd in plan_firewall_commands(tools, allow, deny) {
            let is_delete = cmd.iter().any(|a| a == "-D" || a == "delete");
            if cmd[0] == "iptables" && cmd[1] == "-I" {
                // Skip when the rule already exists (keeps the chain free of duplicates).
                let mut check = cmd.clone();
                check[1] = "-C".into();
                check.remove(3);
                if run_privileged(&check).0 {
                    continue;
                }
            }
            if cmd[0] == "iptables" && is_delete {
                for _ in 0..16 {
                    if !run_privileged(&cmd).0 {
                        break;
                    }
                }
                continue;
            }
            let (ok, err) = run_privileged(&cmd);
            // Deleting a rule that isn't there is fine.
            if !ok && !is_delete {
                failures.push(format!("{}: {}", cmd.join(" "), if err.is_empty() { "failed".into() } else { err }));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!("Some firewall changes failed: {}", failures.join("; ")))
        }
    }
    #[cfg(not(unix))]
    {
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

#[cfg(test)]
mod firewall_plan_tests {
    use super::*;

    #[test]
    fn ufw_hosts_use_ufw_and_also_clean_iptables_on_deny() {
        let cmds = plan_firewall_commands(FirewallTools { ufw: true, iptables: true }, &[9001], &[9002]);
        assert_eq!(cmds[0], vec!["ufw", "allow", "9001/tcp"]);
        assert_eq!(cmds[1], vec!["ufw", "delete", "allow", "9002/tcp"]);
        assert_eq!(cmds[2][0..3], ["iptables", "-D", "INPUT"]);
        assert!(cmds[2].contains(&"9002".to_string()));
        assert_eq!(cmds.len(), 3);
    }

    #[test]
    fn iptables_only_hosts_insert_at_the_top() {
        let cmds = plan_firewall_commands(FirewallTools { ufw: false, iptables: true }, &[9001], &[]);
        assert_eq!(cmds[0][0..4], ["iptables", "-I", "INPUT", "1"]);
        assert!(cmds[0].ends_with(&["-j".to_string(), "ACCEPT".to_string()]));
    }

    #[test]
    fn no_tools_plans_nothing() {
        assert!(plan_firewall_commands(FirewallTools { ufw: false, iptables: false }, &[1], &[2]).is_empty());
    }
}

