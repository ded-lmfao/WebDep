-- WebDep Core Initial PostgreSQL Schema (Migration 0001)
-- Replaces SQLite v3 schema with async PostgreSQL equivalents + strict FK constraints.

CREATE TABLE IF NOT EXISTS _meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    repo_url TEXT NOT NULL,
    branch TEXT NOT NULL DEFAULT 'main',
    project_type TEXT NOT NULL DEFAULT 'node',
    deploy_method TEXT NOT NULL DEFAULT 'managed',
    deploy_dir TEXT NOT NULL,
    port INTEGER,
    install_cmd TEXT NOT NULL DEFAULT '',
    build_cmd TEXT NOT NULL DEFAULT '',
    start_cmd TEXT NOT NULL DEFAULT '',
    health_url TEXT NOT NULL DEFAULT '',
    health_timeout_s INTEGER NOT NULL DEFAULT 30,
    stability_s INTEGER NOT NULL DEFAULT 5,
    auto_deploy BOOLEAN NOT NULL DEFAULT FALSE,
    webhook_secret TEXT,
    current_release_id BIGINT,
    deployed_sha TEXT,
    last_poll_at BIGINT,
    app_status TEXT NOT NULL DEFAULT 'stopped',
    app_pid BIGINT,
    app_started_at BIGINT,
    domains TEXT NOT NULL DEFAULT '[]',
    ssl_email TEXT NOT NULL DEFAULT '',
    project_kind TEXT NOT NULL DEFAULT 'web',
    access_mode TEXT NOT NULL DEFAULT 'local',
    allowed_ips TEXT NOT NULL DEFAULT '',
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_projects_created ON projects(created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS project_env (
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value_enc TEXT NOT NULL,
    is_secret BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY(project_id, key)
);

CREATE TABLE IF NOT EXISTS deployments (
    id BIGSERIAL PRIMARY KEY,
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    release_id BIGINT,
    from_release_id BIGINT,
    kind TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    stage TEXT NOT NULL DEFAULT 'pending',
    sha TEXT,
    trigger TEXT NOT NULL DEFAULT 'manual',
    actor TEXT NOT NULL DEFAULT 'system',
    error TEXT,
    started_at BIGINT NOT NULL,
    finished_at BIGINT
);
CREATE INDEX IF NOT EXISTS idx_deployments_proj ON deployments(project_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_deployments_proj_status ON deployments(project_id, status, id DESC);
CREATE INDEX IF NOT EXISTS idx_deployments_active ON deployments(status, id DESC) WHERE status IN ('running', 'pending');

CREATE TABLE IF NOT EXISTS releases (
    id BIGSERIAL PRIMARY KEY,
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    sha TEXT NOT NULL,
    short_sha TEXT NOT NULL,
    message TEXT,
    author TEXT,
    committed_at BIGINT,
    dir TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at BIGINT NOT NULL,
    deployed_at BIGINT,
    deployment_id BIGINT REFERENCES deployments(id) ON DELETE SET NULL,
    build_status TEXT NOT NULL DEFAULT 'pending',
    health_status TEXT NOT NULL DEFAULT 'pending'
);
CREATE INDEX IF NOT EXISTS idx_releases_proj ON releases(project_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_releases_dep ON releases(deployment_id);

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'fk_deployments_release_id') THEN
        ALTER TABLE deployments
            ADD CONSTRAINT fk_deployments_release_id
            FOREIGN KEY (release_id) REFERENCES releases(id) ON DELETE SET NULL;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'fk_deployments_from_release_id') THEN
        ALTER TABLE deployments
            ADD CONSTRAINT fk_deployments_from_release_id
            FOREIGN KEY (from_release_id) REFERENCES releases(id) ON DELETE SET NULL;
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS logs (
    id BIGSERIAL PRIMARY KEY,
    project_id BIGINT REFERENCES projects(id) ON DELETE CASCADE,
    deployment_id BIGINT REFERENCES deployments(id) ON DELETE CASCADE,
    channel TEXT NOT NULL DEFAULT 'system',
    level TEXT NOT NULL DEFAULT 'info',
    message TEXT NOT NULL,
    ts BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_logs_proj ON logs(project_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_logs_dep ON logs(deployment_id, id ASC);
CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs(ts DESC);

CREATE TABLE IF NOT EXISTS users (
    id BIGSERIAL PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    disabled BOOLEAN NOT NULL DEFAULT FALSE,
    permissions TEXT NOT NULL DEFAULT '["*"]',
    allowed_projects TEXT NOT NULL DEFAULT '["*"]',
    totp_secret TEXT,
    totp_pending_secret TEXT,
    totp_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    totp_backup_codes TEXT NOT NULL DEFAULT '[]',
    created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at BIGINT NOT NULL,
    last_active_at BIGINT NOT NULL DEFAULT 0,
    csrf_token TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);
CREATE INDEX IF NOT EXISTS idx_sessions_last_active ON sessions(last_active_at) WHERE last_active_at > 0;

CREATE TABLE IF NOT EXISTS backups (
    id BIGSERIAL PRIMARY KEY,
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_at BIGINT NOT NULL DEFAULT 0,
    path TEXT NOT NULL DEFAULT '',
    size_bytes BIGINT NOT NULL DEFAULT 0,
    kind TEXT NOT NULL DEFAULT 'full-snapshot',
    note TEXT NOT NULL DEFAULT '',
    release_id BIGINT REFERENCES releases(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_backups_proj ON backups(project_id, id DESC);

CREATE TABLE IF NOT EXISTS audit (
    id BIGSERIAL PRIMARY KEY,
    ts BIGINT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    detail TEXT,
    ip TEXT,
    project_id BIGINT REFERENCES projects(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit(ts DESC);
CREATE INDEX IF NOT EXISTS idx_audit_ts_id ON audit(ts DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_audit_proj ON audit(project_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_audit_proj_ts_id ON audit(project_id, ts DESC, id DESC);

CREATE TABLE IF NOT EXISTS project_keys (
    project_id BIGINT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    public_key TEXT NOT NULL,
    private_key TEXT NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS user_passkeys (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    credential_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    public_key TEXT NOT NULL,
    alg INTEGER NOT NULL DEFAULT -7,
    sign_count INTEGER NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    last_used_at BIGINT
);
CREATE INDEX IF NOT EXISTS idx_user_passkeys_user ON user_passkeys(user_id, id DESC);

CREATE TABLE IF NOT EXISTS databases (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    engine TEXT NOT NULL DEFAULT 'postgres',
    db_name TEXT NOT NULL,
    db_user TEXT NOT NULL,
    db_password TEXT NOT NULL,
    port INTEGER NOT NULL,
    is_superuser BOOLEAN NOT NULL DEFAULT FALSE,
    domain TEXT NOT NULL DEFAULT '',
    access_mode TEXT NOT NULL DEFAULT 'local',
    allowed_ips TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'stopped',
    container_id TEXT,
    started_at BIGINT,
    data_dir TEXT NOT NULL DEFAULT '',
    linked_project_id BIGINT REFERENCES projects(id) ON DELETE SET NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_databases_port ON databases(port);
CREATE INDEX IF NOT EXISTS idx_databases_linked_project ON databases(linked_project_id);

INSERT INTO settings(key, value) VALUES ('min_free_mb', '500') ON CONFLICT(key) DO NOTHING;
INSERT INTO settings(key, value) VALUES ('default_retention', '10') ON CONFLICT(key) DO NOTHING;
INSERT INTO settings(key, value) VALUES ('step_timeout_s', '900') ON CONFLICT(key) DO NOTHING;
INSERT INTO settings(key, value) VALUES ('deploy_timeout_s', '1800') ON CONFLICT(key) DO NOTHING;
INSERT INTO _meta(key, value) VALUES('schema_version', '4') ON CONFLICT(key) DO UPDATE SET value = EXCLUDED.value;
