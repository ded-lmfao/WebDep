-- Backups become single archives with a status and a checksum.
-- Existing backups keep working: they are marked complete and have no archive_path.
ALTER TABLE backups ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'complete';
ALTER TABLE backups ADD COLUMN IF NOT EXISTS error TEXT NOT NULL DEFAULT '';
ALTER TABLE backups ADD COLUMN IF NOT EXISTS checksum_sha256 TEXT NOT NULL DEFAULT '';
ALTER TABLE backups ADD COLUMN IF NOT EXISTS file_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE backups ADD COLUMN IF NOT EXISTS locked BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE backups ADD COLUMN IF NOT EXISTS created_by TEXT NOT NULL DEFAULT '';
ALTER TABLE backups ADD COLUMN IF NOT EXISTS completed_at BIGINT NOT NULL DEFAULT 0;
ALTER TABLE backups ADD COLUMN IF NOT EXISTS archive_path TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS idx_backups_running ON backups(project_id) WHERE status = 'running';
