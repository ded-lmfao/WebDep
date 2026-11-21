use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use crate::db::now;
use crate::projects::Project;
use super::container::{
    build_container_env_pure, clear_container_pids_in, link_work_dir_to_container_dir,
    record_container_pid_to, WebsiteContainer,
};
use super::Runtime;

pub use super::tree::{detect_java_exe, is_pid_alive};

pub async fn spawn_project_process_pure(
    p: &Project,
    container: &WebsiteContainer,
    env_vars: HashMap<String, String>,
    release_dir: Option<&Path>,
    log_cb: Arc<dyn Fn(&str, &str, &str) + Send + Sync>,
) -> Result<(u32, tokio::process::Child, PathBuf, String), String> {
    if p.start_cmd.trim().is_empty() {
        return Err("No start command configured for this project".to_string());
    }

    let port_str = container.port.to_string();
    let resolved_cmd = p.start_cmd.replace("{{PORT}}", &port_str);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let raw_base = Path::new(&p.deploy_dir);
    let base_dir: PathBuf = if raw_base.is_absolute() {
        raw_base.to_path_buf()
    } else {
        let s = p.deploy_dir.trim_start_matches("./").trim_start_matches(".\\");
        cwd.join(s)
    };
    let _ = tokio::fs::create_dir_all(&base_dir).await;

    let current_path = base_dir.join("current");
    async fn check_dir(d: &Path) -> bool {
        tokio::fs::metadata(d).await.map(|m| m.is_dir()).unwrap_or(false)
    }

    let mut work_dir: Option<PathBuf> = None;
    if let Some(rd) = release_dir {
        let abs_rd = if rd.is_absolute() {
            rd.to_path_buf()
        } else {
            let s = rd.to_string_lossy();
            cwd.join(s.trim_start_matches("./").trim_start_matches(".\\"))
        };
        if check_dir(&abs_rd).await {
            work_dir = Some(abs_rd);
        }
    }
    if work_dir.is_none() && check_dir(&current_path).await {
        work_dir = Some(current_path);
    }
    if work_dir.is_none() {
        let releases_dir = base_dir.join("releases");
        if check_dir(&releases_dir).await {
            if let Ok(mut entries) = tokio::fs::read_dir(&releases_dir).await {
                let mut max_id = 0i64;
                let mut latest: Option<PathBuf> = None;
                while let Ok(Some(e)) = entries.next_entry().await {
                    let path = e.path();
                    if check_dir(&path).await {
                        let n = e.file_name().to_string_lossy().to_string();
                        if let Some(id_str) = n.strip_prefix("rel-") {
                            if let Ok(id) = id_str.parse::<i64>() {
                                if id > max_id {
                                    max_id = id;
                                    latest = Some(path);
                                }
                            }
                        }
                    }
                }
                work_dir = latest;
            }
        }
    }

    let work_dir = match work_dir {
        Some(d) => d,
        None => {
            if p.project_kind == "service" {
                let app_dir = base_dir.join("app");
                let _ = tokio::fs::create_dir_all(&app_dir).await;
                app_dir
            } else {
                return Err("NOT_DEPLOYED_YET".to_string());
            }
        }
    };

    let is_service = p.project_kind == "service";
    if !is_service {
        link_work_dir_to_container_dir(&work_dir, container);
    }

    let (isolated_env, dotenv_body) = build_container_env_pure(p, &env_vars, container);
    if !is_service {
        crate::runtime::container::write_app_dotenv(&work_dir.join(".env"), dotenv_body).await;
    }

    let mut final_cmd = resolved_cmd;
    if final_cmd.contains("vite preview") && work_dir.join("server.js").exists() {
        final_cmd = "node server.js".to_string();
    }

    #[cfg(unix)]
    {
        let has_python = std::path::Path::new("/usr/bin/python").exists()
            || std::path::Path::new("/usr/local/bin/python").exists()
            || work_dir.join(".venv/bin/python").exists()
            || work_dir.join("venv/bin/python").exists();

        if !has_python && final_cmd.contains("python") {
            let parts: Vec<&str> = final_cmd.split(' ').collect();
            final_cmd = parts
                .into_iter()
                .map(|token| if token == "python" { "python3" } else { token })
                .collect::<Vec<_>>()
                .join(" ");
        }
    }

    let java_info = detect_java_exe();

    #[cfg(windows)]
    let mut cmd = tokio::process::Command::new("cmd");
    #[cfg(windows)]
    {
        cmd.arg("/C");
        cmd.raw_arg(&final_cmd);
    }

    #[cfg(unix)]
    let mut cmd = tokio::process::Command::new("sh");
    #[cfg(unix)]
    cmd.args(["-c", &final_cmd]);

    cmd.current_dir(&work_dir);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(false);

    let limits_procs = match crate::runtime::limits::prepare(&format!("app-{}", p.id), p.cpu_limit, p.memory_limit_mb) {
        Ok(path) => path,
        Err(e) => {
            log_cb("system", "warn", &format!("Resource limits not applied: {e}. The app runs without limits."));
            None
        }
    };
    #[cfg(not(target_os = "linux"))]
    let _ = &limits_procs;

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(target_os = "linux")]
            if let Some(ref procs) = limits_procs {
                crate::runtime::limits::join_in_child(procs)?;
            }
            libc::setsid();
            libc::umask(0o027);
            let no_core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &no_core);
            Ok(())
        });
    }

    crate::config::Config::scrub_control_plane_env(&mut cmd);

    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.env("PYTHONUTF8", "1");
    cmd.env("PYTHONUNBUFFERED", "1");
    cmd.env("PIP_BREAK_SYSTEM_PACKAGES", "1");
    cmd.env("WEBDEP_PROJECT_ID", p.id.to_string());
    cmd.env("WEBDEP_PROJECT_SLUG", &p.slug);
    cmd.env("WEBDEP_CONTAINER_ID", &container.container_id);

    if let Some((_, ref java_home)) = java_info {
        let bin_dir = java_home.join("bin");
        let cur_path = std::env::var("PATH").unwrap_or_default();
        #[cfg(windows)]
        let new_path = format!("{};{}", bin_dir.display(), cur_path);
        #[cfg(unix)]
        let new_path = format!("{}:{}", bin_dir.display(), cur_path);

        cmd.env("PATH", new_path);
        cmd.env("JAVA_HOME", java_home);
    }

    let venv_dir = if work_dir.join(".venv").is_dir() {
        Some(work_dir.join(".venv"))
    } else if work_dir.join("venv").is_dir() {
        Some(work_dir.join("venv"))
    } else {
        None
    };
    if let Some(ref venv) = venv_dir {
        #[cfg(windows)]
        let venv_bin = venv.join("Scripts");
        #[cfg(not(windows))]
        let venv_bin = venv.join("bin");

        if venv_bin.exists() {
            let cur_path = std::env::var("PATH").unwrap_or_default();
            #[cfg(windows)]
            let new_path = format!("{};{}", venv_bin.display(), cur_path);
            #[cfg(not(windows))]
            let new_path = format!("{}:{}", venv_bin.display(), cur_path);

            cmd.env("PATH", new_path);
            cmd.env("VIRTUAL_ENV", venv);
        }
    }

    for (k, v) in &isolated_env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "Failed to spawn process '{}' in '{}': {}",
            final_cmd,
            work_dir.display(),
            e
        )
    })?;

    // Windows: put the app under its CPU and memory limits as soon as it exists.
    #[cfg(windows)]
    if p.cpu_limit > 0 || p.memory_limit_mb > 0 {
        if let Some(handle) = child.raw_handle() {
            if let Err(e) = crate::runtime::limits::apply_to_process(handle, p.cpu_limit, p.memory_limit_mb) {
                log_cb("system", "warn", &format!("Resource limits not applied: {e}. The app runs without limits."));
            }
        }
    }

    let pid = child.id().unwrap_or(0);
    clear_container_pids_in(container);
    record_container_pid_to(container, pid);

    let run_dir = container.run_dir.clone();
    if let Some(stdout) = child.stdout.take() {
        let cb = log_cb.clone();
        let run_dir_clone = run_dir.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                cb("app", "info", &line);
                if let Some(pos) = line.find("PID") {
                    let rem = &line[pos + 3..];
                    let num_str: String = rem
                        .chars()
                        .skip_while(|c| !c.is_ascii_digit())
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if let Ok(cpid) = num_str.parse::<u32>() {
                        if cpid > 1 && cpid != std::process::id() {
                            let pid_file = run_dir_clone.join("pids.txt");
                            if let Ok(mut f) = tokio::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&pid_file)
                                .await
                            {
                                use tokio::io::AsyncWriteExt;
                                let _ = f.write_all(format!("{}\n", cpid).as_bytes()).await;
                            }
                        }
                    }
                }
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let cb = log_cb.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                cb("app", "warn", &line);
            }
        });
    }

    tokio::time::sleep(Duration::from_millis(250)).await;

    if let Ok(Some(status)) = child.try_wait() {
        let err_msg = format!("Process exited immediately with code {}", status);
        log_cb("system", "warn", &err_msg);
        return Err(err_msg);
    }

    let mut final_pid = pid;
    if p.port.is_some() {
        let port_pids = Runtime::find_pids_on_port(container.port);
        for &port_pid in &port_pids {
            record_container_pid_to(container, port_pid);
        }
        if let Some(&first_port_pid) = port_pids.first() {
            if first_port_pid > 0 {
                final_pid = first_port_pid;
            }
        }
    }

    Ok((final_pid, child, work_dir, final_cmd))
}

impl Runtime {
    pub async fn start_project(
        &self,
        p: &Project,
        env_vars: HashMap<String, String>,
        release_dir: Option<&Path>,
    ) -> Result<u32, String> {
        let proj_lock = self.get_project_lock(p.id);
        let _guard = proj_lock.lock().await;

        // First shut down any existing process or port listener for this website
        self.kill_project_everything(p).await;

        let container = self.ensure_container(p).await;
        let logs_ref = self.logs.clone();
        let proj_id = p.id;
        let log_cb: Arc<dyn Fn(&str, &str, &str) + Send + Sync> = Arc::new(move |ch, lvl, msg| {
            logs_ref.add(Some(proj_id), None, ch, lvl, msg);
        });

        let (final_pid, child, work_dir, final_cmd) =
            match spawn_project_process_pure(p, &container, env_vars, release_dir, log_cb).await {
                Ok(res) => res,
                Err(e) => {
                    self.mark_project_stopped(p.id).await;
                    return Err(e);
                }
            };
        let pid = child.id().unwrap_or(final_pid);

        // Update database status
        let ts = now();
        let _ = sqlx::query("UPDATE projects SET app_status='running', app_pid=$1, app_started_at=$2 WHERE id=$3")
            .bind(final_pid as i64)
            .bind(ts)
            .bind(p.id)
            .execute(&self.db)
            .await;

        self.logs.add(
            Some(p.id),
            None,
            "system",
            "info",
            &if p.project_kind == "service" {
                format!(
                    "Started service '{}' with command '{}' (PID: {}, folder: {})",
                    p.name,
                    final_cmd,
                    pid,
                    work_dir.display()
                )
            } else {
                format!(
                    "Started container '{}' process '{}' (PID: {}, Port: {}, DB: {})",
                    container.container_id,
                    final_cmd,
                    pid,
                    container.port,
                    container.db_dir.display()
                )
            },
        );

        {
            let mut map = self.processes.lock().unwrap();
            map.insert(p.id, child);
        }
        Ok(pid)
    }



    pub(crate) async fn mark_project_stopped(&self, project_id: i64) {
        let _ = sqlx::query(
            "UPDATE projects SET app_status='stopped', app_pid=NULL, app_started_at=NULL WHERE id=$1",
        )
        .bind(project_id)
        .execute(&self.db)
        .await;
    }

    pub async fn stop_project(&self, p: &Project) -> Result<(), String> {
        let was_active = sqlx::query_scalar::<_, String>("SELECT app_status FROM projects WHERE id = $1")
            .bind(p.id)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten()
            .map(|s| s == "running")
            .unwrap_or(p.app_status == "running");

        // Bump stop_seq first so any in-progress deployment knows Stop was requested
        self.bump_stop_seq(p.id);

        // Immediately update status to stopped
        self.mark_project_stopped(p.id).await;

        // Acquire per-project lock and synchronously terminate all processes + port listeners for this website
        let proj_lock = self.get_project_lock(p.id);
        let _guard = proj_lock.lock().await;
        let killed_any = self.kill_project_everything(p).await;

        // Re-ensure status is stopped in case a concurrent start finished just before the lock
        self.mark_project_stopped(p.id).await;

        if was_active || killed_any {
            self.logs.add(
                Some(p.id),
                None,
                "system",
                "info",
                &if p.project_kind == "service" {
                    format!("Stopped service '{}' (all processes terminated)", p.name)
                } else {
                    format!(
                        "Stopped website container '{}' (Port {} released and all processes terminated)",
                        p.name,
                        p.port.unwrap_or(3000)
                    )
                },
            );
        }

        Ok(())
    }

    pub async fn is_running(&self, project_id: i64) -> bool {
        let in_mem = {
            let mut map = match self.processes.lock() {
                Ok(m) => m,
                Err(_) => return false,
            };
            if let Some(child) = map.get_mut(&project_id) {
                // Report only. The supervisor removes exited children and decides whether to restart
                // them; removing here (status polls run constantly) would hide crashes from it.
                // tokio caches the exit status, so the supervisor's own try_wait still sees it.
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => Some(false),
                    Ok(None) => Some(true),
                }
            } else {
                None
            }
        };
        if let Some(running) = in_mem {
            return running;
        }

        if let Ok(Some(Some(pid_i64))) = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT app_pid FROM projects WHERE id = $1 AND app_status = 'running'",
        )
        .bind(project_id)
        .fetch_optional(&self.db)
        .await
        {
            if let Ok(pid) = u32::try_from(pid_i64) {
                if pid > 0 && is_pid_alive(pid) {
                    return true;
                }
            }
        }
        false
    }

}
