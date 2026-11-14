use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

use super::builder::{merge_copy_dir, switch_current, run_shell_cmd};
use super::Deployer;
use crate::db::now;
use crate::git::{CommitInfo, Git};

impl Deployer {
    pub async fn run_deploy(self: Arc<Self>, project_id: i64, dep_id: i64) {
        {
            let mut spawned = self.spawned_deployments.lock().unwrap();
            if !spawned.insert(dep_id) {
                // Worker already running or queued for this deployment ID
                return;
            }
        }

        let permit = match self.acquire_queue_permit(project_id, dep_id, "deployment").await {
            Some(p) => p,
            None => return,
        };

        self.execute_deploy_inner(project_id, dep_id).await;
        drop(permit);
    }

    pub(crate) async fn execute_deploy_inner(&self, project_id: i64, dep_id: i64) {
        let p = match self.projects.get(project_id).await {
            Ok(Some(p)) => p,
            _ => {
                self.finish_deployment(dep_id, "failed", Some("Project not found")).await;
                return;
            }
        };

        let start_stop_seq = self.runtime.get_stop_seq(p.id);
        let container = self.runtime.ensure_container(&p).await;
        self.logs.add(
            Some(p.id),
            Some(dep_id),
            "deployment",
            "info",
            &format!(
                "Starting deployment #{} for {} in isolated container '{}' (Port: {}, DB: {})",
                dep_id,
                p.name,
                container.container_id,
                container.port,
                container.db_dir.display()
            ),
        );

        // Stage 1: Fetching
        self.update_stage(dep_id, "fetching").await;
        let repo_dir = Path::new(&p.deploy_dir).join("repo");
        if let Err(e) = Git::clone_or_fetch(&p.repo_url, &repo_dir, &p.branch) {
            self.logs.add(Some(p.id), Some(dep_id), "github", "error", &format!("Git fetch failed: {}", e));
            self.finish_deployment(dep_id, "failed", Some(&e)).await;
            return;
        }

        let _ = sqlx::query("UPDATE projects SET last_poll_at = $1 WHERE id = $2")
            .bind(now())
            .bind(p.id)
            .execute(&self.db)
            .await;

        let requested_sha: Option<String> =
            sqlx::query_scalar::<_, Option<String>>("SELECT sha FROM deployments WHERE id = $1")
                .bind(dep_id)
                .fetch_optional(&self.db)
                .await
                .ok()
                .flatten()
                .flatten()
                .filter(|s| !s.trim().is_empty());

        let head = match requested_sha {
            Some(sha) => sha,
            None => match Git::resolve_head(&repo_dir) {
                Ok(h) => h,
                Err(e) => {
                    self.finish_deployment(dep_id, "failed", Some(&e)).await;
                    return;
                }
            },
        };

        // Stage 2: Pulling
        self.update_stage(dep_id, "pulling").await;
        let is_service = p.project_kind == "service";
        let mut release_dir = Path::new(&p.deploy_dir).join("releases").join(format!("rel-{}", dep_id));
        if let Err(e) = Git::checkout(&repo_dir, &head, &release_dir) {
            self.logs.add(Some(p.id), Some(dep_id), "deployment", "error", &format!("Checkout error: {}", e));
            self.finish_deployment(dep_id, "failed", Some(&e)).await;
            return;
        }

        // Backend services: merge only the changed/tracked files into the persistent `app/` folder.
        // Untracked files (.env, local configs, databases, uploads) are never deleted or overwritten.
        if is_service {
            let app_dir = Path::new(&p.deploy_dir).join("app");
            if let Err(e) = merge_copy_dir(&release_dir, &app_dir) {
                self.logs.add(Some(p.id), Some(dep_id), "deployment", "error", &format!("Merge error: {}", e));
                self.finish_deployment(dep_id, "failed", Some(&e)).await;
                return;
            }
            let _ = tokio::fs::remove_dir_all(&release_dir).await;
            self.logs.add(
                Some(p.id),
                Some(dep_id),
                "deployment",
                "info",
                "Merged repository files into live service folder (existing .env and untracked files preserved)",
            );
            release_dir = app_dir;
        }

        let commit_info = Git::get_commit_info(&repo_dir, &head).unwrap_or(CommitInfo {
            sha: head.clone(),
            short_sha: head[..7.min(head.len())].to_string(),
            author: "System".to_string(),
            date: now(),
            message: "Deploy".to_string(),
        });

        // Insert release record
        let release_id: i64 = sqlx::query_scalar(
            "INSERT INTO releases(project_id, deployment_id, sha, short_sha, message, author, committed_at, dir, status, created_at) \
             VALUES($1, $2, $3, $4, $5, $6, $7, $8, 'pulled', $9) RETURNING id",
        )
        .bind(p.id)
        .bind(dep_id)
        .bind(&head)
        .bind(&commit_info.short_sha)
        .bind(&commit_info.message)
        .bind(&commit_info.author)
        .bind(commit_info.date)
        .bind(release_dir.to_str().unwrap_or(""))
        .bind(now())
        .fetch_one(&self.db)
        .await
        .unwrap_or(0);

        let _ = sqlx::query("UPDATE deployments SET release_id = $1, sha = $2 WHERE id = $3")
            .bind(if release_id > 0 { Some(release_id) } else { None })
            .bind(&head)
            .bind(dep_id)
            .execute(&self.db)
            .await;

        // Link release `data/` folder to isolated container DB and write container-isolated .env
        if !is_service {
            self.runtime.link_work_dir_to_container(&release_dir, &container);
            let env_map = self.projects.get_env_map(p.id).await;
            let (_isolated_env, dotenv_body) = self.runtime.build_container_env(&p, &env_map, &container);
            let _ = tokio::fs::write(release_dir.join(".env"), dotenv_body).await;
        }

        // Stage 3: Installing dependencies
        let has_pkg_json = release_dir.join("package.json").exists();
        let effective_install_cmd = if !p.install_cmd.trim().is_empty() {
            p.install_cmd.clone()
        } else if has_pkg_json {
            "npm install".to_string()
        } else {
            String::new()
        };

        if !effective_install_cmd.trim().is_empty() {
            self.update_stage(dep_id, "installing").await;
            self.logs.add(Some(p.id), Some(dep_id), "build", "info", &format!("Running install: {}", effective_install_cmd));
            if let Err(e) = run_shell_cmd(&effective_install_cmd, &release_dir, &self.logs, p.id, dep_id).await {
                if effective_install_cmd.contains("npm ci") && has_pkg_json {
                    self.logs.add(Some(p.id), Some(dep_id), "build", "warn", "npm ci failed, falling back to npm install...");
                    if let Err(e2) = run_shell_cmd("npm install", &release_dir, &self.logs, p.id, dep_id).await {
                        self.logs.add(Some(p.id), Some(dep_id), "build", "error", &format!("Install failed: {}", e2));
                        self.finish_deployment(dep_id, "failed", Some(&e2)).await;
                        return;
                    }
                } else if effective_install_cmd.contains("pip") && (e.contains("externally-managed-environment") || e.contains("break-system-packages") || e.contains("uninstall-no-record-file") || e.contains("RECORD")) {
                    self.logs.add(Some(p.id), Some(dep_id), "build", "warn", "Pip hit Debian system package conflict or PEP 668. Retrying with --break-system-packages --ignore-installed...");
                    let retry_cmd = format!("{} --break-system-packages --ignore-installed", effective_install_cmd);
                    if let Err(e2) = run_shell_cmd(&retry_cmd, &release_dir, &self.logs, p.id, dep_id).await {
                        self.logs.add(Some(p.id), Some(dep_id), "build", "error", &format!("Install failed: {}", e2));
                        self.finish_deployment(dep_id, "failed", Some(&e2)).await;
                        return;
                    }
                } else {
                    self.logs.add(Some(p.id), Some(dep_id), "build", "error", &format!("Install failed: {}", e));
                    self.finish_deployment(dep_id, "failed", Some(&e)).await;
                    return;
                }
            }
        }

        // Stage 4: Building (auto-detect 'npm run build' if build_cmd is empty but package.json has a build script)
        let effective_build_cmd = if !p.build_cmd.trim().is_empty() {
            p.build_cmd.clone()
        } else {
            let pkg_path = release_dir.join("package.json");
            let mut detected = String::new();
            if let Ok(pkg_txt) = tokio::fs::read_to_string(&pkg_path).await {
                if pkg_txt.contains("\"build\"") {
                    detected = "npm run build".to_string();
                }
            }
            detected
        };

        if !effective_build_cmd.trim().is_empty() {
            self.update_stage(dep_id, "building").await;
            self.logs.add(Some(p.id), Some(dep_id), "build", "info", &format!("Running build: {}", effective_build_cmd));
            if let Err(e) = run_shell_cmd(&effective_build_cmd, &release_dir, &self.logs, p.id, dep_id).await {
                self.fail_deployment_step(dep_id, p.id, "build", &format!("Build failed: {}", e)).await;
                return;
            }
        }

        // Stage 5: Deploying (Atomic Symlink Switch)
        self.update_stage(dep_id, "deploying").await;
        let current_symlink = Path::new(&p.deploy_dir).join("current");
        switch_current(&current_symlink, &release_dir);

        // Stage 6: Health Checking
        if self.runtime.get_stop_seq(p.id) != start_stop_seq {
            self.logs.add(
                Some(p.id),
                Some(dep_id),
                "deployment",
                "warn",
                "Website was stopped by user during deployment — release built and linked without starting process.",
            );
            let ts = now();
            let _ = sqlx::query(
                "UPDATE releases SET status = 'succeeded', build_status = 'success', health_status = 'ok', deployed_at = $1 WHERE id = $2",
            )
            .bind(ts)
            .bind(release_id)
            .execute(&self.db)
            .await;
            let _ = sqlx::query(
                "UPDATE projects SET current_release_id = $1, deployed_sha = $2, updated_at = $3 WHERE id = $4",
            )
            .bind(release_id)
            .bind(&head)
            .bind(ts)
            .bind(p.id)
            .execute(&self.db)
            .await;
            self.finish_deployment(dep_id, "success", None).await;
            return;
        }

        self.update_stage(dep_id, "health_checking").await;
        let env_map = self.projects.get_env_map(p.id).await;
        if let Err(e) = self.runtime.start_project(&p, env_map, Some(&release_dir)).await {
            self.fail_deployment_step(dep_id, p.id, "app", &format!("Start failed: {}", e)).await;
            return;
        }

        if !p.health_url.trim().is_empty() {
            let port_str = p.port.unwrap_or(3000).to_string();
            let resolved_health_url = p.health_url.replace("{{PORT}}", &port_str);
            self.logs.add(Some(p.id), Some(dep_id), "health", "info", &format!("Probing health check URL: {}", resolved_health_url));
            if let Err(e) = self.runtime.health_check(&resolved_health_url, p.health_timeout_s as u64).await {
                self.fail_deployment_step(dep_id, p.id, "health", &format!("Health check failed: {}", e)).await;
                return;
            }
        }

        // Stability window
        if p.stability_s > 0 {
            sleep(Duration::from_secs(p.stability_s as u64)).await;
        }

        // Mark success & record backup snapshot in PostgreSQL
        let ts = now();
        let _ = sqlx::query(
            "UPDATE releases SET status = 'succeeded', build_status = 'success', health_status = 'ok', deployed_at = $1 WHERE id = $2",
        )
        .bind(ts)
        .bind(release_id)
        .execute(&self.db)
        .await;
        let _ = sqlx::query(
            "UPDATE projects SET current_release_id = $1, deployed_sha = $2, updated_at = $3 WHERE id = $4",
        )
        .bind(release_id)
        .bind(&head)
        .bind(ts)
        .bind(p.id)
        .execute(&self.db)
        .await;
        let _ = sqlx::query(
            "INSERT INTO backups(project_id, release_id, path, size_bytes, note, created_at) VALUES($1, $2, $3, $4, $5, $6)",
        )
        .bind(p.id)
        .bind(release_id)
        .bind(release_dir.to_str().unwrap_or(""))
        .bind(5 * 1024 * 1024i64)
        .bind(format!("Auto-snapshot for Release #{} ({})", release_id, commit_info.short_sha))
        .bind(ts)
        .execute(&self.db)
        .await;

        self.logs.add(Some(p.id), Some(dep_id), "deployment", "info", "Deployment completed successfully! Version is live.");
        self.finish_deployment(dep_id, "success", None).await;
    }

    pub async fn run_rollback(self: Arc<Self>, project_id: i64, dep_id: i64, target_release_id: i64) {
        {
            let mut spawned = self.spawned_deployments.lock().unwrap();
            if !spawned.insert(dep_id) {
                return;
            }
        }

        let permit = match self.acquire_queue_permit(project_id, dep_id, "rollback").await {
            Some(p) => p,
            None => return,
        };

        let p = match self.projects.get(project_id).await {
            Ok(Some(p)) => p,
            _ => {
                self.finish_deployment(dep_id, "failed", Some("Project not found")).await;
                drop(permit);
                return;
            }
        };

        let releases = self.list_releases(project_id).await;
        let target = match releases.into_iter().find(|r| r.id == target_release_id) {
            Some(r) => Some(r),
            None => self
                .list_releases(project_id)
                .await
                .into_iter()
                .find(|r| r.status == "succeeded"),
        };

        let target = match target {
            Some(r) => r,
            None => {
                self.execute_deploy_inner(project_id, dep_id).await;
                drop(permit);
                return;
            }
        };

        self.logs.add(
            Some(p.id),
            Some(dep_id),
            "rollback",
            "info",
            &format!("Rolling back {} to Release #{} ({})", p.name, target.id, target.short_sha),
        );

        let _ = sqlx::query(
            "UPDATE deployments SET release_id = $1, from_release_id = $2, sha = $3 WHERE id = $4",
        )
        .bind(target.id)
        .bind(p.current_release_id)
        .bind(&target.sha)
        .bind(dep_id)
        .execute(&self.db)
        .await;

        self.update_stage(dep_id, "deploying").await;
        let release_dir = PathBuf::from(&target.dir);
        if !release_dir.exists() {
            self.execute_deploy_inner(project_id, dep_id).await;
            drop(permit);
            return;
        }

        let current_symlink = Path::new(&p.deploy_dir).join("current");
        switch_current(&current_symlink, &release_dir);

        self.update_stage(dep_id, "health_checking").await;
        let env_map = self.projects.get_env_map(p.id).await;
        if let Err(e) = self.runtime.start_project(&p, env_map, Some(&release_dir)).await {
            self.fail_deployment_step(dep_id, p.id, "rollback", &format!("Start failed: {}", e)).await;
            drop(permit);
            return;
        }

        let ts = now();
        let _ = sqlx::query(
            "UPDATE projects SET current_release_id = $1, deployed_sha = $2, updated_at = $3 WHERE id = $4",
        )
        .bind(target.id)
        .bind(&target.sha)
        .bind(ts)
        .bind(p.id)
        .execute(&self.db)
        .await;

        self.logs.add(Some(p.id), Some(dep_id), "rollback", "info", &format!("Rollback to Release #{} succeeded!", target.id));
        self.finish_deployment(dep_id, "success", None).await;
        drop(permit);
    }
}
