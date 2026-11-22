-- Per-app limits. 0 means "no limit" for every field.
-- cpu_limit: percent of one core; memory_limit_mb / disk_limit_mb: megabytes;
-- backup_limit: how many backups the app may hold at a time.
ALTER TABLE projects ADD COLUMN IF NOT EXISTS disk_limit_mb BIGINT NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN IF NOT EXISTS backup_limit INTEGER NOT NULL DEFAULT 0;

ALTER TABLE databases ADD COLUMN IF NOT EXISTS cpu_limit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE databases ADD COLUMN IF NOT EXISTS memory_limit_mb INTEGER NOT NULL DEFAULT 0;
ALTER TABLE databases ADD COLUMN IF NOT EXISTS disk_limit_mb BIGINT NOT NULL DEFAULT 0;
ALTER TABLE databases ADD COLUMN IF NOT EXISTS backup_limit INTEGER NOT NULL DEFAULT 0;
