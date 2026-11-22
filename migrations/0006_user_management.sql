-- Phase 1 of user management: account profile fields, admin role, registration
-- requests, and per-database activity. Per-app permissions live in access_grants
-- (0002); legacy users.permissions / users.allowed_projects are kept for rollback.

ALTER TABLE users ADD COLUMN IF NOT EXISTS email TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS display_name TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN IF NOT EXISTS role TEXT NOT NULL DEFAULT 'user'
    CHECK (role IN ('admin', 'user'));
ALTER TABLE users ADD COLUMN IF NOT EXISTS must_change_password BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE users ADD COLUMN IF NOT EXISTS last_login_at BIGINT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email_lower
    ON users(LOWER(email))
    WHERE email IS NOT NULL;

UPDATE users SET role = 'admin' WHERE id = 1 OR LOWER(username) = 'admin';

CREATE TABLE IF NOT EXISTS registration_requests (
    id            BIGSERIAL PRIMARY KEY,
    email         TEXT NOT NULL,
    username      TEXT NOT NULL,
    display_name  TEXT NOT NULL DEFAULT '',
    password_hash TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'rejected')),
    reason        TEXT,
    ip            TEXT,
    created_at    BIGINT NOT NULL,
    reviewed_by   BIGINT REFERENCES users(id) ON DELETE SET NULL,
    reviewed_at   BIGINT,
    user_id       BIGINT REFERENCES users(id) ON DELETE SET NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_registration_pending_email
    ON registration_requests(LOWER(email))
    WHERE status = 'pending';

CREATE UNIQUE INDEX IF NOT EXISTS idx_registration_pending_username
    ON registration_requests(LOWER(username))
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_registration_status
    ON registration_requests(status, created_at DESC);

ALTER TABLE audit ADD COLUMN IF NOT EXISTS database_id BIGINT
    REFERENCES databases(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_audit_database
    ON audit(database_id, id DESC)
    WHERE database_id IS NOT NULL;
