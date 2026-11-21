//! Backup archives for websites and services.
//!
//! A backup is one `.tar.gz` under `<data dir>/backups/apps/project-<id>/`, outside the app's
//! own folder so cleaning or deleting the app can't remove it. The archive holds the app's
//! files under `files/` and its environment variables as `backup.env`. It is written to a
//! `.part` file first and renamed only when complete, and its SHA-256 and file count are saved
//! with the backup row, so an unfinished or damaged archive is never treated as a good backup.

use std::path::{Path, PathBuf};

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
}
