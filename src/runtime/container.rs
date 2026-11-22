use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::projects::Project;
use super::Runtime;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct WebsiteContainer {
    pub container_id: String,
    pub project_id: i64,
    pub slug: String,
    pub port: u16,
    pub root_dir: PathBuf,
    pub db_dir: PathBuf,
    pub storage_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub home_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub run_dir: PathBuf,
}

fn copy_dir_files_if_missing(src_dir: &Path, dst_dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(src_dir) {
        for entry in entries.flatten() {
            let src = entry.path();
            if src.is_file() {
                let dst = dst_dir.join(entry.file_name());
                if !dst.exists() {
                    let _ = std::fs::copy(&src, &dst);
                }
            }
        }
    }
}



pub fn resolve_data_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Ok(val) = std::env::var("DATA_DIR") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            let p = Path::new(trimmed);
            if p.is_absolute() {
                return p.to_path_buf();
            }
            let clean = trimmed.trim_start_matches("./").trim_start_matches(".\\");
            return cwd.join(clean);
        }
    }
    cwd.join("data")
}

pub fn ensure_container_with_port(p: &Project, port: u16) -> WebsiteContainer {
    let data_dir = resolve_data_dir();
    ensure_container_in_data_dir(&data_dir, p, port)
}

pub fn ensure_container_in_data_dir(data_dir: &Path, p: &Project, port: u16) -> WebsiteContainer {
    let safe_slug = if p.slug.trim().is_empty() {
        format!("site-{}", p.id)
    } else {
        p.slug.trim().to_string()
    };

    let root_dir = data_dir.join("containers").join(&safe_slug);
    let db_dir = root_dir.join("db");
    let storage_dir = root_dir.join("storage");
    let tmp_dir = root_dir.join("tmp");
    let home_dir = root_dir.join("home");
    let cache_dir = root_dir.join("cache");
    let run_dir = root_dir.join("run");

    let _ = std::fs::create_dir_all(&db_dir);
    let _ = std::fs::create_dir_all(&storage_dir);
    let _ = std::fs::create_dir_all(&tmp_dir);
    let _ = std::fs::create_dir_all(&home_dir);
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::create_dir_all(&run_dir);

    let legacy_app_data = data_dir.join("app-data").join(&safe_slug);
    if legacy_app_data.is_dir() {
        copy_dir_files_if_missing(&legacy_app_data, &db_dir);
    }

    let container = WebsiteContainer {
        container_id: format!("webdep-ctr-{}-{}", p.id, safe_slug),
        project_id: p.id,
        slug: safe_slug,
        port,
        root_dir: root_dir.clone(),
        db_dir,
        storage_dir,
        tmp_dir,
        home_dir,
        cache_dir,
        run_dir,
    };

    let manifest_path = root_dir.join("container.json");
    if let Ok(json_str) = serde_json::to_string_pretty(&container) {
        let _ = std::fs::write(manifest_path, json_str);
    }

    container
}

fn clear_readonly_recursive(path: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return;
        }
        let mut perms = meta.permissions();
        if perms.readonly() {
            // On Unix `set_readonly(false)` grants write to group and others too; only restore the owner's.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                perms.set_mode(perms.mode() | 0o200);
            }
            // Windows has no group/other bits: this only clears the file's read-only attribute.
            #[cfg(not(unix))]
            perms.set_readonly(false); // NOSONAR
            let _ = std::fs::set_permissions(path, perms);
        }
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    clear_readonly_recursive(&entry.path());
                }
            }
        }
    }
}

fn force_remove_dir_all(dir: &Path) -> Result<bool, String> {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return Ok(false);
    };
    if meta.file_type().is_symlink() {
        #[cfg(windows)]
        {
            if std::fs::remove_dir(dir).is_ok() || std::fs::remove_file(dir).is_ok() {
                return Ok(true);
            }
        }
        #[cfg(unix)]
        {
            if std::fs::remove_file(dir).is_ok() {
                return Ok(true);
            }
        }
    }

    #[cfg(windows)]
    {
        for link_name in ["current", "app"] {
            let link = dir.join(link_name);
            let cur_str = link.to_string_lossy().trim_start_matches(r"\\?\").to_string();
            let _ = std::process::Command::new("cmd")
                .args(["/C", "rmdir", &cur_str])
                .output();
            if let Ok(lmeta) = std::fs::symlink_metadata(&link) {
                if lmeta.is_dir() {
                    let data_link = link.join("data");
                    let d_str = data_link.to_string_lossy().trim_start_matches(r"\\?\").to_string();
                    let _ = std::process::Command::new("cmd")
                        .args(["/C", "rmdir", &d_str])
                        .output();
                }
            }
        }
    }

    if let Err(first_err) = std::fs::remove_dir_all(dir) {
        clear_readonly_recursive(dir);
        if let Err(second_err) = std::fs::remove_dir_all(dir) {
            return Err(format!(
                "Failed removing '{}' ({}; retry: {})",
                dir.display(),
                first_err,
                second_err
            ));
        }
    }
    Ok(true)
}

pub fn cleanup_project_files_dirs(p: &Project, remaining_projects: &[Project]) {
    let data_dir = resolve_data_dir();
    cleanup_project_files_dirs_in_data_dir(&data_dir, p, remaining_projects);
}

pub fn cleanup_project_files_dirs_in_data_dir(
    data_dir: &Path,
    p: &Project,
    remaining_projects: &[Project],
) {
    let _ = cleanup_project_files_dirs_in_data_dir_checked(data_dir, p, remaining_projects);
}

pub fn cleanup_project_files_dirs_in_data_dir_checked(
    data_dir: &Path,
    p: &Project,
    remaining_projects: &[Project],
) -> Result<usize, String> {
    let raw_slug = p.slug.trim().to_string();
    let clean_slug: String = raw_slug
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();

    let key_dir = data_dir.join("keys");
    let _ = std::fs::remove_file(key_dir.join(format!("deploy_key_{}", p.id)));
    let _ = std::fs::remove_file(key_dir.join(format!("deploy_key_{}.pub", p.id)));

    if raw_slug.is_empty()
        || raw_slug.contains("..")
        || raw_slug.contains('/')
        || raw_slug.contains('\\')
    {
        return Ok(0);
    }

    let slug_shared = remaining_projects.iter().any(|other| {
        if other.id == p.id {
            return false;
        }
        let o_raw = other.slug.trim();
        let o_clean: String = o_raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        o_raw.eq_ignore_ascii_case(&raw_slug)
            || (!clean_slug.is_empty() && o_clean.eq_ignore_ascii_case(&clean_slug))
    });
    if slug_shared {
        return Ok(0);
    }

    let mut dirs_to_remove: Vec<PathBuf> = Vec::new();
    let mut slug_variants = vec![raw_slug.clone()];
    if !clean_slug.is_empty() && clean_slug != raw_slug {
        slug_variants.push(clean_slug.clone());
    }
    for s in &slug_variants {
        dirs_to_remove.push(data_dir.join("containers").join(s));
        dirs_to_remove.push(data_dir.join("app-data").join(s));
        dirs_to_remove.push(data_dir.join("sites").join(s));
        dirs_to_remove.push(data_dir.join("services").join(s));
    }

    let dep_trimmed = p.deploy_dir.trim();
    if !dep_trimmed.is_empty() {
        let dir_shared = remaining_projects
            .iter()
            .any(|other| other.id != p.id && other.deploy_dir.trim() == dep_trimmed);
        if !dir_shared {
            let mut candidate_paths = vec![PathBuf::from(dep_trimmed)];
            let norm = dep_trimmed.replace('\\', "/");
            if let Some(rel) = norm.strip_prefix("./data/").or_else(|| norm.strip_prefix("data/")) {
                candidate_paths.push(data_dir.join(rel));
            }
            for dep_path in candidate_paths {
                let ends_with_slug = dep_path
                    .file_name()
                    .map(|n| {
                        let ns = n.to_string_lossy();
                        ns.eq_ignore_ascii_case(&raw_slug)
                            || (!clean_slug.is_empty() && ns.eq_ignore_ascii_case(&clean_slug))
                    })
                    .unwrap_or(false);
                if ends_with_slug
                    && (dep_path.components().count() >= 3 || dep_path.starts_with(data_dir))
                    && !dirs_to_remove.contains(&dep_path)
                {
                    dirs_to_remove.push(dep_path);
                }
            }
        }
    }

    let mut removed_count = 0usize;
    let mut errors = Vec::new();
    for dir in dirs_to_remove {
        match force_remove_dir_all(&dir) {
            Ok(true) => removed_count += 1,
            Ok(false) => {}
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(removed_count)
    } else {
        Err(errors.join("; "))
    }
}

pub fn link_work_dir_to_container_dir(work_dir: &Path, container: &WebsiteContainer) {
    link_work_dir_to_db_dir(work_dir, &container.db_dir);
}

pub fn link_work_dir_to_db_dir(work_dir: &Path, target_db_dir: &Path) {
    let data_subdir = work_dir.join("data");
    let _ = std::fs::create_dir_all(target_db_dir);

    if let Ok(meta) = std::fs::symlink_metadata(&data_subdir) {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            copy_dir_files_if_missing(&data_subdir, target_db_dir);
            #[cfg(windows)]
            {
                let link_str = data_subdir.to_string_lossy().replace('/', "\\").trim_start_matches(r"\\?\").to_string();
                let _ = std::process::Command::new("cmd")
                    .args(["/C", "rmdir", &link_str])
                    .output();
            }
            let _ = std::fs::remove_dir_all(&data_subdir);
        }
    }

    if !data_subdir.exists() && std::fs::symlink_metadata(&data_subdir).is_err() {
        #[cfg(windows)]
        {
            let link_str = data_subdir.to_string_lossy().replace('/', "\\").trim_start_matches(r"\\?\").to_string();
            let target_str = target_db_dir.to_string_lossy().replace('/', "\\").trim_start_matches(r"\\?\").to_string();
            let out = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J", &link_str, &target_str])
                .output();
            if out.map(|o| !o.status.success()).unwrap_or(true) {
                let _ = std::os::windows::fs::symlink_dir(target_db_dir, &data_subdir);
            }
        }
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(target_db_dir, &data_subdir);
        }
    }
}

/// Default `AUTH_ENCRYPTION_KEY` for an app that does not set its own.
///
/// It used to be SHA-256("webdep-container-auth-key-{id}-{slug}-{port}"), which anyone who knows
/// those three values can recompute. New apps now get a random key stored (mode 600) in their data
/// directory. Apps that already have data keep the legacy derived key so nothing they encrypted
/// becomes unreadable.
fn app_auth_encryption_key(p: &Project, port: u16, container: &WebsiteContainer) -> String {
    let is_hex64 = |k: &str| k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit());
    let key_file = container.db_dir.join(".webdep-auth-key");
    if let Ok(existing) = std::fs::read_to_string(&key_file) {
        let k = existing.trim();
        if is_hex64(k) {
            return k.to_string();
        }
    }
    let legacy = {
        let mut hasher = Sha256::new();
        hasher.update(format!("webdep-container-auth-key-{}-{}-{}", p.id, p.slug, port).as_bytes());
        format!("{:x}", hasher.finalize())
    };
    let has_existing_data = std::fs::read_dir(&container.db_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if has_existing_data {
        return legacy;
    }
    let mut bytes = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
    let fresh = hex::encode(bytes);
    if std::fs::create_dir_all(&container.db_dir).is_err() || std::fs::write(&key_file, &fresh).is_err() {
        // Without a persisted key a random one would change on every start: keep the stable legacy key.
        return legacy;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600));
    }
    fresh
}

/// Write an app's generated `.env` readable by its owner only: it holds the app's secrets.
pub async fn write_app_dotenv(path: &std::path::Path, body: String) {
    if tokio::fs::write(path, body).await.is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
        }
    }
}

pub fn build_container_env_pure(
    p: &Project,
    user_env: &HashMap<String, String>,
    container: &WebsiteContainer,
) -> (HashMap<String, String>, String) {
    let mut final_env = user_env.clone();
    let port = p.port.unwrap_or(container.port);
    let port_str = port.to_string();

    let db_dir_str = container.db_dir.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let storage_dir_str = container.storage_dir.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let tmp_dir_str = container.tmp_dir.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let cache_dir_str = container.cache_dir.to_string_lossy().trim_start_matches(r"\\?\").to_string();

    final_env.insert("PORT".to_string(), port_str.clone());
    if p.project_kind != "service" {
        final_env.entry("SERVER_PORT".to_string()).or_insert(port_str);
    }
    final_env.insert("WEBDEP_CONTAINER_ID".to_string(), container.container_id.clone());
    final_env.insert("WEBDEP_SITE_SLUG".to_string(), container.slug.clone());

    final_env.insert("CERT_DATA_DIR".to_string(), db_dir_str.clone());
    final_env.insert("APP_DATA_DIR".to_string(), db_dir_str.clone());
    final_env.insert("DATA_DIR".to_string(), db_dir_str.clone());
    final_env.insert("DB_DIR".to_string(), db_dir_str.clone());
    final_env.insert("SQLITE_DIR".to_string(), db_dir_str.clone());
    final_env.insert("STORAGE_DIR".to_string(), storage_dir_str.clone());
    final_env.insert("UPLOAD_DIR".to_string(), storage_dir_str);
    final_env.insert("TEMP".to_string(), tmp_dir_str.clone());
    final_env.insert("TMP".to_string(), tmp_dir_str.clone());
    final_env.insert("TMPDIR".to_string(), tmp_dir_str);
    final_env.insert("NPM_CONFIG_CACHE".to_string(), cache_dir_str);

    if let Some(app_url) = final_env.get("APP_URL").cloned() {
        let trimmed = app_url.trim();
        if trimmed.starts_with("http://localhost:") || trimmed.starts_with("http://127.0.0.1:") {
            let host_prefix = if trimmed.starts_with("http://localhost:") {
                "http://localhost"
            } else {
                "http://127.0.0.1"
            };
            final_env.insert("APP_URL".to_string(), format!("{}:{}", host_prefix, port));
        } else if trimmed.is_empty() {
            final_env.insert("APP_URL".to_string(), format!("http://127.0.0.1:{}", port));
        }
    }

    let needs_auth_key = match final_env.get("AUTH_ENCRYPTION_KEY") {
        Some(v) => v.trim().len() != 64 || !v.trim().chars().all(|c| c.is_ascii_hexdigit()),
        None => true,
    };
    if needs_auth_key {
        final_env.insert("AUTH_ENCRYPTION_KEY".to_string(), app_auth_encryption_key(p, port, container));
    }

    let mut dotenv_body = format!(
        "# Auto-generated by WebDep Container Engine ({})\n# Isolated DB: {}\n",
        container.container_id, db_dir_str
    );

    let mut keys: Vec<&String> = final_env.keys().collect();
    keys.sort();
    for k in keys {
        if let Some(v) = final_env.get(k) {
            if v.contains(' ') || v.contains('\n') || v.contains('"') || v.contains('#') {
                dotenv_body.push_str(&format!("{}=\"{}\"\n", k, v.replace('"', "\\\"")));
            } else {
                dotenv_body.push_str(&format!("{}={}\n", k, v));
            }
        }
    }

    (final_env, dotenv_body)
}

pub fn record_container_pid_to(container: &WebsiteContainer, pid: u32) {
    if pid == 0 {
        return;
    }
    let pid_file = container.run_dir.join("pids.txt");
    let mut existing = std::fs::read_to_string(&pid_file).unwrap_or_default();
    existing.push_str(&format!("{}\n", pid));
    let _ = std::fs::write(&pid_file, existing);
}

pub fn read_container_pids_from(container: &WebsiteContainer) -> Vec<u32> {
    let pid_file = container.run_dir.join("pids.txt");
    let content = std::fs::read_to_string(&pid_file).unwrap_or_default();
    content
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|&pid| pid > 0 && pid != std::process::id())
        .collect()
}

pub fn clear_container_pids_in(container: &WebsiteContainer) {
    let pid_file = container.run_dir.join("pids.txt");
    let _ = std::fs::remove_file(pid_file);
}

impl Runtime {
    /// Provision an isolated container directory structure for a website (`data/containers/<slug>/`)
    /// so its database, uploads, temp files, caches, and environment never clash with any other website.
    pub async fn ensure_container(&self, p: &Project) -> WebsiteContainer {
        let port = if let Some(p_port) = p.port.filter(|&pt| pt > 0) {
            p_port
        } else {
            let default_start = if p.project_kind == "service" { 8000 } else { 3000 };
            let alloc = crate::db::allocate_global_unique_port(&self.db, None, default_start, Some(p.id), None)
                .await
                .unwrap_or(default_start);
            let _ = sqlx::query("UPDATE projects SET port = $1 WHERE id = $2")
                .bind(alloc as i32)
                .bind(p.id)
                .execute(&self.db)
                .await;
            alloc
        };
        // Nodes with a port range follow the app's access mode and the port's public flag;
        // nodes without one keep opening the app port as before.
        if p.access_mode == "public" || crate::cluster::ports::should_open_publicly(&self.db, p.node_id, port).await {
            self.allow_firewall_port(port, Some(p.id));
        }
        ensure_container_with_port(p, port)
    }

    /// Delete all on-disk directories, releases, git clones, backups, isolated container DB/cache/storage,
    /// and deploy keys associated with a deleted website, after verifying no other active website shares them.
    pub fn cleanup_project_files(&self, p: &Project, remaining_projects: &[Project]) {
        if let Ok(mut map) = self.processes.lock() {
            map.remove(&p.id);
        }
        if let Ok(mut map) = self.project_locks.lock() {
            map.remove(&p.id);
        }
        if let Ok(mut map) = self.stop_seq.lock() {
            map.remove(&p.id);
        }
        cleanup_project_files_dirs(p, remaining_projects);
    }

    /// Link `<work_dir>/data` to the website's isolated container `db/` folder so relative `./data` paths
    /// inside any Node/Python app stay isolated per container and persist across releases.
    pub fn link_work_dir_to_container(&self, work_dir: &Path, container: &WebsiteContainer) {
        link_work_dir_to_container_dir(work_dir, container);
    }

    /// Build isolated environment variables and `.env` file content for a website container.
    /// Ensures `PORT`, `CERT_DATA_DIR`, `APP_DATA_DIR`, `DATA_DIR`, `STORAGE_DIR`, and `APP_URL`
    /// never clash with another website even if the user copied `.env` from another project.
    pub fn build_container_env(
        &self,
        p: &Project,
        user_env: &HashMap<String, String>,
        container: &WebsiteContainer,
    ) -> (HashMap<String, String>, String) {
        build_container_env_pure(p, user_env, container)
    }
}
