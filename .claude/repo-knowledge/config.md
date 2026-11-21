# Config (.env, see .env.example)
ADMIN_USERNAME, ADMIN_PASSWORD (blank => generated, logged once), ADMIN_RESET_2FA, HOST(0.0.0.0), PORT(4600), DATA_DIR(./data), SESSION_TTL_S, SESSION_IDLE_TIMEOUT_S, FORCE_SECURE_COOKIES, WEBDEP_MANAGE_FIREWALL, WEBDEP_SKIP_DB_BACKUP (skip pre-upgrade pg_dump), DATABASE_URL (optional external PG).
Env used by scripts: WEBDEP_REPO, WEBDEP_FORCE_RELEASE, WEBDEP_NODE_P2P_PORT(4601), CARGO_BUILD_JOBS.
Runtime settings (DB `settings`, Platform Settings UI): accounts (allow_registration, registration_email_domains, require_2fa off|admins|everyone) and maintenance (audit_retention_days 365, db_backup_keep 7, notify_webhook_url) — validated in api/system/settings.rs via maintenance::validate_setting.
