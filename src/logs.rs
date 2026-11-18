use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder, Row};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use crate::db::{now, DbPool};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LogEntry {
    pub id: i64,
    pub project_id: Option<i64>,
    pub deployment_id: Option<i64>,
    pub channel: String,
    pub level: String,
    pub message: String,
    pub ts: i64,
}

#[derive(Clone)]
pub struct Logs {
    db: DbPool,
    sender: Arc<broadcast::Sender<LogEntry>>,
    last_entry: Arc<Mutex<Option<(Option<i64>, String, String, i64)>>>,
    seq: Arc<AtomicI64>,
}

impl Logs {
    pub fn new(db: DbPool) -> Self {
        let (sender, _) = broadcast::channel(2048);
        Self {
            db,
            sender: Arc::new(sender),
            last_entry: Arc::new(Mutex::new(None)),
            seq: Arc::new(AtomicI64::new(1)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.sender.subscribe()
    }

    /// Broadcasts log immediately to live SSE subscribers and persists asynchronously to PostgreSQL.
    pub fn add(
        &self,
        project_id: Option<i64>,
        deployment_id: Option<i64>,
        channel: &str,
        level: &str,
        message: &str,
    ) -> i64 {
        let ts = now();
        let trimmed_msg = message.trim();
        if trimmed_msg.is_empty() {
            return 0;
        }

        // Deduplicate identical consecutive messages for the same project & channel within 3 seconds
        {
            if let Ok(mut guard) = self.last_entry.lock() {
                if let Some((prev_pid, ref prev_ch, ref prev_msg, prev_ts)) = *guard {
                    if prev_pid == project_id && prev_ch == channel && prev_msg == trimmed_msg && (ts - prev_ts) < 3000 {
                        return 0;
                    }
                }
                *guard = Some((project_id, channel.to_string(), trimmed_msg.to_string(), ts));
            }
        }

        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let entry = LogEntry {
            id,
            project_id,
            deployment_id,
            channel: channel.to_string(),
            level: level.to_string(),
            message: message.to_string(),
            ts,
        };

        let _ = self.sender.send(entry.clone());

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pool = self.db.clone();
            handle.spawn(async move {
                let _ = sqlx::query(
                    "INSERT INTO logs(project_id, deployment_id, channel, level, message, ts) VALUES($1, $2, $3, $4, $5, $6)",
                )
                .bind(entry.project_id)
                .bind(entry.deployment_id)
                .bind(entry.channel)
                .bind(entry.level)
                .bind(entry.message)
                .bind(entry.ts)
                .execute(&pool)
                .await;
            });
        }

        id
    }

    pub async fn list(
        &self,
        project_id: Option<i64>,
        deployment_id: Option<i64>,
        channel: Option<&str>,
        level: Option<&str>,
        limit: usize,
    ) -> Vec<LogEntry> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT id, project_id, deployment_id, channel, level, message, ts FROM logs WHERE 1=1",
        );

        if let Some(pid) = project_id {
            qb.push(" AND project_id = ").push_bind(pid);
        }
        if let Some(did) = deployment_id {
            qb.push(" AND deployment_id = ").push_bind(did);
        }
        if let Some(ch) = channel {
            if !ch.is_empty() {
                qb.push(" AND channel = ").push_bind(ch);
            }
        }
        if let Some(lvl) = level {
            if !lvl.is_empty() {
                qb.push(" AND level = ").push_bind(lvl);
            }
        }

        qb.push(" ORDER BY id DESC LIMIT ").push_bind(limit as i64);

        let Ok(rows) = qb.build().fetch_all(&self.db).await else {
            return Vec::new();
        };

        let mut list: Vec<LogEntry> = rows
            .into_iter()
            .filter_map(|r| {
                Some(LogEntry {
                    id: r.try_get(0).ok()?,
                    project_id: r.try_get(1).ok()?,
                    deployment_id: r.try_get(2).ok()?,
                    channel: r.try_get(3).ok()?,
                    level: r.try_get(4).ok()?,
                    message: r.try_get(5).ok()?,
                    ts: r.try_get(6).ok()?,
                })
            })
            .collect();
        list.reverse();
        list
    }
}
