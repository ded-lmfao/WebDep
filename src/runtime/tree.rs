use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::projects::Project;
use super::Runtime;

#[cfg(windows)]
mod win_pid {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut core::ffi::c_void;
        fn WaitForSingleObject(hHandle: *mut core::ffi::c_void, dwMilliseconds: u32) -> u32;
        fn GetExitCodeProcess(hProcess: *mut core::ffi::c_void, lpExitCode: *mut u32) -> i32;
        fn CloseHandle(hObject: *mut core::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const WAIT_TIMEOUT: u32 = 0x0000_0102;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_ACCESS_DENIED: u32 = 5;

    pub fn is_alive(pid: u32) -> bool {
        unsafe {
            // SYNCHRONIZE allows WaitForSingleObject(..., 0) to detect signaled/exited processes cleanly
            // without any ambiguity on exit code 259.
            let h = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !h.is_null() {
                let wait = WaitForSingleObject(h, 0);
                CloseHandle(h);
                return wait == WAIT_TIMEOUT;
            }
            if GetLastError() != ERROR_ACCESS_DENIED {
                return false;
            }
            let h_limited = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !h_limited.is_null() {
                let mut code: u32 = 0;
                let ok = GetExitCodeProcess(h_limited, &mut code);
                CloseHandle(h_limited);
                return ok != 0 && code == STILL_ACTIVE;
            }
            // ERROR_ACCESS_DENIED on both implies an elevated/protected live process exists.
            GetLastError() == ERROR_ACCESS_DENIED
        }
    }
}

pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0
    }
    #[cfg(windows)]
    {
        win_pid::is_alive(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from(pid as usize)]), true);
        sys.process(sysinfo::Pid::from(pid as usize)).is_some()
    }
}

/// Locate Java executable and JDK/JRE home across environment variables, common installation directories, and system PATH.
pub fn detect_java_exe() -> Option<(PathBuf, PathBuf)> {
    // 1. Check JAVA_HOME environment variable
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&jh);
        #[cfg(windows)]
        let exe = p.join("bin").join("java.exe");
        #[cfg(not(windows))]
        let exe = p.join("bin").join("java");
        if exe.exists() {
            return Some((exe, p));
        }
    }

    #[cfg(windows)]
    {
        // 2. Scan standard Windows JDK/JRE directories
        let search_dirs = [
            r"C:\Program Files\Eclipse Adoptium",
            r"C:\Program Files\Java",
            r"C:\Program Files\Microsoft",
            r"C:\Program Files\Amazon Corretto",
            r"C:\Program Files\BellSoft",
            r"C:\Program Files\Zulu",
            r"C:\Program Files (x86)\Java",
        ];

        for root_str in &search_dirs {
            let root = Path::new(root_str);
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let java_exe = path.join("bin").join("java.exe");
                        if java_exe.exists() {
                            return Some((java_exe, path));
                        }
                        if let Ok(subentries) = std::fs::read_dir(&path) {
                            for sub in subentries.flatten() {
                                let subpath = sub.path();
                                if subpath.is_dir() {
                                    let sub_java = subpath.join("bin").join("java.exe");
                                    if sub_java.exists() {
                                        return Some((sub_java, subpath));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        // 2. Scan standard Linux JVM / JDK directories
        let search_dirs = [
            "/usr/lib/jvm",
            "/usr/java",
            "/opt/java",
            "/opt/jdk",
        ];
        for root_str in &search_dirs {
            let root = Path::new(root_str);
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let java_exe = path.join("bin").join("java");
                    if java_exe.exists() {
                        return Some((java_exe, path));
                    }
                }
            }
        }
    }

    // 3. Scan PATH
    #[cfg(windows)]
    let java_name = "java.exe";
    #[cfg(not(windows))]
    let java_name = "java";

    if let Ok(path_var) = std::env::var("PATH") {
        let separator = if cfg!(windows) { ';' } else { ':' };
        for part in path_var.split(separator) {
            let candidate = Path::new(part).join(java_name);
            if candidate.exists() {
                let home = Path::new(part).parent().unwrap_or(Path::new(part)).to_path_buf();
                return Some((candidate, home));
            }
        }
    }

    None
}

pub(crate) async fn kill_project_processes_pure(
    project_id: i64,
    slug: &str,
    deploy_dir: &str,
    port: Option<u16>,
    known_extra_pids: &[u32],
    old_child: Option<tokio::process::Child>,
) -> bool {
    let fake_proj = Project {
        id: project_id,
        name: slug.to_string(),
        slug: slug.to_string(),
        repo_url: String::new(),
        branch: String::new(),
        project_type: String::new(),
        deploy_method: String::new(),
        deploy_dir: deploy_dir.to_string(),
        port,
        install_cmd: String::new(),
        build_cmd: String::new(),
        start_cmd: String::new(),
        health_url: String::new(),
        health_timeout_s: 30,
        stability_s: 5,
        auto_deploy: false,
        webhook_secret: None,
        current_release_id: None,
        deployed_sha: None,
        last_poll_at: None,
        app_status: String::new(),
        app_pid: None,
        app_started_at: None,
        domains: Vec::new(),
        ssl_email: String::new(),
        created_at: 0,
        updated_at: 0,
        project_kind: "web".to_string(),
        access_mode: "local".to_string(),
        allowed_ips: String::new(),
        node_id: None,
        placement_version: 1,
        assigned_port: None,
    };
    let container = super::container::ensure_container_with_port(&fake_proj, port.unwrap_or(3000));
    let mut pids_to_kill: HashSet<u32> = HashSet::new();

    let had_child = old_child.is_some();
    if let Some(ref child) = old_child {
        if let Some(pid) = child.id() {
            pids_to_kill.insert(pid);
        }
    }

    for &pid in known_extra_pids {
        if pid > 0 {
            pids_to_kill.insert(pid);
        }
    }

    for cpid in super::container::read_container_pids_from(&container) {
        pids_to_kill.insert(cpid);
    }

    if let Some(port_num) = port {
        for port_pid in Runtime::find_pids_on_port(port_num) {
            pids_to_kill.insert(port_pid);
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base_dir = if Path::new(deploy_dir).is_absolute() {
        PathBuf::from(deploy_dir)
    } else {
        cwd.join(deploy_dir.trim_start_matches("./").trim_start_matches(".\\"))
    };
    let sysinfo_pids = Runtime::find_project_processes_sysinfo(project_id, slug, &base_dir, &pids_to_kill);
    for spid in sysinfo_pids {
        pids_to_kill.insert(spid);
    }

    for pid in &pids_to_kill {
        Runtime::kill_pid_tree(*pid).await;
    }

    if let Some(mut child) = old_child {
        let _ = child.start_kill();
    }

    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(60)).await;
        let lingering = Runtime::find_project_processes_sysinfo(project_id, slug, &base_dir, &HashSet::new());
        if lingering.is_empty() {
            break;
        }
        for pid in lingering {
            Runtime::kill_pid_tree(pid).await;
        }
    }

    if let Some(port_num) = port {
        for _ in 0..12 {
            let remaining_pids = Runtime::find_pids_on_port(port_num);
            if remaining_pids.is_empty() && !Runtime::is_port_accepting(port_num).await {
                break;
            }
            for pid in remaining_pids {
                Runtime::kill_pid_tree(pid).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    super::container::clear_container_pids_in(&container);
    !pids_to_kill.is_empty() || had_child
}

impl Runtime {
    pub async fn kill_pid_tree(pid: u32) {
        if pid == 0 || pid == std::process::id() {
            return;
        }
        #[cfg(windows)]
        {
            // 1. First taskkill /F /T /PID to take out the entire process tree while parent is still recognized
            let _ = tokio::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            // 2. Also taskkill /F /PID directly in case /T fails because the process has no children
            let _ = tokio::process::Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        #[cfg(unix)]
        {
            let _ = tokio::process::Command::new("kill")
                .args(["-9", &format!("-{}", pid)])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            let _ = tokio::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
    }

    /// Comprehensively locate ALL OS processes belonging to this project (parent, children,
    /// detached subprocesses, multi-bot workers) using sysinfo inspection.
    pub fn find_project_processes_sysinfo(
        project_id: i64,
        slug: &str,
        base_dir: &Path,
        known_pids: &HashSet<u32>,
    ) -> HashSet<u32> {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let mut matched_pids = HashSet::new();
        let self_pid = std::process::id();

        let base_dir_str = base_dir.to_string_lossy().to_lowercase();
        let base_dir_norm = PathBuf::from(&base_dir_str);
        let has_specific_base_dir = !base_dir_str.is_empty() && base_dir.components().count() > 1;
        let slug_lower = slug.to_lowercase();
        let env_proj_id = format!("WEBDEP_PROJECT_ID={}", project_id);
        let env_slug = format!("WEBDEP_PROJECT_SLUG={}", slug);

        for (pid, proc_) in sys.processes() {
            // Skip OS thread entries (`/proc/<tgid>/task/<tid>` on Linux); only match real processes.
            if proc_.thread_kind().is_some() {
                continue;
            }
            let pid_u32 = pid.as_u32();
            if pid_u32 == 0 || pid_u32 == self_pid {
                continue;
            }

            // 1. Direct PID match with known project PIDs
            if known_pids.contains(&pid_u32) {
                matched_pids.insert(pid_u32);
                continue;
            }

            // 2. Direct parent PID match
            if let Some(parent) = proc_.parent() {
                if known_pids.contains(&parent.as_u32()) {
                    matched_pids.insert(pid_u32);
                    continue;
                }
            }

            // 3. Environment variable match (injected by WebDep on process spawn)
            let mut env_match = false;
            for env in proc_.environ() {
                let s = env.to_string_lossy();
                if s == env_proj_id || s == env_slug {
                    env_match = true;
                    break;
                }
            }
            if env_match {
                matched_pids.insert(pid_u32);
                continue;
            }

            // 4. Working directory match (component-safe inside base_dir)
            if has_specific_base_dir {
                if let Some(cwd) = proc_.cwd() {
                    let cwd_norm = PathBuf::from(cwd.to_string_lossy().to_lowercase());
                    if cwd_norm == base_dir_norm || cwd_norm.starts_with(&base_dir_norm) {
                        matched_pids.insert(pid_u32);
                        continue;
                    }
                }
            }

            // 5. Command line match (references services/<slug> or containers/<slug>)
            let mut cmd_match = false;
            for arg in proc_.cmd() {
                let arg_lower = arg.to_string_lossy().to_lowercase();
                if (has_specific_base_dir && arg_lower.contains(&base_dir_str))
                    || arg_lower.contains(&format!("services\\{}", slug_lower))
                    || arg_lower.contains(&format!("services/{}", slug_lower))
                    || arg_lower.contains(&format!("containers\\{}", slug_lower))
                    || arg_lower.contains(&format!("containers/{}", slug_lower))
                {
                    cmd_match = true;
                    break;
                }
            }
            if cmd_match {
                matched_pids.insert(pid_u32);
            }
        }

        // 6. Transitive child expansion: find all children and grandchildren of any matched process
        let mut expanded = true;
        while expanded {
            expanded = false;
            for (pid, proc_) in sys.processes() {
                if proc_.thread_kind().is_some() {
                    continue;
                }
                let pid_u32 = pid.as_u32();
                if pid_u32 == 0 || pid_u32 == self_pid || matched_pids.contains(&pid_u32) {
                    continue;
                }
                if let Some(parent) = proc_.parent() {
                    if matched_pids.contains(&parent.as_u32()) {
                        matched_pids.insert(pid_u32);
                        expanded = true;
                    }
                }
            }
        }

        matched_pids
    }

    /// Comprehensively shut down all processes, child trees, port listeners, and containers for a website or service.
    /// Returns true if any active processes or listening ports were terminated.
    pub(crate) async fn kill_project_everything(&self, p: &Project) -> bool {
        // First stop any maintenance responder bound to this project's port
        self.stop_maintenance_server(p.id);
        tokio::time::sleep(Duration::from_millis(40)).await;

        let container = self.ensure_container(p).await;
        let old_child = {
            let mut map = self.processes.lock().unwrap();
            map.remove(&p.id)
        };
        let mut known_extra = Vec::new();
        if let Some(old_pid) = p.app_pid {
            if old_pid > 0 {
                known_extra.push(old_pid);
            }
        }
        if let Ok(Some(Some(db_pid))) = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT app_pid FROM projects WHERE id = $1",
        )
        .bind(p.id)
        .fetch_optional(&self.db)
        .await
        {
            if let Ok(pid) = u32::try_from(db_pid) {
                if pid > 0 {
                    known_extra.push(pid);
                }
            }
        }

        kill_project_processes_pure(
            p.id,
            &p.slug,
            &p.deploy_dir,
            Some(p.port.unwrap_or(container.port)),
            &known_extra,
            old_child,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::is_pid_alive;

    #[test]
    fn test_is_pid_alive_handles_zero_self_and_exited_child() {
        assert!(!is_pid_alive(0));
        assert!(is_pid_alive(std::process::id()));

        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit 0"])
            .spawn()
            .expect("spawn exit cmd");
        #[cfg(not(windows))]
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");

        let child_pid = child.id();
        let _ = child.wait();
        assert!(!is_pid_alive(child_pid));
    }
}

