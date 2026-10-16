-- ADR-0015: optional publisher-chosen artifact key for publish upsert.
-- At most one artifact per (org, artifact_key). Deletion removes the row, which frees the key.
ALTER TABLE artifacts ADD COLUMN artifact_key TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS artifacts_org_artifact_key
  ON artifacts(org, artifact_key) WHERE artifact_key IS NOT NULL;
