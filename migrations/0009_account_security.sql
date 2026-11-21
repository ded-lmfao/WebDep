-- One-time password reset links an admin hands to a user (no email needed). Only a hash of
-- the token is stored.
CREATE TABLE IF NOT EXISTS password_resets (
    id          BIGSERIAL PRIMARY KEY,
    user_id     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash  TEXT NOT NULL UNIQUE,
    created_by  BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at  BIGINT NOT NULL,
    expires_at  BIGINT NOT NULL,
    used_at     BIGINT
);
CREATE INDEX IF NOT EXISTS idx_password_resets_user ON password_resets(user_id);

-- Where and when each session started, for the sessions list.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS created_at BIGINT NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS ip TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS user_agent TEXT NOT NULL DEFAULT '';
