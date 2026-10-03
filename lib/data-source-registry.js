// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { parseDataSources, validateParams } from "./artifact-data.js";

const SOURCE_FIELDS = new Set(["id", "org", "kind", "base_url", "headers_env", "operations", "subscriptions"]);
const SAFE_ID = /^[A-Za-z0-9._-]{1,64}$/;

function fail(code, message = code, status = 400) { const error = new Error(message); error.code = code; error.status = status; throw error; }
function object(value) { return value && typeof value === "object" && !Array.isArray(value); }
function definitionOf(source) {
  const out = {};
  for (const key of SOURCE_FIELDS) if (source[key] !== undefined) {
    if (source.kind === "push" && (key === "base_url" || key === "headers_env")) continue;
    if (key === "headers_env" && (!source[key] || !Object.keys(source[key]).length)) continue;
    out[key] = source[key];
  }
  return out;
}
function missingReferences(definition, env) {
  const missing = [];
  for (const name of Object.values(definition.headers_env || {})) if (!env[name]) missing.push(name);
  return [...new Set(missing)].sort();
}
function validateEnvelope(body, fields) {
  if (!object(body)) fail("bad_params", "Request body must be an object.");
  const unknown = Object.keys(body).find((key) => !fields.has(key));
  if (unknown) fail("bad_params", `Unknown field: ${unknown}`);
}

export function createDataSourceRegistry({ db, data, operatorSources = [], env = process.env, orgs = null, artifacts = null, audit: auditLedger = null, logger = console } = {}) {
  if (!db || !data) throw new Error("Data source registry requires database and artifact data broker");
  const rows = {
    all: db.prepare("SELECT id,org,definition,enabled,version,created_at,updated_at FROM data_sources ORDER BY id"),
    get: db.prepare("SELECT id,org,definition,enabled,version,created_at,updated_at FROM data_sources WHERE id=?"),
    insert: db.prepare("INSERT INTO data_sources (id,org,definition,enabled,version,created_at,updated_at) VALUES (?,?,?, ?,1,datetime('now'),datetime('now'))"),
    update: db.prepare("UPDATE data_sources SET org=?,definition=?,enabled=?,version=version+1,updated_at=datetime('now') WHERE id=? AND version=?"),
    remove: db.prepare("DELETE FROM data_sources WHERE id=? AND version=?")
  };
  const operator = new Map();
  for (const source of operatorSources) { if (operator.has(source.id)) fail("source_conflict", "Duplicate operator connection id.", 409); operator.set(source.id, source); }
  const health = new Map();
  const orgExists = (org) => !orgs?.has || orgs.has(org);
  const managedDefinition = (row) => { try { return JSON.parse(row.definition); } catch { fail("data_unavailable", "Stored source definition is invalid.", 502); } };
  const managedSource = (row) => {
    const definition = managedDefinition(row), parseEnv = { ...env };
    const missing = missingReferences(definition, env);
    for (const name of missing) parseEnv[name] = "__configured_reference__";
    let parsed;
    try { parsed = parseDataSources(JSON.stringify({ sources: [definition] }), parseEnv)[0]; }
    catch { fail("invalid_source", "Stored connection definition is invalid."); }
    if (parsed.id !== row.id || parsed.org !== row.org || !orgExists(parsed.org)) fail("invalid_source", "Stored connection identity is invalid.");
    return { ...parsed, ...(missing.length ? { headers: {}, configError: true } : {}), enabled: Boolean(row.enabled), version: row.version, origin: "managed", missing };
  };
  function allSources() {
    const out = [...operator.values()].map((source) => ({ ...source, origin: "operator", version: 1, enabled: true, missing: [] }));
    for (const row of rows.all.all()) { if (operator.has(row.id)) fail("source_conflict", "Managed connection conflicts with an operator connection.", 409); out.push(managedSource(row)); }
    if (new Set(out.map((item) => item.id)).size !== out.length || out.length > 32) fail("source_conflict", "Combined connection registry is invalid.", 409);
    return out.sort((a, b) => a.id.localeCompare(b.id));
  }
  function source(id) { return allSources().find((item) => item.id === id) || null; }
  function sourceDefinition(value) {
    try { validateEnvelope(value, SOURCE_FIELDS); } catch { fail("invalid_source", "The connection definition is invalid."); }
    if (!SAFE_ID.test(value.id || "") || typeof value.org !== "string" || !value.org) fail("invalid_source", "Source id and organization are required.");
    if (!orgExists(value.org)) fail("unknown_org", "The selected organization does not exist.");
    const parseEnv = { ...env };
    for (const name of Object.values(value.headers_env || {})) if (!parseEnv[name]) parseEnv[name] = "__configured_reference__";
    let parsed;
    try { parsed = parseDataSources(JSON.stringify({ sources: [value] }), parseEnv)[0]; }
    catch (error) { fail("invalid_source", "The connection definition is invalid."); }
    return { parsed, definition: definitionOf(value) };
  }
  function bindingImpact(id) {
    const artifactsOut = [];
    for (const row of db.prepare("SELECT artifact_id,org,bindings FROM artifact_data_bindings ORDER BY artifact_id").all()) {
      let manifest; try { manifest = JSON.parse(row.bindings)?.bindings || {}; } catch { continue; }
      const bindings = [];
      for (const [name, item] of Object.entries(manifest)) if (item.source === id) bindings.push({ name, operations: item.operations || [], subscriptions: item.subscriptions || [] });
      if (!bindings.length) continue;
      const meta = artifacts?.getArtifactMeta?.(row.artifact_id);
      if (!meta || meta.org !== row.org) continue;
      artifactsOut.push({ id: row.artifact_id, title: meta.title || "", org: meta.org, url: `/${row.artifact_id}`, bindings });
    }
    return artifactsOut;
  }
  function view(item) {
    if (!item) return null;
    const status = data.getSourceHealth?.(item.id) || health.get(item.id) || {};
    return { id: item.id, org: item.org, origin: item.origin, enabled: item.enabled !== false, version: item.version || 1,
      definition: definitionOf(item), missing_references: item.missing || [],
      health: { state: item.enabled === false ? "disabled" : item.configError ? "config_error" : status.state || "idle", last_success_at: status.last_success_at || null, last_query_at: status.last_query_at || null, last_event_at: status.last_event_at || null, retry_count: status.retry_count || 0, error: status.error || null },
      artifact_count: bindingImpact(item.id).length };
  }
  function auditInTransaction(context, operation, id, result, org) {
    if (!auditLedger) return;
    auditLedger.appendInTransaction({ ...context, tenant: org || rows.get.get(id)?.org }, { operation, targetType: "data_source", targetId: id, result, classification: "configuration" });
  }
  function list(org) { return allSources().filter((item) => !org || item.org === org).map(view); }
  function get(id) { const item = source(id); if (!item) fail("not_found", "Connection not found.", 404); return view(item); }
  function create(body, context) {
    validateEnvelope(body, new Set(["definition", "enabled"]));
    if (typeof body.enabled !== "boolean" && body.enabled !== undefined) fail("bad_params", "enabled must be boolean.");
    let id;
    db.transaction(() => {
      const { parsed, definition } = sourceDefinition(body.definition); id = parsed.id;
      if (operator.has(id) || rows.get.get(id)) fail("source_conflict", "A connection with this id already exists.", 409);
      if (allSources().length >= 32) fail("invalid_source", "The maximum number of connections is 32.");
      rows.insert.run(id, parsed.org, JSON.stringify(definition), body.enabled === false ? 0 : 1);
      auditInTransaction(context, "data_source.create", id, "success");
    }).immediate();
    data.refreshSources?.(allSources()); return get(id);
  }
  function update(id, body, context, auditOperation = "data_source.update") {
    validateEnvelope(body, new Set(["definition", "enabled", "expected_version"]));
    if (operator.has(id)) fail("operator_read_only", "Operator connections are read-only.", 409);
    if (body.definition?.id !== id) fail("invalid_source", "Connection id cannot change.");
    if (typeof body.enabled !== "boolean" || !Number.isSafeInteger(body.expected_version) || body.expected_version < 0) fail("bad_params", "Use a boolean enabled state and nonnegative expected_version.");
    db.transaction(() => {
      const row = rows.get.get(id); if (!row) fail("not_found", "Connection not found.", 404);
      if (row.version !== body.expected_version) fail("stale_version", "Connection changed; reload before saving.", 409);
      const { parsed, definition } = sourceDefinition(body.definition), impact = bindingImpact(id);
      if (parsed.org !== row.org && impact.length) fail("org_in_use", "Move or remove bindings before changing organization.", 409);
      for (const item of impact.flatMap((artifact) => artifact.bindings)) {
        for (const name of item.operations || []) if (!parsed.operations?.[name]) fail("binding_in_use", "Remove or migrate the affected bindings first.", 409);
        for (const name of item.subscriptions || []) if (!parsed.subscriptions?.[name]) fail("binding_in_use", "Remove or migrate the affected bindings first.", 409);
      }
      if (!rows.update.run(parsed.org, JSON.stringify(definition), body.enabled ? 1 : 0, id, row.version).changes) fail("stale_version", "Connection changed; reload before saving.", 409);
      auditInTransaction(context, auditOperation, id, "success");
    }).immediate();
    data.refreshSources?.(allSources()); return get(id);
  }
  function setEnabled(id, enabled, expected, context) {
    const item = source(id); if (!item) fail("not_found", "Connection not found.", 404);
    return update(id, { definition: definitionOf(item), enabled, expected_version: expected }, context, enabled ? "data_source.enable" : "data_source.disable");
  }
  function remove(id, expected, context) {
    if (operator.has(id)) fail("operator_read_only", "Operator connections are read-only.", 409);
    if (!Number.isSafeInteger(expected) || expected < 0) fail("bad_params", "expected_version must be a nonnegative integer.");
    db.transaction(() => {
      const row = rows.get.get(id); if (!row) fail("not_found", "Connection not found.", 404);
      if (row.version !== expected) fail("stale_version", "Connection changed; reload before deleting.", 409);
      if (bindingImpact(id).length) fail("source_in_use", "Remove all artifact bindings before deleting this connection.", 409);
      rows.remove.run(id, expected); auditInTransaction(context, "data_source.delete", id, "success", row.org);
    }).immediate();
    data.refreshSources?.(allSources()); return { deleted: true, id };
  }
  function impact(id) { const item = source(id); if (!item) fail("not_found", "Connection not found.", 404); const artifacts = bindingImpact(id); return { artifacts, can_delete: item.origin === "managed" && artifacts.length === 0 }; }
  async function test(id, operation, params = {}, context) {
    const item = source(id); if (!item) fail("not_found", "Connection not found.", 404); if (!item.enabled || item.configError) fail("test_unavailable", "Connection is unavailable.", 502);
    if (!object(params) || item.kind === "push" && (operation !== undefined && operation !== "" || Object.keys(params).length)) fail("bad_params", "Request parameters are invalid.");
    const op = item.operations?.[operation];
    if (item.kind === "http" && (typeof operation !== "string" || !op || !validateParams(op.params, params).ok)) fail("bad_params", "Request parameters are invalid.");
    const started = Date.now(); let result;
    try { result = item.kind === "push" ? { ok: true, summary: { kind: "push" } } : await data.testSource(item, op, params); }
    catch { result = { ok: false, error: "data_unavailable" }; }
    if (result.error === "bad_params") fail("bad_params", "Request parameters are invalid.");
    const out = { ok: Boolean(result.ok), source_id: id, elapsed_ms: Date.now() - started, summary: result.summary || { kind: item.kind }, error: result.ok ? null : (result.error || "test_unavailable") };
    const prior = data.getSourceHealth?.(id) || health.get(id) || {};
    health.set(id, { ...prior, state: out.ok ? "connected" : "unavailable", last_query_at: new Date().toISOString(), last_success_at: out.ok ? new Date().toISOString() : (prior.last_success_at || null), error: out.ok ? null : out.error, retry_count: out.ok ? 0 : (prior.retry_count || 0) + 1 });
    data.recordSourceHealth?.(id, health.get(id));
    try { auditLedger?.append({ ...context, tenant: item.org }, { operation: "data_source.test", targetType: "data_source", targetId: id, result: out.ok ? "success" : "failure", classification: "configuration" }); } catch (error) { logger.warn?.("[artifact-mcp] source test audit failed"); }
    return out;
  }
  data.refreshSources?.(allSources());
  return { list, get, create, update, enable: (id, v, c) => setEnabled(id, true, v, c), disable: (id, v, c) => setEnabled(id, false, v, c), remove, impact, test, source, allSources };
}
