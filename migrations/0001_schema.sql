-- WebDep control database schema.
-- Never edit this file after a release: add 0002_*.sql and register it in src/pg_control.rs.

-- ===== Accounts =====
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT INTO settings(key, value) VALUES
    ('min_free_mb', '500'),
    ('default_retention', '10'),
    ('step_timeout_s', '900'),
    ('deploy_timeout_s', '1800');

CREATE TABLE users (
    id                   BIGSERIAL PRIMARY KEY,
    username             TEXT NOT NULL UNIQUE,
    password_hash        TEXT NOT NULL,
    disabled             BOOLEAN NOT NULL DEFAULT FALSE,
    totp_secret          TEXT,
    totp_pending_secret  TEXT,
    totp_enabled         BOOLEAN NOT NULL DEFAULT FALSE,
    totp_backup_codes    TEXT NOT NULL DEFAULT '[]',
    created_at           BIGINT NOT NULL,
    email                TEXT,
    display_name         TEXT NOT NULL DEFAULT '',
    role                 TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('admin', 'user')),
    must_change_password BOOLEAN NOT NULL DEFAULT FALSE,
    last_login_at        BIGINT
);
CREATE UNIQUE INDEX idx_users_email_lower ON users(LOWER(email)) WHERE email IS NOT NULL;

CREATE TABLE sessions (
    id             TEXT PRIMARY KEY,
    user_id        BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at     BIGINT NOT NULL,
    last_active_at BIGINT NOT NULL DEFAULT 0,
    csrf_token     TEXT NOT NULL,
    -- Where and when each session started, for the sessions list.
    created_at     BIGINT NOT NULL DEFAULT 0,
    ip             TEXT NOT NULL DEFAULT '',
    user_agent     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_expires ON sessions(expires_at);
CREATE INDEX idx_sessions_last_active ON sessions(last_active_at) WHERE last_active_at > 0;

CREATE TABLE user_passkeys (
    id            BIGSERIAL PRIMARY KEY,
    user_id       BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    credential_id TEXT NOT NULL UNIQUE,
    name          TEXT NOT NULL,
    public_key    TEXT NOT NULL,
    alg           INTEGER NOT NULL DEFAULT -7,
    sign_count    INTEGER NOT NULL DEFAULT 0,
    created_at    BIGINT NOT NULL,
    last_used_at  BIGINT
);
CREATE INDEX idx_user_passkeys_user ON user_passkeys(user_id, id DESC);

-- Per-user, per-resource access grants.
CREATE TABLE access_grants (
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
CREATE UNIQUE INDEX idx_access_grants_unique ON access_grants(user_id, scope_type, COALESCE(scope_id, 0));
CREATE INDEX idx_access_grants_user ON access_grants(user_id);
CREATE INDEX idx_access_grants_scope ON access_grants(scope_type, scope_id);
CREATE INDEX idx_access_grants_expires ON access_grants(expires_at) WHERE expires_at IS NOT NULL;

CREATE TABLE registration_requests (
    id            BIGSERIAL PRIMARY KEY,
    email         TEXT NOT NULL,
    username      TEXT NOT NULL,
    display_name  TEXT NOT NULL DEFAULT '',
    password_hash TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'rejected')),
    reason        TEXT,
    ip            TEXT,
    created_at    BIGINT NOT NULL,
    reviewed_by   BIGINT REFERENCES users(id) ON DELETE SET NULL,
    reviewed_at   BIGINT,
    user_id       BIGINT REFERENCES users(id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX idx_registration_pending_email ON registration_requests(LOWER(email)) WHERE status = 'pending';
CREATE UNIQUE INDEX idx_registration_pending_username ON registration_requests(LOWER(username)) WHERE status = 'pending';
CREATE INDEX idx_registration_status ON registration_requests(status, created_at DESC);

-- One-time password reset links an admin hands to a user. Only a hash of the token is stored.
CREATE TABLE password_resets (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    created_by BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    used_at    BIGINT
);
CREATE INDEX idx_password_resets_user ON password_resets(user_id);

-- ===== Cluster =====
CREATE TABLE nodes (
    id                 BIGSERIAL PRIMARY KEY,
    -- Stable identity, set at enrollment. display_name is the operator's optional label.
    name               TEXT NOT NULL UNIQUE,
    region             TEXT NOT NULL DEFAULT 'default',
    tags_json          TEXT NOT NULL DEFAULT '[]',
    status             TEXT NOT NULL DEFAULT 'offline' CHECK (status IN ('online', 'offline', 'draining')),
    is_local           BOOLEAN NOT NULL DEFAULT FALSE,
    public_ip          TEXT NOT NULL DEFAULT '',
    mesh_ip            TEXT NOT NULL DEFAULT '',
    wg_pubkey          TEXT NOT NULL DEFAULT '',
    wg_endpoint        TEXT NOT NULL DEFAULT '',
    node_token_hash    TEXT NOT NULL DEFAULT '',
    node_version       TEXT NOT NULL DEFAULT '',
    capabilities_json  TEXT NOT NULL DEFAULT '{}',
    metrics_json       TEXT NOT NULL DEFAULT '{}',
    last_seen_at       BIGINT,
    created_at         BIGINT NOT NULL,
    updated_at         BIGINT NOT NULL,
    display_name       TEXT NOT NULL DEFAULT '',
    -- Port pool range, opened in the firewall and handed out to apps lowest-first.
    port_min           INTEGER,
    port_max           INTEGER,
    -- 'ok', 'pending' (changes still have to reach the node) or 'failed' (see firewall_error).
    firewall_state     TEXT NOT NULL DEFAULT 'pending',
    firewall_error     TEXT,
    firewall_synced_at BIGINT
);
CREATE INDEX idx_nodes_status ON nodes(status);
CREATE INDEX idx_nodes_mesh_ip ON nodes(mesh_ip);

CREATE TABLE node_enroll_tokens (
    id           BIGSERIAL PRIMARY KEY,
    token_hash   TEXT NOT NULL UNIQUE,
    label        TEXT NOT NULL DEFAULT '',
    region       TEXT NOT NULL DEFAULT 'default',
    tags_json    TEXT NOT NULL DEFAULT '[]',
    created_by   BIGINT REFERENCES users(id) ON DELETE SET NULL,
    expires_at   BIGINT NOT NULL,
    used_at      BIGINT,
    used_node_id BIGINT REFERENCES nodes(id) ON DELETE SET NULL,
    created_at   BIGINT NOT NULL,
    port_min     INTEGER,
    port_max     INTEGER
);
CREATE INDEX idx_node_enroll_expires ON node_enroll_tokens(expires_at);

-- Public IP addresses an admin added to a node. Caddy can serve an app on one of them.
CREATE TABLE node_ips (
    id         BIGSERIAL PRIMARY KEY,
    node_id    BIGINT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    ip         TEXT NOT NULL,
    label      TEXT NOT NULL DEFAULT '',
    created_at BIGINT NOT NULL,
    UNIQUE (node_id, ip)
);

-- ===== Websites and services =====
CREATE TABLE projects (
    id                 BIGSERIAL PRIMARY KEY,
    name               TEXT NOT NULL,
    slug               TEXT NOT NULL UNIQUE,
    repo_url           TEXT NOT NULL,
    branch             TEXT NOT NULL DEFAULT 'main',
    project_type       TEXT NOT NULL DEFAULT 'node',
    deploy_method      TEXT NOT NULL DEFAULT 'managed',
    deploy_dir         TEXT NOT NULL,
    port               INTEGER,
    install_cmd        TEXT NOT NULL DEFAULT '',
    build_cmd          TEXT NOT NULL DEFAULT '',
    start_cmd          TEXT NOT NULL DEFAULT '',
    health_url         TEXT NOT NULL DEFAULT '',
    health_timeout_s   INTEGER NOT NULL DEFAULT 30,
    stability_s        INTEGER NOT NULL DEFAULT 5,
    auto_deploy        BOOLEAN NOT NULL DEFAULT FALSE,
    webhook_secret     TEXT,
    current_release_id BIGINT,
    deployed_sha       TEXT,
    last_poll_at       BIGINT,
    app_status         TEXT NOT NULL DEFAULT 'stopped',
    app_pid            BIGINT,
    app_started_at     BIGINT,
    domains            TEXT NOT NULL DEFAULT '[]',
    ssl_email          TEXT NOT NULL DEFAULT '',
    project_kind       TEXT NOT NULL DEFAULT 'web',
    access_mode        TEXT NOT NULL DEFAULT 'local',
    allowed_ips        TEXT NOT NULL DEFAULT '',
    created_at         BIGINT NOT NULL,
    updated_at         BIGINT NOT NULL,
    node_id            BIGINT REFERENCES nodes(id) ON DELETE SET NULL,
    placement_version  BIGINT NOT NULL DEFAULT 1,
    assigned_port      INTEGER,
    bind_ip            TEXT NOT NULL DEFAULT '',
    -- Limits: 0 means "no limit". CPU is a percentage of one core (150 = one and a half).
    cpu_limit          INTEGER NOT NULL DEFAULT 0,
    memory_limit_mb    INTEGER NOT NULL DEFAULT 0,
    disk_limit_mb      BIGINT NOT NULL DEFAULT 0,
    backup_limit       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_projects_created ON projects(created_at DESC, id DESC);

CREATE TABLE project_env (
    project_id BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value_enc  TEXT NOT NULL,
    is_secret  BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY(project_id, key)
);

CREATE TABLE project_keys (
    project_id  BIGINT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    public_key  TEXT NOT NULL,
    private_key TEXT NOT NULL,
    created_at  BIGINT NOT NULL
);

CREATE TABLE deployments (
    id              BIGSERIAL PRIMARY KEY,
    project_id      BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    release_id      BIGINT,
    from_release_id BIGINT,
    kind            TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',
    stage           TEXT NOT NULL DEFAULT 'pending',
    sha             TEXT,
    trigger         TEXT NOT NULL DEFAULT 'manual',
    actor           TEXT NOT NULL DEFAULT 'system',
    error           TEXT,
    started_at      BIGINT NOT NULL,
    finished_at     BIGINT
);
CREATE INDEX idx_deployments_proj ON deployments(project_id, id DESC);
CREATE INDEX idx_deployments_proj_status ON deployments(project_id, status, id DESC);
CREATE INDEX idx_deployments_active ON deployments(status, id DESC) WHERE status IN ('running', 'pending');

CREATE TABLE releases (
    id            BIGSERIAL PRIMARY KEY,
    project_id    BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    sha           TEXT NOT NULL,
    short_sha     TEXT NOT NULL,
    message       TEXT,
    author        TEXT,
    committed_at  BIGINT,
    dir           TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending',
    created_at    BIGINT NOT NULL,
    deployed_at   BIGINT,
    deployment_id BIGINT REFERENCES deployments(id) ON DELETE SET NULL,
    build_status  TEXT NOT NULL DEFAULT 'pending',
    health_status TEXT NOT NULL DEFAULT 'pending'
);
CREATE INDEX idx_releases_proj ON releases(project_id, id DESC);
CREATE INDEX idx_releases_dep ON releases(deployment_id);

-- deployments and releases point at each other, so these come after both tables exist.
ALTER TABLE deployments
    ADD CONSTRAINT fk_deployments_release_id
        FOREIGN KEY (release_id) REFERENCES releases(id) ON DELETE SET NULL,
    ADD CONSTRAINT fk_deployments_from_release_id
        FOREIGN KEY (from_release_id) REFERENCES releases(id) ON DELETE SET NULL;

CREATE TABLE logs (
    id            BIGSERIAL PRIMARY KEY,
    project_id    BIGINT REFERENCES projects(id) ON DELETE CASCADE,
    deployment_id BIGINT REFERENCES deployments(id) ON DELETE CASCADE,
    channel       TEXT NOT NULL DEFAULT 'system',
    level         TEXT NOT NULL DEFAULT 'info',
    message       TEXT NOT NULL,
    ts            BIGINT NOT NULL
);
CREATE INDEX idx_logs_proj ON logs(project_id, id DESC);
CREATE INDEX idx_logs_dep ON logs(deployment_id, id ASC);
CREATE INDEX idx_logs_ts ON logs(ts DESC);

CREATE TABLE backups (
    id               BIGSERIAL PRIMARY KEY,
    project_id       BIGINT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_at       BIGINT NOT NULL DEFAULT 0,
    path             TEXT NOT NULL DEFAULT '',
    size_bytes       BIGINT NOT NULL DEFAULT 0,
    kind             TEXT NOT NULL DEFAULT 'full-snapshot',
    note             TEXT NOT NULL DEFAULT '',
    release_id       BIGINT REFERENCES releases(id) ON DELETE SET NULL,
    storage_mode     TEXT NOT NULL DEFAULT 'local',
    core_backup_path TEXT NOT NULL DEFAULT '',
    source_node_id   BIGINT,
    status           TEXT NOT NULL DEFAULT 'complete',
    error            TEXT NOT NULL DEFAULT '',
    checksum_sha256  TEXT NOT NULL DEFAULT '',
    file_count       BIGINT NOT NULL DEFAULT 0,
    locked           BOOLEAN NOT NULL DEFAULT FALSE,
    created_by       TEXT NOT NULL DEFAULT '',
    completed_at     BIGINT NOT NULL DEFAULT 0,
    archive_path     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_backups_proj ON backups(project_id, id DESC);
CREATE INDEX idx_backups_running ON backups(project_id) WHERE status = 'running';

-- Moves of an app from one node to another.
CREATE TABLE migrations (
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
    finished_at     BIGINT,
    initiated_by    TEXT NOT NULL DEFAULT '',
    from_ip         TEXT NOT NULL DEFAULT '',
    to_ip           TEXT NOT NULL DEFAULT '',
    target_port     BIGINT NOT NULL DEFAULT 0,
    domains_json    TEXT NOT NULL DEFAULT '[]',
    cutover_mode    TEXT NOT NULL DEFAULT 'dns',
    notes           TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_migrations_proj ON migrations(project_id, id DESC);

-- ===== Databases =====
CREATE TABLE databases (
    id                BIGSERIAL PRIMARY KEY,
    name              TEXT NOT NULL,
    engine            TEXT NOT NULL DEFAULT 'postgres',
    db_name           TEXT NOT NULL,
    db_user           TEXT NOT NULL,
    db_password       TEXT NOT NULL,
    port              INTEGER NOT NULL,
    is_superuser      BOOLEAN NOT NULL DEFAULT FALSE,
    domain            TEXT NOT NULL DEFAULT '',
    access_mode       TEXT NOT NULL DEFAULT 'local',
    allowed_ips       TEXT NOT NULL DEFAULT '',
    status            TEXT NOT NULL DEFAULT 'stopped',
    container_id      TEXT,
    started_at        BIGINT,
    data_dir          TEXT NOT NULL DEFAULT '',
    linked_project_id BIGINT REFERENCES projects(id) ON DELETE SET NULL,
    created_at        BIGINT NOT NULL,
    updated_at        BIGINT NOT NULL,
    node_id           BIGINT REFERENCES nodes(id) ON DELETE SET NULL,
    cpu_limit         INTEGER NOT NULL DEFAULT 0,
    memory_limit_mb   INTEGER NOT NULL DEFAULT 0,
    disk_limit_mb     BIGINT NOT NULL DEFAULT 0,
    backup_limit      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_databases_port ON databases(port);
CREATE INDEX idx_databases_linked_project ON databases(linked_project_id);

-- One logical dump file (pg_dump custom format) per backup.
CREATE TABLE database_backups (
    id              BIGSERIAL PRIMARY KEY,
    database_id     BIGINT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    created_at      BIGINT NOT NULL DEFAULT 0,
    completed_at    BIGINT NOT NULL DEFAULT 0,
    status          TEXT NOT NULL DEFAULT 'running',
    error           TEXT NOT NULL DEFAULT '',
    size_bytes      BIGINT NOT NULL DEFAULT 0,
    checksum_sha256 TEXT NOT NULL DEFAULT '',
    note            TEXT NOT NULL DEFAULT '',
    locked          BOOLEAN NOT NULL DEFAULT FALSE,
    created_by      TEXT NOT NULL DEFAULT '',
    archive_path    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_database_backups_db ON database_backups(database_id, id DESC);

CREATE TABLE node_ports (
    node_id     BIGINT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    port        INTEGER NOT NULL CHECK (port BETWEEN 1024 AND 65535),
    -- Assignable to apps. `public` separately controls the internet firewall rule.
    is_open     BOOLEAN NOT NULL DEFAULT TRUE,
    -- Added by hand rather than from the range: kept when the range changes.
    manual      BOOLEAN NOT NULL DEFAULT FALSE,
    -- Extra port reserved for an app (its main port lives in projects.port or databases.port).
    project_id  BIGINT REFERENCES projects(id) ON DELETE SET NULL,
    updated_at  BIGINT NOT NULL,
    public      BOOLEAN NOT NULL DEFAULT FALSE,
    database_id BIGINT REFERENCES databases(id) ON DELETE SET NULL,
    PRIMARY KEY (node_id, port)
);
CREATE INDEX idx_node_ports_open ON node_ports(node_id, is_open, port);
CREATE INDEX idx_node_ports_project ON node_ports(project_id) WHERE project_id IS NOT NULL;
CREATE INDEX idx_node_ports_database ON node_ports(database_id) WHERE database_id IS NOT NULL;

-- ===== Activity =====
CREATE TABLE audit (
    id          BIGSERIAL PRIMARY KEY,
    ts          BIGINT NOT NULL,
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    target      TEXT,
    detail      TEXT,
    ip          TEXT,
    project_id  BIGINT REFERENCES projects(id) ON DELETE SET NULL,
    database_id BIGINT REFERENCES databases(id) ON DELETE SET NULL
);
CREATE INDEX idx_audit_ts ON audit(ts DESC);
CREATE INDEX idx_audit_ts_id ON audit(ts DESC, id DESC);
CREATE INDEX idx_audit_proj ON audit(project_id, id DESC);
CREATE INDEX idx_audit_proj_ts_id ON audit(project_id, ts DESC, id DESC);
CREATE INDEX idx_audit_database ON audit(database_id, id DESC) WHERE database_id IS NOT NULL;
CREATE INDEX idx_audit_db_target ON audit(LOWER(target), id DESC) WHERE project_id IS NULL;

-- Times an app reached or crossed one of its limits (cpu, ram, disk), shown once as a pop-up.
CREATE TABLE limit_events (
    id         BIGSERIAL PRIMARY KEY,
    created_at BIGINT NOT NULL DEFAULT 0,
    kind       TEXT NOT NULL,
    app_id     BIGINT NOT NULL,
    app_name   TEXT NOT NULL DEFAULT '',
    limit_kind TEXT NOT NULL,
    message    TEXT NOT NULL DEFAULT '',
    stopped    BOOLEAN NOT NULL DEFAULT FALSE
);
