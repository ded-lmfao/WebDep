use serde::{Deserialize, Serialize};

use super::grants::{parse_legacy_permissions, parse_legacy_projects, Grants};

fn default_presence() -> String {
    "offline".to_string()
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub disabled: bool,
    pub permissions: Vec<String>,
    #[serde(rename = "allowedProjects")]
    pub allowed_projects: Vec<i64>,
    #[serde(rename = "allProjects")]
    pub all_projects: bool,
    #[serde(rename = "isMainAdmin")]
    pub is_main_admin: bool,
    #[serde(rename = "totpEnabled")]
    pub totp_enabled: bool,
    #[serde(default = "default_presence")]
    pub presence: String,
    pub created_at: i64,
    /// Authorization source of truth, loaded from `access_grants` for session users.
    /// Users loaded for display (lists, lookups) carry no grants.
    #[serde(skip)]
    pub grants: Grants,
}

impl User {
    pub fn from_db(
        id: i64,
        username: String,
        disabled_int: i64,
        perms_json: String,
        proj_json: String,
        totp_enabled_int: i64,
        created_at: i64,
    ) -> Self {
        let is_main_admin = Self::is_main_admin_identity(id, &username);
        let disabled = !is_main_admin && disabled_int != 0;

        // Legacy columns are kept only for display in the current user editor.
        let (permissions, all_projects, allowed_projects) = if is_main_admin {
            (vec!["*".to_string()], true, Vec::new())
        } else {
            let (all, ids) = parse_legacy_projects(&proj_json);
            (parse_legacy_permissions(&perms_json), all, ids)
        };

        Self {
            id,
            username,
            disabled,
            permissions,
            allowed_projects,
            all_projects,
            is_main_admin,
            totp_enabled: totp_enabled_int != 0,
            presence: default_presence(),
            created_at,
            grants: Grants::default(),
        }
    }

    pub fn from_pg_row(r: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let disabled_bool: bool = r.try_get::<Option<bool>, _>(2)?.unwrap_or(false);
        let totp_bool: bool = r.try_get::<Option<bool>, _>(5)?.unwrap_or(false);
        Ok(Self::from_db(
            r.try_get(0)?,
            r.try_get(1)?,
            if disabled_bool { 1 } else { 0 },
            r.try_get::<Option<String>, _>(3)?.unwrap_or_else(|| "[\"*\"]".into()),
            r.try_get::<Option<String>, _>(4)?.unwrap_or_else(|| "[\"*\"]".into()),
            if totp_bool { 1 } else { 0 },
            r.try_get(6)?,
        ))
    }

    pub fn is_main_admin_identity(id: i64, username: &str) -> bool {
        id == 1 || username.eq_ignore_ascii_case("admin")
    }

    #[cfg(test)]
    pub fn with_grants(mut self, grants: Grants) -> Self {
        self.grants = grants;
        self
    }

    /// Panel-level power (not tied to one app).
    pub fn has_power(&self, power: &str) -> bool {
        self.has_any_power(&[power])
    }

    /// Panel-level powers (not tied to one app).
    pub fn has_any_power(&self, powers: &[&str]) -> bool {
        self.is_main_admin || self.grants.panel_has_any(powers)
    }

    pub fn can_access_project(&self, project_id: i64) -> bool {
        self.is_main_admin || self.grants.can_access_project(project_id)
    }

    pub fn has_project_power(&self, project_id: i64, powers: &[&str]) -> bool {
        self.is_main_admin || self.grants.project_has_any(project_id, powers)
    }

    /// Database powers come from a grant on the database itself or, for a database linked
    /// to a website/service, from the user's grant on that project.
    pub fn has_database_power(&self, database_id: i64, linked_project_id: Option<i64>, powers: &[&str]) -> bool {
        self.is_main_admin
            || self.grants.database_has_any(database_id, powers)
            || linked_project_id.is_some_and(|pid| self.grants.project_has_any(pid, powers))
    }
}

#[cfg(test)]
mod tests {
    use super::User;

    fn sample_user(id: i64, username: &str, perms_json: &str, proj_json: &str) -> User {
        let perms = crate::auth::grants::parse_legacy_permissions(perms_json);
        let (all, ids) = crate::auth::grants::parse_legacy_projects(proj_json);
        User::from_db(
            id,
            username.to_string(),
            0,
            perms_json.to_string(),
            proj_json.to_string(),
            0,
            1_700_000_000,
        )
        .with_grants(crate::auth::grants::Grants::from_legacy(&perms, all, &ids))
    }

    #[test]
    fn test_main_admin_has_all_powers_and_projects() {
        let admin = sample_user(1, "admin", "[]", "[]");
        assert!(admin.is_main_admin);
        assert!(admin.has_power("settings_manage"));
        assert!(admin.has_any_power(&["databases", "databases_manage"]));
        assert!(admin.can_access_project(99));
    }

    #[test]
    fn test_granular_powers_and_project_scoping() {
        let dev = sample_user(2, "dev", r#"["deploy","files","services_manage"]"#, r#"[10, 20]"#);
        assert!(!dev.is_main_admin);
        assert!(dev.has_power("deploy"));
        assert!(dev.has_power("files"));
        assert!(dev.has_power("services_manage"));
        assert!(!dev.has_power("projects_manage"));
        assert!(!dev.has_power("env"));
        assert!(!dev.has_power("files_write"));
        assert!(dev.has_any_power(&["files", "files_write"]));
        assert!(!dev.has_any_power(&["databases", "databases_manage"]));
        assert!(dev.can_access_project(10));
        assert!(!dev.can_access_project(30));
        assert!(dev.has_project_power(10, &["deploy"]));
        assert!(!dev.has_project_power(30, &["deploy"]));
    }

    #[test]
    fn test_users_without_loaded_grants_have_no_access() {
        let u = User::from_db(5, "viewer".into(), 0, r#"["*"]"#.into(), r#"["*"]"#.into(), 0, 0);
        assert!(!u.has_power("deploy"));
        assert!(!u.can_access_project(1));
    }
}


