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
