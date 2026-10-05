CREATE TABLE collections (
  id TEXT PRIMARY KEY,
  org TEXT NOT NULL,
  name TEXT NOT NULL,
  name_key TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  color TEXT,
  created_by TEXT NOT NULL,
  cover_artifact_id TEXT,
  cover_artifact_org TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (id, org),
  UNIQUE (org, name_key),
  FOREIGN KEY (org) REFERENCES orgs(name) ON DELETE CASCADE,
  FOREIGN KEY (cover_artifact_id, cover_artifact_org)
    REFERENCES artifacts(id, org) ON DELETE SET NULL
);
CREATE INDEX collections_org_order_idx ON collections(org, created_at ASC, id ASC);
CREATE TABLE collection_artifacts (
  collection_id TEXT NOT NULL,
  artifact_id TEXT NOT NULL,
  org TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (collection_id, artifact_id),
  FOREIGN KEY (collection_id, org) REFERENCES collections(id, org) ON DELETE CASCADE,
  FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
);
CREATE INDEX collection_artifacts_artifact_idx
  ON collection_artifacts(org, artifact_id, collection_id);
CREATE TRIGGER collection_artifacts_before_artifact_org_move
BEFORE UPDATE OF org ON artifacts
WHEN NEW.org <> OLD.org
BEGIN
  DELETE FROM collection_artifacts
    WHERE artifact_id = OLD.id AND org = OLD.org;
  UPDATE collections
    SET cover_artifact_id = NULL, cover_artifact_org = NULL, updated_at = datetime('now')
    WHERE cover_artifact_id = OLD.id AND cover_artifact_org = OLD.org;
END;
CREATE TABLE gallery_preferences (
  viewer_email TEXT PRIMARY KEY,
  view TEXT NOT NULL DEFAULT 'reel' CHECK (view IN ('reel','sheets','ribbons','all')),
  preview_size TEXT NOT NULL DEFAULT 'compact' CHECK (preview_size IN ('compact','large')),
  artifact_layout TEXT NOT NULL DEFAULT 'grid' CHECK (artifact_layout IN ('grid','list')),
  state_json TEXT NOT NULL DEFAULT '{}',
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
