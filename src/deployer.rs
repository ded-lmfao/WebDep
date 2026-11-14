pub mod builder;
pub mod retention;
pub mod worker;

pub use retention::MAX_DEPLOYMENT_RECORDS;

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

use crate::cache::AppCache;
use crate::db::{now, DbPool};
use crate::logs::Logs;
use crate::projects::{Project, Projects};
use crate::runtime::Runtime;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Deployment {
    pub id: i64,
    pub project_id: i64,
    #[serde(default)]
    pub project_name: Option<String>,
    pub release_id: Option<i64>,
    pub from_release_id: Option<i64>,
    pub kind: String,
    pub status: String,
    pub stage: String,
    pub sha: Option<String>,
    pub trigger: String,
    pub actor: String,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Release {
    pub id: i64,
    pub project_id: i64,
    pub sha: String,
    pub short_sha: String,
    pub message: Option<String>,
    pub author: Option<String>,
    pub committed_at: Option<i64>,
    pub dir: String,
    pub status: String,
    pub build_status: String,
    pub health_status: String,
    pub deployment_id: Option<i64>,
    pub created_at: i64,
    pub deployed_at: Option<i64>,
    pub live: bool,
    pub available: bool,
}

pub struct Deployer {
    db: DbPool,
    cache: Arc<AppCache>,
    logs: Logs,
    projects: Arc<Projects>,
    runtime: Arc<Runtime>,
    /// Global FIFO concurrency limit: allows only 1 deployment/rollback at a time to protect 1 GB VPS RAM
    queue_semaphore: Arc<Semaphore>,
    /// Tracks active or queued deployment ID per project_id to prevent duplicate concurrent deployments for the same website
    active_projects: Arc<Mutex<HashMap<i64, i64>>>,
    /// Tracks deployment IDs that already have a worker task spawned
    spawned_deployments: Arc<Mutex<HashSet<i64>>>,
}

const SELECT_DEPLOYMENT_FIELDS: &str = "\
    d.id, d.project_id, p.name, d.release_id, d.from_release_id, d.kind, d.status, d.stage, \
    d.sha, d.trigger, d.actor, d.error, d.started_at, d.finished_at";

fn pg_row_to_deployment(r: &sqlx::postgres::PgRow) -> Result<Deployment, sqlx::Error> {
    Ok(Deployment {
        id: r.try_get(0)?,
        project_id: r.try_get(1)?,
        project_name: r.try_get(2)?,
        release_id: r.try_get(3)?,
        from_release_id: r.try_get(4)?,
        kind: r.try_get(5)?,
        status: r.try_get(6)?,
        stage: r.try_get(7)?,
        sha: r.try_get(8)?,
        trigger: r.try_get(9)?,
        actor: r.try_get(10)?,
        error: r.try_get(11)?,
        started_at: r.try_get(12)?,
        finished_at: r.try_get(13)?,
    })
}

impl Deployer {
    pub async fn new(
        db: DbPool,
        logs: Logs,
        projects: Arc<Projects>,
        runtime: Arc<Runtime>,
        cache: Arc<AppCache>,
    ) -> Self {
        // Clean up any stale deployments left in active states from a previous server restart/crash
        let _ = sqlx::query(
            "UPDATE deployments SET status = 'failed', stage = 'failed', error = 'Interrupted by server restart', finished_at = $1 \
             WHERE status IN ('pending', 'running', 'in_progress', 'queued')",
        )
        .bind(now())
        .execute(&db)
        .await;

        Self {
            db,
            cache,
            logs,
            projects,
            runtime,
            queue_semaphore: Arc::new(Semaphore::new(1)),
            active_projects: Arc::new(Mutex::new(HashMap::new())),
            spawned_deployments: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Check if a project currently has an in-flight or queued deployment/rollback
    pub async fn has_active_deployment(&self, project_id: i64) -> bool {
        let dep_id_opt = self
            .active_projects
            .lock()
            .ok()
            .and_then(|active| active.get(&project_id).copied());

        if let Some(dep_id) = dep_id_opt {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM deployments WHERE id = $1")
                    .bind(dep_id)
                    .fetch_optional(&self.db)
                    .await
                    .ok()
                    .flatten();
            if let Some(s) = status {
                if s == "in_progress" || s == "running" || s == "pending" || s == "queued" {
                    return true;
                }
            }
        }
        false
    }

    pub async fn list_deployments(&self, project_id: Option<i64>, limit: usize) -> Vec<Deployment> {
        let mut qb = sqlx::QueryBuilder::<Postgres>::new(format!(
            "SELECT {} FROM deployments d LEFT JOIN projects p ON d.project_id = p.id",
            SELECT_DEPLOYMENT_FIELDS
        ));
        if let Some(pid) = project_id {
            qb.push(" WHERE d.project_id = ");
            qb.push_bind(pid);
        }
        qb.push(" ORDER BY d.id DESC LIMIT ");
        qb.push_bind(limit as i64);

        let rows = qb.build().fetch_all(&self.db).await.unwrap_or_default();
        rows.iter()
            .filter_map(|r| pg_row_to_deployment(r).ok())
            .collect()
    }

    pub async fn get_deployment(&self, id: i64) -> Option<Deployment> {
        let sql = format!(
            "SELECT {} FROM deployments d LEFT JOIN projects p ON d.project_id = p.id WHERE d.id = $1",
            SELECT_DEPLOYMENT_FIELDS
        );
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&self.db)
            .await
            .ok()??;
        pg_row_to_deployment(&row).ok()
    }

    pub async fn list_releases(&self, project_id: i64) -> Vec<Release> {
        let current_rel_id: Option<i64> =
            sqlx::query_scalar("SELECT current_release_id FROM projects WHERE id = $1")
                .bind(project_id)
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten();

        let rows = sqlx::query(
            "SELECT id, project_id, sha, short_sha, message, author, committed_at, dir, \
                    status, build_status, health_status, deployment_id, created_at, deployed_at \
             FROM releases WHERE project_id = $1 ORDER BY id DESC LIMIT 50",
        )
        .bind(project_id)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();

        rows.into_iter()
            .filter_map(|r| {
                let id: i64 = r.try_get(0).ok()?;
                let dir_str: String = r.try_get(7).ok()?;
                let status: String = r.try_get(8).ok()?;
                let dir_path = Path::new(&dir_str);
                let available = dir_path.exists() && status == "succeeded";
                let live = current_rel_id == Some(id);

                Some(Release {
                    id,
                    project_id: r.try_get(1).ok()?,
                    sha: r.try_get(2).ok()?,
                    short_sha: r.try_get(3).ok()?,
                    message: r.try_get(4).ok()?,
                    author: r.try_get(5).ok()?,
                    committed_at: r.try_get(6).ok()?,
                    dir: dir_str,
                    status,
                    build_status: r.try_get(9).ok()?,
                    health_status: r.try_get(10).ok()?,
                    deployment_id: r.try_get(11).ok()?,
                    created_at: r.try_get(12).ok()?,
                    deployed_at: r.try_get(13).ok()?,
                    live,
                    available,
                })
            })
            .collect()
    }

    pub async fn create_deployment(&self, p: &Project, kind: &str, actor: &str, trigger: &str) -> i64 {
        let ts = now();
        let action_name = if kind == "rollback" { "rollback.start" } else { "deploy.start" };
        let action_title = if kind == "rollback" { "Rollback" } else { "Deployment" };

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO deployments(project_id, kind, status, stage, trigger, actor, started_at) \
             VALUES($1, $2, 'running', 'queued', $3, $4, $5) RETURNING id",
        )
        .bind(p.id)
        .bind(kind)
        .bind(trigger)
        .bind(actor)
        .bind(ts)
        .fetch_one(&self.db)
        .await
        .unwrap_or(0);

        if id > 0 {
            let _ = sqlx::query(
                "INSERT INTO audit(project_id, action, actor, target, detail, ts) VALUES($1, $2, $3, $4, $5, $6)",
            )
            .bind(p.id)
            .bind(action_name)
            .bind(actor)
            .bind(&p.name)
            .bind(format!("Triggered {} #{} (Trigger: {})", action_title, id, trigger))
            .bind(ts)
            .execute(&self.db)
            .await;
        }

        // Keep at most the latest 5 deployment records per project and reclaim old storage
        // immediately as the new deployment starts.
        retention::prune_project_deployments(&self.db, p.id, retention::MAX_DEPLOYMENT_RECORDS).await;

        if let Ok(mut active) = self.active_projects.lock() {
            active.insert(p.id, id);
        }

        self.cache.invalidate_project(p.id);

        self.logs.add(
            Some(p.id),
            Some(id),
            if kind == "rollback" { "rollback" } else { "deployment" },
            "info",
            &format!("{} #{} queued by {} ({})", action_title, id, actor, trigger),
        );

        id
    }

    pub async fn update_stage(&self, dep_id: i64, stage: &str) {
        let _ = sqlx::query("UPDATE deployments SET stage = $1 WHERE id = $2")
            .bind(stage)
            .bind(dep_id)
            .execute(&self.db)
            .await;
    }

    pub async fn fail_deployment_step(&self, dep_id: i64, project_id: i64, channel: &str, error: &str) {
        self.logs.add(Some(project_id), Some(dep_id), channel, "error", error);
        self.finish_deployment(dep_id, "failed", Some(error)).await;
    }

    pub async fn finish_deployment(&self, dep_id: i64, status: &str, error: Option<&str>) {
        let _ = sqlx::query(
            "UPDATE deployments SET status = $1, error = $2, finished_at = $3 WHERE id = $4",
        )
        .bind(status)
        .bind(error)
        .bind(now())
        .bind(dep_id)
        .execute(&self.db)
        .await;

        if status != "success" {
            let _ = sqlx::query(
                "UPDATE releases SET status = 'failed' WHERE deployment_id = $1 AND status != 'succeeded'",
            )
            .bind(dep_id)
            .execute(&self.db)
            .await;
        }

        let (project_id, prev_stage, kind, actor, target_name) = sqlx::query_as::<_, (i64, String, String, String, Option<String>)>(
            "SELECT d.project_id, d.stage, d.kind, d.actor, p.name \
             FROM deployments d LEFT JOIN projects p ON d.project_id = p.id WHERE d.id = $1",
        )
        .bind(dep_id)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten()
        .map(|(pid, st, k, a, n)| (pid, st, k, a, n.unwrap_or_else(|| "project".to_string())))
        .unwrap_or_else(|| {
            (
                0,
                "queued".to_string(),
                "deploy".to_string(),
                "system".to_string(),
                "project".to_string(),
            )
        });

        if status == "success" {
            self.update_stage(dep_id, "ready").await;
        } else if prev_stage == "queued" {
            self.update_stage(dep_id, "failed_queue").await;
        }

        let is_rollback = kind == "rollback";
        let action_name = if status == "success" {
            if is_rollback { "rollback.success" } else { "deploy.success" }
        } else {
            if is_rollback { "rollback.fail" } else { "deploy.fail" }
        };

        let action_title = if is_rollback { "Rollback" } else { "Deployment" };
        let detail_msg = if status == "success" {
            format!("{} #{} finished successfully", action_title, dep_id)
        } else {
            format!("{} #{} failed: {}", action_title, dep_id, error.unwrap_or("unknown error"))
        };

        let _ = sqlx::query(
            "INSERT INTO audit(project_id, action, actor, target, detail, ts) VALUES($1, $2, $3, $4, $5, $6)",
        )
        .bind(project_id)
        .bind(action_name)
        .bind(actor)
        .bind(target_name)
        .bind(detail_msg)
        .bind(now())
        .execute(&self.db)
        .await;

        if project_id > 0 {
            let pid = project_id;
            if let Ok(mut active_map) = self.active_projects.lock() {
                if active_map.get(&pid) == Some(&dep_id) {
                    active_map.remove(&pid);
                }
            }
            retention::prune_project_deployments(&self.db, pid, retention::MAX_DEPLOYMENT_RECORDS).await;
            self.cache.invalidate_project(pid);
        } else {
            self.cache.invalidate_all();
        }
        if let Ok(mut spawned) = self.spawned_deployments.lock() {
            spawned.remove(&dep_id);
        }
    }

    async fn acquire_queue_permit(
        &self,
        project_id: i64,
        dep_id: i64,
        action_name: &str,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        match self.queue_semaphore.clone().try_acquire_owned() {
            Ok(p) => Some(p),
            Err(_) => {
                let action_title = if action_name == "rollback" { "Rollback" } else { "Deployment" };
                self.logs.add(
                    Some(project_id),
                    Some(dep_id),
                    action_name,
                    "info",
                    &format!(
                        "{} #{} queued — waiting for current active deployment to finish (1-at-a-time concurrency limit)",
                        action_title, dep_id
                    ),
                );
                match self.queue_semaphore.clone().acquire_owned().await {
                    Ok(p) => Some(p),
                    Err(_) => {
                        self.finish_deployment(dep_id, "failed", Some("Deployment queue closed")).await;
                        None
                    }
                }
            }
        }
    }
}
