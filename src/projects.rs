use sqlx::Row;
use std::sync::Arc;

use crate::db::{now, DbPool};
use crate::vault::Vault;

pub mod models;
pub use models::*;

pub struct Projects {
    db: DbPool,
    vault: Arc<Vault>,
}

impl Projects {
    pub fn new(db: DbPool, vault: Arc<Vault>) -> Self {
        Self { db, vault }
    }

    async fn ensure_project_port(&self, p: &mut Project) {
        if p.port.is_none() || p.port == Some(0) {
            let default_start = if p.project_kind == "service" { 8000 } else { 3000 };
            if let Ok(new_port) = crate::db::allocate_global_unique_port(&self.db, None, default_start, Some(p.id), None).await {
                let _ = sqlx::query("UPDATE projects SET port = $1 WHERE id = $2")
                    .bind(new_port as i32)
                    .bind(p.id)
                    .execute(&self.db)
                    .await;
                p.port = Some(new_port);
            }
        }
    }

    pub async fn list(&self) -> Result<Vec<Project>, sqlx::Error> {
        let query = format!("SELECT {} FROM projects ORDER BY id DESC", SELECT_PROJECT_FIELDS);
        let rows = sqlx::query(&query).fetch_all(&self.db).await?;

        let mut list = Vec::with_capacity(rows.len());
        for row in rows {
            let mut p = pg_row_to_project(&row)?;
            self.ensure_project_port(&mut p).await;
            list.push(p);
        }
        Ok(list)
    }

    pub async fn get(&self, id: i64) -> Result<Option<Project>, sqlx::Error> {
        let query = format!("SELECT {} FROM projects WHERE id = $1", SELECT_PROJECT_FIELDS);
        let row = sqlx::query(&query).bind(id).fetch_optional(&self.db).await?;

        if let Some(r) = row {
            let mut p = pg_row_to_project(&r)?;
            self.ensure_project_port(&mut p).await;
            return Ok(Some(p));
        }
        Ok(None)
    }

    pub async fn get_by_slug(&self, slug: &str) -> Result<Option<Project>, sqlx::Error> {
        let query = format!("SELECT {} FROM projects WHERE slug = $1 LIMIT 1", SELECT_PROJECT_FIELDS);
        let row = sqlx::query(&query).bind(slug).fetch_optional(&self.db).await?;

        if let Some(r) = row {
            let mut p = pg_row_to_project(&r)?;
            self.ensure_project_port(&mut p).await;
            return Ok(Some(p));
        }
        Ok(None)
    }

    pub async fn create(&self, p: &Project) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        let ts = now();
        let domains_json = serde_json::to_string(&p.domains)?;

        let final_port = if let Some(req) = p.port.filter(|&pt| pt > 0) {
            crate::db::allocate_global_unique_port(&self.db, Some(req), req, None, None).await.unwrap_or(req)
        } else {
            let default_start = if p.project_kind == "service" { 8000 } else { 3000 };
            crate::db::allocate_global_unique_port(&self.db, None, default_start, None, None).await.unwrap_or(default_start)
        };

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO projects(
                name, slug, repo_url, branch, project_type, deploy_method, deploy_dir, port,
                install_cmd, build_cmd, start_cmd, health_url, health_timeout_s,
                stability_s, auto_deploy, webhook_secret,
                app_status, domains, ssl_email,
                created_at, updated_at, project_kind, access_mode, allowed_ips
            ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24)
            RETURNING id",
        )
        .bind(&p.name)
        .bind(&p.slug)
        .bind(&p.repo_url)
        .bind(&p.branch)
        .bind(&p.project_type)
        .bind(&p.deploy_method)
        .bind(&p.deploy_dir)
        .bind(final_port as i32)
        .bind(&p.install_cmd)
        .bind(&p.build_cmd)
        .bind(&p.start_cmd)
        .bind(&p.health_url)
        .bind(p.health_timeout_s as i32)
        .bind(p.stability_s as i32)
        .bind(p.auto_deploy)
        .bind(&p.webhook_secret)
        .bind("stopped")
        .bind(domains_json)
        .bind(&p.ssl_email)
        .bind(ts)
        .bind(ts)
        .bind(&p.project_kind)
        .bind(&p.access_mode)
        .bind(&p.allowed_ips)
        .fetch_one(&self.db)
        .await?;

        Ok(id)
    }

    pub async fn update(&self, id: i64, p: &Project) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let ts = now();
        let domains_json = serde_json::to_string(&p.domains)?;

        let final_port = if let Some(req) = p.port.filter(|&pt| pt > 0) {
            crate::db::allocate_global_unique_port(&self.db, Some(req), req, Some(id), None).await.unwrap_or(req)
        } else {
            let default_start = if p.project_kind == "service" { 8000 } else { 3000 };
            crate::db::allocate_global_unique_port(&self.db, None, default_start, Some(id), None).await.unwrap_or(default_start)
        };

        sqlx::query(
            "UPDATE projects SET
                name=$1, repo_url=$2, branch=$3, project_type=$4, deploy_method=$5, deploy_dir=$6, port=$7,
                install_cmd=$8, build_cmd=$9, start_cmd=$10, health_url=$11, health_timeout_s=$12,
                stability_s=$13, auto_deploy=$14,
                domains=$15, ssl_email=$16, updated_at=$17,
                project_kind=$18, access_mode=$19, allowed_ips=$20
             WHERE id=$21",
        )
        .bind(&p.name)
        .bind(&p.repo_url)
        .bind(&p.branch)
        .bind(&p.project_type)
        .bind(&p.deploy_method)
        .bind(&p.deploy_dir)
        .bind(final_port as i32)
        .bind(&p.install_cmd)
        .bind(&p.build_cmd)
        .bind(&p.start_cmd)
        .bind(&p.health_url)
        .bind(p.health_timeout_s as i32)
        .bind(p.stability_s as i32)
        .bind(p.auto_deploy)
        .bind(domains_json)
        .bind(&p.ssl_email)
        .bind(ts)
        .bind(&p.project_kind)
        .bind(&p.access_mode)
        .bind(&p.allowed_ips)
        .bind(id)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    pub async fn delete(&self, id: i64) -> Result<(), sqlx::Error> {
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE projects SET current_release_id = NULL WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("UPDATE deployments SET release_id = NULL, from_release_id = NULL WHERE project_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("DELETE FROM logs WHERE project_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("DELETE FROM backups WHERE project_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("DELETE FROM deployments WHERE project_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("DELETE FROM releases WHERE project_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("DELETE FROM project_env WHERE project_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("DELETE FROM project_keys WHERE project_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("DELETE FROM audit WHERE project_id = $1 AND action != 'website.delete'")
            .bind(id)
            .execute(&mut *tx)
            .await
            .ok();
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        crate::db::purge_orphaned_project_data(&self.db).await;
        Ok(())
    }

    pub async fn get_env_vars(&self, project_id: i64) -> Result<Vec<EnvVarView>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT key, value_enc, is_secret FROM project_env WHERE project_id = $1 ORDER BY key ASC",
        )
        .bind(project_id)
        .fetch_all(&self.db)
        .await?;

        let mut res = Vec::with_capacity(rows.len());
        for r in rows {
            let key: String = r.try_get(0)?;
            let val_enc: String = r.try_get(1)?;
            let is_secret: bool = r.try_get(2)?;
            let decrypted = self.vault.decrypt(&val_enc).unwrap_or_default();
            let value = if is_secret {
                "********".to_string()
            } else {
                decrypted.clone()
            };
            res.push(EnvVarView {
                key,
                value,
                real_value: decrypted,
                is_secret,
            });
        }
        Ok(res)
    }

    pub async fn set_env_var(
        &self,
        project_id: i64,
        key: &str,
        value: &str,
        is_secret: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.set_env_var_with_rename(project_id, key, value, is_secret, None).await
    }

    pub async fn set_env_var_with_rename(
        &self,
        project_id: i64,
        key: &str,
        value: &str,
        is_secret: bool,
        previous_key: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let clean_key = key.trim();
        if clean_key.is_empty() {
            return Err("Variable key cannot be empty".into());
        }
        let enc = self.vault.encrypt(value)?;
        let mut tx = self.db.begin().await?;
        if let Some(prev) = previous_key.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if prev != clean_key {
                sqlx::query("DELETE FROM project_env WHERE project_id = $1 AND key = $2")
                    .bind(project_id)
                    .bind(prev)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        sqlx::query(
            "INSERT INTO project_env(project_id, key, value_enc, is_secret) VALUES($1, $2, $3, $4)
             ON CONFLICT(project_id, key) DO UPDATE SET value_enc=EXCLUDED.value_enc, is_secret=EXCLUDED.is_secret",
        )
        .bind(project_id)
        .bind(clean_key)
        .bind(enc)
        .bind(is_secret)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_env_var(&self, project_id: i64, key: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM project_env WHERE project_id = $1 AND key = $2")
            .bind(project_id)
            .bind(key)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn reveal_env_var(&self, project_id: i64, key: &str) -> Option<String> {
        let val_enc: String = sqlx::query_scalar(
            "SELECT value_enc FROM project_env WHERE project_id = $1 AND key = $2",
        )
        .bind(project_id)
        .bind(key)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten()?;

        self.vault.decrypt(&val_enc).ok()
    }

    pub async fn get_env_map(&self, project_id: i64) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        let Ok(rows) = sqlx::query("SELECT key, value_enc FROM project_env WHERE project_id = $1")
            .bind(project_id)
            .fetch_all(&self.db)
            .await
        else {
            return map;
        };

        for row in rows {
            if let (Ok(k), Ok(enc)) = (row.try_get::<String, _>(0), row.try_get::<String, _>(1)) {
                if let Ok(dec) = self.vault.decrypt(&enc) {
                    map.insert(k, dec);
                }
            }
        }
        map
    }

    pub async fn set_bulk_env(
        &self,
        project_id: i64,
        raw_or_vars: &str,
        is_secret: bool,
        replace_all: bool,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let pairs = parse_dotenv(raw_or_vars);
        if pairs.is_empty() && !replace_all {
            return Ok(0);
        }

        let mut tx = self.db.begin().await?;
        if replace_all {
            sqlx::query("DELETE FROM project_env WHERE project_id = $1")
                .bind(project_id)
                .execute(&mut *tx)
                .await?;
        }

        let mut count = 0;
        for (k, v) in &pairs {
            let enc = self.vault.encrypt(v)?;
            sqlx::query(
                "INSERT INTO project_env(project_id, key, value_enc, is_secret) VALUES($1, $2, $3, $4)
                 ON CONFLICT(project_id, key) DO UPDATE SET value_enc=EXCLUDED.value_enc, is_secret=EXCLUDED.is_secret",
            )
            .bind(project_id)
            .bind(k)
            .bind(enc)
            .bind(is_secret)
            .execute(&mut *tx)
            .await?;
            count += 1;
        }

        tx.commit().await?;
        Ok(count)
    }

    pub async fn set_batch_env_items(
        &self,
        project_id: i64,
        items: &[EnvVarInput],
        replace_all: bool,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let mut tx = self.db.begin().await?;
        if replace_all {
            sqlx::query("DELETE FROM project_env WHERE project_id = $1")
                .bind(project_id)
                .execute(&mut *tx)
                .await?;
        }

        let mut count = 0;
        for item in items {
            let k = item.key.trim();
            if k.is_empty() {
                continue;
            }
            let enc = self.vault.encrypt(&item.value)?;
            sqlx::query(
                "INSERT INTO project_env(project_id, key, value_enc, is_secret) VALUES($1, $2, $3, $4)
                 ON CONFLICT(project_id, key) DO UPDATE SET value_enc=EXCLUDED.value_enc, is_secret=EXCLUDED.is_secret",
            )
            .bind(project_id)
            .bind(k)
            .bind(enc)
            .bind(item.is_secret)
            .execute(&mut *tx)
            .await?;
            count += 1;
        }

        tx.commit().await?;
        Ok(count)
    }

    pub async fn export_raw_env(&self, project_id: i64) -> Result<String, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT key, value_enc FROM project_env WHERE project_id = $1 ORDER BY key ASC",
        )
        .bind(project_id)
        .fetch_all(&self.db)
        .await?;

        let mut out = String::new();
        for r in rows {
            let k: String = r.try_get(0)?;
            let enc: String = r.try_get(1)?;
            let v = self.vault.decrypt(&enc).unwrap_or_default();
            if v.contains(' ') || v.contains('\n') || v.contains('"') {
                out.push_str(&format!("{}=\"{}\"\n", k, v.replace('"', "\\\"")));
            } else {
                out.push_str(&format!("{}={}\n", k, v));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dotenv() {
        let sample = r#"
        # Comment line
        PORT=3000
        APP_URL="http://localhost:3000"
        export JWT_SECRET='super-secret-key-123'
        DEBUG=true # inline comment
        SPACED_KEY = value with spaces
        EMPTY_LINE_BEFORE=123
        "#;

        let parsed = parse_dotenv(sample);
        assert_eq!(parsed.len(), 6);
        assert_eq!(parsed[0], ("PORT".into(), "3000".into()));
        assert_eq!(parsed[1], ("APP_URL".into(), "http://localhost:3000".into()));
        assert_eq!(parsed[2], ("JWT_SECRET".into(), "super-secret-key-123".into()));
        assert_eq!(parsed[3], ("DEBUG".into(), "true".into()));
        assert_eq!(parsed[4], ("SPACED_KEY".into(), "value with spaces".into()));
        assert_eq!(parsed[5], ("EMPTY_LINE_BEFORE".into(), "123".into()));
    }

    #[test]
    fn test_project_kind_service_and_default() {
        let json_default = r#"{
            "name": "My Web App",
            "slug": "my-web-app",
            "repo_url": "https://github.com/org/repo"
        }"#;
        let proj_web: Project = serde_json::from_str(json_default).unwrap();
        assert_eq!(proj_web.project_kind, "web");

        let json_service = r#"{
            "name": "Discord Music Bot",
            "slug": "discord-music-bot",
            "repo_url": "https://github.com/org/bot",
            "project_kind": "service",
            "project_type": "discord-node",
            "start_cmd": "node index.js"
        }"#;
        let proj_service: Project = serde_json::from_str(json_service).unwrap();
        assert_eq!(proj_service.project_kind, "service");
        assert_eq!(proj_service.project_type, "discord-node");
        assert_eq!(proj_service.start_cmd, "node index.js");
    }

    #[tokio::test]
    async fn test_project_db_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!("webdep-test-{}", now()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let pool = crate::pg_control::test_db::isolated_pool().await;
        let vault = Arc::new(crate::vault::Vault::new(&temp_dir).unwrap());
        let projects = Projects::new(pool, vault);

        let json_service = r#"{
            "name": "Discord Music Bot",
            "slug": "discord-music-bot",
            "repo_url": "https://github.com/org/bot",
            "project_kind": "service",
            "project_type": "discord-node",
            "start_cmd": "node index.js"
        }"#;
        let mut proj: Project = serde_json::from_str(json_service).unwrap();
        proj.deploy_dir = temp_dir.to_string_lossy().to_string();
        let id = projects.create(&proj).await.unwrap();
        let fetched = projects.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.project_kind, "service");
        assert_eq!(fetched.name, "Discord Music Bot");
        assert!(fetched.port.is_some() && fetched.port.unwrap() >= 8000);

        let list = projects.list().await.unwrap();
        assert!(list.iter().any(|p| p.id == id && p.project_kind == "service"));

        std::fs::remove_dir_all(&temp_dir).ok();
    }
}
