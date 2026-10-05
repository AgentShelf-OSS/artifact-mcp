ALTER TABLE collections
  ADD COLUMN created_by_kind TEXT NOT NULL DEFAULT 'email'
    CHECK (created_by_kind IN ('email', 'api_key', 'oauth'));
