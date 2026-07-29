#!/usr/bin/env node
// Registers categories already in use by artifacts into org_categories, so they appear in the
// Settings picker.
//
// WHY THIS IS A ONE-OFF SCRIPT AND NOT A STARTUP BACKFILL
//
// Deleting a category chip in Settings removes the row from org_categories but deliberately leaves
// the artifacts alone — they keep their category text. If this ran at every boot it would resurrect
// any category an admin had just deleted, and the delete button would look broken. So this is run
// deliberately, once, by an operator.
//
// WHY THERE IS NO RUST TWIN
//
// It operates on the shared SQLite database, not on application behaviour: running it once fixes
// the data for whichever implementation serves it. It is an ops tool, so it introduces no
// Node/Rust parity divergence.
//
// Normalization needs no special handling: `normalizeCategory` in lib/store.js and `normCategory`
// in lib/orgs.js are character-for-character identical (trim, collapse whitespace runs, slice 60),
// so a category stored on an artifact is already in registry form.
//
// Usage:
//   node scripts/backfill-categories.mjs [--apply] [--data-dir DIR]
//
// Defaults to a DRY RUN. Idempotent: uses INSERT OR IGNORE, so re-running is a no-op.

import Database from "better-sqlite3";
import path from "node:path";

const args = process.argv.slice(2);
const apply = args.includes("--apply");
const dirFlag = args.indexOf("--data-dir");
const dataDir = dirFlag !== -1 ? args[dirFlag + 1] : process.env.DATA_DIR || "/data";
const dbPath = path.join(dataDir, "artifacts.db");

const db = new Database(dbPath, { readonly: !apply });

// org_categories.org REFERENCES orgs(name), so a category whose org has no registry row cannot be
// inserted. Those are reported rather than silently dropped.
const rows = db
  .prepare(
    `SELECT a.org AS org, a.category AS category, COUNT(*) AS artifacts,
            EXISTS(SELECT 1 FROM orgs o WHERE o.name = a.org) AS org_known
     FROM artifacts a
     WHERE TRIM(COALESCE(a.category, '')) <> ''
       AND NOT EXISTS (
         SELECT 1 FROM org_categories c WHERE c.org = a.org AND c.name = a.category
       )
     GROUP BY a.org, a.category
     ORDER BY a.org, a.category`
  )
  .all();

const insertable = rows.filter((r) => r.org_known);
const orphans = rows.filter((r) => !r.org_known);

console.log(`database: ${dbPath}`);
console.log(`unregistered categories in use: ${rows.length}`);
for (const r of insertable) {
  console.log(`  + ${r.org} / "${r.category}"  (${r.artifacts} artifact${r.artifacts === 1 ? "" : "s"})`);
}
for (const r of orphans) {
  console.log(`  ! ${r.org} / "${r.category}"  SKIPPED — no such organization in the registry`);
}

if (!apply) {
  console.log(`\nDRY RUN — nothing written. Re-run with --apply to insert ${insertable.length} row(s).`);
  process.exit(0);
}

const insert = db.prepare("INSERT OR IGNORE INTO org_categories (org, name) VALUES (?, ?)");
const run = db.transaction((items) => {
  let added = 0;
  for (const item of items) added += insert.run(item.org, item.category).changes;
  return added;
});
const added = run(insertable);
console.log(`\napplied: ${added} row(s) inserted${orphans.length ? `, ${orphans.length} skipped` : ""}.`);
