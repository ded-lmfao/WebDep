mod api;
mod auth;
mod cache;
mod caddy;
mod cluster;
mod config;
mod databases;
mod db;
mod deployer;
mod git;
mod logs;
mod monitor;
mod monitor_net;
mod pg_control;
mod projects;
mod runtime;
mod sqlite_import;
mod static_assets;
mod updater;
mod vault;

use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::api::{create_router, AppState};
use crate::auth::Auth;
use crate::cache::AppCache;
use crate::caddy::Caddy;
use crate::cluster::{
    detect_hostname, enroll_with_core, run_node_agent, ClusterManager, JoinOptions,
};
use crate::config::Config;
use crate::db::open_db;
use crate::deployer::Deployer;
use crate::logs::Logs;
use crate::monitor::Monitor;
use crate::projects::Projects;
use crate::runtime::Runtime;
use crate::updater::Updater;
use crate::vault::Vault;

#[derive(Default)]
struct CliOptions {
    core_only: bool,
    node_mode: bool,
    pair_local: bool,
    enroll_only: bool,
    join_url: Option<String>,
    token: Option<String>,
    name: Option<String>,
    region: Option<String>,
    public_ip: Option<String>,
    wg_port: u16,
}

fn parse_cli_options() -> Result<CliOptions, String> {
    let mut opts = CliOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--core" => opts.core_only = true,
            "--node" => opts.node_mode = true,
            "--pair-local" => opts.pair_local = true,
            "--enroll-only" => opts.enroll_only = true,
            "--join" => {
                opts.join_url = Some(args.next().ok_or("--join requires a Core URL")?);
            }
            "--token" => {
                opts.token = Some(args.next().ok_or("--token requires an enrollment token")?);
            }
            "--name" => {
                opts.name = Some(args.next().ok_or("--name requires a node name")?);
            }
            "--region" => {
                opts.region = Some(args.next().ok_or("--region requires a region")?);
            }
            "--public-ip" => {
                opts.public_ip = Some(args.next().ok_or("--public-ip requires an IP/host")?);
            }
            "--wg-port" => {
                let raw = args.next().ok_or("--wg-port requires a UDP port")?;
                opts.wg_port = raw.parse::<u16>().map_err(|_| format!("Invalid --wg-port '{raw}'"))?;
            }
            _ => {}
        }
    }
    if opts.core_only && opts.node_mode {
        return Err("Cannot specify both --core and --node in the same process".to_string());
    }
    Ok(opts)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if std::env::args()
        .nth(1)
        .map(|arg| arg == "--version" || arg == "-V")
        .unwrap_or(false)
    {
        println!(
            "webdep {} (tag={} commit={})",
            env!("CARGO_PKG_VERSION"),
            env!("WEBDEP_RELEASE_TAG"),
            env!("WEBDEP_COMMIT_SHA")
        );
        return Ok(());
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        extern "C" {
            fn mallopt(param: libc::c_int, value: libc::c_int) -> libc::c_int;
        }
        mallopt(-8, 2); // M_ARENA_MAX = 2 (eliminates glibc multi-arena memory bloat)
        mallopt(-1, 64 * 1024); // M_TRIM_THRESHOLD = 64KB (returns memory to OS immediately)
    }
    // Fast, zero-overhead tracing subscriber
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "webdep=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env();
    let cli = parse_cli_options().map_err(|e| format!("CLI error: {e}"))?;

    if std::env::args().nth(1).as_deref() == Some("--import-sqlite") {
        let sqlite_path = std::env::args()
            .nth(2)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config.data_dir.join("webdep.sqlite"));
        let overwrite = std::env::args().any(|a| a == "--overwrite");
        let (db, handle) = open_db(&config.data_dir).await?;
        let report =
            crate::sqlite_import::import_sqlite_into_postgres(&db, &sqlite_path, overwrite).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        if let Some(h) = handle {
            h.stop().await;
        }
        return Ok(());
    }

    // Pure Web-Node worker mode: zero PostgreSQL dependency, stores state in data/node-state.json
    if cli.node_mode {
        info!("Starting WebDep in Web-Node worker mode");
        info!("Data directory: {}", config.data_dir.display());
        if let Some(join_url) = cli.join_url.clone() {
            let token = cli
                .token
                .clone()
                .ok_or("--node --join requires --token <ENROLL_TOKEN>")?;
            let manifest = enroll_with_core(
                &config.data_dir,
                JoinOptions {
                    core_url: join_url,
                    token,
                    name: cli.name.clone(),
                    region: cli.region.clone(),
                    public_ip: cli.public_ip.clone(),
                    wg_port: cli.wg_port,
                },
            )
            .await?;
            println!(
                "Enrolled Web-Node '{}' (#{}, region '{}', mesh {}) into Core ({})",
                manifest.name, manifest.node_id, manifest.region, manifest.mesh_ip, manifest.core_url
            );
            if cli.enroll_only {
                return Ok(());
            }
        }
        run_node_agent(config.data_dir.clone()).await;
        return Ok(());
    }

    info!("Starting WebDep — Multi-Website Deployment & Hosting Platform");
    info!("Data directory: {}", config.data_dir.display());

    let (db, control_pg_handle) = open_db(&config.data_dir).await?;
    if let Some(ref h) = control_pg_handle {
        tracing::info!("Local control-plane PostgreSQL active on 127.0.0.1:{}", h.port);
    }
    let vault = Arc::new(Vault::new(&config.data_dir)?);
    let mut auth_mgr = Auth::with_timeouts(
        db.clone(),
        config.session_ttl_s,
        config.session_idle_timeout_s,
    );
    auth_mgr.force_secure_cookies = config.force_secure_cookies;
    let auth = Arc::new(auth_mgr);
    auth.ensure_admin().await;
    auth.start_revocation_listener();
    let projects = Arc::new(Projects::new(db.clone(), vault.clone()));
    let logs = Arc::new(Logs::new(db.clone()));
    let _ = crate::git::Git::read_github_tokens(&db).await;
    tracing::info!("WebDep engine initialized (PostgreSQL control DB connected and live log stream active)");
    let monitor = Arc::new(Monitor::new());
    monitor.start_background_sampler();
    let cache = Arc::new(AppCache::new());
    let caddy = Arc::new(Caddy::new(&config.data_dir));
    let runtime = Arc::new(Runtime::new(db.clone(), (*logs).clone()));
    runtime.start_background_monitor();
    let deployer = Arc::new(
        Deployer::new(
            db.clone(),
            (*logs).clone(),
            projects.clone(),
            runtime.clone(),
            cache.clone(),
        )
        .await,
    );
    let databases = Arc::new(crate::databases::Databases::new(
        db.clone(),
        vault.clone(),
        (*logs).clone(),
        &config.data_dir,
    ));
    let updater = Arc::new(Updater::with_db(db.clone()));
    let cluster = Arc::new(ClusterManager::new(db.clone(), config.data_dir.clone()));

    let local_core_http = format!("http://127.0.0.1:{}", config.port);
    let local_core_ws = format!("ws://127.0.0.1:{}/api/cluster/node-ws", config.port);
    let local_hostname = cli.name.clone().unwrap_or_else(detect_hostname);

    if cli.pair_local {
        let manifest = cluster
            .pair_local_node(&local_core_http, &local_core_ws, &local_hostname)
            .await?;
        println!(
            "Paired local control node '{}' (#{}, mesh {}) -> {}/node-state.json",
            manifest.name,
            manifest.node_id,
            manifest.mesh_ip,
            config.data_dir.display()
        );
        if let Some(h) = control_pg_handle {
            h.stop().await;
        }
        return Ok(());
    }

    cluster.start_staleness_reaper();
    if !cli.core_only {
        match cluster
            .pair_local_node(&local_core_http, &local_core_ws, &local_hostname)
            .await
        {
            Ok(_) => {
                let node_data_dir = config.data_dir.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    run_node_agent(node_data_dir).await;
                });
            }
            Err(e) => {
                tracing::warn!("Could not auto-pair local Web-Node: {}", e);
            }
        }
    }

    // Periodic memory maintenance: prunes expired cache, purges expired sessions, trims old logs, and trims Linux heap
    {
        let mem_cache = cache.clone();
        let mem_db = db.clone();
        let mem_auth = auth.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                mem_cache.prune_expired();
                mem_auth.purge_expired_sessions().await;
                mem_auth.prune_in_memory();
                crate::db::prune_old_logs(&mem_db, 20_000).await;
                trim_process_heap();
            }
        });
    }

    // Auto-resume any websites marked 'running' in PostgreSQL on platform startup,
    // and ensure any websites marked 'stopped' have no orphan processes left on their ports
    {
        let resume_projects = projects.clone();
        let resume_runtime = runtime.clone();
        let resume_logs = logs.clone();
        let platform_port = config.port;
        tokio::spawn(async move {
            resume_runtime.allow_firewall_port(80, None);
            resume_runtime.allow_firewall_port(443, None);
            resume_runtime.allow_firewall_port(platform_port, None);
            if let Ok(list) = resume_projects.list().await {
                for p in list {
                    let _ = resume_runtime.ensure_container(&p).await;
                    if p.app_status == "running" {
                        // Check if the workload is ALREADY running from before the panel restart
                        let is_already_running = if let Some(pid) = p.app_pid {
                            pid > 0 && crate::runtime::is_pid_alive(pid)
                        } else if let Some(port) = p.port {
                            !crate::runtime::Runtime::find_pids_on_port(port).is_empty()
                        } else {
                            false
                        };

                        if is_already_running {
                            tracing::info!(
                                "Workload '{}' (PID: {:?}) is already running; preserving zero downtime",
                                p.name,
                                p.app_pid
                            );
                            continue;
                        }

                        let env = resume_projects.get_env_map(p.id).await;
                        if let Err(e) = resume_runtime.start_project(&p, env, None).await {
                            resume_logs.add(
                                Some(p.id),
                                None,
                                "system",
                                "warn",
                                &format!("Could not auto-resume '{}': {}", p.name, e),
                            );
                        }
                    } else if p.app_status == "maintenance" {
                        let _ = resume_runtime.set_maintenance_mode(&p, true).await;
                    } else {
                        // Project is marked stopped. Clean up any orphan listeners found on its port without polluting user project logs.
                        if let Some(port) = p.port {
                            if !crate::runtime::Runtime::find_pids_on_port(port).is_empty() {
                                let killed = resume_runtime.kill_project_everything(&p).await;
                                if killed {
                                    tracing::info!("Cleaned up orphan processes for stopped project '{}' on port {}", p.name, port);
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Enforce max-5 deployment retention cleanup across existing projects at startup.
    crate::deployer::retention::prune_all_projects(&db, crate::deployer::MAX_DEPLOYMENT_RECORDS).await;

    let state = AppState {
        db,
        auth,
        projects: projects.clone(),
        logs: logs.clone(),
        monitor,
        caddy,
        runtime,
        deployer: deployer.clone(),
        cache,
        databases,
        updater,
        cluster,
    };

    let app = create_router(state);

    // Background binary hot-reload watcher: if target/release/webdep.exe is rebuilt on disk,
    // automatically hand off to the new binary so changes take effect without manual restarts.
    tokio::spawn(async move {
        let exe_path = std::path::PathBuf::from("target")
            .join("release")
            .join(if cfg!(windows) { "webdep.exe" } else { "webdep" });
        let initial_mtime = tokio::fs::metadata(&exe_path)
            .await
            .and_then(|m| m.modified())
            .ok();
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            if let (Some(init_t), Ok(meta)) = (initial_mtime, tokio::fs::metadata(&exe_path).await) {
                if let Ok(cur_t) = meta.modified() {
                    if cur_t > init_t && meta.len() > 1_000_000 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
                        let mut cmd = std::process::Command::new(&exe_path);
                        cmd.env("WEBDEP_RESTARTED", "1")
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null());
                        #[cfg(windows)]
                        {
                            use std::os::windows::process::CommandExt;
                            cmd.creation_flags(0x00000008 | 0x00000200);
                        }
                        if cmd.spawn().is_ok() {
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                            std::process::exit(0);
                        }
                    }
                }
            }
        }
    });

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("Invalid HOST or PORT");

    info!("WebDep listening on http://{}", addr);

    // Retry binding up to 30s if a previous instance is shutting down
    let max_attempts = 60;
    let mut attempt = 0;
    let listener = loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) => {
                attempt += 1;
                if attempt >= max_attempts {
                    return Err(e.into());
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    };
    axum::serve(listener, app).await?;
    if let Some(h) = control_pg_handle {
        h.stop().await;
    }

    Ok(())
}

/// Returns free glibc heap pages back to the OS. Only glibc (`target_env = "gnu"`)
/// exports `malloc_trim`; musl reclaims freed pages automatically and omits the symbol.
#[inline]
pub(crate) fn trim_process_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        extern "C" {
            fn malloc_trim(pad: libc::size_t) -> libc::c_int;
        }
        malloc_trim(0);
    }
}
