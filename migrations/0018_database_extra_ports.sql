-- A database can hold extra ports from its node's pool, like websites and services do.
ALTER TABLE node_ports ADD COLUMN IF NOT EXISTS database_id BIGINT REFERENCES databases(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_node_ports_database ON node_ports(database_id) WHERE database_id IS NOT NULL;
