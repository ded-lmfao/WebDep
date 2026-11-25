use serde::{Deserialize, Serialize};

use super::grants::Grants;

fn default_presence() -> String {
    "offline".to_string()
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub disabled: bool,
    #[serde(rename = "isMainAdmin")]
    pub is_main_admin: bool,
    #[serde(rename = "totpEnabled")]
    pub totp_enabled: bool,
    #[serde(default = "default_presence")]
    pub presence: String,
    pub created_at: i64,
    #[serde(default)]
    pub email: String,
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    /// `admin` or `user`. Admins (and the main admin) bypass every permission check.
    #[serde(default = "default_role")]
    pub role: String,
    #[serde(rename = "isAdmin", default)]
    pub is_admin: bool,
    #[serde(rename = "mustChangePassword", default)]
    pub must_change_password: bool,
    #[serde(rename = "lastLoginAt", default)]
    pub last_login_at: Option<i64>,
    /// Authorization source of truth, loaded from `access_grants`.
    #[serde(rename = "access", skip_deserializing)]
    pub grants: Grants,
}

fn default_role() -> String {
    "user".to_string()
}

/// Column list every user query selects, in the order `from_pg_row` reads.
pub const USER_COLUMNS: &str = "id, username, disabled, COALESCE(totp_enabled, FALSE), created_at, COALESCE(email, ''), COALESCE(display_name, ''), COALESCE(role, 'user'), COALESCE(must_change_password, FALSE), last_login_at";

impl User {
    pub fn from_db(id: i64, username: String, disabled: bool, totp_enabled: bool, created_at: i64) -> Self {
        let is_main_admin = Self::is_main_admin_identity(id, &username);

        Self {
            id,
            username,
            disabled: !is_main_admin && disabled,
            is_main_admin,
            totp_enabled,
            presence: default_presence(),
            created_at,
            email: String::new(),
            display_name: String::new(),
            role: if is_main_admin { "admin".into() } else { default_role() },
            is_admin: is_main_admin,
            must_change_password: false,
            last_login_at: None,
            grants: Grants::default(),
        }
    }

    pub fn from_pg_row(r: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let mut u = Self::from_db(
            r.try_get(0)?,
            r.try_get(1)?,
            r.try_get::<Option<bool>, _>(2)?.unwrap_or(false),
            r.try_get::<Option<bool>, _>(3)?.unwrap_or(false),
            r.try_get(4)?,
        );
        u.email = r.try_get(5)?;
        u.display_name = r.try_get(6)?;
        let role: String = r.try_get(7)?;
        if !u.is_main_admin {
            u.is_admin = role == "admin";
            u.role = role;
        }
        u.must_change_password = !u.is_main_admin && r.try_get::<bool, _>(8)?;
        u.last_login_at = r.try_get(9)?;
        Ok(u)
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
        self.is_admin || self.grants.panel_has_any(powers)
    }

    pub fn can_access_project(&self, project_id: i64) -> bool {
        self.is_admin || self.grants.can_access_project(project_id)
    }

    pub fn has_project_power(&self, project_id: i64, powers: &[&str]) -> bool {
        self.is_admin || self.grants.project_has_any(project_id, powers)
    }

    /// Database powers come from a grant on the database itself or, for a database linked
    /// to a website/service, from the user's grant on that project.
    pub fn has_database_power(&self, database_id: i64, linked_project_id: Option<i64>, powers: &[&str]) -> bool {
        self.is_admin
            || self.grants.database_has_any(database_id, powers)
            || linked_project_id.is_some_and(|pid| self.grants.project_has_any(pid, powers))
    }
}

#[cfg(test)]
mod tests {
    use super::User;

    fn sample_user(id: i64, username: &str, powers: &[&str], projects: &[i64]) -> User {
        User::from_db(id, username.to_string(), false, false, 1_700_000_000)
            .with_grants(crate::auth::grants::Grants::fixture(powers, Some(projects)))
    }

    #[test]
    fn test_main_admin_has_all_powers_and_projects() {
        let admin = sample_user(1, "admin", &[], &[]);
        assert!(admin.is_main_admin);
        assert!(admin.has_power("settings_manage"));
        assert!(admin.has_any_power(&["databases", "databases_manage"]));
        assert!(admin.can_access_project(99));
    }

    #[test]
    fn test_granular_powers_and_project_scoping() {
        let dev = sample_user(2, "dev", &["deploy", "files", "services_manage"], &[10, 20]);
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
        let u = User::from_db(5, "viewer".into(), false, false, 0);
        assert!(!u.has_power("deploy"));
        assert!(!u.can_access_project(1));
    }
}


