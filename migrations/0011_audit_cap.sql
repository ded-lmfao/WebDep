-- Keep only the newest 50 audit entries per website or service. Panel-wide entries (no app)
-- are not affected; they keep the days-based retention setting.
DELETE FROM audit a
USING (
    SELECT id, ROW_NUMBER() OVER (PARTITION BY project_id ORDER BY id DESC) AS rn
    FROM audit
    WHERE project_id IS NOT NULL
) r
WHERE a.id = r.id AND r.rn > 50;
