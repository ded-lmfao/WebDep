use std::sync::Arc;
use std::time::Duration;

use super::process::is_pid_alive;
use super::Runtime;

impl Runtime {

    /// Background task to continuously supervise long-running processes (services, bots, servers)
    /// and immediately mark them stopped in the database and log warnings when they exit or crash.
    /// When `projects` is given, apps that exit on their own
    /// (crash, OOM kill, `kill -9`) are restarted with backoff: up to `CRASH_RESTART_LIMIT` times per
    /// `CRASH_RESTART_WINDOW`, then left stopped. User stops remove the process from the map first,
    /// so they never trigger a restart.
    /// When an app that has a memory limit exits on its own (killed by the OS for using too much,
    /// or crashed right after starting), tell the people who can see it why that may be.
    async fn explain_memory_crash(&self, p: &crate::projects::Project, status: &std::process::ExitStatus) {
        if p.memory_limit_mb <= 0 {
            return;
        }
        #[cfg(unix)]
        let killed = {
            use std::os::unix::process::ExitStatusExt;
            status.signal() == Some(9)
        };
        #[cfg(not(unix))]
        let killed = false;
        let started = p.app_started_at.unwrap_or(0);
        let soon = started > 0 && crate::db::now().saturating_sub(started) < 60_000;
        let Some(msg) = crate::app_limits::memory_crash_message(&p.name, p.memory_limit_mb, &status.to_string(), killed, soon) else {
            return;
        };
        // The restart loop can crash it several times in a row: one pop-up per 10 minutes is enough.
        if crate::app_limits::has_recent_event(&self.db, false, p.id, "ram_start", 600_000).await {
            return;
        }
        crate::app_limits::record_event(&self.db, false, p.id, &p.name, "ram_start", &msg, false).await;
    }

    pub fn start_background_monitor_with_restarts(self: &Arc<Self>, projects: Option<Arc<crate::projects::Projects>>) {
        const CRASH_RESTART_LIMIT: usize = 5;
        const CRASH_RESTART_WINDOW: Duration = Duration::from_secs(600);
        let rt = self.clone();
        tokio::spawn(async move {
            let mut tick = 0u64;
            let mut crash_history: std::collections::HashMap<i64, Vec<std::time::Instant>> = std::collections::HashMap::new();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                tick = tick.wrapping_add(1);
                let mut exited = Vec::new();
                {
                    if let Ok(mut map) = rt.processes.lock() {
                        for (&proj_id, child) in map.iter_mut() {
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    exited.push((proj_id, Some(status)));
                                }
                                Err(_) => {
                                    exited.push((proj_id, None));
                                }
                                Ok(None) => {}
                            }
                        }
                        for (proj_id, _) in &exited {
                            map.remove(proj_id);
                        }
                    }
                }
                for (proj_id, status) in exited {
                    // Read the app first: stopping it clears its start time.
                    let before = match &projects {
                        Some(ps) => ps.get(proj_id).await.ok().flatten(),
                        None => None,
                    };
                    rt.mark_project_stopped(proj_id).await;
                    if let (Some(p), Some(s)) = (&before, &status) {
                        rt.explain_memory_crash(p, s).await;
                    }
                    let msg = match status {
                        Some(s) => format!("Process exited ({})", s),
                        None => "Process terminated unexpectedly".to_string(),
                    };
                    rt.logs.add(Some(proj_id), None, "system", "warn", &msg);

                    let Some(projects) = projects.clone() else { continue };
                    let history = crash_history.entry(proj_id).or_default();
                    history.retain(|t| t.elapsed() < CRASH_RESTART_WINDOW);
                    if history.len() >= CRASH_RESTART_LIMIT {
                        rt.logs.add(
                            Some(proj_id),
                            None,
                            "system",
                            "error",
                            &format!(
                                "Not restarting: the app exited {CRASH_RESTART_LIMIT} times in {} minutes. Fix the cause, then start it again.",
                                CRASH_RESTART_WINDOW.as_secs() / 60
                            ),
                        );
                        continue;
                    }
                    history.push(std::time::Instant::now());
                    let attempt = history.len();
                    let delay = Duration::from_secs(2u64.pow(attempt as u32).min(60));
                    let seq = rt.get_stop_seq(proj_id);
                    let rt2 = rt.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        // The user stopped/redeployed it meanwhile, or something already started it.
                        if rt2.get_stop_seq(proj_id) != seq
                            || rt2.processes.lock().map(|m| m.contains_key(&proj_id)).unwrap_or(true)
                        {
                            return;
                        }
                        let Ok(Some(p)) = projects.get(proj_id).await else { return };
                        if p.app_status == "maintenance" {
                            return;
                        }
                        let env = projects.get_env_map(proj_id).await;
                        match rt2.start_project(&p, env, None).await {
                            Ok(pid) => rt2.logs.add(
                                Some(proj_id),
                                None,
                                "system",
                                "info",
                                &format!("Restarted after an unexpected exit (attempt {attempt}/{CRASH_RESTART_LIMIT}, PID {pid})"),
                            ),
                            Err(e) => rt2.logs.add(
                                Some(proj_id),
                                None,
                                "system",
                                "warn",
                                &format!("Automatic restart failed (attempt {attempt}/{CRASH_RESTART_LIMIT}): {e}"),
                            ),
                        };
                    });
                }

                // Every 6s (every 3rd tick), also check preserved local workloads from previous panel instances
                if tick.is_multiple_of(3) {
                    let external_running: Vec<(i64, u32)> = sqlx::query_as::<_, (i64, Option<i64>)>(
                        "SELECT id, app_pid FROM projects
                         WHERE app_status = 'running'
                           AND (node_id IS NULL OR node_id IN (SELECT id FROM nodes WHERE is_local = TRUE))",
                    )
                    .fetch_all(&rt.db)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|(id, opt_pid)| {
                        opt_pid.and_then(|p| u32::try_from(p).ok()).map(|p| (id, p))
                    })
                    .collect();

                    for (proj_id, pid) in external_running {
                        let is_in_memory = rt.processes.lock().map(|m| m.contains_key(&proj_id)).unwrap_or(false);
                        if !is_in_memory && !is_pid_alive(pid) {
                            let still_listening = sqlx::query_scalar::<_, Option<i32>>(
                                "SELECT port FROM projects WHERE id = $1",
                            )
                            .bind(proj_id)
                            .fetch_optional(&rt.db)
                            .await
                            .ok()
                            .flatten()
                            .flatten()
                            .and_then(|p| u16::try_from(p).ok())
                            .map(|port| !Runtime::find_pids_on_port(port).is_empty())
                            .unwrap_or(false);
                            if !still_listening {
                                rt.mark_project_stopped(proj_id).await;
                                rt.logs.add(Some(proj_id), None, "system", "warn", "Process terminated");
                            }
                        }
                    }
                }
            }
        });
    }

    pub async fn health_check(&self, url: &str, timeout_s: u64) -> Result<(), String> {
        health_check_url(url, timeout_s).await
    }
}

pub async fn health_check_url(url: &str, timeout_s: u64) -> Result<(), String> {
    if url.trim().is_empty() {
        return Ok(());
    }

    let start = std::time::Instant::now();
    let max_duration = Duration::from_secs(timeout_s.max(5));
    let mut last_err = String::from("Service starting up...");

    while start.elapsed() < max_duration {
        match reqwest_or_hyper(url, Duration::from_secs(2)).await {
            Ok(true) => return Ok(()),
            Ok(false) => {
                last_err = "Health check responded with non-2xx status".to_string();
            }
            Err(e) => {
                last_err = e;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Err(format!(
        "Health check timed out after {}s (endpoint: {}): {}",
        timeout_s, url, last_err
    ))
}

async fn reqwest_or_hyper(url_str: &str, timeout: Duration) -> Result<bool, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let stripped = url_str.strip_prefix("http://").unwrap_or(url_str);
    let (host_port, path) = match stripped.find('/') {
        Some(idx) => (&stripped[..idx], &stripped[idx..]),
        None => (stripped, "/"),
    };
    let (raw_host, port) = match host_port.find(':') {
        Some(idx) => (&host_port[..idx], host_port[idx + 1..].parse().unwrap_or(80)),
        None => (host_port, 80),
    };

    // Normalize localhost to 127.0.0.1 to avoid Windows IPv6 (::1) bind mismatches
    let host = if raw_host == "localhost" { "127.0.0.1" } else { raw_host };

    let connect_fut = TcpStream::connect((host, port));
    let mut stream = tokio::time::timeout(timeout, connect_fut)
        .await
        .map_err(|_| "Connection timed out".to_string())?
        .map_err(|e| e.to_string())?;

    let req = format!("GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", path, host);
    stream.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;

    let mut buf = [0u8; 12];
    let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
    if n >= 12 {
        let res_line = String::from_utf8_lossy(&buf);
        if res_line.contains("200")
            || res_line.contains("204")
            || res_line.contains("301")
            || res_line.contains("302")
            || res_line.contains("304")
        {
            return Ok(true);
        }
    }
    Ok(true)
}

