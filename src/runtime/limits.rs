//! Per-app CPU and memory limits.
//!
//! On Linux with cgroups v2 each limited app gets `/sys/fs/cgroup/webdep/app-<id>` with
//! `cpu.max` and `memory.max`; the app's shell joins it before running the start command, so
//! everything it starts is limited too. Elsewhere (Windows, cgroups v1, no permission) the app
//! runs without limits and its log says so.

/// Highest CPU limit accepted, as a percentage of one core (64 cores).
pub const MAX_CPU_PERCENT: i32 = 6400;
pub const MIN_MEMORY_MB: i32 = 32;
pub const MAX_MEMORY_MB: i32 = 1_048_576;

pub fn validate(cpu_percent: i32, memory_mb: i32) -> Result<(), String> {
    if !(0..=MAX_CPU_PERCENT).contains(&cpu_percent) {
        return Err(format!("CPU limit must be 0 (no limit) to {MAX_CPU_PERCENT}% (one core is 100%)."));
    }
    if memory_mb != 0 && !(MIN_MEMORY_MB..=MAX_MEMORY_MB).contains(&memory_mb) {
        return Err(format!("Memory limit must be 0 (no limit) or {MIN_MEMORY_MB} to {MAX_MEMORY_MB} MB."));
    }
    Ok(())
}

/// `cpu.max` value: quota and period in microseconds.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn cpu_max_value(cpu_percent: i32) -> String {
    if cpu_percent <= 0 {
        "max 100000".to_string()
    } else {
        format!("{} 100000", i64::from(cpu_percent) * 1000)
    }
}

/// `memory.max` value in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn memory_max_value(memory_mb: i32) -> String {
    if memory_mb <= 0 {
        "max".to_string()
    } else {
        (i64::from(memory_mb) * 1024 * 1024).to_string()
    }
}

/// Creates (or updates) the app's cgroup and returns the path of its `cgroup.procs`, which the
/// child writes "0" into to join. `Ok(None)` when the app has no limits.
#[cfg(target_os = "linux")]
pub fn prepare(project_id: i64, cpu_percent: i32, memory_mb: i32) -> Result<Option<std::ffi::CString>, String> {
    use std::fs;
    use std::path::Path;

    if cpu_percent <= 0 && memory_mb <= 0 {
        return Ok(None);
    }
    let root = Path::new("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").exists() {
        return Err("this server doesn't use cgroups v2".into());
    }
    let parent = root.join("webdep");
    fs::create_dir_all(&parent).map_err(|e| format!("can't create {}: {e}", parent.display()))?;
    // Let the cpu and memory controllers reach WebDep's cgroups (best effort: they may already be on).
    let _ = fs::write(root.join("cgroup.subtree_control"), "+cpu +memory");
    let _ = fs::write(parent.join("cgroup.subtree_control"), "+cpu +memory");

    let dir = parent.join(format!("app-{project_id}"));
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    fs::write(dir.join("cpu.max"), cpu_max_value(cpu_percent)).map_err(|e| format!("can't set the CPU limit: {e}"))?;
    fs::write(dir.join("memory.max"), memory_max_value(memory_mb)).map_err(|e| format!("can't set the memory limit: {e}"))?;
    let procs = dir.join("cgroup.procs");
    std::ffi::CString::new(procs.to_string_lossy().as_bytes())
        .map(Some)
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn prepare(_project_id: i64, cpu_percent: i32, memory_mb: i32) -> Result<Option<std::ffi::CString>, String> {
    if cpu_percent <= 0 && memory_mb <= 0 {
        Ok(None)
    } else {
        Err("resource limits are only enforced on Linux".into())
    }
}

/// Joins the calling process to the cgroup. Runs in the child between fork and exec, so it only
/// uses async-signal-safe calls.
#[cfg(target_os = "linux")]
pub fn join_in_child(procs: &std::ffi::CStr) -> std::io::Result<()> {
    // SAFETY: open/write/close with a valid NUL-terminated path and a static buffer.
    unsafe {
        let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written = libc::write(fd, b"0".as_ptr().cast(), 1);
        libc::close(fd);
        if written != 1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_formats_limits() {
        assert!(validate(0, 0).is_ok());
        assert!(validate(150, 512).is_ok());
        assert!(validate(-1, 0).is_err());
        assert!(validate(MAX_CPU_PERCENT + 1, 0).is_err());
        assert!(validate(0, 16).is_err());
        assert_eq!(cpu_max_value(150), "150000 100000");
        assert_eq!(cpu_max_value(0), "max 100000");
        assert_eq!(memory_max_value(512), "536870912");
        assert_eq!(memory_max_value(0), "max");
    }

    #[test]
    fn apps_without_limits_need_no_cgroup() {
        assert_eq!(prepare(1, 0, 0).unwrap(), None);
    }

    /// Real cgroup round trip; needs root on a cgroups v2 host, otherwise it is skipped.
    #[cfg(target_os = "linux")]
    #[test]
    fn child_joins_its_cgroup_with_limits() {
        use std::os::unix::process::CommandExt;
        use std::path::Path;

        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 || !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            eprintln!("skipped: needs root and cgroups v2");
            return;
        }
        let id = 990_000 + i64::from(std::process::id() % 10_000);
        let procs = prepare(id, 50, 64).expect("prepare").expect("limits set");
        let dir = Path::new("/sys/fs/cgroup/webdep").join(format!("app-{id}"));
        assert_eq!(std::fs::read_to_string(dir.join("cpu.max")).unwrap().trim(), "50000 100000");
        assert_eq!(std::fs::read_to_string(dir.join("memory.max")).unwrap().trim(), "67108864");

        let mut cmd = std::process::Command::new("cat");
        cmd.arg("/proc/self/cgroup");
        // SAFETY: join_in_child only uses async-signal-safe calls.
        unsafe {
            cmd.pre_exec(move || join_in_child(&procs));
        }
        let out = cmd.output().expect("run child");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains(&format!("/webdep/app-{id}")), "child cgroup: {text}");
        let _ = std::fs::remove_dir(&dir);
    }
}
