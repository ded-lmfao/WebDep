use axum::{
    body::Body,
    extract::{Path as AxPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Response,
    },
    Json,
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::convert::Infallible;
use tokio_stream::StreamExt;

use super::common::*;

#[derive(Serialize)]
pub(crate) struct AppLogsResponse {
    lines: Vec<String>,
}

pub(crate) async fn get_app_logs_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<i64>,
) -> Result<Json<AppLogsResponse>, (StatusCode, String)> {
    let _actor = verify_power(&state, &headers, "logs", Some(id)).await?;
    let entries = state.logs.list(Some(id), None, Some("app"), None, 200).await;
    let logs = entries
        .into_iter()
        .map(|l| l.message)
        .collect();

    Ok(Json(AppLogsResponse { lines: logs }))
}


// ---------------------------------------------------------------------------
// Real-Time SSE Log Streaming & Log History
// ---------------------------------------------------------------------------
#[derive(Deserialize)]
pub(crate) struct LogsQuery {
    project: Option<i64>,
    /// Log channel to list (`app` by default; e.g. `system`, `deployment`, `health`; `all` for every channel).
    channel: Option<String>,
    level: Option<String>,
    backlog: Option<usize>,
    limit: Option<usize>,
}

pub(crate) async fn list_logs_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LogsQuery>,
) -> Result<Json<Vec<crate::logs::LogEntry>>, (StatusCode, String)> {
    let Some(pid) = q.project else {
        let _actor = require_current_user(&state, &headers).await?;
        return Ok(Json(Vec::new()));
    };
    let _actor = verify_power(&state, &headers, "logs", Some(pid)).await?;
    let limit = q.limit.or(q.backlog).unwrap_or(300).min(2000);
    let channel = match q.channel.as_deref().map(str::trim) {
        None | Some("") => Some("app"),
        Some(c) if c.eq_ignore_ascii_case("all") => None,
        Some(c) => Some(c),
    };
    let logs = state.logs.list(Some(pid), None, channel, q.level.as_deref(), limit).await;
    Ok(Json(logs))
}

pub(crate) async fn logs_sse_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LogsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, String)> {
    let actor = if let Some(pid) = q.project {
        verify_power(&state, &headers, "logs", Some(pid)).await?
    } else {
        require_current_user(&state, &headers).await?
    };
    let backlog_limit = q.backlog.unwrap_or(200).min(1000);
    let initial_logs = match q.project {
        Some(pid) => {
            state
                .logs
                .list(
                    Some(pid),
                    None,
                    Some("app"),
                    q.level.as_deref(),
                    backlog_limit,
                )
                .await
        }
        None => Vec::new(),
    };
    let project_filter = q.project;
    let level_filter = q.level;

    let initial_event = if !initial_logs.is_empty() {
        serde_json::to_string(&initial_logs)
            .ok()
            .map(|json| Ok(Event::default().data(json)))
    } else {
        None
    };

    let log_rx = state.logs.subscribe();
    let rev_rx = state.auth.subscribe_revocations();
    let rev_state = state.clone();
    let rev_user = actor;
    let live_stream = futures_util::stream::unfold(
        (log_rx, rev_rx, false),
        move |(mut log_rx, mut rev_rx, done)| {
            let state = rev_state.clone();
            let user = rev_user.clone();
            let level_filter = level_filter.clone();
            async move {
                if done {
                    return None;
                }
                loop {
                    tokio::select! {
                        biased;
                        rev = rev_rx.recv() => {
                            match rev {
                                Ok(ev) if ev.user_id == user.id => {
                                    let revoked = if matches!(ev.kind.as_str(), "session_revoked" | "user_disabled") {
                                        true
                                    } else if let Some(pid) = project_filter {
                                        !state.auth.has_power_for_scope(&user, "logs", "project", Some(pid)).await
                                    } else {
                                        false
                                    };
                                    if revoked {
                                        let payload = serde_json::json!({
                                            "reason": ev.reason,
                                            "kind": ev.kind,
                                        }).to_string();
                                        let evt = Event::default().event("revoked").data(payload);
                                        return Some((Ok::<Event, Infallible>(evt), (log_rx, rev_rx, true)));
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    if let Some(pid) = project_filter {
                                        if !state.auth.has_power_for_scope(&user, "logs", "project", Some(pid)).await {
                                            let evt = Event::default()
                                                .event("revoked")
                                                .data(r#"{"reason":"Access revoked","kind":"grant_revoked"}"#);
                                            return Some((Ok::<Event, Infallible>(evt), (log_rx, rev_rx, true)));
                                        }
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                                _ => {}
                            }
                        }
                        msg = log_rx.recv() => {
                            match msg {
                                Ok(entry) => {
                                    // Strict per-app console isolation: only emit `app` channel logs for this exact project.
                                    let Some(pid) = project_filter else {
                                        continue;
                                    };
                                    if entry.project_id != Some(pid) || entry.channel != "app" {
                                        continue;
                                    }
                                    if let Some(ref lvl) = level_filter {
                                        if !lvl.is_empty() && &entry.level != lvl {
                                            continue;
                                        }
                                    }
                                    if let Ok(json) = serde_json::to_string(&vec![entry]) {
                                        return Some((Ok(Event::default().data(json)), (log_rx, rev_rx, false)));
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                            }
                        }
                    }
                }
            }
        },
    );

    let combined = futures_util::stream::iter(initial_event).chain(live_stream);
    Ok(Sse::new(combined).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
pub(crate) struct DownloadLogsQuery {
    project: Option<i64>,
}

pub(crate) async fn logs_download_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DownloadLogsQuery>,
) -> Result<Response, (StatusCode, String)> {
    let logs = match q.project {
        Some(pid) => {
            let _actor = verify_power(&state, &headers, "logs", Some(pid)).await?;
            state.logs.list(Some(pid), None, Some("app"), None, 5000).await
        }
        None => {
            let _actor = require_current_user(&state, &headers).await?;
            Vec::new()
        }
    };
    let mut out = String::new();
    for l in logs {
        out.push_str(&format!("[{}] {}\n", l.ts, l.message));
    }

    let res = Response::builder()
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, "attachment; filename=\"webdep-logs.txt\"")
        .body(Body::from(out))
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response".to_string()))?;

    Ok(res)
}



#[derive(Serialize)]
pub(crate) struct AuditItem {
    id: i64,
    #[serde(rename = "projectId")]
    project_id: Option<i64>,
    ts: i64,
    actor: String,
    action: String,
    target: Option<String>,
    detail: Option<String>,
    ip: Option<String>,
}

pub(crate) fn pg_row_to_audit_item(r: &sqlx::postgres::PgRow, default_target: Option<String>) -> Result<AuditItem, sqlx::Error> {
    Ok(AuditItem {
        id: r.try_get(0)?,
        project_id: r.try_get(1)?,
        ts: r.try_get(2)?,
        actor: r.try_get(3)?,
        action: r.try_get(4)?,
        target: r.try_get::<Option<String>, _>(5)?.or(default_target),
        detail: r.try_get(6)?,
        ip: r.try_get(7)?,
    })
}

#[derive(Deserialize)]
pub(crate) struct AuditQuery {
    #[serde(rename = "projectId")]
    project_id: Option<i64>,
    #[serde(rename = "databaseId")]
    database_id: Option<i64>,
    limit: Option<usize>,
    /// Panel-wide feed only: entries older than this id (next page).
    before: Option<i64>,
    /// Panel-wide feed only: exact user name.
    actor: Option<String>,
    /// Panel-wide feed only: text in the action, target, or details.
    q: Option<String>,
}

/// Panel-wide audit rows matching the filters, newest first.
async fn global_audit_rows(
    state: &AppState,
    before: Option<i64>,
    actor: Option<&str>,
    q: Option<&str>,
    limit: i64,
) -> Result<Vec<sqlx::postgres::PgRow>, (StatusCode, String)> {
    let pattern = q.map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("%{}%", s.replace(['%', '_'], "")));
    sqlx::query(
        "SELECT id, project_id, ts, actor, action, target, detail, ip FROM audit
         WHERE ($1::BIGINT IS NULL OR id < $1)
           AND ($2::TEXT IS NULL OR LOWER(actor) = LOWER($2))
           AND ($3::TEXT IS NULL OR action ILIKE $3 OR COALESCE(target, '') ILIKE $3 OR COALESCE(detail, '') ILIKE $3)
         ORDER BY id DESC LIMIT $4",
    )
    .bind(before)
    .bind(actor.map(str::trim).filter(|s| !s.is_empty()))
    .bind(pattern)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn csv_field(s: &str) -> String {
    // Quote every field; neutralise spreadsheet formulas.
    let mut v = s.replace('"', "\"\"");
    if v.starts_with(['=', '+', '-', '@']) {
        v.insert(0, '\'');
    }
    format!("\"{v}\"")
}

/// `GET /api/audit/export?actor=&q=`: the panel-wide audit log as CSV (up to 50,000 rows).
pub(crate) async fn audit_export_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
) -> Result<Response, (StatusCode, String)> {
    let u = verify_power(&state, &headers, "audit", None).await?;
    if !u.has_power("settings_manage") {
        return Err((StatusCode::FORBIDDEN, "Only admins can export the panel audit log.".into()));
    }
    let rows = global_audit_rows(&state, None, q.actor.as_deref(), q.q.as_deref(), 50_000).await?;
    let mut out = String::from("time,user,action,target,details,ip\n");
    for row in rows {
        if let Ok(r) = pg_row_to_audit_item(&row, None) {
            let when = chrono_like(r.ts);
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                csv_field(&when),
                csv_field(&r.actor),
                csv_field(&r.action),
                csv_field(r.target.as_deref().unwrap_or("")),
                csv_field(r.detail.as_deref().unwrap_or("")),
                csv_field(r.ip.as_deref().unwrap_or("")),
            ));
        }
    }
    let mut res = out.into_response();
    res.headers_mut().insert(header::CONTENT_TYPE, axum::http::HeaderValue::from_static("text/csv; charset=utf-8"));
    res.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=\"webdep-audit.csv\""),
    );
    Ok(res)
}

/// UTC timestamp `YYYY-MM-DD HH:MM:SS` from unix milliseconds (no date crate needed).
fn chrono_like(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

pub(crate) async fn audit_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
) -> Result<Response, (StatusCode, String)> {
    let (u, db_opt) = if let Some(db_id) = q.database_id {
        let (user, db) = crate::api::databases::get_authorized_database(
            &state,
            &headers,
            &["databases", "databases_manage", "audit", "activity"],
            db_id,
        )
        .await?;
        (user, Some(db))
    } else {
        // Per-app activity: the app's `activity` power (or `audit`); the panel-wide
        // feed still needs panel-level `audit`.
        let powers: &[&str] = if q.project_id.is_some() { &["activity", "audit"] } else { &["audit"] };
        (verify_any_power(&state, &headers, powers, q.project_id).await?, None)
    };
    let max_limit = q.limit.unwrap_or(500).min(1000);
    let cache_key = format!(
        "audit:d{}:p{}:u{}:l{}:b{}:a{}:q{}",
        q.database_id.unwrap_or(0),
        q.project_id.unwrap_or(0),
        u.id,
        max_limit,
        q.before.unwrap_or(0),
        q.actor.as_deref().unwrap_or(""),
        q.q.as_deref().unwrap_or("")
    );
    if let Some(res) = check_api_cache(&state.cache, &cache_key) {
        return Ok(res);
    }

    let mut list = Vec::new();
    if let Some(db) = db_opt {
        let db_tag = format!("DB #{}", db.id);
        let rows = sqlx::query(
            "SELECT id, project_id, ts, actor, action, target, detail, ip \
             FROM audit \
             WHERE LOWER(COALESCE(target, '')) = LOWER($1) \
                OR (LOWER(COALESCE(target, '')) = LOWER($2) AND action LIKE 'database.%') \
             ORDER BY ts DESC, id DESC LIMIT $3",
        )
        .bind(&db_tag)
        .bind(&db.name)
        .bind(max_limit as i64)
        .fetch_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        for r in rows {
            if let Ok(item) = pg_row_to_audit_item(&r, Some(db.name.clone())) {
                list.push(item);
            }
        }
    } else if let Some(pid) = q.project_id {
        let p_info: Option<(String, String)> = sqlx::query_as(
            "SELECT name, slug FROM projects WHERE id = $1",
        )
        .bind(pid)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        let (p_name, p_slug) = p_info.unwrap_or_default();

        let rows = sqlx::query(
            "SELECT id, project_id, ts, actor, action, target, detail, ip \
             FROM audit \
             WHERE project_id = $1 \
                OR LOWER(COALESCE(target, '')) = LOWER($2) \
                OR LOWER(COALESCE(target, '')) = LOWER($3) \
                OR COALESCE(target, '') = CAST($1 AS TEXT) \
             ORDER BY ts DESC, id DESC LIMIT $4",
        )
        .bind(pid)
        .bind(&p_name)
        .bind(&p_slug)
        .bind(max_limit as i64)
        .fetch_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        for r in rows {
            if let Ok(item) = pg_row_to_audit_item(&r, Some(p_name.clone())) {
                list.push(item);
            }
        }

        let mut existing_dep_ids = std::collections::HashSet::with_capacity(list.len());
        let mut backup_ts: Vec<i64> = Vec::new();
        for a in &list {
            if a.action.starts_with("backup.") {
                backup_ts.push(a.ts);
            }
            if let Some(detail) = a.detail.as_deref() {
                for seg in detail.split('#').skip(1) {
                    let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if let Ok(did) = digits.parse::<i64>() {
                        existing_dep_ids.insert(did);
                    }
                }
            }
        }
        backup_ts.sort_unstable();

        // Also include historical deployments & rollbacks from the `deployments` table
        // that predate per-website audit rows so the Activity tab shows the full timeline.
        if let Ok(drows) = sqlx::query_as::<_, (i64, String, String, Option<String>, String, String, Option<String>, i64, Option<i64>, Option<i64>)>(
            "SELECT id, kind, status, sha, trigger, actor, error, started_at, finished_at, release_id \
             FROM deployments WHERE project_id = $1 ORDER BY id DESC LIMIT 200",
        )
        .bind(pid)
        .fetch_all(&state.db)
        .await
        {
            for (dep_id, kind, status, sha, trigger, actor, error, started_at, finished_at, rel_id) in drows {
                if !existing_dep_ids.contains(&dep_id) {
                    let short_sha = sha.as_deref().map(|s| &s[..s.len().min(7)]).unwrap_or("HEAD");
                    let action = match (kind.as_str(), status.as_str()) {
                        ("rollback", "success") => "rollback.success",
                        ("rollback", "failed") => "rollback.failed",
                        ("rollback", _) => "rollback.trigger",
                        (_, "success") => "deploy.success",
                        (_, "failed") => "deploy.failed",
                        _ => "deploy.trigger",
                    };
                    let detail = match status.as_str() {
                        "success" => format!(
                            "{} #{} succeeded (commit {}, release #{}, trigger: {})",
                            if kind == "rollback" { "Rollback" } else { "Deployment" },
                            dep_id,
                            short_sha,
                            rel_id.unwrap_or(0),
                            trigger
                        ),
                        "failed" => format!(
                            "{} #{} failed (commit {}): {}",
                            if kind == "rollback" { "Rollback" } else { "Deployment" },
                            dep_id,
                            short_sha,
                            error.unwrap_or_else(|| "build/runtime error".to_string())
                        ),
                        _ => format!(
                            "{} #{} started (commit {}, trigger: {})",
                            if kind == "rollback" { "Rollback" } else { "Deployment" },
                            dep_id,
                            short_sha,
                            trigger
                        ),
                    };
                    list.push(AuditItem {
                        id: 1_000_000 + dep_id,
                        project_id: Some(pid),
                        ts: finished_at.unwrap_or(started_at),
                        actor,
                        action: action.to_string(),
                        target: Some(p_name.clone()),
                        detail: Some(detail),
                        ip: None,
                    });
                }
            }
        }

        // Also include historical snapshots from the `backups` table if not already in audit
        if let Ok(brows) = sqlx::query_as::<_, (i64, String, Option<String>, i64, i64)>(
            "SELECT id, kind, note, size_bytes, created_at FROM backups WHERE project_id = $1 ORDER BY id DESC LIMIT 100",
        )
        .bind(pid)
        .fetch_all(&state.db)
        .await
        {
            for (bid, bkind, bnote, bsize, bts) in brows {
                let note_str = bnote.unwrap_or_else(|| "Snapshot".to_string());
                let idx = backup_ts.partition_point(|&ts| ts <= bts - 5000);
                let already_logged = idx < backup_ts.len() && backup_ts[idx] < bts + 5000;
                if !already_logged && bts > 0 {
                    list.push(AuditItem {
                        id: 2_000_000 + bid,
                        project_id: Some(pid),
                        ts: bts,
                        actor: "system".to_string(),
                        action: "backup.create".to_string(),
                        target: Some(p_name.clone()),
                        detail: Some(format!(
                            "Backup #{}: {} ({}, {} KB)",
                            bid,
                            note_str,
                            bkind,
                            (bsize / 1024).max(1)
                        )),
                        ip: None,
                    });
                }
            }
        }

        list.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));
        list.truncate(max_limit);
    } else {
        let rows = global_audit_rows(&state, q.before, q.actor.as_deref(), q.q.as_deref(), max_limit as i64).await?;

        let can_global = u.has_power("settings_manage");
        for row in rows {
            if let Ok(r) = pg_row_to_audit_item(&row, None) {
                match r.project_id {
                    Some(pid) if !u.can_access_project(pid) => continue,
                    None if !can_global => continue,
                    _ => {}
                }
                list.push(r);
            }
        }
    }

    cached_json_response(&state.cache, cache_key, &list, std::time::Duration::from_secs(5))
        .map_err(|c| (c, "Serialization error".to_string()))
}
