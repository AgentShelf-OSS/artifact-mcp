// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
(function () {
  "use strict";
  const panel = document.getElementById("connections-workspace");
  if (!panel) return;
  const $ = id => document.getElementById(id);
  const api = "/settings/data-sources";
  let records = [], orgs = [], selected = null, loaded = false, loading = false, sequence = 0;
  let bindingArtifact = null, canDelete = false, selectionRequest = 0, stale = false;
  const node = (tag, text, className) => {
    const el = document.createElement(tag);
    if (text !== undefined) el.textContent = text;
    if (className) el.className = className;
    return el;
  };
  function notify(text, bad = false) {
    $(bad ? "conn-error" : "conn-status").textContent = text;
    $("conn-error").hidden = !bad;
    if (bad) $("conn-status").textContent = "";
  }
  async function request(url, method = "GET", body) {
    const options = { method, headers: { accept: "application/json" }, cache: "no-store" };
    if (method !== "GET") options.headers["x-artifact-mutation"] = "1";
    if (body !== undefined) {
      options.headers["content-type"] = "application/json";
      options.body = JSON.stringify(body);
    }
    const response = await fetch(url, options);
    const result = await response.json();
    if (!response.ok) {
      const error = new Error(result.message || result.error || "This action could not be completed.");
      error.code = result.error;
      throw error;
    }
    return result;
  }
  async function action(button, task) {
    if (button) { button.disabled = true; button.dataset.busy = "true"; }
    $("conn-error").hidden = true;
    try { await task(); } catch (error) { notify(error.message || "Connection unavailable. Try again.", true); }
    finally { if (button) { delete button.dataset.busy; button.disabled = button.id === "conn-delete" ? !canDelete : ["conn-save", "conn-toggle"].includes(button.id) && stale; } }
  }
  function field(parent, label, key, value = "", type = "text", placeholder = "") {
    const wrap = node("div", undefined, "field"), caption = node("label", label);
    const input = node(type === "textarea" ? "textarea" : "input");
    input.id = "conn-field-" + ++sequence;
    caption.htmlFor = input.id;
    input.dataset.field = key;
    if (type !== "textarea") input.type = type;
    input.value = value ?? "";
    input.placeholder = placeholder;
    input.autocomplete = "off";
    wrap.append(caption, input); parent.append(wrap);
    return input;
  }
  function choice(parent, label, key, value, options) {
    const wrap = node("div", undefined, "field"), caption = node("label", label), select = node("select");
    select.id = "conn-field-" + ++sequence; select.dataset.field = key; caption.htmlFor = select.id;
    for (const option of options) select.add(new Option(option.label || option, option.value || option));
    select.value = value; wrap.append(caption, select); parent.append(wrap); return select;
  }
  const value = (parent, key) => parent.querySelector('[data-field="' + key + '"]')?.value.trim() || "";
  function removeButton(parent, target, label = "Remove") {
    const button = node("button", label, "secondary-button conn-remove"); button.type = "button";
    button.addEventListener("click", () => target.remove()); parent.append(button); return button;
  }
  function card(parent, title, className, open = false) {
    const details = node("details", undefined, "conn-rule " + className), summary = node("summary", title);
    const body = node("div", undefined, "conn-rule-body"); details.append(summary, body); details.open = open || !selected;
    parent.append(details); return { details, summary, body };
  }
  function mapHeader(name = "", env = "") {
    const row = node("div", undefined, "conn-map-row");
    field(row, "Header name", "header", name, "text", "Authorization").required = true;
    field(row, "Environment variable", "env", env, "text", "METRICS_AUTHORIZATION").required = true;
    removeButton(row, row); $("conn-headers").append(row);
  }
  function parameter(parent, name = "", spec = {}) {
    const row = node("div", undefined, "conn-param"), grid = node("div", undefined, "field-grid");
    field(grid, "Parameter name", "name", name).required = true;
    choice(grid, "Value type", "type", spec.type || "string", ["string", "integer", "boolean"]);
    choice(grid, "Required", "required", spec.required ? "yes" : "no", ["no", "yes"]);
    field(grid, "Minimum (integer)", "minimum", spec.minimum, "number");
    field(grid, "Maximum (integer)", "maximum", spec.maximum, "number");
    field(grid, "Maximum length (string)", "max_length", spec.max_length, "number");
    field(grid, "Allowed values (JSON array)", "enum", spec.enum == null ? "" : JSON.stringify(spec.enum), "text", '["open", "closed"]');
    field(grid, "Default value (JSON)", "default", spec.default == null ? "" : JSON.stringify(spec.default), "text", "Leave blank for no default");
    row.append(grid); removeButton(row, row, "Remove parameter"); parent.append(row);
  }
  function operation(name = "", spec = {}) {
    const { details, summary, body } = card($("conn-operations"), name || "New operation", "conn-operation", !name);
    const grid = node("div", undefined, "field-grid");
    const nameInput = field(grid, "Operation name", "name", name); nameInput.required = true;
    nameInput.addEventListener("input", () => { summary.textContent = nameInput.value || "New operation"; });
    const path = field(grid, "GET path", "path", spec.path, "text", "/api/status"); path.closest(".field").classList.add("conn-http-only");
    field(grid, "Snapshot key", "key", spec.key, "text", "Defaults to the operation name").closest(".field").classList.add("conn-push-only");
    field(grid, "Timeout (ms)", "timeout_ms", spec.timeout_ms ?? 10000, "number");
    field(grid, "Response size limit (bytes)", "max_bytes", spec.max_bytes ?? 1048576, "number");
    body.append(grid);
    const params = node("div", undefined, "conn-params conn-http-only");
    for (const [paramName, config] of Object.entries(spec.params || {})) parameter(params, paramName, config);
    body.append(params);
    const actions = node("div", undefined, "conn-rule-actions"), add = node("button", "Add parameter", "secondary-button conn-http-only");
    add.type = "button"; add.addEventListener("click", () => parameter(params)); actions.append(add);
    removeButton(actions, details, "Remove operation"); body.append(actions); kindFields();
  }
  function subscription(name = "", spec = {}) {
    const { details, summary, body } = card($("conn-subscriptions"), name || "New subscription", "conn-subscription", !name);
    const grid = node("div", undefined, "field-grid");
    const input = field(grid, "Subscription name", "name", name); input.required = true;
    input.addEventListener("input", () => { summary.textContent = input.value || "New subscription"; });
    const transport = choice(grid, "Transport", "transport", spec.transport || ($("conn-kind").value === "push" ? "push" : "sse"), ["sse", "poll", "push"]);
    const path = field(grid, "Event stream path", "path", spec.path, "text", "/api/events");
    const events = field(grid, "Event names (one per line)", "events", (spec.events || []).join("\n"), "textarea", "dashboard-event");
    const op = field(grid, "Polling operation", "operation", spec.operation, "text", "status");
    const interval = field(grid, "Poll interval (ms)", "interval_ms", spec.interval_ms ?? 2000, "number");
    function visibility() {
      path.closest(".field").hidden = events.closest(".field").hidden = transport.value !== "sse";
      op.closest(".field").hidden = interval.closest(".field").hidden = transport.value !== "poll";
    }
    transport.addEventListener("change", visibility); visibility(); body.append(grid);
    const actions = node("div", undefined, "conn-rule-actions"); removeButton(actions, details, "Remove subscription"); body.append(actions);
  }
  function kindFields() {
    const http = $("conn-kind").value === "http";
    $("conn-url-field").hidden = $("conn-credentials").hidden = !http;
    $("conn-url").required = http;
    panel.querySelectorAll(".conn-http-only").forEach(el => { el.hidden = !http; el.querySelectorAll("input,select,textarea,button").forEach(control => { control.disabled = !http; }); });
    panel.querySelectorAll(".conn-push-only").forEach(el => { el.hidden = http; });
  }
  function unique(target, name, spec) {
    if (!name) throw new Error("Every operation, subscription and parameter needs a name.");
    if (Object.hasOwn(target, name)) throw new Error("Duplicate name: " + name);
    target[name] = spec;
  }
  function number(parent, key, spec) {
    const raw = value(parent, key);
    if (raw) { const parsed = Number(raw); if (!Number.isSafeInteger(parsed)) throw new Error("Use a whole number for " + key + "."); spec[key] = parsed; }
  }
  function jsonField(parent, key, spec) {
    const raw = value(parent, key);
    if (raw) { try { spec[key] = JSON.parse(raw); } catch { throw new Error("Use valid JSON for " + key + "."); } }
  }
  function definition() {
    const http = $("conn-kind").value === "http";
    const source = { id: $("conn-id").value.trim(), org: $("conn-org").value, kind: $("conn-kind").value, operations: Object.create(null), subscriptions: Object.create(null) };
    if (http) {
      source.base_url = $("conn-url").value.trim(); source.headers_env = Object.create(null);
      $("conn-headers").querySelectorAll(".conn-map-row").forEach(row => unique(source.headers_env, value(row, "header"), value(row, "env")));
    }
    $("conn-operations").querySelectorAll(".conn-operation").forEach(row => {
      const spec = {}; number(row, "timeout_ms", spec); number(row, "max_bytes", spec);
      if (http) {
        spec.path = value(row, "path"); spec.params = Object.create(null);
        row.querySelectorAll(".conn-param").forEach(param => {
          const rule = { type: value(param, "type"), required: value(param, "required") === "yes" };
          if (rule.type === "integer") { number(param, "minimum", rule); number(param, "maximum", rule); }
          if (rule.type === "string") number(param, "max_length", rule);
          jsonField(param, "enum", rule); jsonField(param, "default", rule);
          unique(spec.params, value(param, "name"), rule);
        });
      } else if (value(row, "key")) spec.key = value(row, "key");
      unique(source.operations, value(row, "name"), spec);
    });
    $("conn-subscriptions").querySelectorAll(".conn-subscription").forEach(row => {
      const spec = { transport: value(row, "transport") };
      if (spec.transport === "sse") { spec.path = value(row, "path"); spec.events = value(row, "events").split("\n").map(v => v.trim()).filter(Boolean); }
      if (spec.transport === "poll") { spec.operation = value(row, "operation"); number(row, "interval_ms", spec); }
      unique(source.subscriptions, value(row, "name"), spec);
    });
    return source;
  }
  function options(select, names, initial) {
    select.replaceChildren(); if (initial) select.add(new Option(initial, ""));
    names.forEach(name => select.add(new Option(name, name)));
  }
  function age(raw) {
    if (!raw) return "Not observed";
    const date = Date.parse(/Z$|[+-]\d\d:\d\d$/.test(raw) ? raw : raw.replace(" ", "T") + "Z");
    if (!Number.isFinite(date)) return "Not observed";
    const seconds = Math.max(0, Math.floor((Date.now() - date) / 1000));
    return seconds < 60 ? seconds + "s ago" : seconds < 3600 ? Math.floor(seconds / 60) + "m ago" : Math.floor(seconds / 3600) + "h ago";
  }
  function health(row) {
    const container = $("conn-health"); container.replaceChildren(); container.hidden = false;
    const info = row.health || {};
    for (const [label, text] of [["State", info.state || "idle"], ["Last success", age(info.last_success_at)], ["Last query", age(info.last_query_at)], ["Last event", age(info.last_event_at)], ["Retries / error", (info.retry_count || 0) + " / " + (info.error || "none")]]) {
      const cell = node("div"); cell.append(node("small", label), node("strong", text)); container.append(cell);
    }
  }
  function list() {
    const container = $("conn-list"); container.replaceChildren();
    const search = $("conn-search").value.toLowerCase(), org = $("conn-org-filter").value;
    const rows = records.filter(row => (!org || row.org === org) && (!search || (row.id + " " + row.org + " " + row.definition.kind).toLowerCase().includes(search)));
    for (const row of rows) {
      const button = node("button", undefined, "org-list-button"); button.type = "button"; button.dataset.connectionId = row.id;
      button.setAttribute("aria-current", String(selected?.id === row.id));
      button.append(node("strong", row.id), node("small", row.org + " · " + row.origin));
      const meta = node("span", undefined, "conn-list-meta"), pill = node("span", row.health?.state || "idle", "conn-pill"); pill.dataset.state = row.health?.state || "idle";
      const transports = [...new Set(Object.values(row.definition.subscriptions || {}).map(sub => sub.transport))].join(" / ") || row.definition.kind;
      meta.append(pill, node("span", transports + " · " + row.artifact_count + " artifacts")); button.append(meta);
      button.addEventListener("click", () => action(button, () => open(row.id))); container.append(button);
    }
    if (!rows.length) container.append(node("p", records.length ? "No matching connections." : "No connections yet. Add an API or producer channel.", "directory-empty"));
  }
  async function load(choose = false) {
    if (loading) return; loading = true;
    try {
      const result = await request(api); records = result.connections; orgs = result.orgs; loaded = true;
      const org = $("conn-org-filter").value; options($("conn-org-filter"), orgs, "All organizations"); $("conn-org-filter").value = org;
      list(); const current = selected && records.find(row => row.id === selected.id);
      if (selected && (!current || current.version !== selected.version)) { stale = true; $("conn-stale").hidden = false; $("conn-save").disabled = $("conn-toggle").disabled = true; } if (current) { health(current); selected.artifact_count = current.artifact_count; $("conn-description").textContent = current.artifact_count + " connected artifacts"; $("conn-save-impact").textContent = current.artifact_count ? "Saving restarts this source for " + current.artifact_count + " artifacts. Other sources continue." : "Changes take effect without restarting the application."; }
      if (choose && !selected && records.length) await open(records[0].id);
    } finally { loading = false; }
  }
  function fill(row) {
    selected = row; stale = false; $("conn-stale").hidden = true; $("conn-save").disabled = $("conn-toggle").disabled = false; canDelete = false; $("conn-delete").disabled = true; const source = row?.definition || { kind: "http", operations: {}, subscriptions: {}, headers_env: {} };
    $("conn-empty").hidden = true; $("conn-detail").hidden = false; $("conn-origin").textContent = row ? row.origin + " connection · " + row.org : "Managed connection";
    $("conn-title").textContent = row?.id || "New connection"; $("conn-description").textContent = row ? row.artifact_count + " connected artifacts" : "Save a source, then grant its operations to an artifact.";
    $("conn-id").value = source.id || ""; $("conn-id").readOnly = !!row;
    options($("conn-org"), orgs); $("conn-org").value = source.org || $("conn-org-filter").value || orgs[0] || "";
    $("conn-kind").value = source.kind; $("conn-url").value = source.base_url || ""; $("conn-enabled").checked = row ? row.enabled : true;
    $("conn-config").disabled = row?.origin === "operator";
    $("conn-headers").replaceChildren(); Object.entries(source.headers_env || {}).forEach(([name, env]) => mapHeader(name, env));
    $("conn-operations").replaceChildren(); Object.entries(source.operations || {}).forEach(([name, spec]) => operation(name, spec));
    $("conn-subscriptions").replaceChildren(); Object.entries(source.subscriptions || {}).forEach(([name, spec]) => subscription(name, spec));
    $("conn-notice").hidden = true;
    const missing = row?.missing_references || [];
    if (row?.origin === "operator" || missing.length) {
      $("conn-notice").hidden = false;
      $("conn-notice").textContent = (row?.origin === "operator" ? "This connection comes from the operator file. Edit that file to change its configuration. " : "") + (missing.length ? "Provide these environment variables in deployment configuration: " + missing.join(", ") + "." : "");
    }
    $("conn-toggle").hidden = !row || row.origin === "operator"; $("conn-toggle").textContent = row?.enabled ? "Disable connection" : "Enable connection";
    $("conn-save").textContent = row ? "Save connection" : "Create connection";
    $("conn-save-impact").textContent = row?.artifact_count ? "Saving restarts this source for " + row.artifact_count + " artifacts. Other sources continue." : "Changes take effect without restarting the application.";
    $("conn-health").hidden = !row; if (row) health(row);
    $("conn-test-section").hidden = $("conn-impact-section").hidden = !row; $("conn-test-result").textContent = "";
    if (row) { options($("conn-test-operation"), Object.keys(source.operations || {})); testParameters(); }
    kindFields(); list();
  }
  async function open(id) {
    const ticket = ++selectionRequest;
    const row = await request(api + "/" + encodeURIComponent(id));
    if (ticket !== selectionRequest) return;
    fill(row); await impact();
  }
  async function impact() {
    if (!selected) return;
    const id = selected.id, result = await request(api + "/" + encodeURIComponent(id) + "/impact");
    if (selected?.id !== id) return;
    const container = $("conn-artifacts"); container.replaceChildren();
    for (const artifact of result.artifacts) {
      const item = node("div", undefined, "conn-artifact"), detail = node("div"), link = node("a", artifact.title || artifact.id);
      link.href = "/" + encodeURIComponent(artifact.id); link.target = "_blank"; link.rel = "noopener";
      detail.append(link, node("small", artifact.bindings.map(binding => binding.name + ": " + binding.operations.length + " operations, " + binding.subscriptions.length + " subscriptions").join("; ")));
      const edit = node("button", "Manage bindings", "secondary-button"); edit.type = "button";
      edit.addEventListener("click", () => action(edit, () => editBindings(artifact))); item.append(detail, edit); container.append(item);
    }
    if (!result.artifacts.length) container.append(node("p", "No artifacts use this connection."));
    canDelete = !!result.can_delete; $("conn-delete").hidden = selected.origin === "operator"; $("conn-delete").disabled = !canDelete || $("conn-delete").dataset.busy === "true";
    $("conn-delete-help").textContent = result.can_delete ? "This connection has no artifact bindings. Deletion removes its saved configuration." : "Remove or migrate these bindings before deleting this connection.";
    return canDelete;
  }
  function testParameters() {
    const container = $("conn-test-params"); container.replaceChildren(); if (!selected) return;
    $("conn-test-operation").closest(".field").hidden = selected.definition.kind === "push";
    const spec = selected.definition.operations[$("conn-test-operation").value];
    for (const [name, param] of Object.entries(spec?.params || {})) {
      const input = field(container, name, name, param.default == null ? "" : String(param.default), param.type === "integer" ? "number" : "text", param.type);
      input.required = !!param.required;
    }
  }
  function bindingCard(name = "", binding = {}, artifactOrg) {
    const { details, body } = card($("conn-bindings"), name || "New binding", "conn-binding"); details.open = true;
    const grid = node("div", undefined, "field-grid"); field(grid, "Binding name", "name", name).required = true;
    const sources = records.filter(row => row.org === artifactOrg);
    const sourceIds = sources.map(row => row.id); if (binding.source && !sourceIds.includes(binding.source)) sourceIds.push(binding.source);
    const select = choice(grid, "Connection", "source", binding.source || sources[0]?.id, sourceIds); body.append(grid);
    const grants = node("div"); body.append(grants);
    function draw(current = {}) {
      grants.replaceChildren(); const source = sources.find(row => row.id === select.value)?.definition;
      for (const key of ["operations", "subscriptions"]) {
        const group = node("fieldset", undefined, "conn-grants"); group.dataset.grant = key; group.append(node("legend", key));
        const capabilities = [...new Set([...Object.keys(source?.[key] || {}), ...(current[key] || [])])];
        for (const capability of capabilities) {
          const label = node("label"), check = node("input"); check.type = "checkbox"; check.value = capability; check.checked = (current[key] || []).includes(capability);
          label.append(check, document.createTextNode(capability + (source?.[key]?.[capability] ? "" : " (unavailable; reload before saving)"))); group.append(label);
        }
        grants.append(group);
      }
    }
    select.addEventListener("change", () => draw()); draw(binding); removeButton(body, details, "Remove binding");
  }
  async function editBindings(artifact) {
    await load();
    const result = await request("/settings/data-bindings/" + encodeURIComponent(artifact.id)); bindingArtifact = artifact;
    $("conn-binding-title").textContent = artifact.title || artifact.id; $("conn-binding-error").textContent = ""; $("conn-bindings").replaceChildren();
    Object.entries(result.bindings).forEach(([name, binding]) => bindingCard(name, binding, artifact.org)); $("conn-binding-dialog").showModal();
  }
  $("conn-form").addEventListener("submit", event => {
    event.preventDefault(); action($("conn-save"), async () => {
      const body = { definition: definition(), enabled: $("conn-enabled").checked };
      if (selected) body.expected_version = selected.version;
      const saved = await request(selected ? api + "/" + encodeURIComponent(selected.id) : api, selected ? "PATCH" : "POST", body);
      await load(); await open(saved.id); notify("Connection saved.");
    });
  });
  $("conn-toggle").addEventListener("click", () => action($("conn-toggle"), async () => {
    if (!selected) return;
    if (selected.enabled && selected.artifact_count && !await window.ArtifactDialogs.confirm({title:"Disable connection",subject:selected.id,message:selected.artifact_count + " artifacts use this connection. Their other sources will continue.",action:"Disable connection",danger:true})) return;
    const saved = await request(api + "/" + encodeURIComponent(selected.id) + (selected.enabled ? "/disable" : "/enable"), "POST", { expected_version: selected.version });
    await load(); await open(saved.id); notify(saved.enabled ? "Connection enabled." : "Connection disabled. Existing bindings are retained.");
  }));
  $("conn-test-form").addEventListener("submit", event => {
    event.preventDefault(); action($("conn-test"), async () => {
      if (!selected) return; const body = {};
      if (selected.definition.kind === "http") {
        body.operation = $("conn-test-operation").value; body.params = Object.create(null);
        const spec = selected.definition.operations[body.operation];
        $("conn-test-params").querySelectorAll("[data-field]").forEach(input => {
          if (!input.value.trim()) return; const type = spec.params[input.dataset.field].type;
          if (type === "boolean" && !["true", "false"].includes(input.value.trim())) throw new Error("Use true or false for " + input.dataset.field + ".");
          body.params[input.dataset.field] = type === "integer" ? Number(input.value) : type === "boolean" ? input.value.trim() === "true" : input.value;
        });
      }
      const result = await request(api + "/" + encodeURIComponent(selected.id) + "/test", "POST", body);
      $("conn-test-result").textContent = (result.ok ? "Check passed" : "Check failed: " + (result.error || "unavailable")) + " · " + result.elapsed_ms + " ms" + (result.summary?.bytes != null ? " · " + result.summary.bytes + " bytes · " + result.summary.response_type : "");
      await load();
    });
  });
  $("conn-delete").addEventListener("click", () => action($("conn-delete"), async () => {
    if (!selected || !await impact()) return;
    if (!await window.ArtifactDialogs.confirm({title:"Delete saved connection",subject:selected.id,message:"No artifacts are bound to this connection. Its saved definition will be removed.",action:"Delete connection",danger:true})) return;
    await request(api + "/" + encodeURIComponent(selected.id), "DELETE", { expected_version: selected.version });
    selected = null; $("conn-detail").hidden = true; $("conn-empty").hidden = false; await load(); notify("Connection deleted.");
  }));
  $("conn-binding-form").addEventListener("submit", async event => {
    event.preventDefault(); const button = $("conn-binding-save"); button.disabled = true;
    try {
      const bindings = Object.create(null);
      $("conn-bindings").querySelectorAll(".conn-binding").forEach(row => {
        const spec = { source: value(row, "source"), operations: [], subscriptions: [] };
        for (const key of ["operations", "subscriptions"]) spec[key] = [...row.querySelectorAll('[data-grant="' + key + '"] input:checked')].map(input => input.value);
        unique(bindings, value(row, "name"), spec);
      });
      await request("/settings/data-bindings/" + encodeURIComponent(bindingArtifact.id), "PUT", { bindings });
      $("conn-binding-dialog").close(); await load(); await impact(); notify("Artifact bindings saved.");
    } catch (error) { $("conn-binding-error").textContent = error.message; }
    finally { button.disabled = false; }
  });
  $("conn-binding-close").addEventListener("click", () => $("conn-binding-dialog").close());
  $("conn-add-binding").addEventListener("click", () => bindingCard("", {}, bindingArtifact.org));
  $("conn-add-header").addEventListener("click", () => mapHeader());
  $("conn-add-operation").addEventListener("click", () => operation());
  $("conn-add-subscription").addEventListener("click", () => subscription());
  $("conn-kind").addEventListener("change", kindFields);
  $("conn-test-operation").addEventListener("change", testParameters);
  $("conn-new").addEventListener("click", () => action($("conn-new"), async () => { if (!loaded) await load(); ++selectionRequest; fill(null); $("conn-id").focus(); }));
  $("conn-reload").addEventListener("click", () => action($("conn-reload"), () => selected && open(selected.id)));
  $("conn-search").addEventListener("input", list); $("conn-org-filter").addEventListener("change", list);
  $("conn-refresh").addEventListener("click", () => action($("conn-refresh"), async () => { await load(); notify("Connection health refreshed."); }));
  document.querySelector('[data-admin-tab="connections"]')?.addEventListener("click", () => action(null, () => load(!loaded)));
  if (!panel.hidden) action(null, () => load(true));
  const timer = setInterval(() => { if (!panel.hidden && document.visibilityState === "visible") action(null, () => load()); }, 15000);
  window.addEventListener("pagehide", () => clearInterval(timer), { once: true });
})();
