use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::DbPool;
use super::password::{constant_time_eq, hash_password, random_token, verify_password};
use super::session::Session;
use super::totp::{sync_real_utc_offset, verify_totp_code};
use super::user::{User, USER_COLUMNS};

pub const ACCESS_REVOKE_CHANNEL: &str = "webdep_access_revoke";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessRevocationEvent {
    pub kind: String,
    pub user_id: i64,
    pub scope_type: Option<String>,
    pub scope_id: Option<i64>,
    pub reason: String,
    pub ts: i64,
}

async fn publish_revocation(db: &DbPool, event: &AccessRevocationEvent) {
    if let Ok(payload) = serde_json::to_string(event) {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(ACCESS_REVOKE_CHANNEL)
            .bind(&payload)
            .execute(db)
            .await;
    }
}

#[derive(Clone, Default)]
pub(crate) struct FailureRecord {
    pub(crate) count: u32,
    pub(crate) first_at_s: u64,
    pub(crate) locked_until_s: u64,
}

const PRESENCE_TTL_MS: u64 = 95_000;

pub struct Auth {
    pub(crate) db: DbPool,
    pub session_ttl_s: u64,
    pub session_idle_timeout_s: u64,
    pub force_secure_cookies: bool,
    pub(crate) mfa_challenges: Mutex<HashMap<String, (i64, u64)>>,
    pub(crate) passkey_challenges: Mutex<HashMap<String, (Option<i64>, String, u64)>>,
    pub(crate) failures: Mutex<HashMap<String, FailureRecord>>,
    pub(crate) session_cache: RwLock<HashMap<String, (Session, User, u64)>>,
    pub(crate) session_presence: Mutex<HashMap<String, (i64, u64, bool)>>,
    pub(crate) revocation_tx: tokio::sync::broadcast::Sender<AccessRevocationEvent>,
}

impl Auth {
    #[cfg(test)]
    pub fn new(db: DbPool, session_ttl_s: u64) -> Self {
        Self::with_timeouts(db, session_ttl_s, 1800)
    }

    pub fn with_timeouts(db: DbPool, session_ttl_s: u64, session_idle_timeout_s: u64) -> Self {
        std::thread::spawn(|| {
            sync_real_utc_offset();
        });
        let (revocation_tx, _) = tokio::sync::broadcast::channel(128);
        Self {
            db,
            session_ttl_s,
            session_idle_timeout_s,
            force_secure_cookies: false,
            mfa_challenges: Mutex::new(HashMap::new()),
            passkey_challenges: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            session_cache: RwLock::new(HashMap::new()),
            session_presence: Mutex::new(HashMap::new()),
            revocation_tx,
        }
    }

    pub(crate) fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub fn set_session_presence(&self, session_id: &str, user_id: i64, state_str: &str) {
        let now_ms = Self::now_millis();
        if let Ok(mut map) = self.session_presence.lock() {
            map.retain(|_, (_, seen_ms, _)| now_ms.saturating_sub(*seen_ms) <= PRESENCE_TTL_MS);
            match state_str {
                "offline" => {
                    map.remove(session_id);
                }
                "idle" => {
                    map.insert(session_id.to_string(), (user_id, now_ms, false));
                }
                _ => {
                    map.insert(session_id.to_string(), (user_id, now_ms, true));
                }
            }
        }
    }

    pub fn ensure_session_presence(&self, session_id: &str, user_id: i64) {
        let now_ms = Self::now_millis();
        if let Ok(mut map) = self.session_presence.lock() {
            if let Some(entry) = map.get_mut(session_id) {
                if now_ms.saturating_sub(entry.1) >= 15_000 {
                    entry.1 = now_ms;
                }
                return;
            }
            if map.len() > 256 {
                map.retain(|_, (_, seen_ms, _)| now_ms.saturating_sub(*seen_ms) <= PRESENCE_TTL_MS);
            }
            map.insert(session_id.to_string(), (user_id, now_ms, true));
        }
    }

    pub fn clear_user_presence(&self, user_id: i64) {
        self.invalidate_sessions();
        if let Ok(mut map) = self.session_presence.lock() {
            map.retain(|_, (uid, _, _)| *uid != user_id);
        }
    }

    pub fn presence_for_user(&self, user_id: i64) -> String {
        let now_ms = Self::now_millis();
        if let Ok(mut map) = self.session_presence.lock() {
            map.retain(|_, (_, seen_ms, _)| now_ms.saturating_sub(*seen_ms) <= PRESENCE_TTL_MS);
            let mut has_session = false;
            for &(uid, _, active) in map.values() {
                if uid == user_id {
                    if active {
                        return "online".to_string();
                    }
                    has_session = true;
                }
            }
            if has_session {
                return "idle".to_string();
            }
        }
        "offline".to_string()
    }

    pub fn invalidate_sessions(&self) {
        if let Ok(mut cache) = self.session_cache.write() {
            cache.clear();
        }
    }

    pub(crate) fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn check_lockout(&self, key: &str) -> Option<u64> {
        let now = Self::now_secs();
        let mut map = self.failures.lock().unwrap();
        if let Some(rec) = map.get_mut(key) {
            if rec.locked_until_s > now {
                return Some(rec.locked_until_s - now);
            }
            if now.saturating_sub(rec.first_at_s) > 600 {
                map.remove(key);
            }
        }
        None
    }

    pub fn record_failure(&self, key: &str) -> Option<u64> {
        let now = Self::now_secs();
        let mut map = self.failures.lock().unwrap();
        let entry = map.entry(key.to_string()).or_insert(FailureRecord {
            count: 0,
            first_at_s: now,
            locked_until_s: 0,
        });
        if now.saturating_sub(entry.first_at_s) > 600 {
            entry.count = 0;
            entry.first_at_s = now;
            entry.locked_until_s = 0;
        }
        entry.count += 1;
        let max_attempts = if key.starts_with("2fa:") || key.starts_with("2fa_user:") {
            3
        } else {
            5
        };
        if entry.count >= max_attempts {
            entry.locked_until_s = now + 600;
            return Some(600);
        }
        None
    }

    pub fn clear_failures(&self, key: &str) {
        let mut map = self.failures.lock().unwrap();
        map.remove(key);
    }

    pub fn create_mfa_challenge(&self, user_id: i64) -> String {
        let now = Self::now_secs();
        let token = random_token(32);
        let mut map = self.mfa_challenges.lock().unwrap();
        map.retain(|_, (uid, exp)| *exp > now && *uid != user_id);
        map.insert(token.clone(), (user_id, now + 180));
        token
    }

    pub fn peek_mfa_challenge(&self, token: &str) -> Option<i64> {
        let now = Self::now_secs();
        let map = self.mfa_challenges.lock().unwrap();
        map.get(token).and_then(|(uid, exp)| if *exp > now { Some(*uid) } else { None })
    }

    pub fn consume_mfa_challenge(&self, token: &str) -> Option<i64> {
        let now = Self::now_secs();
        let mut map = self.mfa_challenges.lock().unwrap();
        map.retain(|_, (_, exp)| *exp > now);
        map.remove(token).map(|(uid, _)| uid)
    }

    pub async fn verify_user_2fa(&self, user_id: i64, code: &str) -> bool {
        let trimmed = code.trim();
        if trimmed.is_empty() {
            return false;
        }

        let row: Option<(Option<String>, String)> = sqlx::query_as(
            "SELECT totp_secret, COALESCE(totp_backup_codes, '[]') FROM users WHERE id = $1 AND totp_enabled = TRUE",
        )
        .bind(user_id)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten();

        let (secret_opt, backup_json) = row.unwrap_or((None, "[]".to_string()));

        if let Some(secret) = secret_opt {
            if verify_totp_code(&secret, trimmed) {
                return true;
            }
        }

        let norm_input: String = trimmed
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if norm_input.len() >= 8 {
            if let Ok(mut codes) = serde_json::from_str::<Vec<String>>(&backup_json) {
                if let Some(pos) = codes.iter().position(|c| {
                    let norm_c: String = c
                        .chars()
                        .filter(|ch| ch.is_ascii_alphanumeric())
                        .map(|ch| ch.to_ascii_uppercase())
                        .collect();
                    constant_time_eq(&norm_c, &norm_input)
                }) {
                    codes.remove(pos);
                    let updated_json = serde_json::to_string(&codes).unwrap_or_else(|_| "[]".to_string());
                    let _ = sqlx::query("UPDATE users SET totp_backup_codes = $1 WHERE id = $2")
                        .bind(updated_json)
                        .bind(user_id)
                        .execute(&self.db)
                        .await;
                    return true;
                }
            }
        }

        false
    }

    pub async fn ensure_admin(&self) {
        let (env_user, env_pass, reset_2fa) = crate::config::Config::read_dotenv_admin();

        let existing_main: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT id, username, password_hash FROM users
             WHERE id = 1 OR LOWER(username) = LOWER($1)
             ORDER BY CASE WHEN id = 1 THEN 0 ELSE 1 END, id ASC
             LIMIT 1",
        )
        .bind(&env_user)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten();

        if let Some((admin_id, current_username, current_hash)) = existing_main {
            let mut updated_session_needed = false;

            if current_username != env_user {
                let _ = sqlx::query("UPDATE users SET username = $1, disabled = FALSE WHERE id = $2")
                    .bind(&env_user)
                    .bind(admin_id)
                    .execute(&self.db)
                    .await;
                updated_session_needed = true;
            }

            if current_hash.starts_with("scrypt$") || !verify_password(&env_pass, &current_hash) {
                if let Ok(hashed) = hash_password(&env_pass) {
                    let _ = sqlx::query("UPDATE users SET password_hash = $1, disabled = FALSE WHERE id = $2")
                        .bind(hashed)
                        .bind(admin_id)
                        .execute(&self.db)
                        .await;
                    updated_session_needed = true;
                }
            }

            if reset_2fa {
                let _ = sqlx::query(
                    "UPDATE users SET totp_enabled = FALSE, totp_secret = NULL, totp_pending_secret = NULL, totp_backup_codes = '[]' WHERE id = $1",
                )
                .bind(admin_id)
                .execute(&self.db)
                .await;
                crate::config::Config::sync_dotenv_admin(None, None);
                self.invalidate_sessions();
                return;
            }

            if updated_session_needed {
                self.invalidate_sessions();
            }
        } else if let Ok(hashed) = hash_password(&env_pass) {
            let now = Self::now_millis() as i64;
            let _ = sqlx::query(
                "INSERT INTO users(id, username, password_hash, disabled, permissions, allowed_projects, created_at)
                 VALUES(1, $1, $2, FALSE, '[\"*\"]', '[\"*\"]', $3)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(&env_user)
            .bind(hashed)
            .bind(now)
            .execute(&self.db)
            .await;
            let _ = sqlx::query(
                "SELECT setval(pg_get_serial_sequence('users', 'id'), COALESCE((SELECT MAX(id) FROM users), 1), true)",
            )
            .execute(&self.db)
            .await;
        }
    }

    pub async fn get_user_by_id(&self, id: i64) -> Option<User> {
        let row = sqlx::query(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1"))
        .bind(id)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten()?;

        let mut u = User::from_pg_row(&row).ok()?;
        u.grants = self.load_grants(&u).await?;
        u.presence = if u.disabled {
            "offline".to_string()
        } else {
            self.presence_for_user(u.id)
        };
        Some(u)
    }

    /// Main admin needs no grant rows. A load failure denies (returns None) rather than
    /// silently authorizing with empty grants.
    async fn load_grants(&self, user: &User) -> Option<super::grants::Grants> {
        if user.is_admin {
            return Some(Default::default());
        }
        match super::grants::load(&self.db, user.id).await {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!("Failed to load access grants for user {}: {}", user.id, e);
                None
            }
        }
    }

    /// Every user with their grants (for the admin user list).
    pub async fn list_users(&self) -> Result<Vec<User>, sqlx::Error> {
        let rows = sqlx::query(&format!("SELECT {USER_COLUMNS} FROM users ORDER BY id ASC"))
            .fetch_all(&self.db)
            .await?;
        let mut all_grants = super::grants::load_all(&self.db).await?;

        let mut list = Vec::with_capacity(rows.len());
        for row in rows {
            let mut u = User::from_pg_row(&row)?;
            if !u.is_admin {
                u.grants = all_grants.remove(&u.id).unwrap_or_default();
            }
            u.presence = if u.disabled {
                "offline".to_string()
            } else {
                self.presence_for_user(u.id)
            };
            list.push(u);
        }
        Ok(list)
    }

    /// Looks a user up by username or email (case-insensitive) and returns the password hash.
    /// The returned user carries no grants: load it by id before authorizing.
    pub async fn get_user_by_username(&self, login: &str) -> Option<(User, String)> {
        let row = sqlx::query(&format!(
            "SELECT {USER_COLUMNS}, password_hash FROM users
             WHERE LOWER(username) = LOWER($1) OR (email IS NOT NULL AND LOWER(email) = LOWER($1))
             ORDER BY (LOWER(username) = LOWER($1)) DESC LIMIT 1"
        ))
        .bind(login)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten()?;

        let user = User::from_pg_row(&row).ok()?;
        let hash: String = row.try_get(12).ok()?;
        Some((user, hash))
    }

    fn is_idle_expired(&self, last_active_at: i64, now_s: u64) -> bool {
        self.session_idle_timeout_s > 0
            && last_active_at > 0
            && (now_s as i64).saturating_sub(last_active_at) > (self.session_idle_timeout_s as i64)
    }

    pub async fn create_session(&self, user_id: i64) -> Result<Session, sqlx::Error> {
        let session_id = random_token(32);
        let csrf_token = random_token(24);
        let now_s = Self::now_secs();
        let expires_at = (now_s + self.session_ttl_s) as i64;
        let last_active_at = now_s as i64;

        sqlx::query(
            "INSERT INTO sessions(id, user_id, expires_at, last_active_at, csrf_token) VALUES($1, $2, $3, $4, $5)",
        )
        .bind(&session_id)
        .bind(user_id)
        .bind(expires_at)
        .bind(last_active_at)
        .bind(&csrf_token)
        .execute(&self.db)
        .await?;

        self.set_session_presence(&session_id, user_id, "online");

        Ok(Session {
            id: session_id,
            user_id,
            expires_at,
            last_active_at,
            csrf_token,
        })
    }

    async fn touch_session_activity(&self, session_id: &str, prev_active: i64, now_s: u64) {
        let now_i64 = now_s as i64;
        if now_i64.saturating_sub(prev_active) < 30 {
            return;
        }
        if let Ok(mut cache) = self.session_cache.write() {
            if let Some((sess, _, _)) = cache.get_mut(session_id) {
                sess.last_active_at = now_i64;
            }
        }
        let _ = sqlx::query("UPDATE sessions SET last_active_at = $1 WHERE id = $2")
            .bind(now_i64)
            .bind(session_id)
            .execute(&self.db)
            .await;
    }

    pub async fn get_session(&self, session_id: &str) -> Option<(Session, User)> {
        self.get_session_with_touch(session_id, false).await
    }

    pub async fn get_session_with_touch(&self, session_id: &str, touch_activity: bool) -> Option<(Session, User)> {
        let now_s = Self::now_secs();

        #[allow(clippy::large_enum_variant)]
        enum CacheCheck {
            Miss,
            Expired,
            Hit(Session, User),
        }

        let cached = {
            if let Ok(cache) = self.session_cache.read() {
                if let Some((sess, usr, cached_until)) = cache.get(session_id) {
                    if *cached_until > now_s && sess.expires_at > (now_s as i64) && !usr.disabled {
                        if self.is_idle_expired(sess.last_active_at, now_s) {
                            CacheCheck::Expired
                        } else {
                            CacheCheck::Hit(sess.clone(), usr.clone())
                        }
                    } else {
                        CacheCheck::Miss
                    }
                } else {
                    CacheCheck::Miss
                }
            } else {
                CacheCheck::Miss
            }
        };

        match cached {
            CacheCheck::Expired => {
                self.delete_session(session_id).await;
                return None;
            }
            CacheCheck::Hit(mut out_sess, out_usr) => {
                let prev_active = out_sess.last_active_at;
                if touch_activity {
                    self.touch_session_activity(session_id, prev_active, now_s).await;
                    if (now_s as i64).saturating_sub(prev_active) >= 30 {
                        out_sess.last_active_at = now_s as i64;
                    }
                }
                self.ensure_session_presence(session_id, out_usr.id);
                return Some((out_sess, out_usr));
            }
            CacheCheck::Miss => {}
        }

        let row = sqlx::query(
            "SELECT id, user_id, expires_at, last_active_at, csrf_token FROM sessions WHERE id = $1 AND expires_at > $2",
        )
        .bind(session_id)
        .bind(now_s as i64)
        .fetch_optional(&self.db)
        .await
        .ok()
        .flatten()?;

        let session = Session {
            id: row.try_get(0).ok()?,
            user_id: row.try_get(1).ok()?,
            expires_at: row.try_get(2).ok()?,
            last_active_at: row.try_get(3).ok()?,
            csrf_token: row.try_get(4).ok()?,
        };
        // Loads profile and grants; a grant load failure denies the session.
        let user = self.get_user_by_id(session.user_id).await?;
        let mut valid = (!user.disabled).then_some((session, user))?;
        if self.is_idle_expired(valid.0.last_active_at, now_s) {
            self.delete_session(session_id).await;
            return None;
        }
        if touch_activity && (now_s as i64).saturating_sub(valid.0.last_active_at) >= 30 {
            self.touch_session_activity(session_id, valid.0.last_active_at, now_s).await;
            valid.0.last_active_at = now_s as i64;
        }
        self.ensure_session_presence(session_id, valid.1.id);
        if let Ok(mut cache) = self.session_cache.write() {
            if cache.len() > 256 {
                cache.retain(|_, (_, _, exp)| *exp > now_s);
            }
            cache.insert(
                session_id.to_string(),
                (valid.0.clone(), valid.1.clone(), now_s + 30),
            );
        }
        Some(valid)
    }

    pub async fn delete_session(&self, session_id: &str) {
        if let Ok(mut cache) = self.session_cache.write() {
            cache.remove(session_id);
        }
        if let Ok(mut map) = self.session_presence.lock() {
            map.remove(session_id);
        }
        let _ = sqlx::query("DELETE FROM sessions WHERE id = $1")
            .bind(session_id)
            .execute(&self.db)
            .await;
    }

    pub async fn revoke_other_sessions(&self, user_id: i64, keep_session_id: &str) -> usize {
        self.invalidate_sessions();
        if let Ok(mut map) = self.session_presence.lock() {
            map.retain(|sid, (uid, _, _)| *uid != user_id || sid == keep_session_id);
        }
        sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id != $2")
            .bind(user_id)
            .bind(keep_session_id)
            .execute(&self.db)
            .await
            .map(|r| r.rows_affected() as usize)
            .unwrap_or(0)
    }

    pub async fn revoke_all_user_sessions(&self, user_id: i64, reason: &str) -> usize {
        self.invalidate_sessions();
        self.clear_user_presence(user_id);
        let removed = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.db)
            .await
            .map(|r| r.rows_affected() as usize)
            .unwrap_or(0);
        self.notify_revocation(AccessRevocationEvent {
            kind: "session_revoked".to_string(),
            user_id,
            scope_type: None,
            scope_id: None,
            reason: reason.to_string(),
            ts: Self::now_secs() as i64,
        })
        .await;
        removed
    }

    pub fn subscribe_revocations(&self) -> tokio::sync::broadcast::Receiver<AccessRevocationEvent> {
        self.revocation_tx.subscribe()
    }

    pub async fn notify_revocation(&self, event: AccessRevocationEvent) {
        self.invalidate_sessions();
        let _ = self.revocation_tx.send(event.clone());
        publish_revocation(&self.db, &event).await;
    }

    pub fn start_revocation_listener(&self) {
        let db = self.db.clone();
        let tx = self.revocation_tx.clone();
        tokio::spawn(async move {
            loop {
                match sqlx::postgres::PgListener::connect_with(&db).await {
                    Ok(mut listener) => {
                        if let Err(err) = listener.listen(ACCESS_REVOKE_CHANNEL).await {
                            tracing::warn!("PgListener subscribe {} failed: {}", ACCESS_REVOKE_CHANNEL, err);
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            continue;
                        }
                        loop {
                            match listener.recv().await {
                                Ok(notif) => {
                                    if let Ok(ev) = serde_json::from_str::<AccessRevocationEvent>(notif.payload()) {
                                        let _ = tx.send(ev);
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!("PgListener recv error: {}", err);
                                    break;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!("PgListener connect error: {}", err);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    pub async fn can_access_project_for_user(&self, user: &User, project_id: i64) -> bool {
        !user.disabled && user.can_access_project(project_id)
    }

    pub async fn has_power_for_scope(
        &self,
        user: &User,
        power: &str,
        scope_type: &str,
        scope_id: Option<i64>,
    ) -> bool {
        self.has_any_power_for_scope(user, &[power], scope_type, scope_id)
            .await
    }

    /// `project` / `database` with an id check that app's grants; anything else (or no id)
    /// checks panel-level powers.
    pub async fn has_any_power_for_scope(
        &self,
        user: &User,
        powers: &[&str],
        scope_type: &str,
        scope_id: Option<i64>,
    ) -> bool {
        if user.disabled {
            return false;
        }
        match (scope_type, scope_id) {
            (super::grants::SCOPE_PROJECT, Some(pid)) => user.has_project_power(pid, powers),
            (super::grants::SCOPE_DATABASE, Some(db_id)) => user.has_database_power(db_id, None, powers),
            _ => user.has_any_power(powers),
        }
    }

    pub fn user_has_database_power(
        user: &User,
        powers: &[&str],
        db_id: i64,
        linked_project_id: Option<i64>,
    ) -> bool {
        !user.disabled && user.has_database_power(db_id, linked_project_id, powers)
    }

    pub fn user_can_view_db_credentials(user: &User, db_id: i64, linked_project_id: Option<i64>) -> bool {
        Self::user_has_database_power(user, &["databases_manage", "env"], db_id, linked_project_id)
    }

    pub async fn has_database_power_for_user(
        &self,
        user: &User,
        powers: &[&str],
        db_id: i64,
        linked_project_id: Option<i64>,
    ) -> bool {
        Self::user_has_database_power(user, powers, db_id, linked_project_id)
    }

    /// True when the user holds a database power at panel level, on any database, or on
    /// any project they can reach.
    pub async fn can_list_databases_for_user(&self, user: &User) -> bool {
        const DB_POWERS: [&str; 2] = ["databases", "databases_manage"];
        !user.disabled
            && (user.has_any_power(&DB_POWERS)
                || user.grants.databases.values().chain(user.grants.projects.values()).any(|set| {
                    set.contains(super::grants::ALL_POWERS) || DB_POWERS.iter().any(|p| set.contains(*p))
                }))
    }

    pub async fn can_view_db_credentials_for_user(
        &self,
        user: &User,
        db_id: i64,
        linked_project_id: Option<i64>,
    ) -> bool {
        Self::user_can_view_db_credentials(user, db_id, linked_project_id)
    }

    pub async fn count_active_sessions(&self, user_id: i64) -> usize {
        let now_s = Self::now_secs() as i64;
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sessions WHERE user_id = $1 AND expires_at > $2",
        )
        .bind(user_id)
        .bind(now_s)
        .fetch_one(&self.db)
        .await
        .unwrap_or(1) as usize
    }

    pub async fn purge_expired_sessions(&self) {
        let now_s = Self::now_secs();
        let idle_cutoff = (now_s as i64).saturating_sub(self.session_idle_timeout_s as i64);
        if let Ok(mut cache) = self.session_cache.write() {
            cache.retain(|_, (sess, _, exp)| *exp > now_s && !self.is_idle_expired(sess.last_active_at, now_s));
        }
        let _ = sqlx::query("DELETE FROM sessions WHERE expires_at <= $1")
            .bind(now_s as i64)
            .execute(&self.db)
            .await;
        if self.session_idle_timeout_s > 0 {
            let _ = sqlx::query("DELETE FROM sessions WHERE last_active_at > 0 AND last_active_at <= $1")
                .bind(idle_cutoff)
                .execute(&self.db)
                .await;
        }
    }

    pub fn prune_in_memory(&self) {
        let now_s = Self::now_secs();
        let now_ms = Self::now_millis();

        if let Ok(mut mfa) = self.mfa_challenges.lock() {
            mfa.retain(|_, (_, exp)| *exp > now_s);
        }
        if let Ok(mut pk) = self.passkey_challenges.lock() {
            pk.retain(|_, (_, _, exp)| *exp > now_s);
        }
        if let Ok(mut fails) = self.failures.lock() {
            fails.retain(|_, rec| {
                rec.locked_until_s > now_s || now_s.saturating_sub(rec.first_at_s) < 900
            });
            if fails.len() > 2048 {
                fails.clear();
            }
        }
        if let Ok(mut cache) = self.session_cache.write() {
            cache.retain(|_, (_, _, exp)| *exp > now_s);
            if cache.len() > 1024 {
                cache.clear();
            }
        }
        if let Ok(mut presence) = self.session_presence.lock() {
            presence.retain(|_, (_, seen_ms, _)| now_ms.saturating_sub(*seen_ms) < PRESENCE_TTL_MS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_presence_transitions() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        let auth = Auth::new(db, 3600);
        auth.ensure_admin().await;

        assert_eq!(auth.presence_for_user(2), "offline");
        auth.set_session_presence("sid-a", 2, "online");
        assert_eq!(auth.presence_for_user(2), "online");

        auth.set_session_presence("sid-b", 2, "idle");
        assert_eq!(auth.presence_for_user(2), "online");

        auth.set_session_presence("sid-a", 2, "idle");
        assert_eq!(auth.presence_for_user(2), "idle");

        auth.set_session_presence("sid-a", 2, "offline");
        assert_eq!(auth.presence_for_user(2), "idle");

        auth.set_session_presence("sid-b", 2, "offline");
        assert_eq!(auth.presence_for_user(2), "offline");
    }

    #[tokio::test]
    async fn test_session_idle_expiration_and_activity_refresh() {
        let db = crate::pg_control::test_db::isolated_pool().await;
        let auth = Auth::with_timeouts(db.clone(), 3600, 120);
        auth.ensure_admin().await;

        let sess = auth.create_session(1).await.unwrap();
        assert!(auth.get_session(&sess.id).await.is_some());

        let now_i64 = Auth::now_secs() as i64;
        sqlx::query("UPDATE sessions SET last_active_at = $1 WHERE id = $2")
            .bind(now_i64 - 60)
            .bind(&sess.id)
            .execute(&db)
            .await
            .unwrap();
        auth.invalidate_sessions();
        let (refreshed, _) = auth
            .get_session_with_touch(&sess.id, true)
            .await
            .expect("session should refresh within idle window");
        assert!(refreshed.last_active_at >= now_i64 - 1);

        sqlx::query("UPDATE sessions SET last_active_at = $1 WHERE id = $2")
            .bind(now_i64 - 200)
            .bind(&sess.id)
            .execute(&db)
            .await
            .unwrap();
        auth.invalidate_sessions();
        assert!(auth.get_session(&sess.id).await.is_none());
    }
}
