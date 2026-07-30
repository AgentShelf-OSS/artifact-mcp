// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { createHash } from "node:crypto";
import {
  closeSync,
  cpSync,
  existsSync,
  fsyncSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync
} from "node:fs";
import path from "node:path";
import { customAlphabet } from "nanoid";
import db, { ARTIFACT_DIR } from "./db.js";
import { orgExists as defaultOrgExists } from "./orgs.js";
import { MAX_ARTIFACT_BYTES, MAX_BUNDLE_BYTES, MAX_BUNDLE_FILES, MAX_HISTORY } from "./config.js";

const MIME = {
  html: "text/html; charset=utf-8", htm: "text/html; charset=utf-8",
  css: "text/css; charset=utf-8", js: "text/javascript; charset=utf-8", mjs: "text/javascript; charset=utf-8",
  json: "application/json", svg: "image/svg+xml", png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg",
  gif: "image/gif", webp: "image/webp", ico: "image/x-icon", woff2: "font/woff2", woff: "font/woff",
  ttf: "font/ttf", txt: "text/plain; charset=utf-8", map: "application/json", xml: "application/xml"
};

const RESERVED = new Set(["mcp", "health", "settings", "raw", "s", "favicon.ico", "robots.txt", ""]);
const generateId = customAlphabet("0123456789abcdefghijkmnpqrstuvwxyz", 12);

const defaultFiles = {
  closeSync,
  cpSync,
  existsSync,
  fsyncSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync
};

const HISTORY_DIR = ".history";

function mimeFor(filePath) {
  return MIME[filePath.split(".").pop().toLowerCase()] || "application/octet-stream";
}

function sanitizeRel(value) {
  const normalized = path.posix.normalize(String(value || "").replace(/\\/g, "/").replace(/^\/+/, ""));
  if (!normalized || normalized === "." || normalized === ".." || normalized.startsWith("../") || path.posix.isAbsolute(normalized)) return null;
  if (normalized.split("/").some((segment) => segment === "..")) return null;
  return normalized;
}

export function sanitizeBundlePath(value) {
  return sanitizeRel(value);
}

function normalizeCategory(value) {
  return String(value || "").trim().replace(/\s+/g, " ").slice(0, 60);
}

function groupBy(rows, keyFn) {
  const groups = new Map();
  for (const row of rows) {
    const key = keyFn(row);
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(row);
  }
  return groups;
}

function safeRemove(files, target) {
  try {
    files.rmSync(target, { recursive: true, force: true });
    syncParent(files, target);
  } catch {}
}

// Unlike pruning, delete acknowledgement is not best effort.  A successful DELETE must leave
// either a durably removed payload or an intent that recovery can finish; silently swallowing a
// parent-directory barrier failure would falsely acknowledge a deletion that can reappear.
function durableRemove(files, target) {
  if (!files.existsSync(target)) return;
  files.rmSync(target, { recursive: true, force: true });
  syncParent(files, target);
}

// Recovery may retry after unlink succeeded but its parent fsync failed. Re-sync an existing
// parent even when the owned path is already absent before releasing the durability intent.
function ensureDurablyRemoved(files, target) {
  if (files.existsSync(target)) return durableRemove(files, target);
  if (files.existsSync(path.dirname(target))) syncParent(files, target);
}

// fsync is required for acknowledged bodies, not merely an optimization.  The injection seam is
// shared with the existing lifecycle failpoint tests, so barrier failures remain reproducible.
function syncPath(files, target) {
  // Validation/parity harnesses use a deliberately tiny in-memory filesystem seam. Production
  // uses defaultFiles (which always supplies these primitives); do not make validation depend on
  // an unrelated durability capability in that test-only seam.
  if (typeof files.openSync !== "function" || typeof files.fsyncSync !== "function" || typeof files.closeSync !== "function") return;
  let fd;
  try {
    fd = files.openSync(target, "r");
    files.fsyncSync(fd);
  } finally {
    if (fd !== undefined) files.closeSync(fd);
  }
}

function syncTree(files, target) {
  if (!files.statSync(target).isDirectory()) return syncPath(files, target);
  for (const name of files.readdirSync(target)) syncTree(files, path.join(target, name));
  return syncPath(files, target);
}

function syncParent(files, target) {
  return syncPath(files, path.dirname(target));
}

// `mkdirSync(..., { recursive: true })` only changes the namespace. Each new directory name
// must be fsynced in its parent before a later history snapshot may be acknowledged; syncing
// just `.history/<id>` cannot persist that child's entry in `.history` (or `.history` in the
// artifact root) across power loss.
function durableMkdirp(files, root, target) {
  const relative = path.relative(root, target);
  if (relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`history directory escapes artifact root: ${target}`);
  }
  let directory = root;
  for (const segment of relative.split(path.sep).filter(Boolean)) {
    directory = path.join(directory, segment);
    if (!files.existsSync(directory)) {
      try {
        files.mkdirSync(directory);
      } catch (error) {
        if (!files.existsSync(directory)) throw error;
      }
      // Test seam analogous to afterRename: it models an interruption after mkdir but before the
      // directory-entry fsync. Production files do not provide this hook.
      if (typeof files.afterDirectoryCreate === "function") files.afterDirectoryCreate(directory);
    }
    // Re-sync every entry in the chain from artifactDir, even if it was created by a failed
    // earlier attempt and still exists in-process. A retry must durably record `.history` in
    // artifactDir before it can acknowledge `.history/<id>`.
    syncPath(files, path.dirname(directory));
    syncPath(files, directory);
  }
}

function durableRename(files, from, to) {
  // EXDEV is deliberately fatal: artifact staging/final/history must share a local filesystem.
  files.renameSync(from, to);
  // Test seam: models a process/interruption after the kernel move and before either parent
  // directory barrier. Production files do not expose this hook.
  if (typeof files.afterRename === "function") files.afterRename(from, to);
  syncParent(files, to);
  if (path.dirname(from) !== path.dirname(to)) syncParent(files, from);
}

function sha256(content) {
  return createHash("sha256").update(content).digest("hex");
}

function bundleManifestDigest(entries) {
  const manifest = entries
    .map(([rel, content]) => [rel, sha256(content)])
    .sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0);
  return sha256(JSON.stringify(manifest));
}

export function isReserved(id) {
  return RESERVED.has(id) || !/^[0-9a-z]{6,24}$/.test(id);
}

export function createArtifactStore({
  db: database,
  artifactDir,
  files = defaultFiles,
  idFactory = generateId,
  maxBytes = MAX_ARTIFACT_BYTES,
  maxBundleBytes = MAX_BUNDLE_BYTES,
  maxBundleFiles = MAX_BUNDLE_FILES,
  maxHistory = MAX_HISTORY,
  orgExists = defaultOrgExists
}) {
  files.mkdirSync(artifactDir, { recursive: true });

  const insert = database.prepare(`
    INSERT INTO artifacts (id, client_id, org, owner_email, uploader_label, title, description, bytes, is_bundle, entry, category, body_sha256)
    VALUES (@id, @client_id, @org, @owner_email, @uploader_label, @title, @description, @bytes, @is_bundle, @entry, @category, @body_sha256)
  `);
  const listIdsByOrg = database.prepare("SELECT id FROM artifacts WHERE org = ? AND hidden = 0 ORDER BY created_at DESC, id DESC");
  const listIdsByOrgIncludingHidden = database.prepare("SELECT id FROM artifacts WHERE org = ? ORDER BY created_at DESC, id DESC");
  const getMetaStmt = database.prepare(`
    SELECT * FROM artifacts a WHERE id = ?
    AND NOT EXISTS (SELECT 1 FROM artifact_durability_intents i
                    WHERE i.artifact_id = a.id)
  `);
  const getRawMetaStmt = database.prepare("SELECT * FROM artifacts WHERE id = ?");
  const startDurabilityIntentStmt = database.prepare(`
    INSERT INTO artifact_durability_intents
      (id, artifact_id, operation, state, expected_sha256, prior_sha256, staging_path)
    VALUES (@id, @artifact_id, @operation, 'prepared', @expected_sha256, @prior_sha256, @staging_path)
  `);
  const finishDurabilityIntentStmt = database.prepare("DELETE FROM artifact_durability_intents WHERE id = ?");
  const advanceDurabilityIntentStmt = database.prepare("UPDATE artifact_durability_intents SET state = ?, updated_at = datetime('now') WHERE id = ?");
  const durabilityPendingStmt = database.prepare(
    "SELECT EXISTS(SELECT 1 FROM artifact_durability_intents WHERE artifact_id = ?)"
  );
  const listByClient = database.prepare("SELECT * FROM artifacts WHERE client_id = ? ORDER BY created_at DESC");
  // Org-scoped variant: after an org move the client_id is preserved, so a non-admin key must
  // be constrained to its OWN org or it could keep listing artifacts moved to another tenant.
  const listByClientOrg = database.prepare("SELECT * FROM artifacts WHERE client_id = ? AND org = ? ORDER BY created_at DESC");
  const listByOrg = database.prepare("SELECT * FROM artifacts WHERE org = ? AND hidden = 0 ORDER BY client_id ASC, created_at DESC");
  const listByOrgIncludingHidden = database.prepare("SELECT * FROM artifacts WHERE org = ? ORDER BY client_id ASC, created_at DESC");
  const listByOrgForOwner = database.prepare("SELECT * FROM artifacts WHERE org = ? AND (hidden = 0 OR owner_email = ?) ORDER BY client_id ASC, created_at DESC");
  const listIdsByOrgForOwner = database.prepare("SELECT id FROM artifacts WHERE org = ? AND (hidden = 0 OR owner_email = ?) ORDER BY created_at DESC, id DESC");
  const listAll = database.prepare("SELECT * FROM artifacts ORDER BY org ASC, client_id ASC, created_at DESC");
  const listAllVisible = database.prepare("SELECT * FROM artifacts WHERE hidden = 0 ORDER BY org ASC, client_id ASC, created_at DESC");
  const listBodies = database.prepare("SELECT id, is_bundle, body_sha256, revision FROM artifacts");
  const deleteById = database.prepare("DELETE FROM artifacts WHERE id = ?");
  const updateMetaStmt = database.prepare(`
    UPDATE artifacts
    SET title = @title, description = @description, bytes = @bytes, entry = @entry, category = @category,
        body_sha256 = @body_sha256, revision = revision + 1, updated_at = datetime('now')
    WHERE id = @id AND client_id = @expected_client_id AND org = @expected_org
      AND revision = @expected_revision
  `);
  const updateCategoryStmt = database.prepare(
    "UPDATE artifacts SET category = @category, updated_at = datetime('now') WHERE id = @id"
  );
  const updateHiddenStmt = database.prepare(
    "UPDATE artifacts SET hidden = @hidden, updated_at = datetime('now') WHERE id = @id"
  );
  const moveArtifactStmt = database.prepare(
    "UPDATE artifacts SET org = @org, category = @category, updated_at = datetime('now') WHERE id = @id"
  );
  const moveFeedbackStmt = database.prepare("UPDATE feedback SET org = ? WHERE artifact_id = ?");
  const moveRevisionsStmt = database.prepare("UPDATE artifact_revisions SET org = ? WHERE artifact_id = ?");
  const moveViewsStmt = database.prepare("UPDATE artifact_views SET org = ? WHERE artifact_id = ?");
  // An org move REVOKES existing public share links rather than carrying them into the new
  // tenant: a link created under the old org shouldn't keep exposing content the artifact now
  // belongs to a different tenant. The new tenant re-shares deliberately.
  const dropSharesOnMoveStmt = database.prepare("DELETE FROM artifact_shares WHERE artifact_id = ?");
  const restoreMetaStmt = database.prepare(`
    UPDATE artifacts
    SET title = @title, description = @description, bytes = @bytes, entry = @entry,
        category = @category, body_sha256 = @body_sha256,
        revision = @revision, updated_at = @updated_at
    WHERE id = @id
  `);
  const insertRevisionStmt = database.prepare(`
    INSERT OR REPLACE INTO artifact_revisions
      (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256, client_id)
    VALUES (@artifact_id, @org, @revision, @title, @description, @category, @bytes, @is_bundle, @entry, @body_sha256, @client_id)
  `);
  const insertLegacyRevisionStmt = database.prepare(`
    INSERT OR IGNORE INTO artifact_revisions
      (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256, client_id)
    VALUES (@artifact_id, @org, @revision, @title, @description, @category, @bytes, @is_bundle, @entry, @body_sha256, NULL)
  `);
  const deleteRevisionStmt = database.prepare(
    "DELETE FROM artifact_revisions WHERE artifact_id = ? AND revision = ?"
  );
  const listRevisionsStmt = database.prepare(
    "SELECT revision, title, description, category, bytes, is_bundle, entry, body_sha256, created_at, client_id FROM artifact_revisions WHERE artifact_id = ? AND revision < ? ORDER BY revision DESC"
  );
  const getRevisionStmt = database.prepare(
    "SELECT * FROM artifact_revisions WHERE artifact_id = ? AND revision = ? AND revision < (SELECT revision FROM artifacts WHERE id = ?)"
  );
  const prunableRevisionsStmt = database.prepare(
    "SELECT revision, is_bundle FROM artifact_revisions WHERE artifact_id = ? AND revision < ? ORDER BY revision DESC LIMIT -1 OFFSET ?"
  );

  function filePath(id) {
    return path.join(artifactDir, `${id}.html`);
  }

  function bundleDir(id) {
    return path.join(artifactDir, id);
  }

  function transientPath(id, kind) {
    return path.join(artifactDir, `.${id}.${kind}-${generateId()}`);
  }

  function nextId() {
    let id;
    do {
      id = idFactory();
    } while (RESERVED.has(id) || files.existsSync(filePath(id)) || files.existsSync(bundleDir(id)) || getMetaStmt.get(id));
    return id;
  }

  function metadata({ id, clientId, org, ownerEmail, uploaderLabel, title, description, bytes, isBundle, entry, category, bodySha256 }) {
    return {
      id,
      client_id: clientId,
      org: org || "default",
      owner_email: ownerEmail || null,
      uploader_label: String(uploaderLabel || "").slice(0, 60),
      title: String(title || "Untitled artifact").slice(0, 200),
      description: String(description || "").slice(0, 500),
      bytes,
      is_bundle: isBundle ? 1 : 0,
      entry: entry || "",
      category: normalizeCategory(category),
      body_sha256: bodySha256
    };
  }

  function startDurabilityIntent({ id, artifactId, operation, expectedSha256, priorSha256, stagingPath }) {
    const stagingName = path.basename(String(stagingPath || ""));
    if (!/^\.[0-9a-z]{1,64}\.(?:staging|trash)-[0-9a-z]{1,64}$/.test(stagingName)) {
      throw new Error("invalid durability transient path");
    }
    startDurabilityIntentStmt.run({
      id, artifact_id: artifactId, operation, expected_sha256: expectedSha256 || "",
      prior_sha256: priorSha256 || "", staging_path: stagingName
    });
  }

  function finishDurabilityIntent(id) {
    finishDurabilityIntentStmt.run(id);
  }

  function advanceDurabilityIntent(id, state) {
    if (advanceDurabilityIntentStmt.run(state, id).changes !== 1) throw new Error("durability intent is missing");
  }

  function intentTransientPath(intent) {
    const name = String(intent.staging_path || "");
    const match = name.match(/^\.([0-9a-z]{1,64})\.(?:staging|trash)-[0-9a-z]{1,64}$/);
    return match && match[1] === intent.artifact_id ? path.join(artifactDir, name) : null;
  }

  function ready(row) {
    return !durabilityPendingStmt.pluck().get(row.id);
  }

  function readyRows(rows) {
    return rows.filter(ready);
  }

  function publish({ clientId, org, ownerEmail, uploaderLabel, html, title, description, category }) {
    if (typeof html !== "string" || !html.trim()) throw new Error("html is required");
    const bytes = Buffer.byteLength(html, "utf8");
    const bodySha256 = sha256(html);
    if (bytes > maxBytes) throw new Error(`html exceeds ${maxBytes} bytes (got ${bytes})`);

    const id = nextId();
    const staging = transientPath(id, "staging");
    const finalPath = filePath(id);
    let metadataCommitted = false;
    const intentId = `publish:${id}`;
    try {
      startDurabilityIntent({ id: intentId, artifactId: id, operation: "publish", expectedSha256: bodySha256, priorSha256: "", stagingPath: staging });
      files.writeFileSync(staging, html, "utf8");
      syncPath(files, staging);
      syncParent(files, staging);
      database.transaction(() => {
        insert.run(metadata({ id, clientId, org, ownerEmail, uploaderLabel, title, description, bytes, isBundle: false, category, bodySha256 }));
        recordRevisionRow(getRawMetaStmt.get(id), clientId);
      })();
      metadataCommitted = true;
      advanceDurabilityIntent(intentId, "metadata_committed");
      durableRename(files, staging, finalPath);
      advanceDurabilityIntent(intentId, "body_durable");
      finishDurabilityIntent(intentId);
      return { id, bytes };
    } catch (error) {
      if (files.existsSync(finalPath) && !files.existsSync(staging)) {
        // Physical rename completed but a barrier failed; preserve metadata+intent for startup
        // verification instead of rolling back to a ready row with no final body.
        throw error;
      }
      if (metadataCommitted) deleteById.run(id);
      safeRemove(files, staging);
      finishDurabilityIntent(intentId);
      throw error;
    }
  }

  function publishBundle({ clientId, org, ownerEmail, uploaderLabel, files: bundleFiles, entry, title, description, category }) {
    if (!bundleFiles || typeof bundleFiles !== "object" || Array.isArray(bundleFiles)) {
      throw new Error("files must be an object of { 'path': 'content' }");
    }
    const names = Object.keys(bundleFiles);
    if (names.length === 0) throw new Error("files is empty");
    if (names.length > maxBundleFiles) throw new Error(`too many files (max ${maxBundleFiles})`);

    let total = 0;
    const clean = [];
    const relativePaths = new Set();
    for (const raw of names) {
      const rel = sanitizeRel(raw);
      if (!rel) throw new Error(`unsafe file path: ${raw}`);
      const content = bundleFiles[raw];
      if (typeof content !== "string") throw new Error(`file "${raw}" content must be a string`);
      total += Buffer.byteLength(content, "utf8");
      clean.push([rel, content]);
      relativePaths.add(rel);
    }
    if (total > maxBundleBytes) throw new Error(`bundle exceeds ${maxBundleBytes} bytes (got ${total})`);

    let selectedEntry = entry ? sanitizeRel(entry) : "";
    if (selectedEntry && !relativePaths.has(selectedEntry)) throw new Error(`entry "${entry}" is not one of the files`);
    if (!selectedEntry) selectedEntry = relativePaths.has("index.html") ? "index.html" : clean.map(([rel]) => rel).find((rel) => rel.endsWith(".html"));
    if (!selectedEntry) throw new Error("no HTML entry found — include index.html or pass an 'entry'");

    const id = nextId();
    const staging = transientPath(id, "staging");
    const finalDir = bundleDir(id);
    const bodySha256 = bundleManifestDigest(clean);
    let metadataCommitted = false;
    const intentId = `publish:${id}`;
    try {
      startDurabilityIntent({ id: intentId, artifactId: id, operation: "publish", expectedSha256: bodySha256, priorSha256: "", stagingPath: staging });
      for (const [rel, content] of clean) {
        const full = path.join(staging, rel);
        files.mkdirSync(path.dirname(full), { recursive: true });
        files.writeFileSync(full, content, "utf8");
      }
      syncTree(files, staging);
      syncParent(files, staging);
      database.transaction(() => {
        insert.run(metadata({
          id, clientId, org, ownerEmail, uploaderLabel, title, description, bytes: total, isBundle: true,
          entry: selectedEntry, category, bodySha256
        }));
        recordRevisionRow(getRawMetaStmt.get(id), clientId);
      })();
      metadataCommitted = true;
      advanceDurabilityIntent(intentId, "metadata_committed");
      durableRename(files, staging, finalDir);
      advanceDurabilityIntent(intentId, "body_durable");
      finishDurabilityIntent(intentId);
      return { id, bytes: total, entry: selectedEntry, files: clean.length };
    } catch (error) {
      if (files.existsSync(finalDir) && !files.existsSync(staging)) throw error;
      if (metadataCommitted) deleteById.run(id);
      safeRemove(files, staging);
      finishDurabilityIntent(intentId);
      throw error;
    }
  }

  // Validate a complete bundle snapshot (mirrors publishBundle) -> { clean, total, entry }.
  function validateBundle(bundleFiles, entry, preferEntry) {
    if (!bundleFiles || typeof bundleFiles !== "object" || Array.isArray(bundleFiles)) {
      throw new Error("files must be an object of { 'path': 'content' }");
    }
    const names = Object.keys(bundleFiles);
    if (names.length === 0) throw new Error("files is empty");
    if (names.length > maxBundleFiles) throw new Error(`too many files (max ${maxBundleFiles})`);
    let total = 0;
    const clean = [];
    const relativePaths = new Set();
    for (const raw of names) {
      const rel = sanitizeRel(raw);
      if (!rel) throw new Error(`unsafe file path: ${raw}`);
      const content = bundleFiles[raw];
      if (typeof content !== "string") throw new Error(`file "${raw}" content must be a string`);
      total += Buffer.byteLength(content, "utf8");
      clean.push([rel, content]);
      relativePaths.add(rel);
    }
    if (total > maxBundleBytes) throw new Error(`bundle exceeds ${maxBundleBytes} bytes (got ${total})`);
    let selectedEntry = entry ? sanitizeRel(entry) : "";
    if (selectedEntry && !relativePaths.has(selectedEntry)) throw new Error(`entry "${entry}" is not one of the files`);
    if (!selectedEntry && preferEntry && relativePaths.has(preferEntry)) selectedEntry = preferEntry;
    if (!selectedEntry) selectedEntry = relativePaths.has("index.html") ? "index.html" : clean.map(([rel]) => rel).find((rel) => rel.endsWith(".html"));
    if (!selectedEntry) throw new Error("no HTML entry found — include index.html or pass an 'entry'");
    return { clean, total, entry: selectedEntry };
  }

  // Replace an existing artifact in place (same id/URL); bumps revision. Reuses the
  // staging->rename crash-safe lifecycle and rolls back the body if the DB swap fails.
  function update({ id, clientId, org, expectedRevision, isAdmin = false, html, files: bundleFiles, entry, title, description, category }) {
    const meta = getMetaStmt.get(id);
    if (!meta) return { ok: false, reason: "not_found" };
    if (!isAdmin && (meta.client_id !== clientId || meta.org !== org)) return { ok: false, reason: "forbidden" };
    const guardedRevision = expectedRevision === undefined ? meta.revision : Number(expectedRevision);
    if (!Number.isInteger(guardedRevision) || guardedRevision < 1 || guardedRevision !== meta.revision) {
      return { ok: false, reason: "conflict" };
    }

    const wantsSingle = html !== undefined;
    const wantsBundle = bundleFiles !== undefined;
    const wantsEntry = entry !== undefined;
    if (wantsSingle && wantsBundle) throw new Error("provide either html or files, not both");
    if (wantsSingle && meta.is_bundle) throw new Error("artifact is a bundle; pass files, not html");
    if (wantsBundle && !meta.is_bundle) throw new Error("artifact is single-file; pass html, not files");
    if (wantsEntry && !meta.is_bundle) throw new Error("artifact is single-file; entry only applies to bundles");

    const nextTitle = title === undefined ? meta.title : String(title || "Untitled artifact").slice(0, 200);
    const nextDescription = description === undefined ? meta.description : String(description || "").slice(0, 500);
    const nextCategory = category === undefined ? meta.category : normalizeCategory(category);

    let nextBytes = meta.bytes;
    let nextEntry = meta.entry;
    let nextBodySha256 = meta.body_sha256;
    let nextBundleFiles = null;
    let staged = null;
    let intentId = null;

    // Validate and digest the proposed body before staging so an exact no-op does not create
    // a revision, history snapshot, or replacement bundle.
    if (wantsSingle) {
      if (typeof html !== "string" || !html.trim()) throw new Error("html is required");
      nextBytes = Buffer.byteLength(html, "utf8");
      nextBodySha256 = sha256(html);
      if (nextBytes > maxBytes) throw new Error(`html exceeds ${maxBytes} bytes (got ${nextBytes})`);
    } else if (wantsBundle) {
      const built = validateBundle(bundleFiles, entry, meta.entry);
      nextBytes = built.total;
      nextEntry = built.entry;
      nextBodySha256 = bundleManifestDigest(built.clean);
      nextBundleFiles = built.clean;
    } else if (wantsEntry) {
      const selectedEntry = entry ? sanitizeRel(entry) : meta.entry;
      if (!selectedEntry || !readBundleFile(id, selectedEntry)) {
        throw new Error(`entry "${entry}" is not one of the files`);
      }
      nextEntry = selectedEntry;
    }

    const contentChanged = nextBytes !== meta.bytes || nextBodySha256 !== meta.body_sha256;
    const changed = contentChanged || nextEntry !== meta.entry || nextTitle !== meta.title
      || nextDescription !== meta.description || nextCategory !== meta.category;
    if (!changed) {
      return {
        ok: true, changed: false, id, revision: meta.revision, bytes: meta.bytes,
        is_bundle: !!meta.is_bundle, entry: meta.entry, category: meta.category
      };
    }

    // Do not admit any revision until its predecessor is present and still matches committed
    // metadata. A later history snapshot is the only restore point for this revision.
    const currentDigest = bodyDigestOnDisk(id, !!meta.is_bundle);
    if (currentDigest === null) throw new Error("body_missing");
    if (currentDigest !== meta.body_sha256) throw new Error("body_digest_mismatch");

    // Every revision, including metadata-only/entry-only updates, has a history body that must
    // become durable before it can be served. The marker therefore starts before either staging
    // or the metadata transaction, even when no replacement body exists.
    intentId = `update:${id}:${meta.revision + 1}`;
    const intentStaging = transientPath(id, "staging");
    startDurabilityIntent({ id: intentId, artifactId: id, operation: "update", expectedSha256: nextBodySha256, priorSha256: meta.body_sha256, stagingPath: intentStaging });

    // Stage changed content before touching the DB. An entry-only bundle update is revisioned,
    // but keeps the existing directory live and snapshots it by copy below.
    if (contentChanged && wantsSingle) {
      staged = intentStaging;
      try {
        files.writeFileSync(staged, html, "utf8");
        syncPath(files, staged);
        syncParent(files, staged);
      } catch (error) {
        safeRemove(files, staged);
        finishDurabilityIntent(intentId);
        throw error;
      }
    } else if (contentChanged && wantsBundle) {
      staged = intentStaging;
      try {
        for (const [rel, content] of nextBundleFiles) {
          const full = path.join(staged, rel);
          files.mkdirSync(path.dirname(full), { recursive: true });
          files.writeFileSync(full, content, "utf8");
        }
        syncTree(files, staged);
        syncParent(files, staged);
      } catch (error) {
        safeRemove(files, staged);
        finishDurabilityIntent(intentId);
        throw error;
      }
    }

    // Commit metadata first, THEN swap the body. SQLite's transaction cannot span the
    // filesystem rename, so ordering decides the crash outcome:
    //   - swap-inside-txn: a crash between the rename and the commit rolls the metadata
    //     back but keeps the NEW file, and startup audit then deletes the only copy of the
    //     old body -> permanent data loss + serving uncommitted content.
    //   - commit-then-swap (this): a crash before the swap leaves committed metadata with
    //     the old body still on disk plus the staged committed body, which startup audit
    //     installs by digest. A swap *error* compensates by reverting metadata to pre-update.
    const before = {
      id,
      title: meta.title,
      description: meta.description,
      bytes: meta.bytes,
      entry: meta.entry,
      category: meta.category,
      body_sha256: meta.body_sha256,
      revision: meta.revision,
      updated_at: meta.updated_at
    };
    // Record the OUTGOING revision (metadata) and bump to the next revision atomically.
    const committed = database.transaction(() => {
      const info = updateMetaStmt.run({
        id, title: nextTitle, description: nextDescription, bytes: nextBytes,
        entry: nextEntry, category: nextCategory, body_sha256: nextBodySha256,
        expected_client_id: meta.client_id, expected_org: meta.org, expected_revision: guardedRevision
      });
      if (info.changes !== 1) return false;
      recordLegacyRevisionRow(meta);
      recordRevisionRow({
        ...meta,
        title: nextTitle,
        description: nextDescription,
        bytes: nextBytes,
        entry: nextEntry,
        category: nextCategory,
        body_sha256: nextBodySha256,
        revision: meta.revision + 1
      }, clientId);
      // Metadata/revision attribution and readiness advance are one SQLite commit. Otherwise a
      // crash in the gap leaves `prepared` beside a committed metadata-only revision and older
      // recovery logic can release it without an immutable predecessor snapshot.
      advanceDurabilityIntent(intentId, "metadata_committed");
      return true;
    })();
    if (!committed) {
      if (staged) safeRemove(files, staged);
      if (intentId) finishDurabilityIntent(intentId);
      return { ok: false, reason: "conflict" };
    }
    let snap = null;
    try {
      // Snapshot the outgoing body into .history: MOVE it for a body change (frees the final
      // path for the new body), COPY it for a metadata-only change (body stays live).
      snap = snapshotBody(id, meta, { moveBody: !!staged });
      if (staged) {
        durableRename(files, staged, meta.is_bundle ? bundleDir(id) : filePath(id));
        staged = null;
      }
      advanceDurabilityIntent(intentId, "body_durable");
    } catch (error) {
      const finalPath = meta.is_bundle ? bundleDir(id) : filePath(id);
      const outgoingPath = historyBodyPath(id, meta.revision, !!meta.is_bundle);
      if (intentId && (
        (staged && files.existsSync(finalPath) && !files.existsSync(staged))
        || (staged && !files.existsSync(finalPath) && files.existsSync(outgoingPath))
        // Metadata-only snapshots retain the live body. A completed temp→history rename whose
        // parent fsync then fails is still recoverable when its immutable digest is valid; keep
        // the intent concealed for startup instead of reverting and permanently blocking retry.
        || (!staged && files.existsSync(finalPath) && files.existsSync(outgoingPath)
          && bodyDigestAtPath(outgoingPath, !!meta.is_bundle) === meta.body_sha256)
      )) {
        // A rename completed before its parent fsync failed.  Metadata already names the
        // replacement; recovery can verify/install it. Do not roll back into a missing live body
        // or clear the concealment marker.
        throw error;
      }
      restoreSnapshotBody(snap);
      if (staged) safeRemove(files, staged);
      database.transaction(() => {
        deleteRevisionStmt.run(id, meta.revision + 1);
        restoreMetaStmt.run(before); // revert committed metadata + revision so it matches the body
      })();
      if (intentId) {
        durableRemove(files, historySnapshotTempPath(id, meta.revision, !!meta.is_bundle));
        finishDurabilityIntent(intentId);
      }
      throw error;
    }
    pruneHistory(id, meta.revision + 1);
    finishDurabilityIntent(intentId);

    const updated = getMetaStmt.get(id);
    return { ok: true, changed: true, id, revision: updated.revision, bytes: updated.bytes, is_bundle: !!updated.is_bundle, entry: updated.entry, category: updated.category };
  }

  // Set/clear an artifact's category (bumps updated_at so it surfaces in the new group).
  // Authorization is the caller's responsibility (route uses artifactAccess).
  function setCategory(id, category) {
    const meta = getMetaStmt.get(id);
    if (!meta) return { ok: false, reason: "not_found" };
    const next = normalizeCategory(category);
    updateCategoryStmt.run({ id, category: next });
    return { ok: true, id, category: next };
  }

  // Hidden is an unlisted flag, never an access-control boundary.
  function setHidden(id, hidden) {
    const meta = getMetaStmt.get(id);
    if (!meta) return { ok: false, reason: "not_found" };
    const next = hidden ? 1 : 0;
    updateHiddenStmt.run({ id, hidden: next });
    return { ok: true, id, hidden: !!next };
  }

  function moveArtifactToOrg(id, targetOrg, category) {
    const meta = getMetaStmt.get(id);
    if (!meta) return { ok: false, reason: "not_found" };
    const org = String(targetOrg || "").trim();
    if (!orgExists(org)) throw new Error(`Unknown organization "${org}".`);
    const nextCategory = category === undefined ? meta.category : normalizeCategory(category);
    database.transaction(() => {
      // These composite FKs are checked at commit, after parent and all org-bearing children move.
      database.pragma("defer_foreign_keys = ON");
      moveArtifactStmt.run({ id, org, category: nextCategory });
      moveFeedbackStmt.run(org, id);
      moveRevisionsStmt.run(org, id);
      moveViewsStmt.run(org, id);
      dropSharesOnMoveStmt.run(id);
    })();
    // client_id intentionally remains: its old org-locked key can no longer update this via MCP.
    return { ok: true, id, org, category: nextCategory };
  }

  function readBundleFile(id, relPath) {
    const meta = getMetaStmt.get(id);
    if (!meta || !meta.is_bundle) return null;
    const rel = relPath ? sanitizeRel(relPath) : meta.entry;
    if (!rel) return null;
    const base = path.resolve(bundleDir(id));
    const full = path.resolve(path.join(base, rel));
    if (full !== base && !full.startsWith(base + path.sep)) return null;
    if (!files.existsSync(full) || !files.statSync(full).isFile()) return null;
    return { content: files.readFileSync(full), contentType: mimeFor(rel) };
  }

  function listBundleFiles(id, revision) {
    if (!getMetaStmt.get(id)) return null;
    const row = revision === undefined
      ? getMetaStmt.get(id)
      : getRevisionStmt.get(id, Number(revision), id);
    if (!row || !row.is_bundle) return null;
    const root = revision === undefined
      ? bundleDir(id)
      : historyBodyPath(id, row.revision, true);
    if (!files.existsSync(root) || !files.statSync(root).isDirectory()) return null;

    const listed = [];
    const walk = (dir) => {
      for (const name of files.readdirSync(dir)) {
        const full = path.join(dir, name);
        const stat = files.statSync(full);
        if (stat.isDirectory()) walk(full);
        else if (stat.isFile()) {
          listed.push({
            path: path.relative(root, full).split(path.sep).join("/"),
            bytes: stat.size
          });
        }
      }
    };
    walk(root);
    listed.sort((a, b) => a.path < b.path ? -1 : a.path > b.path ? 1 : 0);
    return {
      revision: row.revision,
      entry: row.entry,
      bytes: row.bytes,
      files: listed.map((file) => ({ ...file, entry: file.path === row.entry }))
    };
  }

  function readArtifact(id) {
    if (isReserved(id)) return null;
    const meta = getMetaStmt.get(id);
    if (!meta) return null;
    const target = filePath(id);
    if (!files.existsSync(target)) return null;
    return { meta, html: files.readFileSync(target, "utf8") };
  }

  function moveBodyToTrash(id, meta, trash = transientPath(id, "trash")) {
    const source = meta?.is_bundle ? bundleDir(id) : filePath(id);
    if (!files.existsSync(source)) return null;
    durableRename(files, source, trash);
    return { source, trash };
  }

  function restoreBody(moved) {
    if (!moved || !files.existsSync(moved.trash)) return;
    durableRename(files, moved.trash, moved.source);
  }

  // ---- Version history ------------------------------------------------------------------
  function historyDir(id) {
    return path.join(artifactDir, HISTORY_DIR, id);
  }
  function historyBodyPath(id, revision, isBundle) {
    return path.join(historyDir(id), isBundle ? String(revision) : `${revision}.html`);
  }
  function historySnapshotTempPath(id, revision, isBundle) {
    return `${historyBodyPath(id, revision, isBundle)}.snapshot-tmp`;
  }
  function removeHistory(id) {
    safeRemove(files, historyDir(id));
  }

  // Read a directory tree into { 'rel/path': content } (utf8) — restores a bundle snapshot.
  function readTree(dir, base = dir) {
    const out = {};
    for (const name of files.readdirSync(dir)) {
      const full = path.join(dir, name);
      if (files.statSync(full).isDirectory()) Object.assign(out, readTree(full, base));
      else out[path.relative(base, full).split(path.sep).join("/")] = files.readFileSync(full, "utf8");
    }
    return out;
  }

  function recordRevisionRow(meta, clientId) {
    insertRevisionStmt.run({
      artifact_id: meta.id, org: meta.org, revision: meta.revision, title: meta.title,
      description: meta.description, category: meta.category, bytes: meta.bytes,
      is_bundle: meta.is_bundle, entry: meta.entry, body_sha256: meta.body_sha256,
      client_id: clientId ?? null
    });
  }

  // A database upgraded from v21 has no marker for its live revision. Preserve that
  // pre-attribution revision with NULL rather than assigning it to the key making the next edit.
  function recordLegacyRevisionRow(meta) {
    insertLegacyRevisionStmt.run({
      artifact_id: meta.id, org: meta.org, revision: meta.revision, title: meta.title,
      description: meta.description, category: meta.category, bytes: meta.bytes,
      is_bundle: meta.is_bundle, entry: meta.entry, body_sha256: meta.body_sha256
    });
  }

  // Snapshot the outgoing revision's body into .history. moveBody=true relocates the live
  // body (freeing the final path for the replacement); moveBody=false copies it (a
  // metadata-only update keeps its body live). Returns a handle for body rollback.
  function snapshotBody(id, meta, { moveBody }) {
    const source = meta.is_bundle ? bundleDir(id) : filePath(id);
    if (!files.existsSync(source)) return null;
    const dest = historyBodyPath(id, meta.revision, meta.is_bundle);
    durableMkdirp(files, artifactDir, path.dirname(dest));
    // Revision snapshots are immutable. Never remove an existing destination to make room: it
    // may be the only durable copy left by an interrupted earlier mutation.
    if (files.existsSync(dest)) throw new Error("history destination already exists");
    if (moveBody) {
      durableRename(files, source, dest);
      return { source, dest, moved: true };
    }
    const temp = historySnapshotTempPath(id, meta.revision, meta.is_bundle);
    if (files.existsSync(temp)) throw new Error("history snapshot temporary already exists");
    files.cpSync(source, temp, { recursive: true });
    syncTree(files, temp);
    durableRename(files, temp, dest);
    return { source, dest, moved: false };
  }
  function restoreSnapshotBody(snap) {
    if (!snap || !snap.moved || !files.existsSync(snap.dest)) return;
    durableRename(files, snap.dest, snap.source);
  }

  // Best-effort: keep only the newest maxHistory snapshots per artifact.
  function pruneHistory(id, knownCurrent = null) {
    try {
      const current = knownCurrent ?? getMetaStmt.get(id)?.revision ?? 0;
      for (const row of prunableRevisionsStmt.all(id, current, Math.max(0, maxHistory))) {
        deleteRevisionStmt.run(id, row.revision);
        safeRemove(files, historyBodyPath(id, row.revision, row.is_bundle));
      }
    } catch {}
  }

  function listRevisions(id) {
    const meta = getMetaStmt.get(id);
    if (!meta) return null;
    return { current: meta.revision, revisions: listRevisionsStmt.all(id, meta.revision) };
  }

  function readHistoryArtifact(id, revision) {
    if (!getMetaStmt.get(id)) return null;
    const rev = getRevisionStmt.get(id, Number(revision), id);
    if (!rev || rev.is_bundle) return null;
    const p = historyBodyPath(id, rev.revision, false);
    if (!files.existsSync(p)) return null;
    return { meta: rev, html: files.readFileSync(p, "utf8") };
  }

  function readHistoryBundleFile(id, revision, relPath) {
    if (!getMetaStmt.get(id)) return null;
    const rev = getRevisionStmt.get(id, Number(revision), id);
    if (!rev || !rev.is_bundle) return null;
    const rel = relPath ? sanitizeRel(relPath) : rev.entry;
    if (!rel) return null;
    const base = path.resolve(historyBodyPath(id, rev.revision, true));
    const full = path.resolve(path.join(base, rel));
    if (full !== base && !full.startsWith(base + path.sep)) return null;
    if (!files.existsSync(full) || !files.statSync(full).isFile()) return null;
    return { content: files.readFileSync(full), contentType: mimeFor(rel) };
  }

  // Replay a past revision as a NEW revision (append-only). update() snapshots the current
  // revision first, so a restore is itself undoable. Auth mirrors removeById.
  function restoreById(id, revision, expectedClientId, isAdmin = false) {
    const meta = getMetaStmt.get(id);
    if (!meta) return { ok: false, reason: "not_found" };
    if (!isAdmin && expectedClientId && meta.client_id !== expectedClientId) return { ok: false, reason: "forbidden" };
    const rev = getRevisionStmt.get(id, Number(revision), id);
    if (!rev) return { ok: false, reason: "revision_not_found" };
    if (!!rev.is_bundle !== !!meta.is_bundle) return { ok: false, reason: "type_mismatch" };
    const bodyPath = historyBodyPath(id, rev.revision, rev.is_bundle);
    if (!files.existsSync(bodyPath)) return { ok: false, reason: "body_missing" };

    const payload = { id, isAdmin: true, title: rev.title, description: rev.description, category: rev.category };
    if (rev.is_bundle) {
      payload.files = readTree(bodyPath);
      payload.entry = rev.entry;
    } else {
      payload.html = files.readFileSync(bodyPath, "utf8");
    }
    const result = update(payload);
    return result.ok ? { ...result, restoredFrom: rev.revision } : result;
  }

  function removeById(id, expectedClientId, isAdmin = false) {
    const meta = getMetaStmt.get(id);
    if (!meta) return false;
    if (!isAdmin && expectedClientId && meta.client_id !== expectedClientId) return false;
    const trash = transientPath(id, "trash");
    const intentId = `delete:${id}:${meta.revision}`;
    startDurabilityIntent({ id: intentId, artifactId: id, operation: "delete", expectedSha256: "", priorSha256: meta.body_sha256, stagingPath: trash });
    let moved;
    try {
      moved = moveBodyToTrash(id, meta, trash);
    } catch (error) {
      // renameSync can have completed before its parent fsync fails.  Preserve the marker in
      // that physical-partial state so the live row cannot be served with a missing final body.
      const source = meta.is_bundle ? bundleDir(id) : filePath(id);
      if (!files.existsSync(trash) || files.existsSync(source)) finishDurabilityIntent(intentId);
      throw error;
    }
    let deleted = false;
    try {
      const info = deleteById.run(id);
      if (info.changes === 0) {
        restoreBody(moved);
        finishDurabilityIntent(intentId);
        return false;
      }
      deleted = true;
      advanceDurabilityIntent(intentId, "metadata_committed");
      if (moved) durableRemove(files, moved.trash);
      durableRemove(files, historyDir(id)); // revision rows cascade via FK; remove their bodies from disk
      advanceDurabilityIntent(intentId, "body_durable");
      finishDurabilityIntent(intentId);
      return true;
    } catch (error) {
      // Once the SQLite delete committed, restoring the body would make an unowned public file
      // and losing the intent would make the cleanup unknowable.  Recovery owns this state.
      if (!deleted) {
        restoreBody(moved);
        finishDurabilityIntent(intentId);
      }
      throw error;
    }
  }

  // SHA-256 of a single body, or of a canonical sorted path->file-digest manifest for a
  // bundle. This is the durable commit marker used by crash recovery; display byte counts
  // are deliberately not involved in reconciliation.
  function bodyDigestAtPath(target, isBundle) {
    try {
      if (!isBundle) return sha256(files.readFileSync(target));
      const entries = [];
      const walk = (dir, base = target) => {
        for (const n of files.readdirSync(dir)) {
          const full = path.join(dir, n);
          const st = files.statSync(full);
          if (st.isDirectory()) walk(full);
          else entries.push([
            path.relative(base, full).split(path.sep).join("/"),
            files.readFileSync(full)
          ]);
        }
      };
      walk(target);
      return bundleManifestDigest(entries);
    } catch {
      return null;
    }
  }

  function bodyDigestOnDisk(id, isBundle) {
    return bodyDigestAtPath(isBundle ? bundleDir(id) : filePath(id), isBundle);
  }

  function ensureMetadataOnlyHistory(meta, expectedDigest) {
    if (meta.revision < 2) return false;
    const outgoing = getRevisionStmt.get(meta.id, Number(meta.revision) - 1, meta.id);
    if (!outgoing) return false;
    const destination = historyBodyPath(meta.id, outgoing.revision, !!outgoing.is_bundle);
    ensureDurablyRemoved(files, historySnapshotTempPath(meta.id, outgoing.revision, !!outgoing.is_bundle));
    if (files.existsSync(destination)) return bodyDigestAtPath(destination, !!outgoing.is_bundle) === expectedDigest;
    const source = meta.is_bundle ? bundleDir(meta.id) : filePath(meta.id);
    if (!files.existsSync(source)) return false;
    durableMkdirp(files, artifactDir, path.dirname(destination));
    const temporary = `${destination}.metadata-recovery-tmp`;
    if (files.existsSync(temporary)) durableRemove(files, temporary);
    files.cpSync(source, temporary, { recursive: true });
    syncTree(files, temporary);
    durableRename(files, temporary, destination);
    return bodyDigestAtPath(destination, !!outgoing.is_bundle) === expectedDigest;
  }

  function classifyMetadataOnlyIntent(intent, meta) {
    if (intent.operation !== "update" || intent.expected_sha256 !== intent.prior_sha256) return "not-applicable";
    if (meta.body_sha256 !== intent.prior_sha256) return "ambiguous";
    const prefix = `update:${meta.id}:`;
    if (!intent.id.startsWith(prefix)) return "ambiguous";
    const rawTarget = intent.id.slice(prefix.length);
    if (!/^[1-9]\d*$/.test(rawTarget)) return "ambiguous";
    const target = Number(rawTarget);
    if (!Number.isSafeInteger(target)) return "ambiguous";
    const current = Number(meta.revision);
    if (!Number.isSafeInteger(current)) return "ambiguous";
    if (target === current) return "committed";
    if (target === current + 1) return "reverted";
    return "ambiguous";
  }

  function recoverDurabilityIntents() {
    const intents = database.prepare("SELECT id, artifact_id, operation, state, expected_sha256, prior_sha256, staging_path FROM artifact_durability_intents").all();
    for (const intent of intents) {
      const meta = getRawMetaStmt.get(intent.artifact_id);
      const finalPath = meta ? (meta.is_bundle ? bundleDir(meta.id) : filePath(meta.id)) : null;
      const stagedPath = intentTransientPath(intent);
      if (intent.operation === "delete") {
        if (!meta) {
          // Retry the entire acknowledged-delete cleanup through fallible barriers. A failure
          // leaves the intent intact; clearing it after trash alone would silently downgrade
          // history cleanup to best effort.
          if (stagedPath && files.existsSync(stagedPath)) durableRemove(files, stagedPath);
          if (isReserved(intent.artifact_id)) continue;
          durableRemove(files, historyDir(intent.artifact_id));
          finishDurabilityIntent(intent.id);
          continue;
        }
        // Do not trust a database path.  Generic reconciliation will restore only a digest- and
        // owner-validated trash file; this pass merely clears a fully undone delete.
        if (files.existsSync(finalPath) && bodyDigestAtPath(finalPath, !!meta.is_bundle) === intent.prior_sha256) finishDurabilityIntent(intent.id);
        continue;
      }
      // A publish may have crossed the durable rename but failed before its atomic metadata
      // transaction. There is no public row to conceal. Clear its marker; the generic sweep
      // then reports/removes the unreferenced transient/final body according to normal orphan
      // policy (it is not retained as forensic evidence).
      if (!meta && intent.operation === "publish") { finishDurabilityIntent(intent.id); continue; }
      if (!meta || !finalPath || !files.existsSync(finalPath)) continue;
      const digest = bodyDigestAtPath(finalPath, !!meta.is_bundle);
      if (digest === intent.expected_sha256) {
        const metadataDisposition = classifyMetadataOnlyIntent(intent, meta);
        if (metadataDisposition === "ambiguous") continue;
        if (metadataDisposition === "committed"
          && !ensureMetadataOnlyHistory(meta, intent.expected_sha256)) continue;
        if (metadataDisposition === "reverted") {
          ensureDurablyRemoved(files, historySnapshotTempPath(meta.id, meta.revision, !!meta.is_bundle));
        }
        finishDurabilityIntent(intent.id);
      }
      else if (meta.body_sha256 === intent.prior_sha256 && digest === intent.prior_sha256) {
        if (stagedPath && files.existsSync(stagedPath) && bodyDigestAtPath(stagedPath, !!meta.is_bundle) === intent.expected_sha256) safeRemove(files, stagedPath);
        finishDurabilityIntent(intent.id);
      }
    }
  }

  function auditStorage({ cleanTransient = false } = {}) {
    if (cleanTransient) recoverDurabilityIntents();
    let rows = listBodies.all();
    const rowsById = new Map(rows.map((row) => [row.id, row]));
    const expected = new Set(rows.map((row) => row.is_bundle ? row.id : `${row.id}.html`));
    const orphanBodies = [];
    const orphanHistory = [];
    const transientPaths = [];
    const recoveredPaths = [];
    const divergentBodies = [];

    // Process .staging- before .trash- so that if both survive a crash for one id, the
    // staging body (the committed-new content) is restored to the final path and the trash
    // (old body) is then discarded — never the other way around.
    const names = files.readdirSync(artifactDir).slice().sort((a, b) => {
      const rank = (n) => (n.includes(".staging-") ? 0 : n.includes(".trash-") ? 1 : 2);
      return rank(a) - rank(b) || a.localeCompare(b);
    });
    for (const name of names) {
      if (name === HISTORY_DIR) continue; // version-history store, not an orphan body
      if (name.startsWith(".") && (name.includes(".staging-") || name.includes(".trash-"))) {
        transientPaths.push(name);
        if (cleanTransient) {
          const match = name.match(/^\.([0-9a-z]{6,24})\.(?:staging|trash)-/);
          const row = match ? rowsById.get(match[1]) : null;
          const transient = path.join(artifactDir, name);
          const finalPath = row ? (row.is_bundle ? bundleDir(row.id) : filePath(row.id)) : null;
          const staged = !!row && name.includes(".staging-");
          const transientMatches = row && (!row.body_sha256 || bodyDigestAtPath(transient, !!row.is_bundle) === row.body_sha256);
          if (row && !files.existsSync(finalPath) && transientMatches) {
            // The interrupted body belongs at the (now-empty) final path.
            durableRename(files, transient, finalPath);
            recoveredPaths.push(name);
          } else if (staged && bodyDigestOnDisk(row.id, row.is_bundle) !== row.body_sha256) {
            // A staged body survived AND the installed body does not match the committed
            // metadata digest — i.e. the process crashed after committing the new revision
            // but before swapping the body in. Preserve the outgoing revision before installing
            // the committed replacement, exactly as the uninterrupted update path does.
            if (!row.body_sha256 || bodyDigestAtPath(transient, row.is_bundle) !== row.body_sha256) {
              // Staging is not the body named by committed metadata. Preserve both paths and let
              // the post-recovery audit report the installed divergence; no destructive step is
              // safe until an operator can inspect the torn or otherwise corrupt staging body.
              continue;
            }
            const outgoing = row.revision > 1 ? getRevisionStmt.get(row.id, row.revision - 1, row.id) : null;
            // Without an immutable outgoing revision, replacing the live body destroys the
            // last viable copy. Preserve both paths for an operator instead.
            if (!outgoing) continue;
            snapshotBody(row.id, outgoing, { moveBody: true });
            durableRename(files, transient, finalPath);
            recoveredPaths.push(name);
          } else {
            safeRemove(files, transient);
          }
        }
      } else if (!expected.has(name)) {
        orphanBodies.push(name);
      }
    }
    // Reclaim history for artifacts that no longer exist (e.g. a crash between the DB delete
    // and removeHistory). Revision rows already cascade-deleted; this removes their bodies.
    if (cleanTransient) {
      const historyRoot = path.join(artifactDir, HISTORY_DIR);
      if (files.existsSync(historyRoot)) {
        for (const hid of files.readdirSync(historyRoot)) {
          if (!rowsById.has(hid)) {
            safeRemove(files, path.join(historyRoot, hid));
            orphanHistory.push(hid);
          }
        }
      }
    }
    // Generic reconciliation may have completed a metadata/body transition. Re-resolve intents
    // and rows so a recovered artifact is no longer concealed by a stale prepared marker.
    if (cleanTransient) {
      recoverDurabilityIntents();
      rows = listBodies.all();
    }
    const missingBodies = rows
      .filter((row) => !files.existsSync(row.is_bundle ? bundleDir(row.id) : filePath(row.id)))
      .map((row) => row.id);
    for (const row of rows) {
      const finalPath = row.is_bundle ? bundleDir(row.id) : filePath(row.id);
      if (row.body_sha256 && files.existsSync(finalPath) && bodyDigestAtPath(finalPath, row.is_bundle) !== row.body_sha256) {
        divergentBodies.push(row.id);
      }
    }
    return { missingBodies, divergentBodies, orphanBodies, orphanHistory, transientPaths, recoveredPaths };
  }

  // Backfill the current content digest for artifacts created before body_sha256 existed (the
  // migration added the column with an empty default and never hashed existing bodies). Recomputes
  // from the installed body using the same function the storage audit trusts, and never bumps the
  // revision or updated_at — this is metadata repair, not a content mutation. Scoped to the
  // artifacts table (the current revision), which is all the gallery/Discord thumbnails key on;
  // archived history rows are not rendered. Idempotent: only touches rows whose digest is blank.
  const backfillDigestStmt = database.prepare(
    "UPDATE artifacts SET body_sha256 = @digest WHERE id = @id AND (body_sha256 IS NULL OR body_sha256 = '')"
  );
  function backfillBodyDigests() {
    const rows = database
      .prepare("SELECT id, is_bundle FROM artifacts WHERE body_sha256 IS NULL OR body_sha256 = ''")
      .all();
    let updated = 0;
    for (const row of rows) {
      const finalPath = row.is_bundle ? bundleDir(row.id) : filePath(row.id);
      if (!files.existsSync(finalPath)) continue;
      let digest;
      try { digest = bodyDigestOnDisk(row.id, row.is_bundle); } catch { continue; }
      if (!digest) continue;
      backfillDigestStmt.run({ id: row.id, digest });
      updated += 1;
    }
    return { scanned: rows.length, updated };
  }

  return {
    publish,
    publishBundle,
    update,
    backfillBodyDigests,
    restore: ({ id, revision, clientId, isAdmin }) => restoreById(id, revision, clientId, isAdmin),
    restoreArtifactRevision: (id, revision) => restoreById(id, revision, null, true),
    listRevisions,
    readHistoryArtifact,
    readHistoryBundleFile,
    setCategory,
    setHidden,
    moveArtifactToOrg,
    readBundleFile,
    listBundleFiles,
    listOrgIds: (org, { includeHidden = false } = {}) => readyRows((includeHidden ? listIdsByOrgIncludingHidden : listIdsByOrg).all(org)).map((row) => row.id),
    readArtifact,
    listForClient: (clientId, org) => readyRows(org == null ? listByClient.all(clientId) : listByClientOrg.all(clientId, org)),
    listOrgGroupedByClient: (org) => groupBy(readyRows(listByOrg.all(org)), (row) => row.client_id),
    listOrgArtifacts: (org, { includeHidden = false, ownerEmail = null } = {}) => ownerEmail && !includeHidden
      ? readyRows(listByOrgForOwner.all(org, ownerEmail))
      : readyRows((includeHidden ? listByOrgIncludingHidden : listByOrg).all(org)),
    listOrgIds: (org, { includeHidden = false, ownerEmail = null } = {}) => ownerEmail && !includeHidden
      ? readyRows(listIdsByOrgForOwner.all(org, ownerEmail)).map((row) => row.id)
      : readyRows((includeHidden ? listIdsByOrgIncludingHidden : listIdsByOrg).all(org)).map((row) => row.id),
    listAllGroupedByOrg: ({ includeHidden = false } = {}) => groupBy(readyRows((includeHidden ? listAll : listAllVisible).all()), (row) => row.org),
    remove: ({ id, clientId, isAdmin }) => removeById(id, clientId, isAdmin),
    getArtifactMeta: (id) => isReserved(id) ? null : getMetaStmt.get(id) || null,
    deleteArtifactById: (id) => removeById(id),
    auditStorage
  };
}

const defaultStore = createArtifactStore({ db, artifactDir: ARTIFACT_DIR });

export const backfillBodyDigests = defaultStore.backfillBodyDigests;
export const publish = defaultStore.publish;
export const publishBundle = defaultStore.publishBundle;
export const update = defaultStore.update;
export const restore = defaultStore.restore;
export const restoreArtifactRevision = defaultStore.restoreArtifactRevision;
export const listRevisions = defaultStore.listRevisions;
export const readHistoryArtifact = defaultStore.readHistoryArtifact;
export const readHistoryBundleFile = defaultStore.readHistoryBundleFile;
export const setCategory = defaultStore.setCategory;
export const setHidden = defaultStore.setHidden;
export const moveArtifactToOrg = defaultStore.moveArtifactToOrg;
export const readBundleFile = defaultStore.readBundleFile;
export const listBundleFiles = defaultStore.listBundleFiles;
export const listOrgIds = defaultStore.listOrgIds;
export const readArtifact = defaultStore.readArtifact;
export const listForClient = defaultStore.listForClient;
export const listOrgGroupedByClient = defaultStore.listOrgGroupedByClient;
export const listOrgArtifacts = defaultStore.listOrgArtifacts;
export const listAllGroupedByOrg = defaultStore.listAllGroupedByOrg;
export const remove = defaultStore.remove;
export const getArtifactMeta = defaultStore.getArtifactMeta;
export const deleteArtifactById = defaultStore.deleteArtifactById;
export const auditStorage = defaultStore.auditStorage;
