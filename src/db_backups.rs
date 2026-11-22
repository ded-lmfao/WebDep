//! Manual backups of managed databases.
//!
//! A backup is one logical dump (`pg_dump -F c`) taken while the database keeps running, so it
//! is consistent without any downtime. A restore replaces the database's contents completely:
//! every schema is dropped and the dump is loaded, all inside ONE transaction. If anything fails
//! the transaction is rolled back and the database is exactly as it was. Nothing is ever
//! half-restored.

use crate::databases::PgBinaries;
use sqlx::Row;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// How a dump or restore connects to a managed database.
pub struct DbConn {
    /// `127.0.0.1` for a database on this server, the node's mesh address otherwise.
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

const DUMP_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

pub fn backups_dir(data_dir: &Path, database_id: i64) -> PathBuf {
    data_dir.join("backups").join("databases").join(format!("db-{database_id}"))
}

pub fn dump_path(data_dir: &Path, database_id: i64, backup_id: i64) -> PathBuf {
    backups_dir(data_dir, database_id).join(format!("backup-{backup_id}.dump"))
}

/// Removes every backup file of a database (when the database is deleted).
pub fn remove_database_backups(data_dir: &Path, database_id: i64) {
    let _ = std::fs::remove_dir_all(backups_dir(data_dir, database_id));
}

fn tool(bins: &PgBinaries, name: &str) -> PathBuf {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    bins.psql.with_file_name(format!("{name}{ext}"))
}

/// Last part of a tool's error output, which is where the reason is.
fn tail(text: &[u8]) -> String {
    let s = String::from_utf8_lossy(text);
    let t = s.trim();
    let start = t.len().saturating_sub(500);
    let start = (start..=t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
    t[start..].to_string()
}

fn base_command(program: &Path, conn: &DbConn) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(["-h", &conn.host, "-p", &conn.port.to_string(), "-U", &conn.user])
        .env("PGPASSWORD", &conn.password)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

pub struct DumpInfo {
    pub size: u64,
    pub sha256: String,
}

/// Writes a dump of the database to `dest` (through a `.part` file, renamed when complete).
pub async fn create_dump(bins: &PgBinaries, conn: &DbConn, dest: &Path) -> Result<DumpInfo, String> {
    let pg_dump = tool(bins, "pg_dump");
    if !pg_dump.is_file() {
        return Err("pg_dump was not found next to PostgreSQL on this server.".to_string());
    }
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| format!("Couldn't create the backup folder: {e}"))?;
    }
    let part = crate::backups::part_path(dest);
    let _ = tokio::fs::remove_file(&part).await;

    let mut cmd = base_command(&pg_dump, conn);
    cmd.args(["-d", &conn.database, "-F", "c", "-Z", "6", "--no-owner", "--no-privileges", "-f"])
        .arg(&part)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let out = match tokio::time::timeout(DUMP_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(format!("Couldn't run pg_dump: {e}"));
        }
        Err(_) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err("The backup took too long and was stopped.".to_string());
        }
    };
    if !out.status.success() {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(format!("pg_dump failed: {}", tail(&out.stderr)));
    }
    tokio::fs::rename(&part, dest)
        .await
        .map_err(|e| format!("Couldn't finish the backup file: {e}"))?;
    let path = dest.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let size = std::fs::metadata(&path).map(|m| m.len()).map_err(|e| e.to_string())?;
        let sha256 = sha256_of_file(&path)?;
        Ok(DumpInfo { size, sha256 })
    })
    .await
    .map_err(|e| format!("Checksum task failed: {e}"))?
}

fn sha256_of_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("Couldn't read the backup file: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("Couldn't read the backup file: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Drops every schema of the database, then recreates an empty `public`.
const WIPE_SQL: &str = "DO $wipe$ DECLARE s text; BEGIN \
FOR s IN SELECT nspname FROM pg_namespace WHERE nspname NOT IN ('pg_catalog','information_schema','pg_toast') \
AND nspname NOT LIKE 'pg\\_temp\\_%' AND nspname NOT LIKE 'pg\\_toast\\_temp\\_%' LOOP \
EXECUTE format('DROP SCHEMA %I CASCADE', s); END LOOP; \
CREATE SCHEMA public; END $wipe$;\n";

/// Replaces the database's contents with the dump, all or nothing.
pub async fn restore_dump(bins: &PgBinaries, conn: &DbConn, file: &Path, expected_sha256: &str) -> Result<(), String> {
    let psql = bins.psql.clone();
    let pg_restore = tool(bins, "pg_restore");
    if !pg_restore.is_file() {
        return Err("pg_restore was not found next to PostgreSQL on this server.".to_string());
    }
    // 1. The file must be the one that was backed up.
    let path = file.to_path_buf();
    let actual = tokio::task::spawn_blocking(move || sha256_of_file(&path))
        .await
        .map_err(|e| format!("Checksum task failed: {e}"))??;
    if !expected_sha256.is_empty() && actual != expected_sha256 {
        return Err("The backup file is damaged (its checksum no longer matches), so nothing was restored.".to_string());
    }

    // 2. Disconnect other sessions so the old tables can be dropped (best effort).
    let mut kick = base_command(&psql, conn);
    kick.args([
        "-d",
        &conn.database,
        "-X",
        "-q",
        "-c",
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid()",
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let _ = tokio::time::timeout(Duration::from_secs(30), kick.status()).await;

    // 3. One transaction: wipe, then replay the dump. Committed only if the whole dump replayed.
    let mut sql = base_command(&psql, conn);
    sql.args(["-d", &conn.database, "-X", "-q", "-v", "ON_ERROR_STOP=1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut sql_child = sql.spawn().map_err(|e| format!("Couldn't run psql: {e}"))?;
    let mut sql_in = sql_child.stdin.take().ok_or("psql input unavailable")?;

    let mut rest = base_command(&pg_restore, conn);
    rest.args(["--no-owner", "--no-privileges", "-f", "-"])
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut rest_child = rest.spawn().map_err(|e| format!("Couldn't run pg_restore: {e}"))?;
    let mut rest_out = rest_child.stdout.take().ok_or("pg_restore output unavailable")?;

    let feed = async {
        sql_in.write_all(b"BEGIN;\n").await?;
        sql_in.write_all(WIPE_SQL.as_bytes()).await?;
        tokio::io::copy(&mut rest_out, &mut sql_in).await?;
        Ok::<_, std::io::Error>(sql_in)
    };
    let fed = feed.await;
    let rest_result = rest_child.wait_with_output().await;
    let restore_ok = matches!(&rest_result, Ok(o) if o.status.success());

    match (fed, restore_ok) {
        (Ok(mut stdin), true) => {
            // Only now is it safe to commit: pg_restore finished and every statement ran.
            let _ = stdin.write_all(b"COMMIT;\n").await;
            drop(stdin);
        }
        (fed, _) => {
            // Closing psql's input without COMMIT rolls the whole transaction back.
            drop(fed);
            let psql_err = match sql_child.wait_with_output().await {
                Ok(o) => tail(&o.stderr),
                Err(_) => String::new(),
            };
            let reason = match rest_result {
                Ok(o) if !o.status.success() => format!("pg_restore failed: {}", tail(&o.stderr)),
                Err(e) => format!("pg_restore couldn't run: {e}"),
                _ if !psql_err.is_empty() => psql_err,
                _ => "the restore was interrupted".to_string(),
            };
            return Err(format!("Nothing was changed: {reason}"));
        }
    }
    let out = sql_child.wait_with_output().await.map_err(|e| format!("psql failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("Nothing was changed: {}", tail(&out.stderr)))
    }
}

/// Backups that hold space against the limit (failed ones don't).
pub async fn count_held(db: &crate::db::DbPool, database_id: i64) -> i64 {
    sqlx::query("SELECT COUNT(*) FROM database_backups WHERE database_id = $1 AND status <> 'failed'")
        .bind(database_id)
        .fetch_one(db)
        .await
        .map(|r| r.get::<i64, _>(0))
        .unwrap_or(0)
}

/// Marks backups that were running when the panel stopped as failed, and removes their half files.
pub async fn recover_interrupted(db: &crate::db::DbPool, data_dir: &Path) -> u64 {
    let rows = sqlx::query("SELECT id, database_id FROM database_backups WHERE status = 'running'")
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let mut n = 0;
    for r in rows {
        let (id, dbid): (i64, i64) = (r.get(0), r.get(1));
        let _ = tokio::fs::remove_file(crate::backups::part_path(&dump_path(data_dir, dbid, id))).await;
        let _ = sqlx::query(
            "UPDATE database_backups SET status = 'failed', error = 'The panel restarted while this backup was running.' WHERE id = $1",
        )
        .bind(id)
        .execute(db)
        .await;
        n += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// One restore at a time per database
// ---------------------------------------------------------------------------

#[derive(Clone, serde::Serialize)]
pub struct RestoreStatus {
    pub state: &'static str,
    #[serde(rename = "backupId")]
    pub backup_id: i64,
    pub message: String,
}

static RESTORES: std::sync::LazyLock<Mutex<HashMap<(PathBuf, i64), RestoreStatus>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn restores() -> std::sync::MutexGuard<'static, HashMap<(PathBuf, i64), RestoreStatus>> {
    RESTORES.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn begin_restore(data_dir: &Path, database_id: i64, backup_id: i64) -> bool {
    let mut map = restores();
    let key = (data_dir.to_path_buf(), database_id);
    if map.get(&key).is_some_and(|s| s.state == "running") {
        return false;
    }
    map.insert(key, RestoreStatus { state: "running", backup_id, message: String::new() });
    true
}

pub fn finish_restore(data_dir: &Path, database_id: i64, ok: bool, message: String) {
    if let Some(s) = restores().get_mut(&(data_dir.to_path_buf(), database_id)) {
        s.state = if ok { "done" } else { "failed" };
        s.message = message;
    }
}

pub fn restore_status(data_dir: &Path, database_id: i64) -> Option<RestoreStatus> {
    restores().get(&(data_dir.to_path_buf(), database_id)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_per_database_and_backup() {
        let root = Path::new("data");
        assert_eq!(dump_path(root, 4, 9), root.join("backups").join("databases").join("db-4").join("backup-9.dump"));
    }

    #[test]
    fn tail_keeps_the_end_of_long_output() {
        let long = "x".repeat(2000) + "the reason";
        let t = tail(long.as_bytes());
        assert!(t.ends_with("the reason") && t.len() <= 500);
        assert_eq!(tail("  short  ".as_bytes()), "short");
    }

    #[test]
    fn one_restore_per_database_at_a_time() {
        let core = Path::new("db-core-a");
        assert!(begin_restore(core, 880_001, 1));
        assert!(!begin_restore(core, 880_001, 2));
        finish_restore(core, 880_001, false, "boom".into());
        assert_eq!(restore_status(core, 880_001).unwrap().state, "failed");
        assert!(begin_restore(core, 880_001, 3), "a finished restore does not block the next");
        finish_restore(core, 880_001, true, String::new());
    }

    /// Real dump and restore against the shared test PostgreSQL cluster.
    #[tokio::test]
    async fn dump_then_restore_replaces_everything_and_failures_change_nothing() {
        let pool = crate::pg_control::test_db::isolated_pool().await;
        let opts = pool.connect_options();
        let password = std::env::var("WEBDEP_TEST_DATABASE_URL").map_or_else(
            |_| {
                let raw = std::fs::read_to_string(std::env::temp_dir().join("webdep-shared-test-pg").join("control-pg").join("conn.json")).unwrap();
                serde_json::from_str::<serde_json::Value>(&raw).unwrap()["password"].as_str().unwrap_or_default().to_string()
            },
            |_| String::new(),
        );
        let conn = DbConn {
            host: "127.0.0.1".to_string(),
            port: opts.get_port(),
            user: opts.get_username().to_string(),
            password,
            database: opts.get_database().unwrap().to_string(),
        };
        let bins = PgBinaries::detect().expect("PostgreSQL binaries");
        let dir = std::env::temp_dir().join(format!("webdep-dbbackup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        sqlx::query("CREATE TABLE keepme(id INT PRIMARY KEY, v TEXT)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO keepme VALUES (1,'one'),(2,'two')").execute(&pool).await.unwrap();

        let dest = dump_path(&dir, 1, 1);
        let info = create_dump(&bins, &conn, &dest).await.expect("dump");
        assert!(info.size > 0 && info.sha256.len() == 64);
        assert!(dest.is_file() && !crate::backups::part_path(&dest).exists(), "no half file is left");

        // Change the database after the backup: edit, add a table, add a schema.
        sqlx::query("UPDATE keepme SET v = 'changed'").execute(&pool).await.unwrap();
        sqlx::query("DROP TABLE keepme").execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE newer(x INT)").execute(&pool).await.unwrap();
        sqlx::query("CREATE SCHEMA extra").execute(&pool).await.unwrap();

        // A damaged file is refused before anything is touched.
        let damaged = dir.join("damaged.dump");
        std::fs::write(&damaged, b"not a dump").unwrap();
        let err = restore_dump(&bins, &conn, &damaged, &info.sha256).await.unwrap_err();
        assert!(err.contains("damaged"), "{err}");
        // A file that passes the checksum but is not a real dump fails and rolls everything back.
        let bogus_sha = sha256_of_file(&damaged).unwrap();
        assert!(restore_dump(&bins, &conn, &damaged, &bogus_sha).await.is_err());
        let still: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM newer").fetch_one(&pool).await.unwrap();
        assert_eq!(still, 0, "a failed restore leaves the database as it was");

        // The real restore brings back exactly the backed-up state.
        restore_dump(&bins, &conn, &dest, &info.sha256).await.expect("restore");
        let rows: Vec<(i32, String)> = sqlx::query_as("SELECT id, v FROM keepme ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(rows, vec![(1, "one".to_string()), (2, "two".to_string())]);
        let leftovers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'newer' OR table_schema = 'extra'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(leftovers, 0, "things added after the backup are gone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
