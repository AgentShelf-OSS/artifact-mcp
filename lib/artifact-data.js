// SPDX-License-Identifier: Apache-2.0
// Server-side data bindings for live HTML artifacts. Credentials and upstream URLs never
// cross this module's public serialization boundary.
import { readFileSync } from "node:fs";

const NAME_RE = /^[A-Za-z0-9._-]{1,64}$/;
const MAX_SOURCES = 32;
const MAX_ITEMS = 16;
const MAX_EVENT_BYTES = 1024 * 1024;
const MAX_SNAPSHOT_BYTES = 256 * 1024;
const MAX_RETAINED_EVENTS = 1000;
const CONTROL_RE = /[\x00-\x1f\x7f-\x9f]/;

function plain(value) { return value !== null && typeof value === "object" && !Array.isArray(value); }
function jsonBytes(value) { return Buffer.byteLength(JSON.stringify(value), "utf8"); }
async function readBoundedBody(response, maxBytes) {
  if (!response.body?.getReader) throw new Error("data_unavailable");
  const reader = response.body.getReader(); const chunks = []; let total = 0;
  while (true) { const part = await reader.read(); if (part.done) break; total += part.value.byteLength; if (total > maxBytes) { await reader.cancel(); throw new Error("too_large"); } chunks.push(Buffer.from(part.value)); }
  return Buffer.concat(chunks).toString("utf8");
}
function safePublicSchema(params = {}) {
  const output = {};
  for (const [name, schema] of Object.entries(params).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) {
    const copy = { type: schema.type };
    for (const key of ["required", "minimum", "maximum", "max_length", "enum", "default"]) {
      if (schema[key] !== undefined && (key !== "required" || schema[key])) copy[key] = schema[key];
    }
    output[name] = Object.fromEntries(Object.entries(copy).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0));
  }
  return output;
}

function fail(message) { throw new Error(message); }
function validateName(name, label) {
  if (typeof name !== "string" || !NAME_RE.test(name)) fail(`${label} must match ${NAME_RE}`);
}
function validateRelativePath(path, label) {
  if (typeof path !== "string" || !path.startsWith("/") || path.startsWith("//") || path.includes("\\") || path.includes("?") || path.includes("#") || CONTROL_RE.test(path) || /%(?:2e|2f|5c)/i.test(path) || path.split("/").some((part) => part === "." || part === "..")) {
    fail(`${label} must be a safe absolute path`);
  }
}
const SOURCE_FIELDS = new Set(["id", "org", "kind", "base_url", "headers_env", "operations", "subscriptions"]);
const OP_FIELDS = new Set(["path", "params", "max_bytes", "timeout_ms", "key"]);
const SUB_FIELDS = new Set(["transport", "path", "events", "operation", "interval_ms"]);
function publicSource(source) {
  return {
    id: source.id,
    kind: source.kind,
    operations: Object.fromEntries(Object.entries(source.operations || {}).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([name, op]) => [name, { params: safePublicSchema(op.params) }])),
    subscriptions: Object.fromEntries(Object.entries(source.subscriptions || {}).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(([name, sub]) => [name, { transport: sub.transport }]))
  };
}

export function parseDataSources(raw = process.env.ARTIFACT_DATA_SOURCES_FILE, env = process.env) {
  if (!raw || !String(raw).trim()) return [];
  let config;
  try {
    const value = String(raw).trim();
    config = JSON.parse(value.startsWith("{") ? value : readFileSync(value, "utf8"));
  } catch { fail("ARTIFACT_DATA_SOURCES_FILE must contain valid JSON"); }
  if (!plain(config) || Object.keys(config).some((key) => key !== "sources") || !Array.isArray(config.sources) || config.sources.length > MAX_SOURCES) fail("invalid data source configuration");
  return config.sources.map((input) => {
    if (!plain(input)) fail("invalid data source");
    if (Object.keys(input).some((key) => !SOURCE_FIELDS.has(key))) fail("unknown data source field");
    validateName(input.id, "source id");
    if (typeof input.org !== "string" || !input.org.trim()) fail("source org is required");
    const kind = input.kind === undefined ? "http" : input.kind;
    if (kind !== "http" && kind !== "push") fail("source kind must be http or push");
    if (kind === "http") {
      let base;
      try { base = new URL(input.base_url); } catch { fail("source base_url must be an absolute http(s) URL"); }
      if (!["http:", "https:"].includes(base.protocol) || base.username || base.password || base.search || base.hash) fail("source base_url is unsafe");
    } else if (input.base_url != null || input.headers_env && Object.keys(input.headers_env).length) fail("push source cannot configure HTTP access");
    for (const field of ["operations", "subscriptions", "headers_env"]) if (input[field] !== undefined && !plain(input[field])) fail("invalid data source field");
    const operations = plain(input.operations) ? input.operations : {};
    const subscriptions = plain(input.subscriptions) ? input.subscriptions : {};
    if (Object.keys(operations).length > MAX_ITEMS || Object.keys(subscriptions).length > MAX_ITEMS) fail("source operation/subscription limit exceeded");
    for (const name of Object.keys(operations).concat(Object.keys(subscriptions))) validateName(name, "operation name");
    const parsedOps = {};
    for (const [name, op] of Object.entries(operations)) {
      if (!plain(op)) fail("invalid data operation");
      if (Object.keys(op).some((key) => !OP_FIELDS.has(key))) fail("unknown operation field");
      if (kind === "http") validateRelativePath(op.path, "operation path");
      if (op.params !== undefined && !plain(op.params)) fail("invalid operation parameters");
      const params = op.params || {};
      if (Object.keys(params).length > MAX_ITEMS) fail("operation parameter limit exceeded");
      if (kind === "push") {
        if (op.path != null || Object.keys(params).length) fail("push operation cannot configure HTTP parameters");
        if (op.key != null) validateName(op.key, "snapshot key");
      } else if (op.key != null) fail("HTTP operation cannot configure a snapshot key");
      for (const [param, schema] of Object.entries(params)) {
        validateName(param, "parameter name");
        if (!plain(schema) || !["string", "integer", "boolean"].includes(schema.type)) fail("invalid operation parameter");
        if (Object.keys(schema).some((key) => !["type", "required", "minimum", "maximum", "max_length", "enum", "default"].includes(key))) fail("unknown operation parameter field");
        if (schema.required !== undefined && typeof schema.required !== "boolean") fail("invalid required parameter flag");
        const enumType = schema.type === "integer" ? "number" : schema.type;
        if (schema.enum !== undefined && (!Array.isArray(schema.enum) || schema.enum.length === 0 || schema.enum.length > 100 || schema.enum.some((value) => typeof value !== enumType || (schema.type === "integer" && !Number.isSafeInteger(value))))) fail("invalid parameter enum");
        if (schema.minimum !== undefined && (schema.type !== "integer" || !Number.isSafeInteger(schema.minimum))) fail("invalid parameter minimum");
        if (schema.maximum !== undefined && (schema.type !== "integer" || !Number.isSafeInteger(schema.maximum))) fail("invalid parameter maximum");
        if (schema.minimum !== undefined && schema.maximum !== undefined && schema.minimum > schema.maximum) fail("invalid parameter bounds");
        if (schema.default !== undefined && (schema.type === "string" && typeof schema.default !== "string" || schema.type === "integer" && !Number.isSafeInteger(schema.default) || schema.type === "boolean" && typeof schema.default !== "boolean")) fail("invalid parameter default");
        if (schema.default !== undefined && schema.enum && !schema.enum.includes(schema.default)) fail("parameter default must be in enum");
        if (schema.max_length !== undefined && (schema.type !== "string" || !Number.isInteger(schema.max_length) || schema.max_length < 1 || schema.max_length > 256)) fail("invalid parameter max_length");
        if (schema.type === "string" && schema.max_length === undefined) schema.max_length = 256;
        if (schema.enum?.some((value) => !validateParams({ value: { ...schema, enum: undefined, default: undefined } }, { value }).ok)) fail("parameter enum violates its constraints");
        if (schema.default !== undefined && !validateParams({ value: schema }, { value: schema.default }).ok) fail("parameter default violates its constraints");
      }
      const maxBytes = op.max_bytes === undefined ? 1048576 : op.max_bytes;
      const timeout = op.timeout_ms === undefined ? 10000 : op.timeout_ms;
      if (!Number.isInteger(maxBytes) || maxBytes < 1 || maxBytes > 1048576 || !Number.isInteger(timeout) || timeout < 100 || timeout > 30000) fail("invalid operation limits");
      if (kind === "http") {
        for (const placeholder of String(op.path).matchAll(/\{([^}]+)\}/g)) {
          const rule = params[placeholder[1]];
          if (!rule || rule.type !== "string" || rule.required !== true) fail("path placeholders require declared required string parameters");
        }
        if (op.path.replace(/\{[^}]+\}/g, "").match(/[{}]/)) fail("invalid path placeholder");
      }
      parsedOps[name] = { ...op, params, max_bytes: maxBytes, timeout_ms: timeout };
    }
    const parsedSubs = {};
    for (const [name, sub] of Object.entries(subscriptions)) {
      if (!plain(sub) || !["sse", "poll", "push"].includes(sub.transport)) fail("invalid data subscription");
      if (Object.keys(sub).some((key) => !SUB_FIELDS.has(key))) fail("unknown subscription field");
      if (kind === "push" && sub.transport !== "push") fail("push source requires push subscriptions");
      if (kind === "http" && sub.transport === "push") fail("HTTP source requires HTTP subscriptions");
      if (sub.transport === "sse") {
        validateRelativePath(sub.path, "subscription path");
        if (/[{}]/.test(sub.path) || sub.operation != null) fail("invalid SSE subscription fields");
        const events = sub.events === undefined || Array.isArray(sub.events) && !sub.events.length ? ["message"] : sub.events;
        if (!Array.isArray(events) || events.length > MAX_ITEMS || events.some((event) => typeof event !== "string" || !event.length || Buffer.byteLength(event) > 128 || CONTROL_RE.test(event))) fail("invalid subscription events");
        parsedSubs[name] = { ...sub, events: [...new Set(events)] };
      } else if (sub.transport === "poll") {
        validateName(sub.operation, "poll operation");
        if (!Object.hasOwn(parsedOps, sub.operation)) fail("poll operation must be configured");
        if (sub.path != null || sub.events !== undefined && (!Array.isArray(sub.events) || sub.events.length) || Object.values(parsedOps[sub.operation].params).some((rule) => rule.required && rule.default === undefined)) fail("poll operation requires complete default parameters");
        const interval = sub.interval_ms === undefined ? 2000 : sub.interval_ms;
        if (!Number.isInteger(interval) || interval < 1000 || interval > 60000) fail("invalid poll interval");
        parsedSubs[name] = { ...sub, interval_ms: interval };
      } else {
        if (sub.path != null || sub.operation != null || sub.events !== undefined && (!Array.isArray(sub.events) || sub.events.length)) fail("invalid push subscription fields");
        parsedSubs[name] = { transport: "push" };
      }
    }
    const headers = {};
    for (const [header, variable] of Object.entries(plain(input.headers_env) ? input.headers_env : {})) {
      if (typeof variable !== "string" || !variable || !env[variable]) fail("configured data header environment value is missing");
      if (["host", "cookie", "connection", "content-length", "transfer-encoding"].includes(header.toLowerCase())) fail("unsafe data source header");
      try { new Headers({ [header]: env[variable] }); } catch { fail("invalid data source header"); }
      headers[header] = env[variable];
    }
    return Object.freeze({ id: input.id, org: input.org, kind, base_url: kind === "http" ? String(input.base_url).replace(/\/$/, "") : "", headers, operations: parsedOps, subscriptions: parsedSubs });
  }).filter((source, index, all) => { if (all.findIndex((candidate) => candidate.id === source.id) !== index) fail("duplicate data source id"); return true; }).sort((a, b) => a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
}

export function validateParams(schema, params) {
  if (!plain(params)) return { ok: false };
  for (const key of Object.keys(params)) if (!Object.hasOwn(schema, key)) return { ok: false };
  const values = { ...params };
  for (const [name, rule] of Object.entries(schema)) {
    if (values[name] === undefined && rule.default !== undefined) values[name] = rule.default;
    if (values[name] === undefined) { if (rule.required) return { ok: false }; continue; }
    const value = values[name];
    if (rule.type === "string" && (typeof value !== "string" || [...value].length > (rule.max_length ?? 256))) return { ok: false };
    if (rule.type === "integer" && (!Number.isSafeInteger(value) || (rule.minimum !== undefined && value < rule.minimum) || (rule.maximum !== undefined && value > rule.maximum))) return { ok: false };
    if (rule.type === "boolean" && typeof value !== "boolean") return { ok: false };
    if (rule.enum && !rule.enum.includes(value)) return { ok: false };
  }
  return { ok: true, values };
}

function publicBindings(row) {
  return row ? JSON.parse(row.bindings) : { bindings: {} };
}

export function createArtifactData({ db, sources = parseDataSources(), fetchImpl = globalThis.fetch } = {}) {
  const sourceMap = new Map(sources.map((source) => [source.id, source]));
  const health = new Map();
  function recordHealth(id, state, { query = false, event = false, error, countFailure = true } = {}) {
    const prior = health.get(id) || {}, now = new Date().toISOString();
    const failed = state === "reconnecting" || state === "unavailable" && query && countFailure;
    health.set(id, { ...prior, state, ...(query ? { last_query_at: now } : {}), ...(event ? { last_event_at: now } : {}), ...(state === "connected" ? { last_success_at: now } : {}), retry_count: state === "connected" ? 0 : (prior.retry_count || 0) + (failed ? 1 : 0), error: state === "unavailable" || state === "reconnecting" ? error === "too_large" ? "too_large" : "data_unavailable" : null });
  }
  const listeners = new Map();
  const upstreams = new Map();
  const bindingStmt = db ? {
    get: db.prepare("SELECT artifact_id, org, bindings FROM artifact_data_bindings WHERE artifact_id = ?"),
    put: db.prepare("INSERT INTO artifact_data_bindings (artifact_id, org, bindings, updated_at) VALUES (?, ?, ?, datetime('now')) ON CONFLICT(artifact_id) DO UPDATE SET org=excluded.org, bindings=excluded.bindings, updated_at=excluded.updated_at"),
    del: db.prepare("DELETE FROM artifact_data_bindings WHERE artifact_id = ?")
  } : null;
  const snapshotStmt = db ? {
    get: db.prepare("SELECT revision, value FROM artifact_data_snapshots WHERE artifact_id=? AND binding=? AND key=?"),
    put: db.prepare("INSERT INTO artifact_data_snapshots (artifact_id,binding,key,value,revision,updated_at) VALUES (?,?,?,?,1,datetime('now')) ON CONFLICT(artifact_id,binding,key) DO UPDATE SET value=excluded.value, revision=artifact_data_snapshots.revision+1, updated_at=excluded.updated_at")
  } : null;
  const eventStmt = db ? {
    find: db.prepare("SELECT 1 FROM artifact_data_events WHERE artifact_id=? AND binding=? AND subscription=? AND event_id=?"),
    add: db.prepare("INSERT OR IGNORE INTO artifact_data_events (artifact_id,binding,subscription,event_id,event_name,data,created_at) VALUES (?,?,?,?,?,?,datetime('now'))"),
    list: db.prepare("SELECT event_id,event_name,data FROM artifact_data_events WHERE artifact_id=? AND binding=? AND subscription=? ORDER BY rowid DESC LIMIT 1000")
  } : null;
  function sourceFor(id, org) { const source = sourceMap.get(id); return source && (!source.org || source.org === org) ? source : null; }
  function bindingFor(artifactId, org, binding, topic) {
    const row = bindingStmt?.get.get(artifactId); if (!row || row.org !== org) return null;
    const manifest = JSON.parse(row.bindings); const item = manifest.bindings?.[binding]; if (!item) return null;
    const source = sourceFor(item.source, row.org); if (!source) return null;
    if (topic && !item.subscriptions?.includes(topic)) return null;
    return { row, item, source };
  }
  function setBindings(artifactId, org, bindings, auditInTransaction = null) {
    // A direct binding named `bindings` is valid. Treat the outer object as a
    // wrapper only when its `bindings` member is itself a binding manifest and
    // does not look like a binding definition (which always has `source`).
    const manifest = plain(bindings) && plain(bindings.bindings) && !Object.hasOwn(bindings.bindings, "source") ? bindings.bindings : bindings;
    if (!plain(manifest) || Object.keys(manifest).length > 8) fail("bad_params");
    const clean = {}, stopped = [];
    db.transaction(() => {
    for (const [name, item] of Object.entries(manifest).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) {
      validateName(name, "binding name"); if (!plain(item) || Object.keys(item).some((key) => !["source", "operations", "subscriptions"].includes(key)) || typeof item.source !== "string") fail("bad_params");
      const source = sourceFor(item.source, org); if (!source) fail("bad_params");
      for (const field of ["operations", "subscriptions"]) {
        if (item[field] !== undefined && (!Array.isArray(item[field]) || item[field].length > MAX_ITEMS || new Set(item[field]).size !== item[field].length)) fail("bad_params");
        for (const entry of item[field] || []) { validateName(entry, "binding entry"); if (field === "operations" && !Object.hasOwn(source.operations, entry) || field === "subscriptions" && !Object.hasOwn(source.subscriptions, entry)) fail("bad_params"); }
      }
      clean[name] = { operations: item.operations || [], source: item.source, subscriptions: item.subscriptions || [] };
    }
    const prior = bindingStmt.get.get(artifactId);
    const priorOrgChanged = prior && prior.org !== org;
      bindingStmt.put.run(artifactId, org, JSON.stringify({ bindings: clean }));
      if (prior && !priorOrgChanged) {
        const old = JSON.parse(prior.bindings).bindings || {};
        for (const name of Object.keys(old)) if (!clean[name] || JSON.stringify(old[name]) !== JSON.stringify(clean[name])) {
          db.prepare("DELETE FROM artifact_data_snapshots WHERE artifact_id=? AND binding=?").run(artifactId, name);
          db.prepare("DELETE FROM artifact_data_events WHERE artifact_id=? AND binding=?").run(artifactId, name);
          for (const topic of old[name].subscriptions || []) stopped.push([name, topic]);
        }
      } else if (priorOrgChanged) {
        const old = JSON.parse(prior.bindings).bindings || {};
        for (const [name, item] of Object.entries(old)) for (const topic of item.subscriptions || []) stopped.push([name, topic]);
        db.prepare("DELETE FROM artifact_data_snapshots WHERE artifact_id=?").run(artifactId);
        db.prepare("DELETE FROM artifact_data_events WHERE artifact_id=?").run(artifactId);
      }
      auditInTransaction?.();
    }).immediate();
    for (const [name, topic] of stopped) {
      stopUpstream(artifactId, name, topic);
      emit(artifactId, name, topic, { event: "data:resync", id: "", data: { reason: "bindings_changed" } });
      const found = bindingFor(artifactId, org, name, topic);
      if (!found) emit(artifactId, name, topic, { event: "data:status", id: "", data: { state: "unavailable" } });
      else if (found.source.enabled === false || found.source.configError) emit(artifactId, name, topic, { event: "data:status", id: "", data: { state: found.source.enabled === false ? "disabled" : "config_error" } });
      else if (found.source.kind === "push") emit(artifactId, name, topic, { event: "data:status", id: "", data: { state: "connected" } });
      else if (listeners.get(upstreamKey(artifactId, name, topic))?.size) startUpstream(artifactId, org, name, topic);
    }
    return { bindings: clean };
  }
  async function query(artifactId, org, binding, operation, params = {}, signal, pollSubscription) {
    const result = await queryValue(artifactId, org, binding, operation, params, signal, pollSubscription);
    const found = bindingFor(artifactId, org, binding, pollSubscription);
    if (found && found.source.enabled !== false && !found.source.configError && result.error !== "bad_params") recordHealth(found.source.id, result.error ? "unavailable" : "connected", { query: true, error: result.error, countFailure: !pollSubscription });
    return result;
  }
  async function queryValue(artifactId, org, binding, operation, params = {}, signal, pollSubscription) {
    const found = bindingFor(artifactId, org, binding, pollSubscription);
    const poll = typeof pollSubscription === "string" ? found?.source.subscriptions[pollSubscription] : null;
    if (!found || !found.item.operations.includes(operation) && !(poll?.transport === "poll" && poll.operation === operation)) return { error: "not_found" };
    if (found.source.enabled === false || found.source.configError) return { error: "data_unavailable" };
    const op = found.source.operations[operation]; const checked = validateParams(op.params, params); if (!checked.ok) return { error: "bad_params" };
    if (found.source.kind === "push") {
      const op = found.source.operations[operation]; const snapshot = snapshotStmt.get.get(artifactId, binding, op.key || operation); return snapshot ? { data: JSON.parse(snapshot.value) } : { error: "not_found" };
    }
    let path = op.path;
    const queryParts = [];
    for (const [name, value] of Object.entries(checked.values)) {
      const encoded = encodeURIComponent(String(value));
      if (path.includes(`{${name}}`)) { if (value === "." || value === ".." || /[\\/\x00-\x1f]/.test(String(value))) return { error: "bad_params" }; path = path.replaceAll(`{${name}}`, encoded); }
      else queryParts.push(`${encodeURIComponent(name)}=${encoded}`);
    }
    const url = `${found.source.base_url}${path}${queryParts.length ? `?${queryParts.join("&")}` : ""}`;
    const controller = new AbortController(); const timer = setTimeout(() => controller.abort(), op.timeout_ms);
    const abort = () => controller.abort(); if (signal?.aborted) abort(); else signal?.addEventListener("abort", abort, { once: true });
    try {
      const response = await fetchImpl(url, { method: "GET", headers: found.source.headers, redirect: "error", signal: controller.signal });
      if (response.status === 404) return { error: "not_found" }; if (!response.ok) return { error: "data_unavailable" };
      let text; try { text = await readBoundedBody(response, op.max_bytes); } catch (error) { if (error.message === "too_large") return { error: "too_large" }; return { error: "data_unavailable" }; }
      try { return { data: JSON.parse(text) }; } catch { return { error: "data_unavailable" }; }
    } catch { return { error: "data_unavailable" }; } finally { clearTimeout(timer); signal?.removeEventListener("abort", abort); }
  }
  async function testSource(source, op, params = {}) {
    if (!source || source.kind !== "http") return { ok: false, error: "test_unavailable" };
    const checked = validateParams(op.params, params); if (!checked.ok) return { ok: false, error: "bad_params" };
    let path = op.path; const queryParts = [];
    for (const [name, value] of Object.entries(checked.values)) {
      const encoded = encodeURIComponent(String(value));
      if (path.includes(`{${name}}`)) { if (value === "." || value === ".." || /[\\/\x00-\x1f]/.test(String(value))) return { ok: false, error: "bad_params" }; path = path.replaceAll(`{${name}}`, encoded); }
      else queryParts.push(`${encodeURIComponent(name)}=${encoded}`);
    }
    const url = `${source.base_url}${path}${queryParts.length ? `?${queryParts.join("&")}` : ""}`;
    const controller = new AbortController(); const timer = setTimeout(() => controller.abort(), op.timeout_ms);
    try {
      const response = await fetchImpl(url, { method: "GET", headers: source.headers, redirect: "error", signal: controller.signal });
      if (!response.ok) return { ok: false, error: response.status === 404 ? "not_found" : "data_unavailable" };
      const text = await readBoundedBody(response, op.max_bytes);
      let value; try { value = JSON.parse(text); } catch { return { ok: false, error: "data_unavailable" }; }
      const responseType = Array.isArray(value) ? "array" : value !== null && typeof value === "object" ? "object" : "scalar";
      return { ok: true, summary: { kind: "http", response_type: responseType, bytes: Buffer.byteLength(JSON.stringify(value)) } };
    } catch (error) { return { ok: false, error: error?.message === "too_large" ? "too_large" : "data_unavailable" }; }
    finally { clearTimeout(timer); }
  }
  function refreshSources(nextSources) {
    const next = new Map(nextSources.map((source) => [source.id, source]));
    const changed = new Set();
    const stable = (value) => Array.isArray(value) ? value.map(stable) : value && typeof value === "object" ? Object.fromEntries(Object.keys(value).sort().map((key) => [key, stable(value[key])])) : value;
    const fingerprint = (source) => JSON.stringify(stable(source ? { id: source.id, org: source.org, kind: source.kind, enabled: source.enabled !== false, version: source.version || 1, headers_env: source.headers_env, operations: source.operations, subscriptions: source.subscriptions, base_url: source.base_url, missing: source.missing || [] } : null));
    for (const id of new Set([...sourceMap.keys(), ...next.keys()])) if (fingerprint(next.get(id)) !== fingerprint(sourceMap.get(id))) {
      changed.add(id);
      for (const key of [...upstreams.keys()]) {
        const [artifactId, binding, ...topicParts] = key.split(":");
        const row = bindingStmt?.get.get(artifactId); if (!row) continue;
        let manifest; try { manifest = JSON.parse(row.bindings)?.bindings || {}; } catch { continue; }
        if (manifest[binding]?.source === id) stopUpstream(artifactId, binding, topicParts.join(":"));
      }
    }
    sourceMap.clear(); for (const [id, source] of next) sourceMap.set(id, source);
    for (const [key, group] of listeners) {
      if (!group.size) continue;
      const [artifactId, binding, ...topicParts] = key.split(":");
      const row = bindingStmt?.get.get(artifactId); if (!row) continue;
      const topic = topicParts.join(":"); const item = (() => { try { return JSON.parse(row.bindings)?.bindings?.[binding]; } catch { return null; } })();
      if (!item?.source || !changed.has(item.source)) continue;
      if (group.size) for (const listener of group) listener({ binding, subscription: topic, event: "data:resync", id: "", data: { reason: "source_changed" } });
      const source = sourceMap.get(item.source);
      const state = !source ? "unavailable" : source.enabled === false ? "disabled" : source.configError ? "config_error" : source.kind === "push" ? "connected" : "connecting";
      emit(artifactId, binding, topic, { event: "data:status", id: "", data: { state } });
      if (source?.kind === "http") {
        db.prepare("DELETE FROM artifact_data_events WHERE artifact_id=? AND binding=? AND subscription=?").run(artifactId, binding, topic);
        if (source.enabled !== false && !source.configError) startUpstream(artifactId, row.org, binding, topic);
      }
    }
  }
  function setData(artifactId, org, binding, key, value) {
    const found = bindingFor(artifactId, org, binding); if (!found || found.source.kind !== "push") fail("not_found");
    if (found.source.enabled === false || found.source.configError) fail("data_unavailable");
    const operation = found.item.operations.map((name) => [name, found.source.operations[name]]).find(([name, op]) => (op.key || name) === key);
    if (!operation) fail("not_found");
    if (jsonBytes(value) > MAX_SNAPSHOT_BYTES) fail("too_large");
    const snapshotKey = operation[1].key || operation[0];
    const revision = db.transaction(() => { snapshotStmt.put.run(artifactId, binding, snapshotKey, JSON.stringify(value)); return snapshotStmt.get.get(artifactId, binding, snapshotKey).revision; })();
    return { revision };
  }
  function appendEvents(artifactId, org, binding, subscription, events) {
    const found = bindingFor(artifactId, org, binding, subscription); if (!found || found.source.kind !== "push") fail("not_found");
    if (found.source.enabled === false || found.source.configError) fail("data_unavailable");
    if (!Array.isArray(events) || events.length > 100) fail("bad_params");
    let accepted = 0; let duplicates = 0;
    if (jsonBytes(events) > MAX_EVENT_BYTES) fail("too_large");
    for (const item of events) {
      if (!plain(item) || Object.keys(item).some((key) => !["id", "event", "data"].includes(key)) || !Object.hasOwn(item, "data") || typeof item.id !== "string" || typeof item.event !== "string" || !item.id.length || !item.event.length || CONTROL_RE.test(item.id) || CONTROL_RE.test(item.event) || Buffer.byteLength(item.id) > 128 || Buffer.byteLength(item.event) > 128) fail("bad_params");
    }
    const acceptedEvents = [];
    db.transaction(() => { for (const item of events) { const existing = eventStmt.find.get(artifactId,binding,subscription,item.id); if (existing) { duplicates++; continue; } eventStmt.add.run(artifactId,binding,subscription,item.id,item.event,JSON.stringify(item.data)); accepted++; acceptedEvents.push(item); } if (accepted) db.prepare("DELETE FROM artifact_data_events WHERE artifact_id=? AND binding=? AND subscription=? AND rowid NOT IN (SELECT rowid FROM artifact_data_events WHERE artifact_id=? AND binding=? AND subscription=? ORDER BY rowid DESC LIMIT 1000)").run(artifactId,binding,subscription,artifactId,binding,subscription); })();
    for (const item of acceptedEvents) emit(artifactId,binding,subscription,{ event:item.event,id:item.id,data:item.data });
    return { accepted, duplicates };
  }
  function emit(artifactId, binding, subscription, envelope) {
    const row = bindingStmt?.get.get(artifactId), found = row && bindingFor(artifactId, row.org, binding, subscription);
    if (found) { if (envelope.event === "data:status") recordHealth(found.source.id, envelope.data.state); else if (envelope.event !== "data:resync") recordHealth(found.source.id, "connected", { event: true }); }
    for (const fn of listeners.get(`${artifactId}:${binding}:${subscription}`) || []) fn({ binding, subscription, ...envelope }); }
  function upstreamKey(artifactId, binding, subscription) { return `${artifactId}:${binding}:${subscription}`; }
  function startUpstream(artifactId, org, binding, subscription) {
    const key = upstreamKey(artifactId, binding, subscription);
    if (upstreams.has(key)) return;
    const found = bindingFor(artifactId, org, binding, subscription);
    if (!found || found.source.kind !== "http" || found.source.enabled === false || found.source.configError) return;
    const state = { controller: null, closed: false, lastId: "", retry: 1000, timer: null };
    upstreams.set(key, state);
    const status = (value) => emit(artifactId, binding, subscription, { event: "data:status", id: "", data: { state: value } });
    const reconnect = () => {
      if (state.closed) return;
      status("reconnecting"); state.timer = setTimeout(run, state.retry);
      state.retry = Math.min(state.retry * 2, 30000);
    };
    const run = async () => {
      if (state.closed) return;
      const current = bindingFor(artifactId, org, binding, subscription);
      if (!current || current.source.id !== found.source.id) { stopUpstream(artifactId, binding, subscription); return; }
      const sub = found.source.subscriptions[subscription];
      if (sub.transport === "poll") {
        state.controller = new AbortController();
        const result = await query(artifactId, org, binding, sub.operation, {}, state.controller.signal, subscription);
        if (state.closed) return;
        if (result.data !== undefined) {
          state.retry = 1000; status("connected");
          emit(artifactId, binding, subscription, { event: "message", id: "", data: result.data });
          state.timer = setTimeout(run, sub.interval_ms);
        } else { status("unavailable"); reconnect(); }
        return;
      }
      const controller = new AbortController(); state.controller = controller;
      let reader, idleTimer, headerTimer;
      try {
        const headers = { ...found.source.headers, accept: "text/event-stream" };
        if (state.lastId) headers["Last-Event-ID"] = state.lastId;
        headerTimer = state.headerTimer = setTimeout(() => controller.abort(), 10000);
        const response = await fetchImpl(`${found.source.base_url}${sub.path}`, { method: "GET", headers, redirect: "error", signal: controller.signal });
        clearTimeout(headerTimer);
        if (!response.ok || !response.body?.getReader || !(response.headers.get("content-type") || "").startsWith("text/event-stream")) throw new Error("data_unavailable");
        reader = state.reader = response.body.getReader();
        state.retry = 1000; status("connected");
        const decoder = new TextDecoder("utf-8", { fatal: true });
        const selected = new Set(sub.events || ["message"]);
        let buffer = "", frameBytes = 0, frame = { event: "message", id: "", data: [] };
        const flush = () => {
          if (frame.data.length && selected.has(frame.event)) {
            const text = frame.data.join("\n"); let value;
            try { value = JSON.parse(text); } catch { value = text; }
            if (frame.id) {
              state.lastId = frame.id;
              eventStmt.add.run(artifactId, binding, subscription, frame.id, frame.event, JSON.stringify(value));
              db.prepare("DELETE FROM artifact_data_events WHERE artifact_id=? AND binding=? AND subscription=? AND rowid NOT IN (SELECT rowid FROM artifact_data_events WHERE artifact_id=? AND binding=? AND subscription=? ORDER BY rowid DESC LIMIT 1000)").run(artifactId, binding, subscription, artifactId, binding, subscription);
            }
            emit(artifactId, binding, subscription, { event: frame.event, id: frame.id, data: value });
          }
          frame = { event: "message", id: "", data: [] }; frameBytes = 0;
        };
        while (!state.closed) {
          idleTimer = state.idleTimer = setTimeout(() => controller.abort(), 45000);
          const chunk = await reader.read(); clearTimeout(idleTimer);
          if (chunk.done) break;
          buffer += decoder.decode(chunk.value, { stream: true });
          if (Buffer.byteLength(buffer) > MAX_EVENT_BYTES) throw new Error("too_large");
          let split;
          while ((split = buffer.indexOf("\n")) >= 0) {
            const line = buffer.slice(0, split).replace(/\r$/, ""); buffer = buffer.slice(split + 1);
            frameBytes += Buffer.byteLength(line) + 1;
            if (frameBytes > MAX_EVENT_BYTES) throw new Error("too_large");
            if (!line) { flush(); continue; }
            if (line.startsWith(":")) continue;
            const colon = line.indexOf(":"), field = colon < 0 ? line : line.slice(0, colon);
            const value = (colon < 0 ? "" : line.slice(colon + 1)).replace(/^ /, "");
            if (field === "event" || field === "id") {
              if (Buffer.byteLength(value) > 128 || CONTROL_RE.test(value)) throw new Error("data_unavailable");
              frame[field] = field === "event" ? value || "message" : value;
            } else if (field === "data") frame.data.push(value);
          }
        }
        reconnect();
      } catch { if (!state.closed) { status("unavailable"); reconnect(); } }
      finally { clearTimeout(headerTimer); clearTimeout(idleTimer); await reader?.cancel().catch(() => {}); }
    };
    state.run = run; void run();
  }
  function stopUpstream(artifactId, binding, subscription) { const key = upstreamKey(artifactId, binding, subscription); const state = upstreams.get(key); if (!state) return; state.closed = true; if (state.timer) clearTimeout(state.timer); clearTimeout(state.headerTimer); clearTimeout(state.idleTimer); state.controller?.abort(); state.reader?.cancel().catch(() => {}); upstreams.delete(key); }
  function subscribe(artifactId, org, requested, onEvent, { start = true } = {}) {
    const clean = []; for (const item of requested) { const found = bindingFor(artifactId, org, item.binding, item.subscription); if (!found) continue; clean.push(item); if (!start) continue; const key = upstreamKey(artifactId, item.binding, item.subscription); if (!listeners.has(key)) listeners.set(key, new Set()); const first = listeners.get(key).size === 0; listeners.get(key).add(onEvent); if (first) { if (found.source.enabled === false || found.source.configError) emit(artifactId, item.binding, item.subscription, { event: "data:status", id: "", data: { state: found.source.enabled === false ? "disabled" : "config_error" } }); else if (found.source.kind === "push") emit(artifactId, item.binding, item.subscription, { event: "data:status", id: "", data: { state: "connected" } }); else startUpstream(artifactId, org, item.binding, item.subscription); } }
    return { subscriptions: clean, close() { for (const item of clean) { const key = upstreamKey(artifactId, item.binding, item.subscription); const group = listeners.get(key); group?.delete(onEvent); if (group && group.size === 0) { listeners.delete(key); stopUpstream(artifactId, item.binding, item.subscription); } } } };
  }
  function replay(artifactId, org, requested, cursor = {}) {
    const events = []; const gaps = [];
    for (const item of requested) {
      const found = bindingFor(artifactId, org, item.binding, item.subscription); if (!found || found.source.enabled === false || found.source.configError) continue;
      const rows = eventStmt.list.all(artifactId, item.binding, item.subscription).reverse();
      const last = cursor[`${item.binding}:${item.subscription}`]; const index = last ? rows.findIndex((row) => row.event_id === last) : -1;
      if (last && index < 0) gaps.push(item);
      for (const row of (index < 0 ? rows : rows.slice(index + 1))) events.push({ binding: item.binding, subscription: item.subscription, event: row.event_name, id: row.event_id, data: JSON.parse(row.data) });
    }
    return { events, gaps };
  }
  return {
    getSourceHealth: (id) => health.get(id),
    recordSourceHealth: (id, value) => health.set(id, value),
    sources: [...sourceMap.values()].map(publicSource),
    listSources: (org, admin = false) => [...sourceMap.values()].filter((source) => source.enabled !== false && !source.configError && (admin && org === "admin" || source.org === org)).map(publicSource),
    getSource: (id, org) => publicSource(sourceFor(id, org)),
    getBindings: (id, org) => { const row = bindingStmt.get.get(id); return publicBindings(row && (org === undefined || row.org === org) ? row : null); }, setBindings, query, testSource, refreshSources, setData, appendEvents, subscribe, replay, publicSource
  };
}

export const ARTIFACT_DATA_LIMITS = { MAX_EVENT_BYTES, MAX_SNAPSHOT_BYTES, MAX_RETAINED_EVENTS };
