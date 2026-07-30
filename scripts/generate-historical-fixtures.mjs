#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
/**
 * Rebuild the synthetic historical fixture corpus.
 *
 * This is intentionally a release-engineering command, not a CI bootstrap: it reads the exact
 * public tag that created each schema, writes only below conformance/fixtures/historical/, and
 * then records every byte it wrote in the case manifest.  Run it from a clone with the release
 * tags available; CI verifies the committed result but never regenerates opaque SQLite files.
 */
import { createCipheriv, createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { access, mkdtemp, mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import Database from "better-sqlite3";
import { LATEST_SCHEMA_VERSION, migrateDatabaseThrough } from "../lib/migrations.js";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const FIXTURE_ROOT = join(ROOT, "conformance", "fixtures", "historical");
const FIXTURE_KEY = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; // 32 synthetic zero bytes
const FIXTURE_API_TOKEN = "fixture-public-token-v1"; // public test material, never a deployment credential
const CASES = [
  ...Array.from({ length: LATEST_SCHEMA_VERSION + 1 }, (_, originSchema) => ({
    name: `boundary-v${String(originSchema).padStart(2, "0")}`,
    tag: null,
    originSchema,
    boundary: true,
    webhookMode: "none",
    recovery: false,
  })),
  { name: "release-v16", tag: "v1.2.0", originSchema: 16, webhookMode: "none", recovery: false },
  { name: "release-v20", tag: "v1.4.0", originSchema: 20, webhookMode: "mixed", recovery: false },
  { name: "release-v21", tag: "v1.5.0", originSchema: 21, webhookMode: "mixed", recovery: false },
  { name: "release-v23-recovery", tag: "v1.6.0", originSchema: 23, webhookMode: "mixed", recovery: true },
  { name: "release-v24-durability-recovery", tag: null, originSchema: 24, webhookMode: "mixed", recovery: true, currentLedger: true, durabilityRecovery: true },
];

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function tagFile(tag, path) {
  return execFileSync("git", ["show", `${tag}:${path}`], { cwd: ROOT, encoding: "utf8" });
}

function gitRef(ref) {
  return execFileSync("git", ["rev-parse", `${ref}^{commit}`], { cwd: ROOT, encoding: "utf8" }).trim();
}

function columns(db, table) {
  return new Set(db.prepare(`PRAGMA table_info(${table})`).all().map((row) => row.name));
}

function tableExists(db, table) {
  return db.prepare("SELECT COUNT(*) AS count FROM sqlite_master WHERE type = 'table' AND name = ?").get(table).count > 0;
}

function insert(db, table, values) {
  if (!tableExists(db, table)) return;
  const available = columns(db, table);
  const entries = Object.entries(values).filter(([key]) => available.has(key));
  const keys = entries.map(([key]) => key);
  db.prepare(`INSERT INTO ${table} (${keys.join(", ")}) VALUES (${keys.map((key) => `@${key}`).join(", ")})`)
    .run(Object.fromEntries(entries));
}

function encrypted(url, nonceByte) {
  const cipher = createCipheriv("aes-256-gcm", Buffer.from(FIXTURE_KEY, "base64"), Buffer.alloc(12, nonceByte));
  const ciphertext = Buffer.concat([cipher.update(url, "utf8"), cipher.final()]);
  return {
    url: `https://discord.com/…${url.slice(-4)}`,
    url_cipher: ciphertext.toString("base64"),
    url_nonce: Buffer.alloc(12, nonceByte).toString("base64"),
    url_tag: cipher.getAuthTag().toString("base64"),
  };
}

async function writeBody(root, relativePath, contents) {
  const full = join(root, "artifacts", relativePath);
  await mkdir(dirname(full), { recursive: true });
  await writeFile(full, contents);
}

async function bodyManifest(root) {
  const base = join(root, "artifacts");
  const result = [];
  async function visit(dir) {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) await visit(full);
      else if (entry.isFile()) {
        const bytes = await (await import("node:fs/promises")).readFile(full);
        result.push({ path: relative(base, full).replaceAll("\\", "/"), bytes: bytes.length, sha256: sha256(bytes) });
      }
    }
  }
  await visit(base);
  return result.sort((left, right) => left.path.localeCompare(right.path));
}

async function historicalMigrations(tag) {
  const temp = await mkdtemp(join(tmpdir(), "artifact-mcp-fixture-migrations-"));
  let source = tagFile(tag, "lib/migrations.js");
  // v20+ imports crypto only to optionally encrypt existing rows. Fixtures deliberately create
  // that synthetic state explicitly below, so this tag-local no-key stub is faithful at migration
  // time and keeps the extracted module self-contained.
  source = source.replace(/^import .* from "\.\/crypto\.js";\n/m, "function parseEncryptionKey() { return null; }\nfunction warnIfWebhookEncryptionDisabled() {}\nfunction encrypt() { throw new Error('fixture migration must not encrypt'); }\n");
  const modulePath = join(temp, "migrations.mjs");
  await writeFile(modulePath, source);
  return { module: await import(`${pathToFileURL(modulePath).href}?${Date.now()}`), temp };
}

async function buildCase(spec) {
  const root = join(FIXTURE_ROOT, spec.name);
  // Historical origins are evidence, not generated build output. A normal --write may add a
  // newly introduced boundary but never rewrites a frozen source fixture; regeneration requires
  // an explicit destructive opt-in after provenance review.
  try {
    await access(root);
    if (!process.argv.includes("--replace-frozen-fixtures")) return;
  } catch {}
  await rm(root, { recursive: true, force: true });
  await mkdir(join(root, "artifacts"), { recursive: true });
  const historical = spec.boundary || spec.currentLedger ? null : await historicalMigrations(spec.tag);
  try {
    const dbPath = join(root, "artifacts.db");
    const db = new Database(dbPath);
    db.pragma("foreign_keys = ON");
    if (spec.originSchema === 0) {
      // A real pre-ledger state: version 1's CREATE TABLE IF NOT EXISTS cannot repair these old
      // layouts, so later migrations must evolve them exactly as a long-lived deployment would.
      db.exec(`CREATE TABLE api_keys (client_id TEXT PRIMARY KEY, key_hash TEXT NOT NULL UNIQUE,
        created_at TEXT NOT NULL DEFAULT (datetime('now')), revoked_at TEXT);
        CREATE TABLE artifacts (id TEXT PRIMARY KEY, client_id TEXT NOT NULL, title TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '', bytes INTEGER NOT NULL DEFAULT 0,
        created_at TEXT NOT NULL DEFAULT (datetime('now')), updated_at TEXT NOT NULL DEFAULT (datetime('now')));
        CREATE TABLE reactions (email TEXT NOT NULL, artifact_id TEXT NOT NULL, favorite INTEGER NOT NULL DEFAULT 0,
        vote INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL DEFAULT (datetime('now')),
        PRIMARY KEY (email, artifact_id));`);
    } else if (spec.boundary) {
      migrateDatabaseThrough(db, spec.originSchema);
    } else if (spec.currentLedger) {
      migrateDatabaseThrough(db, spec.originSchema);
    } else {
      historical.module.migrateDatabase(db);
    }
    const version = tableExists(db, "schema_migrations")
      ? db.prepare("SELECT MAX(version) AS version FROM schema_migrations").get().version ?? 0
      : 0;
    if (version !== spec.originSchema) throw new Error(`${spec.tag || spec.name} produced schema ${version}, expected ${spec.originSchema}`);

    insert(db, "orgs", { name: "fixture", label: "Synthetic fixture", color: "#3569a8" });
    // A real empty tenant exercises migrations that add organization-scoped tables without any
    // keys, artifacts, webhooks, or categories to seed them accidentally.
    insert(db, "orgs", { name: "emptyfixture", label: "Synthetic empty organization" });
    insert(db, "org_categories", { org: "fixture", name: "fixtures" });
    insert(db, "api_keys", {
      client_id: "fixture-key", key_hash: sha256(FIXTURE_API_TOKEN), org: "fixture", label: "Fixture key",
      role: "author", owner_email: "fixture-owner@example.test",
    });
    const artifact = (id, title, options = {}) => insert(db, "artifacts", {
      id, client_id: "fixture-key", org: "fixture", title, description: options.description || "Synthetic historical fixture",
      bytes: options.bytes ?? 0, uploader_label: "Fixture key", is_bundle: options.isBundle ? 1 : 0,
      entry: options.entry || "", revision: options.revision || 1, category: "fixtures", hidden: options.hidden ? 1 : 0,
      body_sha256: options.digest || "", owner_email: "fixture-owner@example.test",
    });
    const currentSingle = "<main>current single v2</main>";
    const historicalSingle = "<main>single revision one</main>";
    const bundleIndex = "<main>historical bundle</main>";
    const bundleCss = "main { color: #3569a8; }\n";
    const suffix = spec.boundary ? `b${String(spec.originSchema).padStart(2, "0")}` : "16";
    const singleId = `single${suffix}`;
    const bundleId = `bundle${suffix}`;
    artifact(singleId, "Historical single-file artifact", { bytes: Buffer.byteLength(currentSingle), revision: tableExists(db, "artifact_revisions") ? 2 : 1 });
    if (!spec.boundary) artifact(bundleId, "Historical bundle artifact", { bytes: Buffer.byteLength(bundleIndex) + Buffer.byteLength(bundleCss), isBundle: true, entry: "index.html" });
    if (tableExists(db, "artifact_revisions")) insert(db, "artifact_revisions", {
      artifact_id: singleId, org: "fixture", revision: 1, title: "Historical single-file artifact",
      description: "Synthetic historical fixture", category: "fixtures", bytes: Buffer.byteLength(historicalSingle), is_bundle: 0, entry: "",
      body_sha256: "", client_id: "fixture-key",
    });
    if (tableExists(db, "feedback")) insert(db, "feedback", {
      id: `feedback${suffix}`, artifact_id: singleId, org: "fixture", viewer_email: "viewer@example.test",
      body: "Synthetic unresolved feedback", artifact_revision: 2,
    });
    if (!spec.boundary && tableExists(db, "feedback")) insert(db, "feedback", {
      id: `resolved${suffix}`, artifact_id: singleId, org: "fixture", viewer_email: "reviewer@example.test",
      body: "Synthetic resolved feedback", artifact_revision: 1,
      resolved_at: "2026-01-02 00:00:00", resolved_by: "fixture-owner@example.test",
    });
    if (!spec.boundary && tableExists(db, "artifact_shares")) {
      insert(db, "artifact_shares", { token: "aaaaaaaaaaaaaaaaaaaaaaaa", artifact_id: singleId, org: "fixture", created_by: "fixture-key" });
      insert(db, "artifact_shares", { token: "bbbbbbbbbbbbbbbbbbbbbbbb", artifact_id: singleId, org: "fixture", created_by: "fixture-key", expires_at: "2000-01-01 00:00:00" });
      insert(db, "artifact_shares", { token: "cccccccccccccccccccccccc", artifact_id: singleId, org: "fixture", created_by: "fixture-key", revoked_at: "2026-01-01 00:00:00" });
    }
    if (spec.originSchema >= 21) insert(db, "org_email_members", { email: "member@example.test", org: "fixture" });
    if (spec.webhookMode === "mixed") {
      insert(db, "org_webhooks", {
        id: "plainwh20", org: "fixture", url: "https://discord.com/api/webhooks/1000/synthetic-plaintext-token",
        label: "Synthetic plaintext", events: "published,feedback",
      });
      insert(db, "org_webhooks", {
        id: "encwh200", org: "fixture", label: "Synthetic encrypted", events: "updated,resolved",
        ...encrypted("https://discord.com/api/webhooks/2000/synthetic-encrypted-token", 20),
      });
    }
    if (spec.recovery) {
      const staged = "<main>recovered staged revision</main>";
      artifact("recover23", "Recover staged update", { bytes: Buffer.byteLength(staged), revision: 2, digest: sha256(staged) });
      insert(db, "artifact_revisions", {
        artifact_id: "recover23", org: "fixture", revision: 1, title: "Recover staged update",
        description: "Synthetic historical fixture", category: "fixtures", bytes: 27, is_bundle: 0, entry: "",
        body_sha256: sha256("<main>outgoing revision</main>"), client_id: "fixture-key",
      });
      artifact("diverge23", "Quarantined divergence", { bytes: 30, digest: sha256("<main>expected but absent</main>") });
      artifact("missing23", "Missing body report", { bytes: 0, digest: sha256("missing") });
      const trashed = "<main>interrupted delete body</main>";
      artifact("trash23", "Recover interrupted delete", { bytes: Buffer.byteLength(trashed), digest: sha256(trashed) });
    }
    if (spec.durabilityRecovery) {
      const next = "<main>durability committed update</main>";
      const prior = "<main>durability outgoing update</main>";
      artifact("intentup24", "Intent update", { bytes: Buffer.byteLength(next), revision: 2, digest: sha256(next) });
      insert(db, "artifact_revisions", {
        artifact_id: "intentup24", org: "fixture", revision: 1, title: "Intent update",
        description: "Synthetic historical fixture", category: "fixtures", bytes: Buffer.byteLength(prior),
        is_bundle: 0, entry: "", body_sha256: sha256(prior), client_id: "fixture-key",
      });
      // Former commit→intent-marker gap: an old binary has committed the metadata/revision but
      // left the marker `prepared`. Current recovery must infer target revision 2, build the
      // immutable predecessor body, then release the marker.
      const preparedBody = "<main>legacy prepared metadata body</main>";
      artifact("prepared24", "Prepared metadata recovery", { bytes: Buffer.byteLength(preparedBody), revision: 2, digest: sha256(preparedBody) });
      insert(db, "artifact_revisions", {
        artifact_id: "prepared24", org: "fixture", revision: 1, title: "Prepared metadata recovery",
        description: "Synthetic historical fixture", category: "fixtures", bytes: Buffer.byteLength(preparedBody),
        is_bundle: 0, entry: "", body_sha256: sha256(preparedBody), client_id: "fixture-key",
      });
      artifact("ambigup24", "Ambiguous truncated staging", { bytes: 31, digest: sha256("<main>expected intact body</main>") });
      const published = "<main>published before final install</main>";
      const deleted = "<main>delete interrupted before SQL</main>";
      artifact("intentpub24", "Intent publish", { bytes: Buffer.byteLength(published), digest: sha256(published) });
      artifact("intentdel24", "Intent delete", { bytes: Buffer.byteLength(deleted), digest: sha256(deleted) });
      insert(db, "artifact_durability_intents", { id: "update:intentup24:2", artifact_id: "intentup24", operation: "update", state: "metadata_committed", expected_sha256: sha256(next), prior_sha256: sha256(prior), staging_path: ".intentup24.staging-fixture" });
      insert(db, "artifact_durability_intents", { id: "update:prepared24:2", artifact_id: "prepared24", operation: "update", state: "prepared", expected_sha256: sha256(preparedBody), prior_sha256: sha256(preparedBody), staging_path: ".prepared24.staging-fixture" });
      insert(db, "artifact_durability_intents", { id: "update:ambigup24:2", artifact_id: "ambigup24", operation: "update", state: "prepared", expected_sha256: sha256("<main>expected intact body</main>"), prior_sha256: "", staging_path: ".ambigup24.staging-fixture" });
      insert(db, "artifact_durability_intents", { id: "delete:gone24:1", artifact_id: "gone24", operation: "delete", state: "metadata_committed", expected_sha256: "", prior_sha256: sha256("<main>deleted durable trash</main>"), staging_path: ".gone24.trash-fixture" });
      insert(db, "artifact_durability_intents", { id: "publish:norow24", artifact_id: "norow24", operation: "publish", state: "prepared", expected_sha256: sha256("<main>uncommitted publish</main>"), prior_sha256: "", staging_path: ".norow24.staging-fixture" });
      insert(db, "artifact_durability_intents", { id: "publish:intentpub24", artifact_id: "intentpub24", operation: "publish", state: "metadata_committed", expected_sha256: sha256(published), prior_sha256: "", staging_path: ".intentpub24.staging-fixture" });
      insert(db, "artifact_durability_intents", { id: "delete:intentdel24:1", artifact_id: "intentdel24", operation: "delete", state: "prepared", expected_sha256: "", prior_sha256: sha256(deleted), staging_path: ".intentdel24.trash-fixture" });
    }
    db.pragma("wal_checkpoint(TRUNCATE)");
    db.close();

    await writeBody(root, `${singleId}.html`, currentSingle);
    if (!spec.boundary) {
      await writeBody(root, `${bundleId}/index.html`, bundleIndex);
      await writeBody(root, `${bundleId}/assets/site.css`, bundleCss);
      await writeBody(root, `.history/${singleId}/1.html`, historicalSingle);
    }
    if (spec.recovery) {
      await writeBody(root, "recover23.html", "<main>outgoing revision</main>");
      await writeBody(root, ".recover23.staging-fixture", "<main>recovered staged revision</main>");
      await writeBody(root, "diverge23.html", "<main>unexpected installed body</main>");
      await writeBody(root, ".diverge23.staging-fixture", "<main>untrusted staged body</main>");
      await writeBody(root, ".trash23.trash-fixture", "<main>interrupted delete body</main>");
      await writeBody(root, "orphan23.html", "<main>orphan body is reported</main>");
    }
    if (spec.durabilityRecovery) {
      await writeBody(root, "intentup24.html", "<main>durability outgoing update</main>");
      await writeBody(root, "prepared24.html", "<main>legacy prepared metadata body</main>");
      await writeBody(root, ".intentup24.staging-fixture", "<main>durability committed update</main>");
      await writeBody(root, "ambigup24.html", "<main>installed but not expected</main>");
      await writeBody(root, ".ambigup24.staging-fixture", "<main>truncated");
      await writeBody(root, ".gone24.trash-fixture", "<main>deleted durable trash</main>");
      await writeBody(root, ".norow24.staging-fixture", "<main>uncommitted publish</main>");
      await writeBody(root, ".intentpub24.staging-fixture", "<main>published before final install</main>");
      await writeBody(root, ".intentdel24.trash-fixture", "<main>delete interrupted before SQL</main>");
    }
    const dbBytes = await (await import("node:fs/promises")).readFile(dbPath);
    const manifest = {
      schemaVersion: 1,
      synthetic: true,
      origin: {
        releaseTag: spec.tag,
        schemaVersion: spec.originSchema,
        kind: spec.boundary ? "migration-boundary" : spec.currentLedger ? "current-ledger-recovery" : "public-release",
        sourceRef: spec.boundary || spec.currentLedger
          ? { ref: "current append-only migration ledger", migrationLedgerSha256: sha256(await readFile(join(ROOT, "lib/migrations.js"))) }
          : { commit: gitRef(spec.tag), migrationSourceSha256: sha256(tagFile(spec.tag, "lib/migrations.js")) },
      },
      // Derived from the exported production ledger, never a copied literal. The verifier and
      // both runtime gates reject a fixture whose target is not this value.
      expectedTargetSchema: LATEST_SCHEMA_VERSION,
      generation: "node scripts/generate-historical-fixtures.mjs",
      webhookEncryption: spec.webhookMode === "mixed" ? { keyId: "fixture-zero-key-v1", key: FIXTURE_KEY } : null,
      authentication: { clientId: "fixture-key", token: FIXTURE_API_TOKEN, classification: "public synthetic test material" },
      expectedRecovery: spec.recovery ? {
        recoveredPaths: spec.durabilityRecovery
          ? [".intentpub24.staging-fixture", ".intentup24.staging-fixture", ".recover23.staging-fixture", ".intentdel24.trash-fixture", ".trash23.trash-fixture"]
          : [".recover23.staging-fixture", ".trash23.trash-fixture"],
        divergentBodies: ["diverge23", ...(spec.durabilityRecovery ? ["ambigup24"] : [])],
        orphanBodies: ["orphan23.html"],
        missingBodies: ["missing23"],
        preservedTransientPaths: spec.durabilityRecovery
          ? [".ambigup24.staging-fixture", ".diverge23.staging-fixture"]
          : [".diverge23.staging-fixture"],
        remainingIntentIds: spec.durabilityRecovery ? ["update:ambigup24:2"] : [],
        preparedMetadataOnly: spec.durabilityRecovery ? { id: "prepared24", revision: 2, historyRevision: 1, html: "<main>legacy prepared metadata body</main>" } : null,
      } : { recoveredPaths: [], divergentBodies: [], orphanBodies: [], missingBodies: [], preservedTransientPaths: [], remainingIntentIds: [] },
      database: { path: "artifacts.db", sha256: sha256(dbBytes) },
      bodies: await bodyManifest(root),
    };
    await writeFile(join(root, "fixture.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  } finally {
    if (historical) await rm(historical.temp, { recursive: true, force: true });
  }
}

if (!process.argv.includes("--write")) {
  console.error("Refusing to overwrite fixtures without --write.");
  process.exit(2);
}
await mkdir(FIXTURE_ROOT, { recursive: true });
const requestedCase = process.argv.find((value) => value.startsWith("--case="))?.slice("--case=".length);
const selectedCases = requestedCase ? CASES.filter((spec) => spec.name === requestedCase) : CASES;
if (requestedCase && selectedCases.length !== 1) {
  console.error(`Unknown fixture case: ${requestedCase}`);
  process.exit(2);
}
for (const spec of selectedCases) await buildCase(spec);
console.log(`wrote ${selectedCases.length} synthetic historical fixture cases under ${relative(ROOT, FIXTURE_ROOT)}`);
