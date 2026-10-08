CREATE TABLE IF NOT EXISTS push_subscriptions (
  id TEXT PRIMARY KEY,
  org TEXT NOT NULL,
  viewer_email TEXT NOT NULL,
  endpoint_ciphertext TEXT NOT NULL,
  endpoint_sha256 TEXT NOT NULL UNIQUE,
  p256dh TEXT NOT NULL,
  auth TEXT NOT NULL,
  label TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  last_success_at TEXT,
  failure_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS push_subscriptions_viewer ON push_subscriptions(org, viewer_email);

CREATE TABLE IF NOT EXISTS artifact_push_optins (
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  viewer_email TEXT NOT NULL,
  org TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (artifact_id, viewer_email)
);

CREATE TABLE IF NOT EXISTS artifact_reminders (
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  scope TEXT NOT NULL CHECK (scope IN ('org','viewer')),
  owner TEXT NOT NULL DEFAULT '',
  key TEXT NOT NULL,
  org TEXT NOT NULL,
  fire_at INTEGER NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'armed' CHECK (state IN ('armed','fired')),
  revision INTEGER NOT NULL DEFAULT 1,
  created_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  fired_at TEXT,
  PRIMARY KEY (artifact_id, scope, owner, key)
);
CREATE INDEX IF NOT EXISTS artifact_reminders_due ON artifact_reminders(state, fire_at);

CREATE TABLE IF NOT EXISTS push_deliveries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  subscription_id TEXT NOT NULL REFERENCES push_subscriptions(id) ON DELETE CASCADE,
  artifact_id TEXT NOT NULL,
  reminder_key TEXT NOT NULL,
  payload TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','accepted','dead')),
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  last_status INTEGER,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS push_deliveries_due ON push_deliveries(state, next_attempt_at);
