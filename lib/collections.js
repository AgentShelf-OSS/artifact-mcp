// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { customAlphabet } from "nanoid";
import { auditContextFromViewer } from "./audit.js";

const makeId = customAlphabet("0123456789abcdefghijkmnpqrstuvwxyz", 12);
const MAX_COLLECTIONS = 200;
const MAX_MEMBERS = 1000;
const MAX_BULK = 100;
const MAX_STATE_IDS = 2000;
const HEX = /^#[0-9a-f]{6}$/i;

export class CollectionError extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

function fail(status, code, message) { throw new CollectionError(status, code, message); }
function objectBody(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) fail(400, "invalid_body", "request body must be an object");
  return value;
}
function strictKeys(value, allowed) {
  const body = objectBody(value);
  if (Object.keys(body).some((key) => !allowed.includes(key))) fail(400, "invalid_body", "request contains an unknown field");
  return body;
}
function text(value, max, field = "value", optional = false) {
  if (value === undefined || value === null) { if (optional) return ""; fail(400, "invalid_body", `${field} must be a string`); }
  if (typeof value !== "string") fail(400, "invalid_body", `${field} must be a string`);
  const normalized = value.trim().replace(/\s+/g, " ");
  if ([...normalized].length > max) fail(400, "invalid_body", `${field} is too long`);
  return normalized;
}
function nameKey(value) { return text(value, 80, "name").toLocaleLowerCase("en-US"); }
function requireName(value) {
  const name = text(value, 80, "name");
  if (!name) fail(400, "invalid_name", "name is required");
  return name;
}
function validateIdList(value, field = "artifactIds", max = MAX_MEMBERS) {
  if (!Array.isArray(value) || value.length > max || value.some((id) => typeof id !== "string" || !/^[A-Za-z0-9_-]{1,128}$/.test(id))) {
    fail(400, "invalid_ids", `${field} must be an array of at most ${max} artifact IDs`);
  }
  return [...new Set(value)];
}
function normalizeState(state, collectionsByOrg, { allowAll = false } = {}) {
  if (state == null) return { collectionOrderByOrg: {}, collapsedCollectionIdsByOrg: {} };
  if (typeof state !== "object" || Array.isArray(state)) fail(400, "invalid_preferences", "state must be an object");
  const allowed = new Map(Object.entries(collectionsByOrg).map(([org, ids]) => [org, new Set(ids)]));
  const clean = (source) => {
    if (source == null) return {};
    if (typeof source !== "object" || Array.isArray(source)) fail(400, "invalid_preferences", "preference lists must be objects");
    const output = Object.create(null);
    for (const [org, ids] of Object.entries(source)) {
      if (!Array.isArray(ids) || ids.length > MAX_STATE_IDS || ids.some(id => typeof id !== "string" || !/^[A-Za-z0-9_-]{1,128}$/.test(id))) fail(400, "invalid_preferences", "preference lists are too large");
      if (!allowed.has(org) || (org === "all" && !allowAll)) continue;
      const set = allowed.get(org);
      output[org] = [...new Set(ids.filter((id) => typeof id === "string" && set.has(id)))].slice(0, MAX_STATE_IDS);
    }
    return { ...output };
  };
  return { collectionOrderByOrg: clean(state.collectionOrderByOrg), collapsedCollectionIdsByOrg: clean(state.collapsedCollectionIdsByOrg) };
}

function canReadArtifact(viewer, artifact) {
  if (!artifact || !viewer?.email || artifact.org !== viewer.org && !viewer.isAdmin) return false;
  return viewer.isAdmin || !artifact.hidden || String(artifact.owner_email || "").toLowerCase() === String(viewer.email).toLowerCase();
}

export function createCollectionService({ db, artifacts, audit = null, thumbnailPath = (id, meta) => `/thumbnails/${encodeURIComponent(id)}?v=${encodeURIComponent(meta.body_sha256 || "")}` }) {
  const listCollections = db.prepare("SELECT * FROM collections WHERE org = ? ORDER BY created_at ASC, id ASC");
  const getCollection = db.prepare("SELECT * FROM collections WHERE id = ? AND org = ?");
  const memberRows = db.prepare("SELECT artifact_id FROM collection_artifacts WHERE collection_id = ? AND org = ? ORDER BY created_at ASC, artifact_id ASC");
  const getPref = db.prepare("SELECT * FROM gallery_preferences WHERE viewer_email = ?");
  const collectionsForOrg = (org) => listCollections.all(org);

  function resolveOrg(viewer, requested) {
    if (!viewer?.email) fail(401, "unauthorized", "Not signed in");
    if (requested !== undefined && requested !== null && typeof requested !== "string") fail(400, "invalid_query", "org must be a string");
    const org = String(requested || (viewer.isAdmin ? "" : viewer.org) || "").trim();
    if (!org && viewer.isAdmin && requested === undefined) return null;
    if (viewer.isAdmin && org === "all") return null;
    if (!org || (!viewer.isAdmin && org !== viewer.org)) fail(viewer.isAdmin ? 400 : 404, viewer.isAdmin ? "org_required" : "not_found", viewer.isAdmin ? "org is required" : "Not found");
    if (!db.prepare("SELECT 1 FROM orgs WHERE name = ?").get(org)) fail(404, "not_found", "Not found");
    return org;
  }
  function collectionOr404(id, org) {
    const row = getCollection.get(id, org);
    if (!row) fail(404, "not_found", "Not found");
    return row;
  }
  function editable(viewer, row) {
    if (!viewer?.isAdmin && String(row.created_by).toLowerCase() !== String(viewer?.email || "").toLowerCase()) fail(403, "forbidden", "Only the collection creator or an administrator can edit this collection");
  }
  function visibleArtifacts(viewer, org) {
    return (artifacts.listOrgArtifacts(org, { includeHidden: viewer.isAdmin, ownerEmail: viewer.isAdmin ? null : viewer.email }) || []).filter((a) => canReadArtifact(viewer, a));
  }
  function project(row, viewer, visible = new Map(visibleArtifacts(viewer, row.org).map((a) => [a.id, a]))) {
    const ids = memberRows.all(row.id, row.org).map(({ artifact_id }) => artifact_id).filter((id) => visible.has(id));
    const cover = row.cover_artifact_id && visible.has(row.cover_artifact_id) ? row.cover_artifact_id : null;
    return {
      id: row.id, org: row.org, name: row.name, description: row.description, color: row.color,
      createdBy: row.created_by, editable: Boolean(viewer.isAdmin || String(row.created_by).toLowerCase() === String(viewer.email).toLowerCase()),
      artifactCount: ids.length, artifactIds: ids, coverArtifactId: cover,
      previewArtifacts: (cover ? [cover, ...ids.filter(id => id !== cover)] : ids).slice(0, 3).map((id) => ({ id, thumbnail: thumbnailPath(id, visible.get(id)) }))
    };
  }
  function preferenceProjection(viewer, org) {
    const rows = org == null
      ? db.prepare("SELECT org, id FROM collections ORDER BY org, created_at, id").all()
      : db.prepare("SELECT org, id FROM collections WHERE org = ? ORDER BY created_at, id").all(org);
    const byOrg = Object.create(null);
    for (const row of rows) (byOrg[row.org] ||= []).push(row.id);
    const pref = getPref.get(String(viewer.email).toLowerCase());
    let state = {};
    try { state = JSON.parse(pref?.state_json || "{}"); } catch {}
    if (viewer.isAdmin && org == null) byOrg.all = [...new Set(Object.values(byOrg).flat())].slice(0, MAX_STATE_IDS);
    const normalized = normalizeState(state, byOrg, { allowAll: viewer.isAdmin && org == null });
    return {
      view: pref?.view || "reel", previewSize: pref?.preview_size || "compact", artifactLayout: pref?.artifact_layout || "grid",
      collectionOrderByOrg: normalized.collectionOrderByOrg, collapsedCollectionIdsByOrg: normalized.collapsedCollectionIdsByOrg
    };
  }
  function projection(viewer, org) {
    const orgs = org == null ? db.prepare("SELECT name FROM orgs ORDER BY name").pluck().all() : [org];
    const rows = orgs.flatMap((name) => collectionsForOrg(name));
    const visible = new Map(orgs.flatMap((name) => visibleArtifacts(viewer, name)).map((a) => [a.id, a]));
    const projected = rows.map((row) => project(row, viewer, visible));
    const collected = new Set(projected.flatMap((row) => row.artifactIds));
    return { collections: projected, uncollectedCount: [...visible.keys()].filter((id) => !collected.has(id)).length, preferences: preferenceProjection(viewer, org) };
  }
  function auditMutation(viewer, operation, row) {
    if (!audit?.appendInTransaction) return;
    audit.appendInTransaction({ ...auditContextFromViewer(viewer, { source: "browser" }), tenant: row.org }, { operation, targetType: "collection", targetId: row?.id || "", result: "success", classification: "collection_mutation" });
  }
  function ensureArtifactTargets(viewer, org, ids, max = MAX_MEMBERS) {
    const unique = validateIdList(ids, "artifactIds", max);
    const rows = unique.map((id) => artifacts.getArtifactMeta(id));
    if (rows.some((a) => !a || a.org !== org || !canReadArtifact(viewer, a))) fail(404, "not_found", "Not found");
    return unique;
  }
  function create(viewer, input) {
    input = strictKeys(input, ["org", "name", "description", "color", "coverArtifactId", "artifactIds"]);
    const org = resolveOrg(viewer, input?.org);
    if (!org) fail(400, "org_required", "org is required");
    const name = requireName(input?.name);
    const ids = ensureArtifactTargets(viewer, org, input.artifactIds === undefined ? [] : input.artifactIds, MAX_BULK);
    if (input?.color !== undefined && input?.color !== null && typeof input.color !== "string") fail(400, "invalid_color", "color must be a string");
    const color = input?.color == null || input.color === "" ? null : input.color;
    if (color && !HEX.test(color)) fail(400, "invalid_color", "color must be a six-digit hex value");
    const id = makeId();
    try {
      db.transaction(() => {
        if (collectionsForOrg(org).length >= MAX_COLLECTIONS) fail(400, "collection_limit", "organization collection limit reached");
        ensureArtifactTargets(viewer, org, ids);
        db.prepare("INSERT INTO collections (id,org,name,name_key,description,color,created_by) VALUES (?,?,?,?,?,?,?)").run(id, org, name, nameKey(name), text(input?.description, 500, "description", true), color, viewer.email);
        const insert = db.prepare("INSERT INTO collection_artifacts (collection_id,artifact_id,org) VALUES (?,?,?)");
        for (const artifactId of ids) insert.run(id, artifactId, org);
        if (input?.coverArtifactId != null) {
          if (typeof input.coverArtifactId !== "string" || !ids.includes(input.coverArtifactId)) fail(400, "invalid_cover", "coverArtifactId must be a current collection member");
          db.prepare("UPDATE collections SET cover_artifact_id = ?, cover_artifact_org = ? WHERE id = ? AND org = ?").run(input.coverArtifactId, org, id, org);
        }
        auditMutation(viewer, "collection.create", { id, org });
      })();
    } catch (error) {
      if (error.code === "SQLITE_CONSTRAINT_UNIQUE") fail(409, "duplicate_name", "A collection with that name already exists");
      throw error;
    }
    const row = collectionOr404(id, org); return project(row, viewer);
  }
  function update(viewer, id, input) {
    input = strictKeys(input, ["org", "name", "description", "color", "coverArtifactId"]);
    const org = resolveOrg(viewer, input?.org);
    if (!org) fail(400, "org_required", "org is required");
    const row = collectionOr404(id, org); editable(viewer, row);
    const name = input?.name === undefined ? row.name : requireName(input.name);
    if (input?.color !== undefined && input?.color !== null && typeof input.color !== "string") fail(400, "invalid_color", "color must be a string");
    const color = input?.color === undefined ? row.color : (input.color === "" || input.color == null ? null : input.color);
    if (color && !HEX.test(color)) fail(400, "invalid_color", "color must be a six-digit hex value");
    let cover = row.cover_artifact_id;
    if (Object.hasOwn(input || {}, "coverArtifactId")) {
      if (input.coverArtifactId !== null && typeof input.coverArtifactId !== "string") fail(400, "invalid_cover", "coverArtifactId must be an artifact ID or null");
      cover = input.coverArtifactId || null;
      if (cover) {
        ensureArtifactTargets(viewer, org, [cover]);
        if (!memberRows.all(id, org).some((member) => member.artifact_id === cover)) fail(400, "invalid_cover", "coverArtifactId must be a current collection member");
      }
    }
    try { db.transaction(() => {
      if (cover) {
        ensureArtifactTargets(viewer, org, [cover]);
        if (!memberRows.all(id, org).some((member) => member.artifact_id === cover)) fail(400, "invalid_cover", "coverArtifactId must be a current collection member");
      }
      db.prepare("UPDATE collections SET name = ?, name_key = ?, description = ?, color = ?, cover_artifact_id = ?, cover_artifact_org = ?, updated_at = datetime('now') WHERE id = ? AND org = ?").run(name, nameKey(name), input?.description === undefined ? row.description : text(input.description, 500, "description", true), color, cover, cover ? org : null, id, org);
      auditMutation(viewer, "collection.update", { id, org });
    })(); }
    catch (error) { if (error.code === "SQLITE_CONSTRAINT_UNIQUE") fail(409, "duplicate_name", "A collection with that name already exists"); throw error; }
    const next = collectionOr404(id, org); return project(next, viewer);
  }
  function remove(viewer, id, requestedOrg) {
    const org = resolveOrg(viewer, requestedOrg); const row = collectionOr404(id, org); editable(viewer, row);
    db.transaction(() => { db.prepare("DELETE FROM collections WHERE id = ? AND org = ?").run(id, org); auditMutation(viewer, "collection.delete", row); })(); return { id, deleted: true };
  }
  function memberships(viewer, id, input, adding, requestedOrg) {
    input = strictKeys(input, ["artifactIds"]);
    const org = resolveOrg(viewer, requestedOrg); const row = collectionOr404(id, org); editable(viewer, row); const ids = ensureArtifactTargets(viewer, org, input.artifactIds === undefined ? [] : input.artifactIds, MAX_BULK);
    let existing;
    const add = db.prepare("INSERT OR IGNORE INTO collection_artifacts (collection_id,artifact_id,org) VALUES (?,?,?)");
    const del = db.prepare("DELETE FROM collection_artifacts WHERE collection_id = ? AND artifact_id = ? AND org = ?");
    db.transaction(() => {
      editable(viewer, collectionOr404(id, org));
      existing = new Set(memberRows.all(id, org).map((r) => r.artifact_id));
      if (adding && new Set([...existing, ...ids]).size > MAX_MEMBERS) fail(400, "membership_limit", "collection member limit reached");
      ensureArtifactTargets(viewer, org, ids, MAX_BULK);
      if (adding) ids.forEach((artifactId) => add.run(id, artifactId, org));
      else {
        ids.forEach((artifactId) => del.run(id, artifactId, org));
        if (ids.length) db.prepare(`UPDATE collections SET cover_artifact_id = NULL, cover_artifact_org = NULL, updated_at = datetime('now') WHERE id = ? AND org = ? AND cover_artifact_id IN (${ids.map(() => "?").join(",")})`).run(id, org, ...ids);
      }
      db.prepare("UPDATE collections SET updated_at=datetime('now') WHERE id=? AND org=?").run(id,org);
      auditMutation(viewer, adding ? "collection.membership.add" : "collection.membership.remove", row);
    })();
    return adding ? { collectionId: id, added: ids.filter((x) => !existing.has(x)), alreadyPresent: ids.filter((x) => existing.has(x)) } : { collectionId: id, removed: ids.filter((x) => existing.has(x)) };
  }
  function getPreferences(viewer, requestedOrg) { const org = resolveOrg(viewer, requestedOrg); return preferenceProjection(viewer, org); }
  function setPreferences(viewer, input, requestedOrg) {
    input = strictKeys(input, ["view", "previewSize", "artifactLayout", "collectionOrderByOrg", "collapsedCollectionIdsByOrg"]);
    const requested = resolveOrg(viewer, requestedOrg); const org = viewer.isAdmin ? null : requested; const current = preferenceProjection(viewer, org);
    const next = { ...current, ...(input || {}) };
    if (!['reel','sheets','ribbons','all'].includes(next.view) || !['compact','large'].includes(next.previewSize) || !['grid','list'].includes(next.artifactLayout)) fail(400, "invalid_preferences", "invalid gallery preference");
    const rows = org == null ? db.prepare("SELECT org,id FROM collections").all() : db.prepare("SELECT org,id FROM collections WHERE org=?").all(org); const byOrg = Object.create(null); for (const r of rows) (byOrg[r.org] ||= []).push(r.id);
    if (viewer.isAdmin && org == null) byOrg.all = [...new Set(Object.values(byOrg).flat())].slice(0, MAX_STATE_IDS);
    const normalized = normalizeState(next, byOrg, { allowAll: viewer.isAdmin && org == null });
    db.prepare("INSERT INTO gallery_preferences (viewer_email,view,preview_size,artifact_layout,state_json,updated_at) VALUES (?,?,?,?,?,datetime('now')) ON CONFLICT(viewer_email) DO UPDATE SET view=excluded.view,preview_size=excluded.preview_size,artifact_layout=excluded.artifact_layout,state_json=excluded.state_json,updated_at=datetime('now')").run(String(viewer.email).toLowerCase(), next.view, next.previewSize, next.artifactLayout, JSON.stringify(normalized));
    return { view: next.view, previewSize: next.previewSize, artifactLayout: next.artifactLayout, ...normalized };
  }
  return { list: (viewer, org) => { const resolved = resolveOrg(viewer, org); return projection(viewer, resolved); }, create, update, remove, addMemberships: (v, id, body, org) => memberships(v, id, body, true, org), removeMemberships: (v, id, body, org) => memberships(v, id, body, false, org), getPreferences, setPreferences, project };
}

export default createCollectionService;
