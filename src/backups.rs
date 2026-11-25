//! Backup archives for websites and services.
//!
//! A backup is one `.tar.gz` under `<data dir>/backups/apps/project-<id>/`, outside the app's
//! own folder so cleaning or deleting the app can't remove it. The archive holds the app's
//! files under `files/` and its environment variables as `backup.env`. It is written to a
//! `.part` file first and renamed only when complete, and its SHA-256 and file count are saved
//! with the backup row, so an unfinished or damaged archive is never treated as a good backup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sqlx::Row;

use crate::db::DbPool;

pub const MAX_IGNORE_PATTERNS: usize = 64;

/// Folder holding every app's backup archives.
pub fn archives_root(data_dir: &Path) -> PathBuf {
    data_dir.join("backups").join("apps")
}

pub fn archive_dir(data_dir: &Path, project_id: i64) -> PathBuf {
    archives_root(data_dir).join(format!("project-{project_id}"))
}

pub fn archive_path(data_dir: &Path, project_id: i64, backup_id: i64) -> PathBuf {
    archive_dir(data_dir, project_id).join(format!("backup-{backup_id}.tar.gz"))
}

/// `.part` path an archive is built in before it is renamed into place.
pub fn part_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    final_path.with_file_name(name)
}

/// Validates the ignore patterns typed by the user: plain names (`cache`, `*.log`, `tmp*`),
/// matched against each file or folder name at any depth.
pub fn clean_ignore(patterns: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for raw in patterns {
        let p = raw.trim();
        if p.is_empty() {
            continue;
        }
        if p.len() > 128 || p.contains(['/', '\\', '\0']) || p == "." || p == ".." {
            return Err(format!("'{p}' isn't a valid ignore pattern: use a file or folder name such as cache or *.log."));
        }
        out.push(p.to_string());
    }
    if out.len() > MAX_IGNORE_PATTERNS {
        return Err(format!("Use at most {MAX_IGNORE_PATTERNS} ignore patterns."));
    }
    Ok(out)
}

/// Free bytes on the disk that holds `path` (the longest matching mount point), if known.
pub fn free_space_bytes(path: &Path) -> Option<u64> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| target.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

pub async fn finish_ok(
    db: &DbPool,
    backup_id: i64,
    archive: &Path,
    size_bytes: u64,
    file_count: u64,
    sha256: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE backups SET status = 'complete', error = '', path = $2, archive_path = $2, size_bytes = $3,
                file_count = $4, checksum_sha256 = $5, completed_at = $6 WHERE id = $1",
    )
    .bind(backup_id)
    .bind(archive.to_string_lossy().to_string())
    .bind(size_bytes as i64)
    .bind(file_count as i64)
    .bind(sha256)
    .bind(crate::db::now())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn finish_failed(db: &DbPool, backup_id: i64, error: &str) {
    let _ = sqlx::query("UPDATE backups SET status = 'failed', error = $2, completed_at = $3 WHERE id = $1")
        .bind(backup_id)
        .bind(error.chars().take(500).collect::<String>())
        .bind(crate::db::now())
        .execute(db)
        .await;
}

/// At startup: a backup still marked running was cut off by a restart. Mark it failed and
/// remove the unfinished `.part` files. Returns how many backups were marked.
pub async fn recover_interrupted(db: &DbPool, data_dir: &Path) -> u64 {
    let marked = sqlx::query(
        "UPDATE backups SET status = 'failed', error = 'Interrupted by a restart', completed_at = $1 WHERE status = 'running'",
    )
    .bind(crate::db::now())
    .execute(db)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);
    remove_part_files(&archives_root(data_dir));
    marked
}

fn remove_part_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.is_dir() {
            remove_part_files(&path);
        } else if meta.is_file() && path.extension().is_some_and(|e| e == "part") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Number of backups that count against an app's backup limit (failed ones don't).
pub async fn count_held(db: &DbPool, project_id: i64) -> i64 {
    sqlx::query("SELECT COUNT(*) FROM backups WHERE project_id = $1 AND status <> 'failed'")
        .bind(project_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64, _>(0))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Restore engine: replace an app's files with a backup, never merge
// ---------------------------------------------------------------------------
//
// Restoring changes nothing until the backup has been proven good:
//   1. verify the archive's checksum,
//   2. unpack it into a staging folder inside the app folder and check the file count,
//   3. (the caller stops the app),
//   4. swap: the current files are renamed into `.pre-restore/` and the staged files take their
//      place. If the second rename fails the first is undone, so the app is never left half
//      restored. Files that were not in the backup are gone from the live folder; they stay in
//      the undo copy for 24 hours.

pub const UNDO_KEEP_SECS: u64 = 24 * 60 * 60;
const UNDO_DIR: &str = ".pre-restore";
const STAGE_DIR: &str = ".restore-tmp";

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The app's own folder (`deploy_dir`), made absolute.
pub fn deploy_base(deploy_dir: &str, cwd: &Path) -> PathBuf {
    let raw = Path::new(deploy_dir);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(deploy_dir.trim_start_matches("./").trim_start_matches(".\\"))
    }
}

/// The folder to replace. A deployed website's `current` is a link to a release folder: the
/// release folder itself is swapped, so the link keeps working.
pub fn resolve_real_root(root: &Path) -> PathBuf {
    match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
        _ => root.to_path_buf(),
    }
}

pub fn verify_archive(path: &Path, expected_sha256: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    if expected_sha256.is_empty() {
        return Err("This backup has no checksum, so it can't be verified. Nothing was changed.".into());
    }
    let mut file = std::fs::File::open(path).map_err(|e| format!("The backup file can't be opened: {e}. Nothing was changed."))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("The backup file can't be read: {e}. Nothing was changed."))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    if format!("{:x}", hasher.finalize()) != expected_sha256 {
        return Err("The backup file is damaged (its checksum doesn't match). Nothing was changed.".into());
    }
    Ok(())
}

fn count_files(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match std::fs::symlink_metadata(e.path()) {
            Ok(m) if m.is_dir() => count_files(&e.path()),
            Ok(m) if m.is_file() => 1,
            _ => 0,
        })
        .sum()
}

/// Files unpacked and ready to swap in, and the environment variables saved with them.
pub struct Staged {
    pub new_root: PathBuf,
    pub env: Option<String>,
}

/// Unpacks `archive` into `<work_dir>/.restore-tmp/` (the same disk as the app, so the swap is a
/// rename) and checks that the number of files matches what the backup recorded.
pub fn stage_archive(archive: &Path, work_dir: &Path, backup_id: i64, expected_files: u64) -> Result<Staged, String> {
    let stage = work_dir.join(STAGE_DIR);
    let extract = stage.join(format!("extract-{backup_id}"));
    let new_root = stage.join(format!("new-{backup_id}"));
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&extract);
        let _ = std::fs::remove_dir_all(&new_root);
    };
    cleanup();

    let result = (|| {
        crate::cluster::archive::unpack_tar_gz_safe(archive, &extract)?;
        let files = extract.join("files");
        std::fs::create_dir_all(&files).map_err(|e| format!("Couldn't prepare the restored files: {e}"))?;
        let found = count_files(&files);
        if found != expected_files {
            return Err(format!("The backup holds {found} file(s) but {expected_files} were recorded, so it can't be trusted."));
        }
        let env = std::fs::read_to_string(extract.join("backup.env")).ok();
        std::fs::rename(&files, &new_root).map_err(|e| format!("Couldn't prepare the restored files: {e}"))?;
        Ok(Staged { new_root: new_root.clone(), env })
    })();

    let _ = std::fs::remove_dir_all(&extract);
    match result {
        Ok(staged) => Ok(staged),
        Err(e) => {
            cleanup();
            Err(format!("{e} Nothing was changed."))
        }
    }
}

/// Removes staged files that won't be used.
pub fn discard_staged(staged: &Staged) {
    let _ = std::fs::remove_dir_all(&staged.new_root);
}

/// Replaces `real_root` with the staged files. The old files are kept in `.pre-restore/` for
/// undo; returns where. If the swap fails, the old files are put back.
pub fn swap_in(real_root: &Path, new_root: &Path, work_dir: &Path, label: &str) -> Result<Option<PathBuf>, String> {
    let undo_dir = work_dir.join(UNDO_DIR);
    std::fs::create_dir_all(&undo_dir).map_err(|e| format!("Couldn't prepare the undo folder: {e}"))?;
    let ts = unix_secs();
    let mut undo = undo_dir.join(format!("{label}-{ts}"));
    let mut n = 0;
    while undo.exists() {
        n += 1;
        undo = undo_dir.join(format!("{label}-{ts}-{n}"));
    }

    let had_old = real_root.exists();
    if had_old {
        std::fs::rename(real_root, &undo).map_err(|e| format!("Couldn't move the current files aside: {e}"))?;
    }
    if let Err(e) = std::fs::rename(new_root, real_root) {
        if had_old {
            if let Err(e2) = std::fs::rename(&undo, real_root) {
                return Err(format!(
                    "Couldn't put the restored files in place ({e}) and couldn't put the old files back ({e2}). The old files are in {}.",
                    undo.display()
                ));
            }
        }
        return Err(format!("Couldn't put the restored files in place: {e}"));
    }

    // Only the newest undo copy per folder is kept.
    if had_old {
        let keep = undo.file_name().map(|n| n.to_os_string());
        if let Ok(entries) = std::fs::read_dir(&undo_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if Some(&name) != keep.as_ref() && name.to_string_lossy().starts_with(&format!("{label}-")) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }
    Ok(had_old.then_some(undo))
}

/// Removes undo copies older than `max_age_secs` and leftover staging folders. Returns how many
/// undo copies were removed.
pub fn prune_undo_copies(work_dir: &Path, max_age_secs: u64) -> u64 {
    let now = unix_secs();
    let mut removed = 0;
    if let Ok(entries) = std::fs::read_dir(work_dir.join(UNDO_DIR)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let taken = name.split('-').nth(1).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
            if now.saturating_sub(taken) > max_age_secs && std::fs::remove_dir_all(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Removes staging folders left behind by a restore that was cut off.
pub fn remove_stale_staging(work_dir: &Path) {
    let _ = std::fs::remove_dir_all(work_dir.join(STAGE_DIR));
}

// ---------------------------------------------------------------------------
// Shared by Web-Core (apps on this server) and Web-Nodes (apps on other VPSes)
// ---------------------------------------------------------------------------

/// Archive path stored while a backup on another VPS is still being built.
pub const PENDING_ARCHIVE: &str = "pending";

pub struct ArchiveInfo {
    pub path: PathBuf,
    pub size: u64,
    pub files: u64,
    pub sha256: String,
}

/// Builds backup `backup_id` of an app: packs `source_root` (and `env`) into the app's archive
/// folder under `data_dir`. Written as `.part` and renamed only when complete.
pub fn create_archive_file(
    data_dir: &Path,
    project_id: i64,
    backup_id: i64,
    source_root: &Path,
    env: Option<&str>,
    ignore: &[String],
) -> Result<ArchiveInfo, String> {
    let dest = archive_path(data_dir, project_id, backup_id);
    let part = part_path(&dest);
    let stats = match crate::cluster::archive::pack_backup_archive(source_root, env, &part, ignore) {
        Ok(stats) => stats,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    if let Err(e) = std::fs::rename(&part, &dest) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("Couldn't finish the archive: {e}"));
    }
    Ok(ArchiveInfo { path: dest, size: stats.compressed_bytes, files: stats.files, sha256: stats.sha256 })
}

/// Deletes one backup archive, only if it is inside `data_dir`'s backup archive folder.
pub fn remove_archive_file(data_dir: &Path, path: &Path) -> bool {
    let (Ok(file), Ok(root)) = (std::fs::canonicalize(path), std::fs::canonicalize(archives_root(data_dir))) else {
        return false;
    };
    file.starts_with(&root) && file != root && std::fs::remove_file(&file).is_ok()
}

/// Deletes every backup archive of an app (when the app is deleted).
pub fn remove_project_archives(data_dir: &Path, project_id: i64) {
    let _ = std::fs::remove_dir_all(archive_dir(data_dir, project_id));
}

/// Where a restore reads the backup from: a single archive, or a folder
/// copies (`<dir>/files` and `<dir>/backup.env`).
#[derive(Clone)]
pub enum RestoreSource {
    Archive { path: PathBuf, sha256: String, files: u64 },
    Dir { path: PathBuf },
}

pub struct RestoreOutcome {
    pub env: Option<String>,
    pub undo: Option<PathBuf>,
    /// False for an environment-only backup, which never touches the app's files.
    pub files_replaced: bool,
}

pub struct RestoreFailure {
    pub message: String,
    /// The app had already been stopped when this failed, so the caller should start it again.
    pub app_stopped: bool,
}

fn read_env_only(src: &RestoreSource, work: &Path, backup_id: i64) -> Result<Option<String>, String> {
    match src {
        RestoreSource::Archive { path, sha256, .. } => {
            verify_archive(path, sha256)?;
            let extract = work.join(STAGE_DIR).join(format!("env-{backup_id}"));
            let _ = std::fs::remove_dir_all(&extract);
            let env = crate::cluster::archive::unpack_tar_gz_safe(path, &extract)
                .map(|_| std::fs::read_to_string(extract.join("backup.env")).ok())
                .map_err(|e| format!("{e} Nothing was changed."));
            let _ = std::fs::remove_dir_all(&extract);
            env
        }
        RestoreSource::Dir { path } => Ok(std::fs::read_to_string(path.join("backup.env")).ok()),
    }
}

/// Copies a folder backup into staging and checks that every file arrived.
pub fn stage_dir(backup_dir: &Path, work: &Path, backup_id: i64) -> Result<Staged, String> {
    let files_dir = backup_dir.join("files");
    let new_root = work.join(STAGE_DIR).join(format!("new-{backup_id}"));
    let _ = std::fs::remove_dir_all(&new_root);
    std::fs::create_dir_all(&new_root).map_err(|e| format!("Couldn't prepare the restored files: {e}. Nothing was changed."))?;
    crate::api::projects::backups::copy_backup_files(&files_dir, &new_root);
    let (expected, copied) = (count_files(&files_dir), count_files(&new_root));
    if expected != copied {
        let _ = std::fs::remove_dir_all(&new_root);
        return Err(format!("Only {copied} of the backup's {expected} files could be copied, so it can't be restored. Nothing was changed."));
    }
    Ok(Staged { new_root, env: std::fs::read_to_string(backup_dir.join("backup.env")).ok() })
}

/// Restores an app's files from a backup: replaces them, never merges. The backup is proven good
/// and staged before `stop_app` runs; the swap rolls back if it fails. An environment-only
/// backup (or a folder backup with no files) only returns its environment variables.
pub async fn restore_files<F, Fut>(
    src: RestoreSource,
    env_only: bool,
    work: &Path,
    real_root: &Path,
    label: &str,
    backup_id: i64,
    stop_app: F,
) -> Result<RestoreOutcome, RestoreFailure>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let fail = |message: String, app_stopped: bool| RestoreFailure { message, app_stopped };
    let env_only = env_only || matches!(&src, RestoreSource::Dir { path } if !path.join("files").exists());

    if env_only {
        let (s, w) = (src, work.to_path_buf());
        let env = tokio::task::spawn_blocking(move || read_env_only(&s, &w, backup_id))
            .await
            .map_err(|e| fail(format!("The restore stopped unexpectedly: {e}"), false))?
            .map_err(|m| fail(m, false))?;
        return Ok(RestoreOutcome { env, undo: None, files_replaced: false });
    }

    let (s, w) = (src, work.to_path_buf());
    let staged = tokio::task::spawn_blocking(move || match s {
        RestoreSource::Archive { path, sha256, files } => {
            verify_archive(&path, &sha256)?;
            stage_archive(&path, &w, backup_id, files)
        }
        RestoreSource::Dir { path } => stage_dir(&path, &w, backup_id),
    })
    .await
    .map_err(|e| fail(format!("The restore stopped unexpectedly: {e}"), false))?
    .map_err(|m| fail(m, false))?;

    stop_app().await;

    let (r, n, w, l) = (real_root.to_path_buf(), staged.new_root.clone(), work.to_path_buf(), label.to_string());
    let swapped = tokio::task::spawn_blocking(move || swap_in(&r, &n, &w, &l))
        .await
        .map_err(|e| fail(format!("The restore stopped unexpectedly: {e}"), true))?;
    match swapped {
        Ok(undo) => Ok(RestoreOutcome { env: staged.env, undo, files_replaced: true }),
        Err(e) => {
            discard_staged(&staged);
            let tail = if e.contains("old files back") { "" } else { " Your files were not changed." };
            Err(fail(format!("{e}{tail}"), true))
        }
    }
}

// ---------------------------------------------------------------------------
// One restore at a time per app, and its progress
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct RestoreStatus {
    /// `running`, `done`, or `failed`.
    pub state: String,
    #[serde(rename = "backupId")]
    pub backup_id: i64,
    pub message: String,
}

/// Keyed by the Core's data folder as well as the app id, so each Core instance has its own.
static RESTORES: std::sync::LazyLock<Mutex<HashMap<(PathBuf, i64), RestoreStatus>>> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn restores() -> std::sync::MutexGuard<'static, HashMap<(PathBuf, i64), RestoreStatus>> {
    RESTORES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Marks a restore as started. False when one is already running for the app.
pub fn begin_restore(data_dir: &Path, project_id: i64, backup_id: i64) -> bool {
    let mut map = restores();
    let key = (data_dir.to_path_buf(), project_id);
    if map.get(&key).is_some_and(|s| s.state == "running") {
        return false;
    }
    map.insert(key, RestoreStatus { state: "running".into(), backup_id, message: String::new() });
    true
}

pub fn finish_restore(data_dir: &Path, project_id: i64, ok: bool, message: String) {
    if let Some(s) = restores().get_mut(&(data_dir.to_path_buf(), project_id)) {
        s.state = if ok { "done" } else { "failed" }.into();
        s.message = message;
    }
}

pub fn restore_status(data_dir: &Path, project_id: i64) -> Option<RestoreStatus> {
    restores().get(&(data_dir.to_path_buf(), project_id)).cloned()
}

pub fn is_restoring(data_dir: &Path, project_id: i64) -> bool {
    restores().get(&(data_dir.to_path_buf(), project_id)).is_some_and(|s| s.state == "running")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::archive::{pack_backup_archive, unpack_tar_gz_safe};
    use sha2::{Digest, Sha256};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("webdep-backups-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn validates_ignore_patterns() {
        assert_eq!(clean_ignore(&[" cache ".into(), "".into(), "*.log".into()]).unwrap(), vec!["cache", "*.log"]);
        assert!(clean_ignore(&["../etc".into()]).is_err());
        assert!(clean_ignore(&["a/b".into()]).is_err());
        assert!(clean_ignore(&[("x".repeat(129))]).is_err());
        assert!(clean_ignore(&vec!["a".to_string(); MAX_IGNORE_PATTERNS + 1]).is_err());
    }

    #[test]
    fn archive_holds_everything_but_ignored_names_with_checksum_and_env() {
        let dir = scratch("pack");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(src.join("cache")).unwrap();
        std::fs::create_dir_all(src.join("public/img")).unwrap();
        std::fs::write(src.join("index.js"), "console.log(1)").unwrap();
        std::fs::write(src.join("node_modules/pkg/a.js"), "x").unwrap();
        std::fs::write(src.join("cache/big.bin"), vec![0u8; 4096]).unwrap();
        std::fs::write(src.join("public/img/logo.png"), "png").unwrap();
        std::fs::write(src.join("debug.log"), "log").unwrap();

        let dest = dir.join("out").join("backup-1.tar.gz");
        let stats = pack_backup_archive(&src, Some("A=1\nB=2\n"), &dest, &["cache".to_string(), "*.log".to_string()]).unwrap();

        // The checksum is of the file on disk.
        let on_disk = std::fs::read(&dest).unwrap();
        assert_eq!(stats.sha256, format!("{:x}", Sha256::digest(&on_disk)));
        assert_eq!(stats.compressed_bytes, on_disk.len() as u64);

        let out = dir.join("unpacked");
        unpack_tar_gz_safe(&dest, &out).unwrap();
        assert_eq!(std::fs::read_to_string(out.join("backup.env")).unwrap(), "A=1\nB=2\n");
        assert!(out.join("files/index.js").is_file());
        assert!(out.join("files/node_modules/pkg/a.js").is_file(), "node_modules is included by default");
        assert!(out.join("files/public/img/logo.png").is_file());
        assert!(!out.join("files/cache").exists(), "ignored folder");
        assert!(!out.join("files/debug.log").exists(), "ignored by pattern");
        // 3 files kept (index.js, a.js, logo.png) + env.
        assert_eq!(stats.files, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn part_files_sit_next_to_the_archive() {
        assert_eq!(part_path(Path::new("/x/backup-3.tar.gz")), PathBuf::from("/x/backup-3.tar.gz.part"));
    }

    #[tokio::test]
    async fn interrupted_backups_are_marked_failed_and_part_files_removed() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        let pid: i64 = sqlx::query_scalar(
            "INSERT INTO projects(name, slug, repo_url, deploy_dir, created_at, updated_at) VALUES('b', 'b-app', 'local', '/tmp/b', 0, 0) RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO backups(project_id, status, created_at) VALUES($1, 'running', 1), ($1, 'complete', 2)")
            .bind(pid)
            .execute(&db)
            .await
            .unwrap();
        let data = scratch("recover");
        let adir = archive_dir(&data, pid);
        std::fs::create_dir_all(&adir).unwrap();
        std::fs::write(adir.join("backup-9.tar.gz.part"), "x").unwrap();
        std::fs::write(adir.join("backup-8.tar.gz"), "keep").unwrap();

        assert_eq!(recover_interrupted(&db, &data).await, 1);
        let failed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backups WHERE status = 'failed' AND error <> ''").fetch_one(&db).await.unwrap();
        assert_eq!(failed, 1);
        assert!(!adir.join("backup-9.tar.gz.part").exists());
        assert!(adir.join("backup-8.tar.gz").exists(), "finished archives are kept");
        assert_eq!(count_held(&db, pid).await, 1, "failed backups don't count against the limit");
        let _ = std::fs::remove_dir_all(&data);
    }

    fn tree(dir: &Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    /// Packs `src` into an archive under `dir` and returns (archive, sha256, files).
    fn make_backup(dir: &Path, src: &Path, env: &str) -> (PathBuf, String, u64) {
        let archive = dir.join("b.tar.gz");
        let stats = pack_backup_archive(src, Some(env), &archive, &[]).unwrap();
        (archive, stats.sha256, stats.files)
    }

    #[test]
    fn restore_replaces_everything_and_keeps_an_undo_copy() {
        let dir = scratch("restore");
        let work = dir.join("app-folder");
        let root = work.join("app");
        tree(&root, &[("a.txt", "original"), ("sub/b.txt", "kept")]);
        let (archive, sha, files) = make_backup(&dir, &root, "K=1\n");

        // The app changes after the backup: a file is edited, one is added, one is deleted.
        tree(&root, &[("a.txt", "edited"), ("extra.txt", "new file"), ("sub/new.txt", "new")]);
        std::fs::remove_file(root.join("sub/b.txt")).unwrap();

        verify_archive(&archive, &sha).unwrap();
        let staged = stage_archive(&archive, &work, 7, files).unwrap();
        assert_eq!(staged.env.as_deref(), Some("K=1\n"));
        let undo = swap_in(&root, &staged.new_root, &work, "app").unwrap().expect("undo copy");

        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(root.join("sub/b.txt")).unwrap(), "kept");
        assert!(!root.join("extra.txt").exists(), "files not in the backup are removed");
        assert!(!root.join("sub/new.txt").exists());
        // The previous state is kept for undo.
        assert_eq!(std::fs::read_to_string(undo.join("extra.txt")).unwrap(), "new file");
        assert!(!work.join(STAGE_DIR).join("new-7").exists(), "staging is consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_backup_is_refused_before_anything_changes() {
        let dir = scratch("damaged");
        let work = dir.join("w");
        let root = work.join("app");
        tree(&root, &[("a.txt", "v1")]);
        let (archive, sha, files) = make_backup(&dir, &root, "");
        tree(&root, &[("a.txt", "v2")]);

        // Wrong checksum.
        let mut bytes = std::fs::read(&archive).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&archive, &bytes).unwrap();
        assert!(verify_archive(&archive, &sha).unwrap_err().contains("damaged"));
        assert!(verify_archive(&archive, "").is_err());

        // Even if the checksum were not checked, a broken archive can't be unpacked.
        assert!(stage_archive(&archive, &work, 1, files).is_err());
        assert!(!work.join(STAGE_DIR).join("new-1").exists());
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v2", "live files untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_count_mismatch_is_refused() {
        let dir = scratch("count");
        let work = dir.join("w");
        let root = work.join("app");
        tree(&root, &[("a.txt", "1"), ("b.txt", "2")]);
        let (archive, _sha, files) = make_backup(&dir, &root, "");
        let err = stage_archive(&archive, &work, 2, files + 5).err().expect("mismatch refused");
        assert!(err.contains("Nothing was changed"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_swap_puts_the_old_files_back() {
        let dir = scratch("rollback");
        let work = dir.join("w");
        let root = work.join("app");
        tree(&root, &[("a.txt", "live")]);
        // The staged folder doesn't exist, so the second rename fails.
        let err = swap_in(&root, &work.join("missing-new"), &work, "app").unwrap_err();
        assert!(err.contains("Couldn't put the restored files in place"), "{err}");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "live", "rolled back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_newest_undo_copy_is_kept_and_old_ones_expire() {
        let dir = scratch("undo");
        let work = dir.join("w");
        let root = work.join("app");
        for round in 0..3 {
            tree(&root, &[("f.txt", &format!("v{round}"))]);
            let new_root = work.join(format!("new-{round}"));
            tree(&new_root, &[("f.txt", "restored")]);
            swap_in(&root, &new_root, &work, "app").unwrap();
        }
        let copies = std::fs::read_dir(work.join(UNDO_DIR)).unwrap().count();
        assert_eq!(copies, 1);

        let old = work.join(UNDO_DIR).join("app-1000");
        std::fs::create_dir_all(&old).unwrap();
        assert_eq!(prune_undo_copies(&work, UNDO_KEEP_SECS), 1, "only the old one expires");
        assert!(!old.exists());
        assert_eq!(std::fs::read_dir(work.join(UNDO_DIR)).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_current_folder_is_swapped_through_the_link() {
        let dir = scratch("link");
        let work = dir.join("w");
        let release = work.join("releases/r1");
        tree(&release, &[("a.txt", "old")]);
        let link = work.join("current");
        std::os::unix::fs::symlink(&release, &link).unwrap();
        assert_eq!(resolve_real_root(&link), std::fs::canonicalize(&release).unwrap());
        let (archive, _sha, files) = {
            let src = dir.join("src");
            tree(&src, &[("a.txt", "from backup")]);
            make_backup(&dir, &src, "")
        };
        let staged = stage_archive(&archive, &work, 3, files).unwrap();
        swap_in(&resolve_real_root(&link), &staged.new_root, &work, "current").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link is untouched");
        assert_eq!(std::fs::read_to_string(link.join("a.txt")).unwrap(), "from backup");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_one_restore_runs_per_app() {
        let core = Path::new("core-a");
        assert!(begin_restore(core, 990_001, 1));
        assert!(!begin_restore(core, 990_001, 2), "second restore refused while one runs");
        assert!(is_restoring(core, 990_001));
        assert!(begin_restore(core, 990_002, 1), "other apps are independent");
        assert!(!is_restoring(Path::new("core-b"), 990_001), "another Core instance has its own state");
        finish_restore(core, 990_001, false, "boom".into());
        assert!(!is_restoring(core, 990_001));
        assert_eq!(restore_status(core, 990_001).unwrap().state, "failed");
        assert!(begin_restore(core, 990_001, 3), "a finished restore doesn't block the next");
        finish_restore(core, 990_001, true, String::new());
        finish_restore(core, 990_002, true, String::new());
    }

    #[tokio::test]
    async fn an_environment_only_backup_never_touches_the_files() {
        let dir = scratch("envonly");
        let work = dir.join("w");
        let root = work.join("app");
        tree(&root, &[("a.txt", "live")]);
        // An env-only backup is an archive with no files in it.
        let archive = dir.join("e.tar.gz");
        let stats = pack_backup_archive(&root, Some("K=1\n"), &archive, &["*".to_string()]).unwrap();
        assert_eq!(stats.files, 0);

        let src = RestoreSource::Archive { path: archive, sha256: stats.sha256, files: 0 };
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let outcome = restore_files(src, true, &work, &root, "app", 1, || async { stopped.store(true, std::sync::atomic::Ordering::SeqCst) })
            .await
            .map_err(|f| f.message)
            .unwrap();
        assert_eq!(outcome.env.as_deref(), Some("K=1\n"));
        assert!(!outcome.files_replaced);
        assert!(!stopped.load(std::sync::atomic::Ordering::SeqCst), "the app isn't stopped for an env-only restore");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "live", "files untouched");
        assert!(!work.join(UNDO_DIR).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_folder_backup_replaces_files_and_one_without_files_changes_nothing() {
        let dir = scratch("folder");
        let work = dir.join("w");
        let root = work.join("current");
        tree(&root, &[("a.txt", "live edit"), ("extra.txt", "added later")]);
        // The folder layout: <backup>/files/... and <backup>/backup.env
        let backup = work.join("backups/backup-1");
        tree(&backup, &[("files/a.txt", "from backup"), ("files/sub/b.txt", "kept"), ("backup.env", "OLD=1\n")]);

        let outcome = restore_files(RestoreSource::Dir { path: backup.clone() }, false, &work, &root, "current", 1, || async {})
            .await
            .map_err(|f| f.message)
            .unwrap();
        assert!(outcome.files_replaced);
        assert_eq!(outcome.env.as_deref(), Some("OLD=1\n"));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "from backup");
        assert_eq!(std::fs::read_to_string(root.join("sub/b.txt")).unwrap(), "kept");
        assert!(!root.join("extra.txt").exists(), "replaced, not merged");
        assert_eq!(std::fs::read_to_string(outcome.undo.unwrap().join("extra.txt")).unwrap(), "added later");

        // A folder backup with no files folder (an env-only one) must never wipe the app.
        let env_only = work.join("backups/backup-2");
        tree(&env_only, &[("backup.env", "ONLY=1\n")]);
        let outcome = restore_files(RestoreSource::Dir { path: env_only }, false, &work, &root, "current", 2, || async {})
            .await
            .map_err(|f| f.message)
            .unwrap();
        assert!(!outcome.files_replaced);
        assert_eq!(outcome.env.as_deref(), Some("ONLY=1\n"));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "from backup", "files untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_app_is_only_stopped_after_the_backup_is_proven_good() {
        let dir = scratch("stoporder");
        let work = dir.join("w");
        let root = work.join("app");
        tree(&root, &[("a.txt", "live")]);
        let (archive, sha, files) = make_backup(&dir, &root, "");
        let mut bytes = std::fs::read(&archive).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&archive, &bytes).unwrap();

        let stopped = std::sync::atomic::AtomicBool::new(false);
        let src = RestoreSource::Archive { path: archive, sha256: sha, files };
        let failure = restore_files(src, false, &work, &root, "app", 3, || async { stopped.store(true, std::sync::atomic::Ordering::SeqCst) })
            .await
            .err()
            .expect("a damaged backup fails");
        assert!(failure.message.contains("damaged"), "{}", failure.message);
        assert!(!failure.app_stopped);
        assert!(!stopped.load(std::sync::atomic::Ordering::SeqCst), "the app keeps running when the backup is bad");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "live");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_files_inside_the_archive_folder_can_be_removed() {
        let dir = scratch("remove");
        let inside = archive_dir(&dir, 9);
        std::fs::create_dir_all(&inside).unwrap();
        let archive = inside.join("backup-1.tar.gz");
        std::fs::write(&archive, "x").unwrap();
        let outside = dir.join("precious.txt");
        std::fs::write(&outside, "keep me").unwrap();

        assert!(!remove_archive_file(&dir, &outside), "a file outside the archive folder is refused");
        assert!(outside.exists());
        assert!(!remove_archive_file(&dir, &archives_root(&dir)), "the folder itself is refused");
        assert!(remove_archive_file(&dir, &archive));
        assert!(!archive.exists());

        std::fs::write(&archive, "x").unwrap();
        remove_project_archives(&dir, 9);
        assert!(!inside.exists(), "deleting an app removes all of its archives");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
