-- WebDep schema, final form. One idempotent script holding everything migrations 1 to 18 created.
--
-- A new database gets the whole schema from this. A database that already ran migrations 1 to 18
-- finds every table, column and index in place and changes nothing (those versions stay recorded
-- and are tolerated as unknown). Every statement here is safe to run again.
--
-- Left out on purpose: one-time data steps of the old migrations (promoting the first admin,
-- trimming old audit entries, hashing existing session ids). A new database has no such data and
-- the panel keeps all three true as it writes.

-- ===== 0001_initial =====
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

-- ===== 0002_access_grants =====
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

-- ===== 0003_cluster_nodes =====
-- Phase 3: Web-Core & Web-Node Cluster Registry, Enrollment Tokens, Peer Mesh & Placement Columns
CREATE TABLE IF NOT EXISTS nodes (
    id                BIGSERIAL PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    region            TEXT NOT NULL DEFAULT 'default',
    tags_json         TEXT NOT NULL DEFAULT '[]',
    status            TEXT NOT NULL DEFAULT 'offline' CHECK (status IN ('online', 'offline', 'draining')),
    is_local          BOOLEAN NOT NULL DEFAULT FALSE,
    public_ip         TEXT NOT NULL DEFAULT '',
    mesh_ip           TEXT NOT NULL DEFAULT '',
    wg_pubkey         TEXT NOT NULL DEFAULT '',
    wg_endpoint       TEXT NOT NULL DEFAULT '',
    node_token_hash   TEXT NOT NULL DEFAULT '',
    node_version      TEXT NOT NULL DEFAULT '',
    capabilities_json TEXT NOT NULL DEFAULT '{}',
    metrics_json      TEXT NOT NULL DEFAULT '{}',
    last_seen_at      BIGINT,
    created_at        BIGINT NOT NULL,
    updated_at        BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_nodes_status ON nodes(status);
CREATE INDEX IF NOT EXISTS idx_nodes_mesh_ip ON nodes(mesh_ip);

CREATE TABLE IF NOT EXISTS node_enroll_tokens (
    id           BIGSERIAL PRIMARY KEY,
    token_hash   TEXT NOT NULL UNIQUE,
    label        TEXT NOT NULL DEFAULT '',
    region       TEXT NOT NULL DEFAULT 'default',
    tags_json    TEXT NOT NULL DEFAULT '[]',
    created_by   BIGINT REFERENCES users(id) ON DELETE SET NULL,
    expires_at   BIGINT NOT NULL,
    used_at      BIGINT,
    used_node_id BIGINT REFERENCES nodes(id) ON DELETE SET NULL,
    created_at   BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_node_enroll_expires ON node_enroll_tokens(expires_at);

ALTER TABLE projects
    ADD COLUMN IF NOT EXISTS node_id BIGINT REFERENCES nodes(id) ON DELETE SET NULL;
ALTER TABLE projects
    ADD COLUMN IF NOT EXISTS placement_version BIGINT NOT NULL DEFAULT 1;
ALTER TABLE projects
    ADD COLUMN IF NOT EXISTS assigned_port INTEGER;

ALTER TABLE databases
    ADD COLUMN IF NOT EXISTS node_id BIGINT REFERENCES nodes(id) ON DELETE SET NULL;

CREATE TABLE IF NOT EXISTS migrations (
    id              BIGSERIAL PRIMARY KEY,
    project_id      BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    from_node_id    BIGINT REFERENCES nodes(id) ON DELETE SET NULL,
    to_node_id      BIGINT REFERENCES nodes(id) ON DELETE SET NULL,
    stage           TEXT NOT NULL DEFAULT 'pending',
    status          TEXT NOT NULL DEFAULT 'pending',
    dns_verified_at BIGINT,
    transfer_bytes  BIGINT NOT NULL DEFAULT 0,
    error           TEXT,
    started_at      BIGINT NOT NULL,
    finished_at     BIGINT
);

CREATE INDEX IF NOT EXISTS idx_migrations_proj ON migrations(project_id, id DESC);

-- ===== 0004_migrations_and_backups =====
-- Multi-node live app shifting metadata & off-node backup storage tracking
ALTER TABLE migrations
    ADD COLUMN IF NOT EXISTS initiated_by TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS from_ip TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS to_ip TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS target_port BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS domains_json TEXT NOT NULL DEFAULT '[]',
    ADD COLUMN IF NOT EXISTS cutover_mode TEXT NOT NULL DEFAULT 'dns',
    ADD COLUMN IF NOT EXISTS notes TEXT NOT NULL DEFAULT '';

ALTER TABLE backups
    ADD COLUMN IF NOT EXISTS storage_mode TEXT NOT NULL DEFAULT 'local',
    ADD COLUMN IF NOT EXISTS core_backup_path TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS source_node_id BIGINT;

-- ===== 0005_node_display_name =====
-- Optional operator-chosen label for a node. Empty means "use the node's own name".
-- Kept separate from nodes.name, which is the node's stable identity (unique, set at enrollment).
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS display_name TEXT NOT NULL DEFAULT '';

-- ===== 0006_user_management =====
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

-- ===== 0007_node_ports =====
-- Port pools per node: an admin-defined range (plus any extra ports added later) that
-- WebDep opens in the firewall and hands out to apps lowest-first. Nodes without a
-- range keep the old cluster-wide allocation.

ALTER TABLE nodes ADD COLUMN IF NOT EXISTS port_min INTEGER;
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS port_max INTEGER;

ALTER TABLE node_enroll_tokens ADD COLUMN IF NOT EXISTS port_min INTEGER;
ALTER TABLE node_enroll_tokens ADD COLUMN IF NOT EXISTS port_max INTEGER;

CREATE TABLE IF NOT EXISTS node_ports (
    node_id    BIGINT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    port       INTEGER NOT NULL CHECK (port BETWEEN 1024 AND 65535),
    is_open    BOOLEAN NOT NULL DEFAULT TRUE,
    -- Added by hand rather than from the range: kept when the range changes.
    manual     BOOLEAN NOT NULL DEFAULT FALSE,
    -- Extra port reserved for an app (its main port lives in projects.port).
    project_id BIGINT REFERENCES projects(id) ON DELETE SET NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (node_id, port)
);

CREATE INDEX IF NOT EXISTS idx_node_ports_open ON node_ports(node_id, is_open, port);
CREATE INDEX IF NOT EXISTS idx_node_ports_project ON node_ports(project_id) WHERE project_id IS NOT NULL;

-- ===== 0008_port_exposure =====
-- Port exposure and firewall sync state.
--
-- A pool port can be open (assignable to apps) without being reachable from the internet:
-- apps behind Caddy only need localhost or the mesh. `public` controls the firewall rule.
-- Existing pool ports start private: only Caddy's 80/443 stay publicly reachable.

ALTER TABLE node_ports ADD COLUMN IF NOT EXISTS public BOOLEAN NOT NULL DEFAULT FALSE;

-- 'ok' after a successful firewall sync, 'pending' when changes still have to reach the
-- node (offline, never synced), 'failed' when the last sync errored (see firewall_error).
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS firewall_state TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS firewall_error TEXT;
ALTER TABLE nodes ADD COLUMN IF NOT EXISTS firewall_synced_at BIGINT;

-- ===== 0009_account_security =====
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

-- ===== 0010_app_placement =====
-- Public IP addresses an admin has added to each node. Websites and services can be served on
-- one of them (Caddy binds to it); with none chosen they are served on every address.
CREATE TABLE IF NOT EXISTS node_ips (
    id          BIGSERIAL PRIMARY KEY,
    node_id     BIGINT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    ip          TEXT NOT NULL,
    label       TEXT NOT NULL DEFAULT '',
    created_at  BIGINT NOT NULL,
    UNIQUE (node_id, ip)
);

-- Per-app placement and resource limits. Empty / 0 means "not set".
ALTER TABLE projects ADD COLUMN IF NOT EXISTS bind_ip TEXT NOT NULL DEFAULT '';
-- CPU as a percentage of one core (150 = one and a half cores).
ALTER TABLE projects ADD COLUMN IF NOT EXISTS cpu_limit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN IF NOT EXISTS memory_limit_mb INTEGER NOT NULL DEFAULT 0;

-- ===== 0019_consolidated_schema =====
-- Per-app limits. 0 means "no limit" for every field.
ALTER TABLE projects ADD COLUMN IF NOT EXISTS disk_limit_mb BIGINT NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN IF NOT EXISTS backup_limit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE databases ADD COLUMN IF NOT EXISTS cpu_limit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE databases ADD COLUMN IF NOT EXISTS memory_limit_mb INTEGER NOT NULL DEFAULT 0;
ALTER TABLE databases ADD COLUMN IF NOT EXISTS disk_limit_mb BIGINT NOT NULL DEFAULT 0;
ALTER TABLE databases ADD COLUMN IF NOT EXISTS backup_limit INTEGER NOT NULL DEFAULT 0;

-- App backups are single archives with a status and a checksum.
ALTER TABLE backups ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'complete';
ALTER TABLE backups ADD COLUMN IF NOT EXISTS error TEXT NOT NULL DEFAULT '';
ALTER TABLE backups ADD COLUMN IF NOT EXISTS checksum_sha256 TEXT NOT NULL DEFAULT '';
ALTER TABLE backups ADD COLUMN IF NOT EXISTS file_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE backups ADD COLUMN IF NOT EXISTS locked BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE backups ADD COLUMN IF NOT EXISTS created_by TEXT NOT NULL DEFAULT '';
ALTER TABLE backups ADD COLUMN IF NOT EXISTS completed_at BIGINT NOT NULL DEFAULT 0;
ALTER TABLE backups ADD COLUMN IF NOT EXISTS archive_path TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS idx_backups_running ON backups(project_id) WHERE status = 'running';

-- Manual backups of managed databases: one logical dump file (pg_dump custom format) per backup.
CREATE TABLE IF NOT EXISTS database_backups (
    id BIGSERIAL PRIMARY KEY,
    database_id BIGINT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    created_at BIGINT NOT NULL DEFAULT 0,
    completed_at BIGINT NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'running',
    error TEXT NOT NULL DEFAULT '',
    size_bytes BIGINT NOT NULL DEFAULT 0,
    checksum_sha256 TEXT NOT NULL DEFAULT '',
    note TEXT NOT NULL DEFAULT '',
    locked BOOLEAN NOT NULL DEFAULT FALSE,
    created_by TEXT NOT NULL DEFAULT '',
    archive_path TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_database_backups_db ON database_backups(database_id, id DESC);

-- Times an app reached or crossed one of its limits (cpu, ram, disk), shown once as a pop-up.
CREATE TABLE IF NOT EXISTS limit_events (
    id BIGSERIAL PRIMARY KEY,
    created_at BIGINT NOT NULL DEFAULT 0,
    kind TEXT NOT NULL,
    app_id BIGINT NOT NULL,
    app_name TEXT NOT NULL DEFAULT '',
    limit_kind TEXT NOT NULL,
    message TEXT NOT NULL DEFAULT '',
    stopped BOOLEAN NOT NULL DEFAULT FALSE
);

-- Keeps trimming a database's activity entries fast.
CREATE INDEX IF NOT EXISTS idx_audit_db_target ON audit(LOWER(target), id DESC) WHERE project_id IS NULL;

-- A database can hold extra ports from its node's pool.
ALTER TABLE node_ports ADD COLUMN IF NOT EXISTS database_id BIGINT REFERENCES databases(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_node_ports_database ON node_ports(database_id) WHERE database_id IS NOT NULL;
