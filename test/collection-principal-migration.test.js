// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { migrateDatabase, migrateDatabaseThrough } from "../lib/migrations.js";

test("schema 38 preserves browser folder creators from a populated schema 37 database", () => {
  const db = new Database(":memory:");
  try {
    db.pragma("foreign_keys = ON");
    migrateDatabaseThrough(db, 37);
    db.prepare("INSERT INTO orgs(name) VALUES ('migration-org')").run();
    db.prepare("INSERT INTO collections(id,org,name,name_key,created_by) VALUES (?,?,?,?,?)")
      .run("existingfolder", "migration-org", "Existing folder", "existing folder", "Alice@example.test");
    const before = db.prepare("SELECT * FROM collections").get();
    migrateDatabase(db);
    const after = db.prepare("SELECT * FROM collections").get();
    assert.equal(after.created_by_kind, "email");
    for (const [key, value] of Object.entries(before)) assert.equal(after[key], value, key);
    assert.equal(db.pragma("quick_check", { simple: true }), "ok");
    assert.deepEqual(db.pragma("foreign_key_check"), []);
    assert.throws(() => db.prepare("UPDATE collections SET created_by_kind='unknown'").run(), /CHECK/);
    migrateDatabase(db);
    assert.deepEqual(db.prepare("SELECT * FROM collections").get(), after);
  } finally { db.close(); }
});
