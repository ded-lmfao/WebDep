-- Phase 2: Per-user, per-resource, per-node granular RBAC grants
CREATE TABLE IF NOT EXISTS access_grants (
    id          BIGSERIAL PRIMARY KEY,
    user_id     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    scope_type  TEXT NOT NULL CHECK (scope_type IN ('global', 'node', 'project', 'database')),
    scope_id    BIGINT,
    role_preset TEXT NOT NULL DEFAULT 'custom',
    powers_json TEXT NOT NULL DEFAULT '[]',
    granted_by  BIGINT REFERENCES users(id) ON DELETE SET NULL,
    expires_at  BIGINT,
    created_at  BIGINT NOT NULL,
    updated_at  BIGINT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_access_grants_unique
    ON access_grants(user_id, scope_type, COALESCE(scope_id, 0));

CREATE INDEX IF NOT EXISTS idx_access_grants_user
    ON access_grants(user_id);

CREATE INDEX IF NOT EXISTS idx_access_grants_scope
    ON access_grants(scope_type, scope_id);

CREATE INDEX IF NOT EXISTS idx_access_grants_expires
    ON access_grants(expires_at)
    WHERE expires_at IS NOT NULL;
