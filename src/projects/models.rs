use serde::{Deserialize, Serialize};

pub fn default_branch() -> String { "main".to_string() }
pub fn default_project_kind() -> String { "web".to_string() }
pub fn default_access_mode() -> String { "local".to_string() }
pub fn default_project_type() -> String { "node".to_string() }
pub fn default_deploy_method() -> String { "managed".to_string() }
pub fn default_app_status() -> String { "stopped".to_string() }
pub fn default_health_timeout_s() -> i64 { 30 }
pub fn default_stability_s() -> i64 { 5 }
pub fn default_true() -> bool { true }

pub fn deserialize_bool_or_int<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrInt {
        Bool(bool),
        Int(i64),
        Str(String),
    }
    match BoolOrInt::deserialize(deserializer)? {
        BoolOrInt::Bool(b) => Ok(b),
        BoolOrInt::Int(i) => Ok(i != 0),
        BoolOrInt::Str(s) => Ok(s == "1" || s.eq_ignore_ascii_case("true")),
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Project {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub repo_url: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default = "default_project_type")]
    pub project_type: String,
    #[serde(default = "default_deploy_method")]
    pub deploy_method: String,
    #[serde(default)]
    pub deploy_dir: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub install_cmd: String,
    #[serde(default)]
    pub build_cmd: String,
    #[serde(default)]
    pub start_cmd: String,
    #[serde(default)]
    pub health_url: String,
    #[serde(default = "default_health_timeout_s")]
    pub health_timeout_s: i64,
    #[serde(default = "default_stability_s")]
    pub stability_s: i64,
    #[serde(default, deserialize_with = "deserialize_bool_or_int")]
    pub auto_deploy: bool,
    #[serde(default, skip_serializing)]
    pub webhook_secret: Option<String>,
    #[serde(default)]
    pub current_release_id: Option<i64>,
    #[serde(default)]
    pub deployed_sha: Option<String>,
    #[serde(default)]
    pub last_poll_at: Option<i64>,
    #[serde(default = "default_app_status")]
    pub app_status: String,
    #[serde(default)]
    pub app_pid: Option<u32>,
    #[serde(default)]
    pub app_started_at: Option<i64>,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub ssl_email: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default = "default_project_kind")]
    pub project_kind: String,
    #[serde(default = "default_access_mode")]
    pub access_mode: String,
    #[serde(default)]
    pub allowed_ips: String,
    #[serde(default)]
    pub node_id: Option<i64>,
    #[serde(default = "default_placement_version")]
    pub placement_version: i64,
    #[serde(default)]
    pub assigned_port: Option<u16>,
    /// Node IP the app's domains are served on; empty = every address.
    #[serde(default)]
    pub bind_ip: String,
    /// CPU limit as a percentage of one core; 0 = no limit.
    #[serde(default)]
    pub cpu_limit: i32,
    /// Memory limit in MB; 0 = no limit.
    #[serde(default)]
    pub memory_limit_mb: i32,
}

pub fn default_placement_version() -> i64 { 1 }

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct EnvVarView {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub real_value: String,
    pub is_secret: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct EnvVarInput {
    pub key: String,
    pub value: String,
    #[serde(default = "default_true")]
    pub is_secret: bool,
}

pub const SELECT_PROJECT_FIELDS: &str = "\
    id, name, slug, repo_url, branch, project_type, deploy_method, deploy_dir, port, \
    install_cmd, build_cmd, start_cmd, health_url, health_timeout_s, \
    stability_s, auto_deploy, webhook_secret, \
    current_release_id, deployed_sha, last_poll_at, app_status, \
    app_pid, app_started_at, domains, ssl_email, \
    created_at, updated_at, project_kind, access_mode, allowed_ips, \
    node_id, placement_version, assigned_port, bind_ip, cpu_limit, memory_limit_mb";

pub fn pg_row_to_project(r: &sqlx::postgres::PgRow) -> Result<Project, sqlx::Error> {
    use sqlx::Row;
    let domains_str: String = r.try_get(23)?;
    let domains: Vec<String> = serde_json::from_str(&domains_str).unwrap_or_default();

    Ok(Project {
        id: r.try_get(0)?,
        name: r.try_get(1)?,
        slug: r.try_get(2)?,
        repo_url: r.try_get(3)?,
        branch: r.try_get(4)?,
        project_type: r.try_get(5)?,
        deploy_method: r.try_get(6)?,
        deploy_dir: r.try_get(7)?,
        port: r.try_get::<Option<i32>, _>(8)?.and_then(|p| u16::try_from(p).ok()),
        install_cmd: r.try_get::<Option<String>, _>(9)?.unwrap_or_default(),
        build_cmd: r.try_get::<Option<String>, _>(10)?.unwrap_or_default(),
        start_cmd: r.try_get::<Option<String>, _>(11)?.unwrap_or_default(),
        health_url: r.try_get::<Option<String>, _>(12)?.unwrap_or_default(),
        health_timeout_s: r.try_get::<i32, _>(13)? as i64,
        stability_s: r.try_get::<i32, _>(14)? as i64,
        auto_deploy: r.try_get(15)?,
        webhook_secret: r.try_get(16)?,
        current_release_id: r.try_get(17)?,
        deployed_sha: r.try_get(18)?,
        last_poll_at: r.try_get(19)?,
        app_status: r.try_get(20)?,
        app_pid: r.try_get::<Option<i64>, _>(21)?.and_then(|p| u32::try_from(p).ok()),
        app_started_at: r.try_get(22)?,
        domains,
        ssl_email: r.try_get::<Option<String>, _>(24)?.unwrap_or_default(),
        created_at: r.try_get(25)?,
        updated_at: r.try_get(26)?,
        project_kind: r.try_get::<Option<String>, _>(27)?.unwrap_or_else(default_project_kind),
        access_mode: r.try_get::<Option<String>, _>(28)?.unwrap_or_else(default_access_mode),
        allowed_ips: r.try_get::<Option<String>, _>(29)?.unwrap_or_default(),
        node_id: r.try_get(30)?,
        placement_version: r.try_get::<Option<i64>, _>(31)?.unwrap_or(1),
        assigned_port: r.try_get::<Option<i32>, _>(32)?.and_then(|p| u16::try_from(p).ok()),
        bind_ip: r.try_get::<Option<String>, _>(33)?.unwrap_or_default(),
        cpu_limit: r.try_get::<Option<i32>, _>(34)?.unwrap_or(0),
        memory_limit_mb: r.try_get::<Option<i32>, _>(35)?.unwrap_or(0),
    })
}

pub fn parse_dotenv(input: &str) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for raw_line in input.lines() {
        let line = raw_line.trim();
        // Ignore comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip optional leading 'export '
        let line = if let Some(stripped) = line.strip_prefix("export ") {
            stripped.trim()
        } else {
            line
        };
        // Split on first '='
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim().to_string();
            if key.is_empty() {
                continue;
            }
            let mut val = v.trim();
            // Strip matching surrounding quotes if present
            if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
                || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
            {
                val = &val[1..val.len() - 1];
            } else if let Some(idx) = val.find(" #") {
                // If unquoted, strip trailing comment
                val = val[..idx].trim_end();
            }
            vars.push((key, val.to_string()));
        }
    }
    vars
}

