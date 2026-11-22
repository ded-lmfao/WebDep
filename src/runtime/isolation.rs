//! Optional isolation of app processes on Linux.
//!
//! Without it an app runs as the same server user as the panel, so code a user deploys could read
//! the panel's data folder, `.env` and every other app's files. Turn it on with
//! `WEBDEP_ISOLATE_APPS=1` (the panel must run as root). Each app process (and each build) is then
//! started like this:
//!
//! * `unshare` gives it its own PID, mount, IPC and UTS namespaces: it cannot see or signal other
//!   processes, and mount changes stay inside its namespace;
//! * inside the mount namespace the panel's data folder and working folder are covered by an empty
//!   tmpfs and only the app's own folder is bound back in, so other apps and the panel's secrets
//!   do not exist for it; `/tmp` is private;
//! * it then drops to an unprivileged user (`WEBDEP_APP_UID` / `WEBDEP_APP_GID`, default 65534,
//!   "nobody") with `setpriv --no-new-privs`, so it cannot undo any of this.
//!
//! CPU and memory limits (cgroups) apply on top, see `limits.rs`. The network namespace is left
//! shared because apps listen on ports that the reverse proxy reaches.
//!
//! The command building here is plain string work and is unit tested everywhere. Running it needs
//! a Linux server with util-linux (`unshare`, `setpriv`, `mount`).

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

pub const ENV_ENABLE: &str = "WEBDEP_ISOLATE_APPS";

/// Who the isolated process runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
}

/// `Some` when isolation was asked for (`WEBDEP_ISOLATE_APPS=1|true|yes`).
pub fn configured() -> Option<Identity> {
    let on = std::env::var(ENV_ENABLE).is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    if !on {
        return None;
    }
    let read = |key: &str| std::env::var(key).ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(65534);
    Some(Identity { uid: read("WEBDEP_APP_UID"), gid: read("WEBDEP_APP_GID") })
}

/// Quotes text for `sh` as one word.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn depth(p: &Path) -> usize {
    p.components().filter(|c| matches!(c, std::path::Component::Normal(_))).count()
}

/// The folders to cover with an empty tmpfs: the panel's data folder and working folder, minus any
/// that sits inside another (the outer one covers it) and anything too close to `/` to be safe.
pub fn hide_roots(data_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for c in [data_dir, cwd] {
        if depth(c) >= 2 && !roots.iter().any(|r| r == c) {
            roots.push(c.to_path_buf());
        }
    }
    let all = roots.clone();
    roots.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o)));
    roots
}

pub struct Plan {
    /// The app's whole folder (kept visible to it).
    pub own_root: PathBuf,
    /// Where the process starts.
    pub work_dir: PathBuf,
    pub hide_roots: Vec<PathBuf>,
    /// A path outside every hidden folder used for a moment to carry the app folder across.
    pub staging: PathBuf,
    pub hostname: String,
    pub identity: Identity,
}

impl Plan {
    pub fn new(own_root: &Path, work_dir: &Path, data_dir: &Path, cwd: &Path, app_key: &str, identity: Identity) -> Plan {
        let safe: String = app_key.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
        Plan {
            own_root: own_root.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            hide_roots: hide_roots(data_dir, cwd),
            staging: PathBuf::from(format!("/run/webdep-iso-{safe}")),
            hostname: format!("app-{safe}"),
            identity,
        }
    }
}

/// The full command line (for `sh -c`) that runs `app_cmd` isolated.
pub fn wrap(plan: &Plan, app_cmd: &str) -> String {
    let q = |p: &Path| shell_quote(&p.to_string_lossy());
    let own = q(&plan.own_root);
    let stage = q(&plan.staging);
    let covering: Vec<&PathBuf> = plan.hide_roots.iter().filter(|r| plan.own_root.starts_with(r)).collect();

    // The panel starts apps with umask 027, which would make the folders created below root-only
    // (750) and block the app user from walking to its own files. Set up with a normal umask and
    // give the app its restrictive one back at the end.
    let mut s = String::from("set -e\numask 022\n");
    s.push_str(&format!("hostname {}\n", shell_quote(&plan.hostname)));
    s.push_str("mount --make-rprivate /\n");
    if !covering.is_empty() {
        s.push_str(&format!("mkdir -p {stage}\nmount --bind {own} {stage}\n"));
    }
    for root in &plan.hide_roots {
        s.push_str(&format!("mount -t tmpfs -o mode=0755 tmpfs {}\n", q(root)));
    }
    if !covering.is_empty() {
        s.push_str(&format!("mkdir -p {own}\nmount --bind {stage} {own}\numount -l {stage}\n"));
    }
    s.push_str("mount -t tmpfs -o mode=1777 tmpfs /tmp\n");
    s.push_str(&format!("cd {}\n", q(&plan.work_dir)));
    s.push_str("umask 027\n");
    s.push_str(&format!(
        "exec setpriv --reuid={} --regid={} --clear-groups --no-new-privs -- sh -c {}\n",
        plan.identity.uid,
        plan.identity.gid,
        shell_quote(app_cmd)
    ));
    format!(
        "exec unshare --pid --fork --kill-child --mount-proc --mount --ipc --uts -- sh -c {}",
        shell_quote(&s)
    )
}

/// What the App Limits page shows about isolation.
#[derive(serde::Serialize)]
pub struct Status {
    /// Asked for with `WEBDEP_ISOLATE_APPS`.
    pub requested: bool,
    /// Asked for and possible on this server.
    pub active: bool,
    pub reason: String,
}

pub fn status() -> Status {
    let requested = configured().is_some();
    let reason = if requested { unavailable_reason().unwrap_or_default() } else { String::new() };
    Status { requested, active: requested && reason.is_empty(), reason }
}

/// Result of `prepare`.
#[cfg(target_os = "linux")]
pub struct Prepared {
    /// The command to run (wrapped when `isolated`).
    pub command: String,
    pub isolated: bool,
    /// Something worth showing in the app's log (isolation asked for but not possible).
    pub warning: Option<String>,
}

/// Wraps `cmd` so it runs isolated inside `own_root`, or returns it unchanged when isolation is off
/// or this server cannot do it. Hands the app's folder to the unprivileged user first. Blocking:
/// call it from `spawn_blocking`.
#[cfg(target_os = "linux")]
pub fn prepare(cmd: &str, own_root: &Path, work_dir: &Path, key: &str) -> Prepared {
    let unchanged = |warning| Prepared { command: cmd.to_string(), isolated: false, warning };
    let Some(identity) = configured() else {
        return unchanged(None);
    };
    if let Some(why) = unavailable_reason() {
        return unchanged(Some(format!("App isolation is turned on but {why}, so this ran without it.")));
    }
    let own = std::fs::canonicalize(own_root).unwrap_or_else(|_| own_root.to_path_buf());
    let work = std::fs::canonicalize(work_dir).unwrap_or_else(|_| work_dir.to_path_buf());
    let cwd = std::env::current_dir().and_then(std::fs::canonicalize).unwrap_or_default();
    let data = std::env::var("DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let data = std::fs::canonicalize(cwd.join(data.trim_start_matches("./"))).unwrap_or_else(|_| cwd.join("data"));
    if let Err(e) = hand_over(&own, identity) {
        return unchanged(Some(format!("App isolation could not give the app its folder ({e}), so this ran without it.")));
    }
    let plan = Plan::new(&own, &work, &data, &cwd, key, identity);
    Prepared { command: wrap(&plan, cmd), isolated: true, warning: None }
}

/// Tools the wrapper needs that are not installed (empty when all are present).
#[cfg(target_os = "linux")]
pub fn missing_tools() -> Vec<&'static str> {
    let path = std::env::var("PATH").unwrap_or_default();
    ["unshare", "setpriv", "mount", "umount", "mkdir", "hostname"]
        .into_iter()
        .filter(|t| !std::env::split_paths(&path).any(|d| d.join(t).is_file()))
        .collect()
}

/// Why isolation cannot be used right now, or `None` when it can.
#[cfg(target_os = "linux")]
pub fn unavailable_reason() -> Option<String> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Some("the panel is not running as root".to_string());
    }
    let missing = missing_tools();
    if !missing.is_empty() {
        return Some(format!("these tools are missing: {}", missing.join(", ")));
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn unavailable_reason() -> Option<String> {
    Some("it is only available on Linux".to_string())
}

/// Gives the unprivileged user ownership of everything in the app's folder, so it can read and write
/// its own files once it no longer runs as root. Every entry is checked on every start: files that
/// root created since the last start (a deploy, `git pull`, `pip install`) are fixed too, and one
/// file that cannot be changed does not stop the rest. Returns the first error, if any.
#[cfg(target_os = "linux")]
pub fn hand_over(own_root: &Path, identity: Identity) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    fn walk(p: &Path, id: Identity, first_error: &mut Option<std::io::Error>) {
        let Ok(meta) = std::fs::symlink_metadata(p) else { return };
        if meta.uid() != id.uid || meta.gid() != id.gid {
            match std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) {
                Ok(c) => {
                    // SAFETY: c is a valid NUL-terminated path; lchown does not follow symlinks.
                    if unsafe { libc::lchown(c.as_ptr(), id.uid, id.gid) } != 0 && first_error.is_none() {
                        *first_error = Some(std::io::Error::last_os_error());
                    }
                }
                Err(e) => {
                    if first_error.is_none() {
                        *first_error = Some(std::io::Error::new(std::io::ErrorKind::InvalidInput, e));
                    }
                }
            }
        }
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(p) {
                for entry in entries.flatten() {
                    walk(&entry.path(), id, first_error);
                }
            }
        }
    }

    let mut first_error = None;
    walk(own_root, identity, &mut first_error);
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    /// Needs no root: handing a folder to the user who already owns it must succeed, walk
    /// everything, and not trip over a dangling symlink.
    #[cfg(target_os = "linux")]
    #[test]
    fn hand_over_walks_everything_and_survives_odd_entries() {
        let dir = std::env::temp_dir().join(format!("webdep-handover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/file.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", dir.join("dangling")).unwrap();
        // SAFETY: getuid/getgid have no preconditions.
        let me = Identity { uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() } };
        assert!(hand_over(&dir, me).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn hides_data_and_working_folders_but_never_the_root() {
        assert_eq!(hide_roots(&p("/var/lib/webdep/data"), &p("/opt/webdep")), vec![p("/var/lib/webdep/data"), p("/opt/webdep")]);
        // A folder inside another is covered by the outer one.
        assert_eq!(hide_roots(&p("/opt/webdep/data"), &p("/opt/webdep")), vec![p("/opt/webdep")]);
        // Too close to / is never covered.
        assert_eq!(hide_roots(&p("/data"), &p("/")), Vec::<PathBuf>::new());
    }

    #[test]
    fn wrapper_hides_other_apps_and_drops_privileges() {
        let plan = Plan::new(
            &p("/opt/webdep/data/deployments/shop"),
            &p("/opt/webdep/data/deployments/shop/releases/rel-3"),
            &p("/opt/webdep/data"),
            &p("/opt/webdep"),
            "app-7",
            Identity { uid: 65534, gid: 65534 },
        );
        let cmd = wrap(&plan, "node server.js");
        assert!(cmd.starts_with("exec unshare --pid --fork --kill-child --mount-proc --mount --ipc --uts -- sh -c "));
        // The script is one quoted word: everything below is inside it.
        let script = cmd.splitn(2, "-- sh -c ").nth(1).unwrap();
        assert!(script.contains("mount --make-rprivate /"));
        assert!(script.contains("mount -t tmpfs -o mode=0755 tmpfs"));
        assert!(script.contains("/opt/webdep/data/deployments/shop"));
        assert!(script.contains("setpriv --reuid=65534 --regid=65534 --clear-groups --no-new-privs"));
        // Folders made while setting up must be walkable by the app user, whatever umask the panel has.
        let setup_umask = script.find("umask 022").unwrap();
        let first_mkdir = script.find("mkdir -p").unwrap();
        let app_umask = script.find("umask 027").unwrap();
        let exec = script.find("exec setpriv").unwrap();
        assert!(setup_umask < first_mkdir, "umask 022 before any mkdir");
        assert!(first_mkdir < app_umask && app_umask < exec, "the app's umask is restored just before it starts");
        assert!(script.contains("node server.js"));
        // The app's folder is carried across the tmpfs before it is covered, and not the other way.
        let carry = script.find("--bind").unwrap();
        let cover = script.find("mode=0755").unwrap();
        assert!(carry < cover, "bind the app folder aside before covering its parent");
    }

    /// Undoes `shell_quote` the way `sh` reads it: single-quoted runs and backslash-escaped quotes.
    fn unquote(s: &str) -> String {
        let (mut out, mut chars, mut inside) = (String::new(), s.chars().peekable(), false);
        while let Some(c) = chars.next() {
            match (c, inside) {
                ('\'', _) => inside = !inside,
                ('\\', false) => out.extend(chars.next()),
                (c, _) => out.push(c),
            }
        }
        out
    }

    #[test]
    fn app_commands_cannot_break_out_of_the_quoting() {
        for tricky in ["echo 'hi'; rm -rf / #", r#"a"b $(id) `id` \ ''"#, "plain"] {
            assert_eq!(unquote(&shell_quote(tricky)), tricky);
        }
        let plan = Plan::new(&p("/srv/a/x"), &p("/srv/a/x"), &p("/srv/a"), &p("/srv/b"), "x", Identity { uid: 1, gid: 1 });
        let app = "echo 'hi'; rm -rf / #";
        let cmd = wrap(&plan, app);
        // The whole script is one word for the outer sh; inside it the app command is one word again.
        let script = unquote(cmd.splitn(2, "-- sh -c ").nth(1).unwrap());
        let last = script.lines().last().unwrap();
        let word = last.splitn(2, "-- sh -c ").nth(1).unwrap();
        assert_eq!(unquote(word), app);
    }

    #[test]
    fn off_unless_asked_for() {
        std::env::remove_var(ENV_ENABLE);
        assert!(configured().is_none());
    }
}
