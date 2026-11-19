CREATE TABLE historical_deletion_checks (
    library TEXT NOT NULL, id TEXT NOT NULL, retry_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (library, id)
) WITHOUT ROWID;
CREATE INDEX historical_deletion_due ON historical_deletion_checks(library, retry_at, id);

INSERT INTO historical_deletion_checks(library,id)
SELECT DISTINCT library,id FROM assets a
WHERE status='downloaded' AND is_deleted=0 AND source='icloud'
  AND NOT EXISTS (SELECT 1 FROM unattributed_legacy p WHERE p.library=a.library AND p.asset_id=a.id);

-- A current observation supersedes this one-time upgrade check, including a
-- lookup already in flight. Do not rely on second-resolution timestamps.
CREATE TRIGGER historical_deletion_observed AFTER UPDATE OF last_seen_at, is_deleted ON assets
BEGIN
    DELETE FROM historical_deletion_checks WHERE library=NEW.library AND id=NEW.id;
END;
CREATE TRIGGER historical_deletion_inserted AFTER INSERT ON assets
BEGIN
    DELETE FROM historical_deletion_checks WHERE library=NEW.library AND id=NEW.id;
END;
