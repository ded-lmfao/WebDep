use rand::Rng;
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::info;

use crate::databases::binaries::{ensure_pg_permissions, make_pg_server_command, PgBinaries};
use crate::databases::security::{
    ensure_tcp_only_pg_conf, generate_pg_hba, is_process_alive, read_pg_pid, to_clean_path_str, wait_for_pg_ready,
};

pub const DEFAULT_CONTROL_PG_PORT: u16 = 5431;

fn build_migrator() -> sqlx::migrate::Migrator {
    use sqlx::migrate::{Migration, MigrationType, Migrator};
    use std::borrow::Cow;

    Migrator {
        migrations: Cow::Owned(vec![
            Migration::new(
                1,
                Cow::Borrowed("initial"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0001_initial.sql")),
                false,
            ),
            Migration::new(
                2,
                Cow::Borrowed("access_grants"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0002_access_grants.sql")),
                false,
            ),
            Migration::new(
                3,
                Cow::Borrowed("cluster_nodes"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0003_cluster_nodes.sql")),
                false,
            ),
            Migration::new(
                4,
                Cow::Borrowed("migrations_and_backups"),
                MigrationType::Simple,
                Cow::Borrowed(include_str!("../migrations/0004_migrations_and_backups.sql")),
                false,
            ),
        ]),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlPgConfig {
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

#[derive(Debug, Clone)]
pub struct ControlPgHandle {
    pub pg_ctl: PathBuf,
    pub pgdata_dir: PathBuf,
    pub port: u16,
}

impl ControlPgHandle {
    /// Gracefully stop the local control-plane PostgreSQL instance.
    pub async fn stop(&self) {
        let data_arg = to_clean_path_str(&self.pgdata_dir);
        let mut cmd = make_pg_server_command(&self.pg_ctl, &["-D", &data_arg, "-m", "fast", "stop"]);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let _ = tokio::time::timeout(Duration::from_secs(5), cmd.status()).await;
    }
}

fn generate_secret(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

fn pick_control_pg_port() -> u16 {
    if std::net::TcpListener::bind(("127.0.0.1", DEFAULT_CONTROL_PG_PORT)).is_ok() {
        return DEFAULT_CONTROL_PG_PORT;
    }
    for port in (5400..DEFAULT_CONTROL_PG_PORT).rev() {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) {
        if let Ok(addr) = listener.local_addr() {
            return addr.port();
        }
    }
    DEFAULT_CONTROL_PG_PORT
}

fn make_pg_connect_options(host: &str, port: u16, user: &str, password: &str, database: &str) -> PgConnectOptions {
    PgConnectOptions::new()
        .host(host)
        .port(port)
        .username(user)
        .password(password)
        .database(database)
        .disable_statement_logging()
}

/// Ensure migrations run on any connected PostgreSQL pool.
pub async fn run_migrations(pool: &PgPool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    build_migrator().run(pool).await?;
    Ok(())
}

/// Zero-config control-plane PostgreSQL initialization:
/// - If `DATABASE_URL` env var is set and non-empty, connects directly to it.
/// - Otherwise auto-initializes, configures, starts, and connects to a dedicated
///   local PostgreSQL instance under `<data_dir>/control-pg/` on port 5431.
pub async fn ensure_control_pg_pool(
    data_dir: &Path,
) -> Result<(PgPool, Option<ControlPgHandle>), Box<dyn std::error::Error + Send + Sync>> {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            let pool = PgPoolOptions::new()
                .max_connections(20)
                .acquire_timeout(Duration::from_secs(10))
                .connect(trimmed)
                .await?;
            run_migrations(&pool).await?;
            info!("Connected to configured external PostgreSQL DATABASE_URL");
            return Ok((pool, None));
        }
    }

    ensure_local_pg_with_port(data_dir, None).await
}

pub async fn ensure_local_pg_with_port(
    data_dir: &Path,
    forced_port: Option<u16>,
) -> Result<(PgPool, Option<ControlPgHandle>), Box<dyn std::error::Error + Send + Sync>> {
    let bins = PgBinaries::detect().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "{}. WebDep now stores its data in PostgreSQL. Install it by re-running the installer \
                 (curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- --dir <your WebDep folder>), \
                 or set DATABASE_URL to an existing PostgreSQL server.",
                e
            ),
        )
    })?;

    let control_dir = data_dir.join("control-pg");
    let pgdata_dir = control_dir.join("pgdata");
    let conn_json_path = control_dir.join("conn.json");
    let log_file = control_dir.join("postgresql.log");
    tokio::fs::create_dir_all(&control_dir).await?;

    let mut cfg = if conn_json_path.is_file() {
        let raw = tokio::fs::read_to_string(&conn_json_path).await?;
        serde_json::from_str::<ControlPgConfig>(&raw)?
    } else {
        let port = forced_port.unwrap_or_else(pick_control_pg_port);
        let new_cfg = ControlPgConfig {
            port,
            user: "webdep_core".to_string(),
            password: generate_secret(32),
            database: "webdep_core".to_string(),
        };
        let serialized = serde_json::to_string_pretty(&new_cfg)?;
        tokio::fs::write(&conn_json_path, serialized).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&conn_json_path, std::fs::Permissions::from_mode(0o600)).await;
        }
        new_cfg
    };

    if let Some(p) = forced_port {
        cfg.port = p;
    }

    let pgdata_initialized = pgdata_dir.join("PG_VERSION").is_file();
    if !pgdata_initialized {
        tokio::fs::create_dir_all(&pgdata_dir).await?;
        let pwfile_path = control_dir.join(".init_pw");
        tokio::fs::write(&pwfile_path, format!("{}\n", cfg.password)).await?;
        ensure_pg_permissions(&control_dir, Some(&pgdata_dir));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Owner-only: ensure_pg_permissions above already chowned it to the user initdb runs as.
            let _ = tokio::fs::set_permissions(&pwfile_path, std::fs::Permissions::from_mode(0o600)).await;
        }

        let data_arg = to_clean_path_str(&pgdata_dir);
        let pw_arg = to_clean_path_str(&pwfile_path);
        let out = make_pg_server_command(
            &bins.initdb,
            &[
                "-D",
                &data_arg,
                "-U",
                &cfg.user,
                "--encoding=UTF8",
                "--auth=scram-sha-256",
                &format!("--pwfile={}", pw_arg),
            ],
        )
        .output()
        .await;
        let _ = tokio::fs::remove_file(&pwfile_path).await;

        let out = out?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            return Err(format!("control-pg initdb failed: {} {}", stdout, stderr).into());
        }

        let pg_conf = format!(
            "\n# WebDep Control-Plane Managed Settings\nlisten_addresses = '127.0.0.1'\nport = {}\nmax_connections = 64\nshared_buffers = 32MB\n",
            cfg.port
        );
        let conf_path = pgdata_dir.join("postgresql.conf");
        let existing = tokio::fs::read_to_string(&conf_path).await.unwrap_or_default();
        tokio::fs::write(&conf_path, format!("{}{}", existing, pg_conf)).await?;

        let hba = generate_pg_hba("local", "");
        tokio::fs::write(pgdata_dir.join("pg_hba.conf"), hba).await?;
        ensure_pg_permissions(&control_dir, Some(&pgdata_dir));
        info!("Initialized zero-config control PostgreSQL cluster at {} (port {})", pgdata_dir.display(), cfg.port);
    }

    let already_running = read_pg_pid(&pgdata_dir)
        .map(is_process_alive)
        .unwrap_or(false)
        && tokio::net::TcpStream::connect(("127.0.0.1", cfg.port)).await.is_ok();

    if !already_running {
        let pid_file = pgdata_dir.join("postmaster.pid");
        if pid_file.exists() {
            let stale = read_pg_pid(&pgdata_dir).map(|p| !is_process_alive(p)).unwrap_or(true);
            if stale {
                let _ = tokio::fs::remove_file(&pid_file).await;
            }
        }
        ensure_tcp_only_pg_conf(&pgdata_dir).await;
        ensure_pg_permissions(&control_dir, Some(&pgdata_dir));
        let data_arg = to_clean_path_str(&pgdata_dir);
        let log_arg = to_clean_path_str(&log_file);
        let mut start_cmd = make_pg_server_command(
            &bins.pg_ctl,
            &["-D", &data_arg, "-l", &log_arg, "-W", "start"],
        );
        start_cmd.stdin(std::process::Stdio::null());
        start_cmd.stdout(std::process::Stdio::null());
        start_cmd.stderr(std::process::Stdio::null());
        let _ = tokio::time::timeout(Duration::from_secs(5), start_cmd.status()).await;

        if wait_for_pg_ready(cfg.port, &pgdata_dir, 15_000).await.is_none() {
            let log_tail = tokio::fs::read_to_string(&log_file).await.unwrap_or_default();
            let pid = read_pg_pid(&pgdata_dir);
            let alive = pid.map(is_process_alive).unwrap_or(false);
            let tcp_ok = tokio::net::TcpStream::connect(("127.0.0.1", cfg.port))
                .await
                .is_ok();
            return Err(format!(
                "control-pg did not become ready on 127.0.0.1:{} (pid={:?}, alive={}, tcp={}) | {}",
                cfg.port, pid, alive, tcp_ok, log_tail
            )
            .into());
        }
    }

    // Ensure the application database exists
    {
        let admin_opts = make_pg_connect_options("127.0.0.1", cfg.port, &cfg.user, &cfg.password, "postgres");
        let admin_pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(admin_opts)
            .await?;
        let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
            .bind(&cfg.database)
            .fetch_optional(&admin_pool)
            .await?;
        if exists.is_none() {
            let safe_db = cfg.database.replace('"', "");
            sqlx::query(&format!("CREATE DATABASE \"{}\"", safe_db))
                .execute(&admin_pool)
                .await?;
        }
        admin_pool.close().await;
    }

    let app_opts = make_pg_connect_options("127.0.0.1", cfg.port, &cfg.user, &cfg.password, &cfg.database);
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(app_opts)
        .await?;

    run_migrations(&pool).await?;

    let handle = ControlPgHandle {
        pg_ctl: bins.pg_ctl,
        pgdata_dir,
        port: cfg.port,
    };

    Ok((pool, Some(handle)))
}

#[cfg(test)]
pub mod test_db {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::OnceCell;

    struct SharedTestCluster {
        port: u16,
        user: String,
        password: String,
    }

    static TEST_CLUSTER: OnceCell<SharedTestCluster> = OnceCell::const_new();
    static TEST_DB_SEQ: AtomicU64 = AtomicU64::new(1);

    async fn get_cluster() -> &'static SharedTestCluster {
        TEST_CLUSTER
            .get_or_init(|| async {
                if let Ok(url) = std::env::var("WEBDEP_TEST_DATABASE_URL") {
                    if let Ok(opts) = url.parse::<PgConnectOptions>() {
                        let port = opts.get_port();
                        let user = opts.get_username().to_string();
                        return SharedTestCluster {
                            port,
                            user,
                            password: String::new(),
                        };
                    }
                }
                let root = std::env::temp_dir().join("webdep-shared-test-pg");
                let port = 55431;
                let _ = ensure_local_pg_with_port(&root, Some(port))
                    .await
                    .expect("Shared PostgreSQL test cluster failed to start");
                let raw = std::fs::read_to_string(root.join("control-pg").join("conn.json")).unwrap();
                let cfg: ControlPgConfig = serde_json::from_str(&raw).unwrap();
                SharedTestCluster {
                    port: cfg.port,
                    user: cfg.user,
                    password: cfg.password,
                }
            })
            .await
    }

    /// Creates a fast isolated PostgreSQL database (`wt_<pid>_<seq>`) and runs all migrations.
    pub async fn isolated_pool() -> PgPool {
        if let Ok(url) = std::env::var("WEBDEP_TEST_DATABASE_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                let pool = PgPoolOptions::new()
                    .max_connections(5)
                    .connect(trimmed)
                    .await
                    .expect("WEBDEP_TEST_DATABASE_URL connect failed");
                run_migrations(&pool).await.expect("migrations failed");
                return pool;
            }
        }

        let cluster = get_cluster().await;
        let seq = TEST_DB_SEQ.fetch_add(1, Ordering::SeqCst);
        let db_name = format!("wt_{}_{}", std::process::id(), seq);

        let admin_opts = make_pg_connect_options("127.0.0.1", cluster.port, &cluster.user, &cluster.password, "postgres");
        let admin_pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(admin_opts)
            .await
            .expect("connect to test cluster postgres db failed");

        sqlx::query(&format!("CREATE DATABASE \"{}\"", db_name))
            .execute(&admin_pool)
            .await
            .expect("CREATE DATABASE failed");
        admin_pool.close().await;

        let db_opts = make_pg_connect_options("127.0.0.1", cluster.port, &cluster.user, &cluster.password, &db_name);
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(db_opts)
            .await
            .expect("connect to isolated test db failed");

        run_migrations(&pool).await.expect("migrations failed");
        pool
    }
}
