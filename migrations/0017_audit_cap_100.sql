-- Each website, service and database keeps its newest 100 activity entries (it was 50 for
-- websites and services, unlimited for databases). Websites and services are already within
-- 100; database entries are tagged 'DB #<id>' and are trimmed here once.
CREATE INDEX IF NOT EXISTS idx_audit_db_target ON audit(LOWER(target), id DESC) WHERE project_id IS NULL;
DELETE FROM audit a
USING (
    SELECT id, ROW_NUMBER() OVER (PARTITION BY LOWER(target) ORDER BY id DESC) AS rn
    FROM audit
    WHERE project_id IS NULL AND target ~* '^DB #[0-9]+$'
) r
WHERE a.id = r.id AND r.rn > 100;
