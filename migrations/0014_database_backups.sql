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
