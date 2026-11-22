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
