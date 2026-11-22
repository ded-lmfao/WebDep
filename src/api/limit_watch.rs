//! Watches running apps against their CPU, memory and disk limits.
//!
//! An app is checked shortly after it starts (a start, a restart and a deploy all count), and then
//! every 2 minutes while it runs. When an app reaches or crosses a limit the panel records an event
//! and shows a pop-up to the people who can see the app:
//!
//! * disk over its limit: the app is stopped and cannot start until space is freed;
//! * memory within 5% of (or over) its limit, or CPU at its cap: the app keeps running, but is
//!   being slowed down or may crash, so its owners are told why.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use sqlx::Row;

use super::common::*;
use super::limits::{database_over_disk, over_message, project_over_disk};

const TICK: Duration = Duration::from_secs(15);
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);
const RECHECK_EVERY: Duration = Duration::from_secs(120);
const ALERT_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const MB: u64 = 1024 * 1024;

/// When one app is due for a check.
struct Schedule {
    started: i64,
    first_due: Instant,
    first_done: bool,
    last_check: Instant,
}

impl Schedule {
    fn new(started: i64, now: Instant) -> Self {
        Schedule { started, first_due: now + FIRST_CHECK_AFTER, first_done: false, last_check: now }
    }

    /// `true` when a check should run now. A new start time (the app was started or restarted)
    /// begins a fresh schedule: one check after a short settle time, then every 2 minutes.
    fn poll(&mut self, started: i64, now: Instant) -> bool {
        if started != self.started {
            *self = Schedule::new(started, now);
        }
        let due = (!self.first_done && now >= self.first_due) || now.duration_since(self.last_check) >= RECHECK_EVERY;
        if due {
            self.first_done = true;
            self.last_check = now;
        }
        due
    }
}

static SCHEDULES: LazyLock<Mutex<HashMap<(bool, i64), Schedule>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static LAST_ALERT: LazyLock<Mutex<HashMap<(bool, i64, &'static str), Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn due_now(is_db: bool, id: i64, started: i64) -> bool {
    let now = Instant::now();
    let mut map = SCHEDULES.lock().unwrap_or_else(|e| e.into_inner());
    let schedule = map.entry((is_db, id)).or_insert_with(|| Schedule::new(started, now));
    if schedule.started != started {
        // Restarted: it gets a fresh check and may alert again straight away.
        LAST_ALERT.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| (k.0, k.1) != (is_db, id));
    }
    schedule.poll(started, now)
}

/// Memory is "at the limit" from 95% of it.
fn ram_reached(used_bytes: u64, limit_mb: i32) -> bool {
    limit_mb > 0 && used_bytes as f64 >= f64::from(limit_mb) * MB as f64 * 0.95
}

/// CPU is "at the cap" from 95% of it (`raw_cpu` and the limit are both percent of one core).
fn cpu_reached(raw_cpu: f32, limit_percent: i32) -> bool {
    limit_percent > 0 && f64::from(raw_cpu) >= f64::from(limit_percent) * 0.95
}

fn cooled_down(key: (bool, i64, &'static str)) -> bool {
    let mut map = LAST_ALERT.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if map.get(&key).is_some_and(|t| now.duration_since(*t) < ALERT_COOLDOWN) {
        return false;
    }
    map.insert(key, now);
    true
}

struct Target {
    is_db: bool,
    id: i64,
    name: String,
    pid: Option<u32>,
    cpu_limit: i32,
    memory_limit_mb: i32,
    disk_limit_mb: i64,
    project: Option<crate::projects::Project>,
    data_dir: String,
}

async fn record_event(state: &AppState, t: &Target, limit_kind: &'static str, message: &str, stopped: bool) {
    crate::app_limits::record_event(&state.db, t.is_db, t.id, &t.name, limit_kind, message, stopped).await;
    let action = if stopped { "app.limit_stop" } else { "app.limit_reached" };
    if t.is_db {
        log_audit(state, "system", action, Some(&database_tag(t.id)), Some(message)).await;
    } else {
        log_audit_project(state, Some(t.id), "system", action, Some(&t.name), Some(message)).await;
    }
    crate::maintenance::notify(&state.db, "app.limit", format!("WebDep: {message}")).await;
    state.cache.invalidate_all();
}

async fn collect_due(state: &AppState) -> Vec<Target> {
    let local = state.cluster.default_local_node_id().await;
    let on_this_server = |node: Option<i64>| node.is_none() || node == local;
    let mut out = Vec::new();

    if let Ok(projects) = state.projects.list().await {
        for p in projects {
            let has_limit = p.cpu_limit > 0 || p.memory_limit_mb > 0 || p.disk_limit_mb > 0;
            let running = p.app_status == "running" || p.app_status == "maintenance";
            if !has_limit || !running || !on_this_server(p.node_id) {
                continue;
            }
            if !due_now(false, p.id, p.app_started_at.unwrap_or(0)) {
                continue;
            }
            out.push(Target {
                is_db: false,
                id: p.id,
                name: p.name.clone(),
                pid: p.app_pid.and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 1),
                cpu_limit: p.cpu_limit,
                memory_limit_mb: p.memory_limit_mb,
                disk_limit_mb: p.disk_limit_mb,
                data_dir: String::new(),
                project: Some(p),
            });
        }
    }

    let rows = sqlx::query(
        "SELECT id, name, data_dir, cpu_limit, memory_limit_mb, disk_limit_mb, node_id, COALESCE(started_at, 0), container_id
         FROM databases WHERE status = 'running' AND (cpu_limit > 0 OR memory_limit_mb > 0 OR disk_limit_mb > 0)",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    for r in rows {
        let (id, name, data_dir): (i64, String, String) = (r.get(0), r.get(1), r.get(2));
        let node: Option<i64> = r.get(6);
        let started: i64 = r.get(7);
        let container: Option<String> = r.get(8);
        if !on_this_server(node) || !due_now(true, id, started) {
            continue;
        }
        let pid = container
            .as_deref()
            .and_then(|c| c.strip_prefix("pid-"))
            .and_then(|s| s.parse::<u32>().ok())
            .or_else(|| crate::databases::read_pg_pid(&std::path::PathBuf::from(&data_dir).join("data")));
        out.push(Target {
            is_db: true,
            id,
            name,
            pid,
            cpu_limit: r.get(3),
            memory_limit_mb: r.get(4),
            disk_limit_mb: r.get(5),
            project: None,
            data_dir,
        });
    }
    out
}

/// Disk first (an app over its limit is stopped), then memory and CPU for the ones still running.
async fn check_targets(state: &AppState, targets: Vec<Target>) {
    let mut running = Vec::new();
    for t in targets {
        if t.disk_limit_mb > 0 {
            let over = if let Some(p) = t.project.clone() {
                tokio::task::spawn_blocking(move || project_over_disk(&p)).await.ok().flatten()
            } else {
                let (dir, mb) = (t.data_dir.clone(), t.disk_limit_mb);
                tokio::task::spawn_blocking(move || database_over_disk(&dir, mb)).await.ok().flatten()
            };
            if let Some((used, limit)) = over {
                let stopped = if let Some(p) = &t.project {
                    state.runtime.stop_project(p).await.is_ok()
                } else {
                    state.databases.stop(t.id).await.is_ok()
                };
                if stopped {
                    let msg = format!("{} The app was stopped.", over_message(&t.name, used, limit));
                    record_event(state, &t, "disk", &msg, true).await;
                }
                continue;
            }
        }
        if t.pid.is_some() && (t.cpu_limit > 0 || t.memory_limit_mb > 0) {
            running.push(t);
        }
    }
    if running.is_empty() {
        return;
    }

    // CPU figures need the sampler to have run a few times: wake it and give it a moment.
    state.monitor.touch_activity();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let pids: Vec<u32> = running.iter().filter_map(|t| t.pid).collect();
    let (_, metrics) = state.monitor.get_panel_and_tree_metrics(std::process::id(), &pids);
    for t in &running {
        let Some(&(raw_cpu, mem)) = t.pid.and_then(|p| metrics.get(&p)) else { continue };
        if ram_reached(mem, t.memory_limit_mb) && cooled_down((t.is_db, t.id, "ram")) {
            let msg = format!(
                "'{}' is using {:.1} MB of its {} MB memory limit. At the limit the app is slowed down, its memory requests fail and it may crash. Ask an admin to raise the limit.",
                t.name,
                mem as f64 / MB as f64,
                t.memory_limit_mb
            );
            record_event(state, t, "ram", &msg, false).await;
        }
        if cpu_reached(raw_cpu, t.cpu_limit) && cooled_down((t.is_db, t.id, "cpu")) {
            let msg = format!(
                "'{}' is using {:.0}% of a CPU core and its CPU limit is {}%. It is being slowed down to stay at the limit. Ask an admin to raise the limit if it needs more.",
                t.name, raw_cpu, t.cpu_limit
            );
            record_event(state, t, "cpu", &msg, false).await;
        }
    }
}

/// Runs for the life of the panel.
pub(crate) async fn run(state: AppState) {
    let mut interval = tokio::time::interval(TICK);
    loop {
        interval.tick().await;
        let due = collect_due(&state).await;
        if !due.is_empty() {
            check_targets(&state, due).await;
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    since: Option<i64>,
}

/// New limit events for the signed-in user. Without `since` it only reports the newest id, so a
/// freshly opened panel does not replay old events.
pub(crate) async fn limit_events_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let user = require_current_user(&state, &headers).await?;
    let latest: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM limit_events")
        .fetch_one(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some(since) = q.since else {
        return Ok(Json(serde_json::json!({ "events": [], "latestId": latest })));
    };
    let rows = sqlx::query(
        "SELECT id, created_at, kind, app_id, app_name, limit_kind, message, stopped
         FROM limit_events WHERE id > $1 ORDER BY id LIMIT 50",
    )
    .bind(since)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut events = Vec::new();
    for r in rows {
        let (kind, app_id): (String, i64) = (r.get(2), r.get(3));
        let visible = !user.disabled
            && if kind == "database" {
                match state.databases.get(app_id).await.ok().flatten() {
                    Some(db) => state
                        .auth
                        .has_database_power_for_user(&user, &["databases", "databases_manage"], db.id, db.linked_project_id)
                        .await,
                    None => false,
                }
            } else {
                user.can_access_project(app_id)
            };
        if visible {
            events.push(serde_json::json!({
                "id": r.get::<i64, _>(0),
                "ts": r.get::<i64, _>(1),
                "kind": kind,
                "appId": app_id,
                "appName": r.get::<String, _>(4),
                "limit": r.get::<String, _>(5),
                "message": r.get::<String, _>(6),
                "stopped": r.get::<bool, _>(7),
            }));
        }
    }
    let newest = events.last().and_then(|e| e["id"].as_i64()).unwrap_or(since).max(since);
    // Events this user cannot see are skipped for good; report the highest id examined.
    Ok(Json(serde_json::json!({ "events": events, "latestId": latest.max(newest) })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_check_comes_after_a_short_settle_then_every_two_minutes() {
        let t0 = Instant::now();
        let mut s = Schedule::new(100, t0);
        assert!(!s.poll(100, t0 + Duration::from_secs(5)), "too soon after the start");
        assert!(s.poll(100, t0 + Duration::from_secs(21)), "the first check");
        assert!(!s.poll(100, t0 + Duration::from_secs(60)));
        assert!(!s.poll(100, t0 + Duration::from_secs(140)), "less than 2 minutes since the last check");
        assert!(s.poll(100, t0 + Duration::from_secs(21 + 120)), "then every 2 minutes");
    }

    #[test]
    fn a_restart_starts_the_schedule_over() {
        let t0 = Instant::now();
        let mut s = Schedule::new(100, t0);
        assert!(s.poll(100, t0 + Duration::from_secs(25)));
        // The app is restarted: its start time changes, so it is checked again soon, not in 2 minutes.
        let restarted = t0 + Duration::from_secs(40);
        assert!(!s.poll(200, restarted));
        assert!(s.poll(200, restarted + Duration::from_secs(21)));
    }

    #[test]
    fn limits_are_reached_from_95_percent() {
        assert!(!ram_reached(100 * MB, 0), "no limit");
        assert!(!ram_reached(94 * MB, 100));
        assert!(ram_reached(95 * MB, 100));
        assert!(ram_reached(300 * MB, 100));
        assert!(!cpu_reached(94.0, 100));
        assert!(cpu_reached(99.0, 100));
        assert!(!cpu_reached(500.0, 0), "no limit");
    }

    #[test]
    fn an_alert_repeats_only_after_the_cooldown() {
        let key = (false, 990_001, "ram");
        assert!(cooled_down(key));
        assert!(!cooled_down(key), "right after an alert");
        assert!(cooled_down((true, 990_001, "ram")), "other apps are independent");
    }
}
