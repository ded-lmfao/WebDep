//! Per-app CPU and memory limits.
//!
//! - **Linux (cgroups v2):** each limited app gets `/sys/fs/cgroup/webdep/<app-id|db-id>` with
//!   `cpu.max` and `memory.max`; the app's shell joins it before running the start command, so
//!   everything it starts is limited too. Needs WebDep to run as root.
//! - **Windows (Job Objects):** right after the app starts, its process is put in a job object
//!   with a hard CPU cap and a memory limit; everything it starts afterwards inherits the job.
//! - **Anywhere else, or without permission:** the app runs without limits and its log says so.
//!   `enforcement_status` tells the admin panel which case applies.

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

/// Creates (or updates) the cgroup `webdep/<name>` (`app-<id>` or `db-<id>`) and returns the path of its `cgroup.procs`, which the
/// child writes "0" into to join. `Ok(None)` when the app has no limits.
#[cfg(target_os = "linux")]
pub fn prepare(name: &str, cpu_percent: i32, memory_mb: i32) -> Result<Option<std::ffi::CString>, String> {
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

    let dir = parent.join(name);
    fs::create_dir_all(&dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    fs::write(dir.join("cpu.max"), cpu_max_value(cpu_percent)).map_err(|e| format!("can't set the CPU limit: {e}"))?;
    fs::write(dir.join("memory.max"), memory_max_value(memory_mb)).map_err(|e| format!("can't set the memory limit: {e}"))?;
    let procs = dir.join("cgroup.procs");
    std::ffi::CString::new(procs.to_string_lossy().as_bytes())
        .map(Some)
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn prepare(_name: &str, cpu_percent: i32, memory_mb: i32) -> Result<Option<std::ffi::CString>, String> {
    // Windows applies its limits to the process after it starts (`apply_to_process`).
    if (cpu_percent <= 0 && memory_mb <= 0) || cfg!(windows) {
        Ok(None)
    } else {
        Err("resource limits are only enforced on Linux and Windows servers".into())
    }
}

/// Whether CPU and memory limits are enforced on this server, for the App Limits page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Enforcement {
    pub works: bool,
    /// How limits are enforced here: `cgroups`, `job-objects`, or `none`.
    pub method: &'static str,
    pub reason: String,
}

#[cfg(target_os = "linux")]
pub fn enforcement_status() -> Enforcement {
    use std::path::Path;
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return Enforcement { works: false, method: "none", reason: "this server doesn't use cgroups v2".into() };
    }
    match std::fs::create_dir_all("/sys/fs/cgroup/webdep") {
        Ok(()) => Enforcement { works: true, method: "cgroups", reason: String::new() },
        Err(e) => Enforcement {
            works: false,
            method: "none",
            reason: format!("WebDep can't use cgroups here ({e}); run it as root"),
        },
    }
}

#[cfg(windows)]
pub fn enforcement_status() -> Enforcement {
    Enforcement { works: true, method: "job-objects", reason: String::new() }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn enforcement_status() -> Enforcement {
    Enforcement { works: false, method: "none", reason: "limits are only enforced on Linux and Windows servers".into() }
}

/// CPU cap as hundredths of a percent of the whole machine (Windows job objects), for a limit
/// given as a percentage of one core. `None` when the limit is 0 or covers every core anyway.
pub fn cpu_rate_hundredths(cpu_percent: i32, cores: usize) -> Option<u32> {
    let cores = cores.max(1) as i64;
    let percent = i64::from(cpu_percent);
    if percent <= 0 || percent >= cores * 100 {
        return None;
    }
    Some((percent * 100 / cores).clamp(1, 10_000) as u32)
}

/// Puts a just-started process (and everything it starts afterwards) under CPU and memory
/// limits using a Windows job object. Call it right after spawning.
#[cfg(windows)]
pub fn apply_to_process(process: std::os::windows::io::RawHandle, cpu_percent: i32, memory_mb: i32) -> Result<(), String> {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_JOB_MEMORY,
    };

    if cpu_percent <= 0 && memory_mb <= 0 {
        return Ok(());
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);

    // SAFETY: plain Win32 calls with valid, correctly sized structs; the job handle is closed
    // below (the job lives on until its processes exit).
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!("can't create a job object: {}", std::io::Error::last_os_error()));
        }
        let result = (|| {
            if memory_mb > 0 {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY;
                info.JobMemoryLimit = (memory_mb as usize).saturating_mul(1024 * 1024);
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    return Err(format!("can't set the memory limit: {}", std::io::Error::last_os_error()));
                }
            }
            if let Some(rate) = cpu_rate_hundredths(cpu_percent, cores) {
                let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = zeroed();
                info.ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
                info.Anonymous.CpuRate = rate;
                if SetInformationJobObject(
                    job,
                    JobObjectCpuRateControlInformation,
                    &info as *const _ as *const c_void,
                    size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                ) == 0
                {
                    return Err(format!("can't set the CPU limit: {}", std::io::Error::last_os_error()));
                }
            }
            if AssignProcessToJobObject(job, process) == 0 {
                return Err(format!("can't add the app to the job: {}", std::io::Error::last_os_error()));
            }
            Ok(())
        })();
        CloseHandle(job);
        result
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
    fn cpu_limits_become_a_share_of_the_whole_machine() {
        assert_eq!(cpu_rate_hundredths(0, 8), None);
        assert_eq!(cpu_rate_hundredths(50, 1), Some(5000));
        assert_eq!(cpu_rate_hundredths(100, 8), Some(1250), "one core of eight");
        assert_eq!(cpu_rate_hundredths(800, 8), None, "all cores is no limit");
        assert_eq!(cpu_rate_hundredths(10, 64), Some(15), "never below the smallest step");
        assert_eq!(cpu_rate_hundredths(1, 64), Some(1));
    }

    #[test]
    fn enforcement_is_reported() {
        let e = enforcement_status();
        assert_eq!(e.works, e.method != "none");
        #[cfg(windows)]
        assert_eq!(e.method, "job-objects");
    }

    /// Real Windows job-object round trips: needs `python` on the PATH, otherwise skipped.
    #[cfg(windows)]
    mod windows_job {
        use super::super::*;
        use std::os::windows::io::AsRawHandle;
        use std::process::{Command, Stdio};

        fn python_works() -> bool {
            Command::new("python")
                .args(["-c", "print(1)"])
                .output()
                .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "1")
                .unwrap_or(false)
        }

        fn run_limited(script: &str, cpu: i32, memory_mb: i32) -> String {
            let child = Command::new("python")
                .args(["-c", script])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start python");
            apply_to_process(child.as_raw_handle(), cpu, memory_mb).expect("apply limits");
            let out = child.wait_with_output().expect("python output");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        #[test]
        fn memory_limit_stops_an_app_from_using_more() {
            if !python_works() {
                eprintln!("skipped: python not found");
                return;
            }
            let script = "import time\ntime.sleep(0.3)\ntry:\n    x = bytearray(300 * 1024 * 1024)\n    print('allocated')\nexcept MemoryError:\n    print('limited')";
            assert_eq!(run_limited(script, 0, 64), "limited", "300 MB is refused under a 64 MB limit");
            assert_eq!(run_limited(script, 0, 0), "allocated", "no limit allocates normally");
        }

        #[test]
        fn cpu_limit_caps_how_much_cpu_an_app_gets() {
            if !python_works() {
                eprintln!("skipped: python not found");
                return;
            }
            let script = "import time\nt = time.time()\nc = time.process_time()\nwhile time.time() - t < 3:\n    pass\nprint(round(time.process_time() - c, 2))";
            let limited: f64 = run_limited(script, 50, 0).parse().expect("cpu seconds");
            let free: f64 = run_limited(script, 0, 0).parse().expect("cpu seconds");
            assert!(free > 2.4, "an unlimited busy loop uses about a core: {free}");
            assert!(limited < 2.2, "half a core over 3 seconds is about 1.5 CPU seconds: {limited}");
            assert!(limited > 0.5, "it still runs: {limited}");
        }
    }

    #[test]
    fn apps_without_limits_need_no_cgroup() {
        assert_eq!(prepare("app-1", 0, 0).unwrap(), None);
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
        let procs = prepare(&format!("app-{id}"), 50, 64).expect("prepare").expect("limits set");
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
