use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::projects::Project;
use crate::runtime::Runtime;

pub struct Caddy {
    caddy_dir: PathBuf,
}

impl Caddy {
    pub fn new(data_dir: &Path) -> Self {
        let caddy_dir = data_dir.join("caddy");
        fs::create_dir_all(&caddy_dir).ok();
        Self { caddy_dir }
    }

    fn is_safe_caddy_token(s: &str) -> bool {
        !s.is_empty()
            && !s.chars().any(|c| {
                c.is_whitespace()
                    || c.is_control()
                    || matches!(c, '{' | '}' | ';' | '#' | '"' | '\'' | '`' | '\\')
            })
    }

    /// Generates Caddyfile snippet for an individual project.
    /// In Caddy, automatic HTTPS via Let's Encrypt / ZeroSSL is 100% automatic for any domain!
    pub fn generate_config(&self, p: &Project) -> String {
        let port = p.port.unwrap_or(3000);
        let safe_slug: String = p
            .slug
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        let fallback_domain = if safe_slug.is_empty() {
            "app.local".to_string()
        } else {
            format!("{}.local", safe_slug)
        };
        let domains: Vec<String> = p
            .domains
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| Self::is_safe_caddy_token(d))
            .collect();
        let domain_str = if domains.is_empty() {
            fallback_domain
        } else {
            domains.join(", ")
        };

        let status_note = if p.app_status == "maintenance" {
            "# Status: Maintenance Page Responder active on upstream port"
        } else {
            "# Status: Live application upstream"
        };

        let safe_name = p.name.replace(['\r', '\n'], " ");
        let mut config = format!(
            "# WebDep Auto-Generated Caddyfile\n# Project: {name} ({slug})\n# Automatic HTTPS, HTTP/3, and reverse proxy\n{status_note}\n\n{domains} {{\n    reverse_proxy 127.0.0.1:{port}\n",
            name = safe_name,
            slug = safe_slug,
            status_note = status_note,
            domains = domain_str,
            port = port
        );

        let ssl_email = p.ssl_email.trim();
        if !ssl_email.is_empty() && Self::is_safe_caddy_token(ssl_email) {
            config.push_str(&format!("    tls {}\n", ssl_email));
        }

        config.push_str("}\n");
        config
    }

    /// Generates the master unified Caddyfile for all active websites.
    pub fn generate_master_caddyfile(&self, projects: &[Project]) -> String {
        let mut master = String::from(
            "# WebDep — Master Multi-Website Caddyfile\n# Generated automatically for all hosted projects\n\n{\n    # Global options\n    auto_https on\n}\n\n",
        );

        for p in projects {
            if p.project_kind == "service" && (p.domains.is_empty() || p.port.is_none()) {
                continue;
            }
            master.push_str(&self.generate_config(p));
            master.push('\n');
        }

        master
    }

    pub fn generate_staging_http_config(&self, p: &Project) -> String {
        let port = p.port.unwrap_or(3000);
        let safe_slug: String = p
            .slug
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        let fallback_domain = if safe_slug.is_empty() {
            "http://app.local".to_string()
        } else {
            format!("http://{}.local", safe_slug)
        };
        let domains: Vec<String> = p
            .domains
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| Self::is_safe_caddy_token(d))
            .map(|d| {
                if d.starts_with("http://") || d.starts_with("https://") {
                    d
                } else {
                    format!("http://{d}")
                }
            })
            .collect();
        let domain_str = if domains.is_empty() {
            fallback_domain
        } else {
            domains.join(", ")
        };

        let safe_name = p.name.replace(['\r', '\n'], " ");
        format!(
            "# WebDep Stage-1 Pre-Warm HTTP Caddyfile (ACME deferred until DNS cutover)\n# Project: {name} ({slug})\n\n{domains} {{\n    reverse_proxy 127.0.0.1:{port}\n}}\n",
            name = safe_name,
            slug = safe_slug,
            domains = domain_str,
            port = port
        )
    }

    pub fn write_staging_http_config(&self, p: &Project, all_projects: &[Project]) -> Result<PathBuf, std::io::Error> {
        let conf = self.generate_staging_http_config(p);
        let path = self.caddy_dir.join(format!("{}.caddy", p.slug));
        fs::write(&path, conf)?;

        let mut master = String::from(
            "# WebDep — Master Multi-Website Caddyfile\n# Generated automatically for all hosted projects\n\n{\n    # Global options\n    auto_https on\n}\n\n",
        );
        for item in all_projects {
            if item.id == p.id {
                master.push_str(&self.generate_staging_http_config(p));
                master.push('\n');
            } else {
                if item.project_kind == "service" && (item.domains.is_empty() || item.port.is_none()) {
                    continue;
                }
                master.push_str(&self.generate_config(item));
                master.push('\n');
            }
        }
        let master_path = self.caddy_dir.join("Caddyfile");
        fs::write(&master_path, master)?;
        Ok(path)
    }

    pub fn write_config(&self, p: &Project, all_projects: &[Project]) -> Result<PathBuf, std::io::Error> {
        // Write static maintenance HTML snapshot for reference/fallback
        let maint_html = Runtime::render_maintenance_html(
            &p.name,
            &p.slug,
            "Scheduled Maintenance • Developers Working",
        );
        let maint_path = self.caddy_dir.join(format!("{}-maintenance.html", p.slug));
        let _ = fs::write(&maint_path, maint_html);

        let conf = self.generate_config(p);
        let path = self.caddy_dir.join(format!("{}.caddy", p.slug));
        fs::write(&path, conf)?;

        // Also update master Caddyfile for all hosted websites
        let master_path = self.caddy_dir.join("Caddyfile");
        fs::write(&master_path, self.generate_master_caddyfile(all_projects))?;

        Ok(path)
    }

    pub fn remove_project_config(&self, slug: &str, remaining_projects: &[Project]) {
        if !slug.trim().is_empty() {
            let _ = fs::remove_file(self.caddy_dir.join(format!("{}.caddy", slug)));
            let _ = fs::remove_file(self.caddy_dir.join(format!("{}-maintenance.html", slug)));
        }
        let master_path = self.caddy_dir.join("Caddyfile");
        let _ = fs::write(&master_path, self.generate_master_caddyfile(remaining_projects));
        let _ = self.reload();
    }

    pub fn reload(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            if Path::new("/etc/caddy/Caddyfile").exists() {
                if let Ok(o) = Command::new("sudo")
                    .args(["-n", "systemctl", "reload", "caddy"])
                    .output()
                {
                    if o.status.success() {
                        return Ok(());
                    }
                }
            }
        }

        let master_path = self.caddy_dir.join("Caddyfile");

        let out = Command::new("caddy")
            .args(["reload", "--config", master_path.to_str().unwrap_or("Caddyfile")])
            .output();

        match out {
            Ok(o) => {
                if o.status.success() {
                    Ok(())
                } else {
                    let err = String::from_utf8_lossy(&o.stderr);
                    Err(err.to_string())
                }
            }
            Err(_) => {
                // Graceful note when caddy is not installed locally
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_project() -> Project {
        Project {
            id: 10,
            name: "Shift Demo".to_string(),
            slug: "shift-demo".to_string(),
            repo_url: "https://github.com/org/repo".to_string(),
            branch: "main".to_string(),
            project_type: "node".to_string(),
            deploy_method: "managed".to_string(),
            deploy_dir: "/var/webdep/sites/shift-demo".to_string(),
            port: Some(43012),
            install_cmd: String::new(),
            build_cmd: String::new(),
            start_cmd: "node server.js".to_string(),
            health_url: String::new(),
            health_timeout_s: 30,
            stability_s: 0,
            auto_deploy: false,
            webhook_secret: None,
            current_release_id: None,
            deployed_sha: None,
            last_poll_at: None,
            app_status: "running".to_string(),
            app_pid: None,
            app_started_at: None,
            domains: vec!["app.example.com".to_string(), "www.example.com".to_string()],
            ssl_email: "ops@example.com".to_string(),
            created_at: 0,
            updated_at: 0,
            project_kind: "website".to_string(),
            access_mode: "public".to_string(),
            allowed_ips: String::new(),
            node_id: Some(2),
            placement_version: 1,
            assigned_port: Some(43012),
        }
    }

    #[test]
    fn staging_http_config_avoids_acme_and_https_config_enables_tls() {
        let dir = std::env::temp_dir().join("webdep-caddy-stage-test");
        let caddy = Caddy::new(&dir);
        let p = sample_project();

        let stage1 = caddy.generate_staging_http_config(&p);
        assert!(stage1.contains("http://app.example.com, http://www.example.com {"));
        assert!(stage1.contains("reverse_proxy 127.0.0.1:43012"));
        assert!(!stage1.contains("tls ops@example.com"));

        let stage2 = caddy.generate_config(&p);
        assert!(stage2.contains("app.example.com, www.example.com {"));
        assert!(stage2.contains("tls ops@example.com"));

        caddy.write_staging_http_config(&p, std::slice::from_ref(&p)).unwrap();
        let disk = fs::read_to_string(dir.join("caddy/shift-demo.caddy")).unwrap();
        assert!(disk.contains("http://app.example.com"));
        let _ = fs::remove_dir_all(&dir);
    }
}

