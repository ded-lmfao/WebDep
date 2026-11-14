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
