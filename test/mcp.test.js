import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createPreviewTaskStore } from "../lib/tasks.js";

const dataDir = mkdtempSync(join(tmpdir(), "artifact-mcp-rpc-"));
process.env.DATA_DIR = dataDir;
const {
  handleMcp,
  validateMcpHttpRequest,
  MODERN_PROTOCOL_VERSION,
  PROTOCOL_VERSION
} = await import("../lib/mcp.js");

test.after(() => rmSync(dataDir, { recursive: true, force: true }));

const auth = { clientId: "publisher", org: "acme", label: "Agent" };

function modernMeta(version = MODERN_PROTOCOL_VERSION) {
  return {
    "io.modelcontextprotocol/protocolVersion": version,
    "io.modelcontextprotocol/clientInfo": { name: "artifact-mcp-test", version: "1.0.0" },
    "io.modelcontextprotocol/clientCapabilities": {}
  };
}

function appsMeta() {
  const meta = modernMeta();
  meta["io.modelcontextprotocol/clientCapabilities"] = {
    extensions: {
      "io.modelcontextprotocol/ui": {
        mimeTypes: ["text/html;profile=mcp-app"]
      }
    }
  };
  return meta;
}

function tasksMeta() {
  const meta = modernMeta();
  meta["io.modelcontextprotocol/clientCapabilities"] = {
    extensions: {
      "io.modelcontextprotocol/tasks": {}
    }
  };
  return meta;
}

async function call(name, args, id = 1) {
  return handleMcp({
    jsonrpc: "2.0",
    id,
    method: "tools/call",
    params: { name, arguments: args }
  }, auth);
}

test("MCP enforces the published tool input schemas", async () => {
  const missing = await call("publish_artifact", { surprise: true });
  assert.equal(missing.error.code, -32602);
  assert.match(missing.error.message, /html is required/);
  assert.match(missing.error.message, /surprise is not allowed/);

  const nested = await call("publish_bundle", { files: { "index.html": 42 } }, 2);
  assert.equal(nested.error.code, -32602);
  assert.match(nested.error.message, /files\.index\.html must be a string/);

  const emptyEdits = await call("patch_artifact", {
    id: "missing1",
    expected_revision: 1,
    edits: []
  }, 21);
  assert.equal(emptyEdits.error.code, -32602);
  assert.match(emptyEdits.error.message, /edits must contain at least 1 item/);

  const malformedEdit = await call("patch_artifact", {
    id: "missing1",
    expected_revision: 1,
    edits: [{ find: "x", replace: 42, surprise: true }]
  }, 22);
  assert.equal(malformedEdit.error.code, -32602);
  assert.match(malformedEdit.error.message, /edits\.0\.surprise is not allowed/);
  assert.match(malformedEdit.error.message, /edits\.0\.replace must be a string/);
});

test("tools/list advertises the exact 21-tool golden", async () => {
  const listed = await handleMcp({ jsonrpc: "2.0", id: 23, method: "tools/list" }, auth);
  const golden = JSON.parse(readFileSync(new URL("../conformance/goldens/mcp.tools-list.json", import.meta.url)));
  assert.deepEqual(listed.result.tools, golden.steps[0].body.json.result.tools);
  assert.equal(listed.result.tools.length, 21);
  assert.equal(listed.result.tools.at(-1).name, "patch_artifact");
});

test("MCP 2026 discovery and tool listing use typed stateless results", async () => {
  const discover = await handleMcp({
    jsonrpc: "2.0",
    id: "discover",
    method: "server/discover",
    params: { _meta: modernMeta() }
  }, auth, { protocolVersion: MODERN_PROTOCOL_VERSION });
  assert.deepEqual(discover.result.supportedVersions, [MODERN_PROTOCOL_VERSION, PROTOCOL_VERSION]);
  assert.equal(discover.result.resultType, "complete");
  assert.equal(discover.result.cacheScope, "private");
  assert.equal(discover.result.ttlMs, 3_600_000);
  assert.deepEqual(
    discover.result.capabilities.extensions["io.modelcontextprotocol/tasks"],
    {}
  );
  assert.equal(
    discover.result.capabilities.extensions["io.modelcontextprotocol/skills"],
    undefined,
    "draft Skills over MCP must remain absent until ADR-0004's gate passes"
  );
  assert.equal(
    discover.result._meta["io.modelcontextprotocol/serverInfo"].name,
    "artifact-mcp"
  );

  const listed = await handleMcp({
    jsonrpc: "2.0",
    id: "tools",
    method: "tools/list",
    params: { _meta: modernMeta() }
  }, auth, { protocolVersion: MODERN_PROTOCOL_VERSION });
  assert.equal(listed.result.tools.length, 22);
  assert.equal(listed.result.tools.at(-1).name, "regenerate_artifact_preview");
  assert.ok(listed.result.tools.every((tool) => tool.outputSchema?.type === "object"));
  assert.equal(listed.result.resultType, "complete");
  assert.equal(listed.result.cacheScope, "private");
  assert.equal(listed.result.ttlMs, 300_000);

  const removed = await handleMcp({
    jsonrpc: "2.0",
    id: "initialize",
    method: "initialize",
    params: { _meta: modernMeta() }
  }, auth, { protocolVersion: MODERN_PROTOCOL_VERSION });
  assert.equal(removed.error.code, -32601);
});

test("preview regeneration uses durable tasks only for opted-in clients", async () => {
  const published = await call("publish_artifact", { html: "<h1>Task preview</h1>" }, 240);
  const artifactId = published.result.structuredContent.id;
  const taskDir = mkdtempSync(join(tmpdir(), "artifact-mcp-tasks-"));
  let taskSequence = 0;
  const tasks = createPreviewTaskStore({
    dataDir: taskDir,
    logger: { error() {} },
    createId: () => `task_${String(++taskSequence).padStart(20, "0")}`
  });
  const preview = {
    enabled: true,
    removed: [],
    async removeArtifact(id) { this.removed.push(id); },
    async ensureThumbnail() { return Buffer.from("png"); }
  };
  const invoke = (id, method, params, meta = tasksMeta(), callAuth = auth) => handleMcp({
    jsonrpc: "2.0",
    id,
    method,
    params: { ...params, _meta: meta }
  }, callAuth, {
    protocolVersion: MODERN_PROTOCOL_VERSION,
    preview,
    tasks
  });

  const fallback = await invoke("sync", "tools/call", {
    name: "regenerate_artifact_preview",
    arguments: { id: artifactId }
  }, modernMeta());
  assert.equal(fallback.result.resultType, "complete");
  assert.equal(fallback.result.structuredContent.regenerated, true);

  const created = await invoke("create", "tools/call", {
    name: "regenerate_artifact_preview",
    arguments: { id: artifactId }
  });
  assert.equal(created.result.resultType, "task");
  assert.equal(created.result.status, "working");
  assert.ok(tasks.get(created.result.taskId));

  await new Promise((resolve) => setImmediate(resolve));
  const completed = await invoke("get", "tasks/get", { taskId: created.result.taskId });
  assert.equal(completed.result.resultType, "complete");
  assert.equal(completed.result.status, "completed");
  assert.equal(completed.result.result.structuredContent.regenerated, true);
  assert.deepEqual(
    completed.result._meta["com.agentshelf.artifact-mcp/progress"],
    { current: 2, total: 2 }
  );
  assert.equal(
    (await invoke("update", "tasks/update", {
      taskId: created.result.taskId,
      inputResponses: {}
    })).result.resultType,
    "complete"
  );

  const restarted = createPreviewTaskStore({ dataDir: taskDir, logger: { error() {} } });
  assert.equal(restarted.get(created.result.taskId).status, "completed");
  const foreign = await handleMcp({
    jsonrpc: "2.0",
    id: "foreign",
    method: "tasks/get",
    params: { taskId: created.result.taskId, _meta: tasksMeta() }
  }, { clientId: "other", org: "other", role: "author" }, {
    protocolVersion: MODERN_PROTOCOL_VERSION,
    preview,
    tasks: restarted
  });
  assert.equal(foreign.error.code, -32602);
  assert.match(foreign.error.message, /Unknown task/);

  rmSync(taskDir, { recursive: true, force: true });
});

test("preview task cancellation is idempotent and wins a completion race", async () => {
  const published = await call("publish_artifact", { html: "<h1>Cancel preview</h1>" }, 241);
  const artifactId = published.result.structuredContent.id;
  const taskDir = mkdtempSync(join(tmpdir(), "artifact-mcp-task-cancel-"));
  const tasks = createPreviewTaskStore({
    dataDir: taskDir,
    logger: { error() {} },
    createId: () => "task_00000000000000000001"
  });
  let releaseRender;
  const preview = {
    enabled: true,
    async removeArtifact() {},
    ensureThumbnail: () => new Promise((resolve) => { releaseRender = resolve; })
  };
  const invoke = (id, method, params) => handleMcp({
    jsonrpc: "2.0",
    id,
    method,
    params: { ...params, _meta: tasksMeta() }
  }, auth, { protocolVersion: MODERN_PROTOCOL_VERSION, preview, tasks });

  const created = await invoke("create-cancel", "tools/call", {
    name: "regenerate_artifact_preview",
    arguments: { id: artifactId }
  });
  await new Promise((resolve) => setImmediate(resolve));
  const taskId = created.result.taskId;
  assert.equal((await invoke("cancel-1", "tasks/cancel", { taskId })).result.resultType, "complete");
  assert.equal((await invoke("cancel-2", "tasks/cancel", { taskId })).result.resultType, "complete");
  releaseRender(Buffer.from("png"));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal((await invoke("get-cancel", "tasks/get", { taskId })).result.status, "cancelled");

  rmSync(taskDir, { recursive: true, force: true });
});

test("MCP resources expose authorized current, revision, and bundle-file content", async () => {
  const invoke = (id, method, params = {}) => handleMcp({
    jsonrpc: "2.0",
    id,
    method,
    params: { ...params, _meta: modernMeta() }
  }, auth, { protocolVersion: MODERN_PROTOCOL_VERSION });

  const published = await invoke("publish-resource", "tools/call", {
    name: "publish_bundle",
    arguments: {
      title: "Resource bundle",
      files: {
        "index.html": "<h1>Revision one</h1>",
        "docs/guide.html": "<p>Guide one</p>"
      }
    }
  });
  const artifactId = published.result.structuredContent.id;
  assert.deepEqual(published.result.content.at(-1), {
    type: "resource_link",
    uri: `artifact://${artifactId}`,
    name: artifactId,
    description: "Authorized artifact resource"
  });

  const listed = await invoke("list-resources", "resources/list");
  assert.equal(listed.result.cacheScope, "private");
  assert.ok(listed.result.resources.some((resource) => resource.uri === `artifact://${artifactId}`));

  const templates = await invoke("templates", "resources/templates/list");
  assert.equal(templates.result.resourceTemplates.length, 5);

  const currentFile = await invoke("read-current-file", "resources/read", {
    uri: `artifact://${artifactId}/files/docs/guide.html`
  });
  assert.equal(currentFile.result.contents[0].text, "<p>Guide one</p>");
  assert.equal(currentFile.result.cacheScope, "private");

  const updated = await invoke("update-resource", "tools/call", {
    name: "update_artifact",
    arguments: {
      id: artifactId,
      expected_revision: 1,
      files: {
        "index.html": "<h1>Revision two</h1>",
        "docs/guide.html": "<p>Guide two</p>"
      }
    }
  });
  assert.equal(updated.result.structuredContent.revision, 2);
  assert.equal(updated.result.content.at(-1).uri, `artifact://${artifactId}`);

  const historicalFile = await invoke("read-revision-file", "resources/read", {
    uri: `artifact://${artifactId}/revisions/1/files/docs/guide.html`
  });
  assert.equal(historicalFile.result.contents[0].text, "<p>Guide one</p>");
  assert.equal(
    historicalFile.result.contents[0]._meta["com.agentshelf.artifact-mcp/revision"],
    1
  );

  const foreign = await handleMcp({
    jsonrpc: "2.0",
    id: "foreign-resource",
    method: "resources/read",
    params: {
      uri: `artifact://${artifactId}`,
      _meta: modernMeta()
    }
  }, { clientId: "foreign", org: "other" }, {
    protocolVersion: MODERN_PROTOCOL_VERSION
  });
  assert.equal(foreign.error.code, -32602);
  assert.equal(foreign.error.message, `Unknown artifact: ${artifactId}`);
});

test("MCP App review is negotiated, isolated, and keeps fallback output intact", async () => {
  const invoke = (id, method, params = {}, meta = appsMeta(), options = {}) => handleMcp({
    jsonrpc: "2.0",
    id,
    method,
    params: { ...params, _meta: meta }
  }, auth, { protocolVersion: MODERN_PROTOCOL_VERSION, ...options });

  const fallbackTools = await invoke("fallback-tools", "tools/list", {}, modernMeta());
  assert.ok(fallbackTools.result.tools.every((tool) => tool._meta === undefined));
  const appTools = await invoke("app-tools", "tools/list");
  assert.equal(appTools.result.tools.length, 23);
  assert.deepEqual(
    appTools.result.tools
      .filter((tool) => tool._meta?.ui?.resourceUri)
      .map((tool) => tool.name),
    ["publish_artifact", "publish_bundle", "list_artifacts", "read_artifact"]
  );
  assert.ok(appTools.result.tools
    .filter((tool) => tool._meta?.ui?.resourceUri)
    .every((tool) => tool._meta.ui.resourceUri === "ui://artifact-mcp/review"));
  assert.ok(appTools.result.tools
    .filter((tool) => tool.name !== "submit_feedback")
    .every((tool) => tool._meta.ui.visibility.includes("model")));
  assert.deepEqual(
    appTools.result.tools.find((tool) => tool.name === "submit_feedback")._meta.ui.visibility,
    ["app"]
  );

  const fallbackResources = await invoke(
    "fallback-resources",
    "resources/list",
    {},
    modernMeta()
  );
  assert.ok(fallbackResources.result.resources
    .every((resource) => resource.uri !== "ui://artifact-mcp/review"));
  const appResources = await invoke("app-resources", "resources/list");
  assert.equal(appResources.result.resources[0].uri, "ui://artifact-mcp/review");

  const fallbackRead = await invoke(
    "fallback-app-read",
    "resources/read",
    { uri: "ui://artifact-mcp/review" },
    modernMeta()
  );
  assert.equal(fallbackRead.error.code, -32602);
  const appRead = await invoke("app-read", "resources/read", {
    uri: "ui://artifact-mcp/review"
  });
  const appContent = appRead.result.contents[0];
  assert.equal(appContent.mimeType, "text/html;profile=mcp-app");
  assert.equal(appContent._meta.ui.prefersBorder, true);
  assert.deepEqual(appContent._meta.ui.csp, {
    connectDomains: [],
    resourceDomains: [],
    frameDomains: [],
    baseUriDomains: []
  });
  assert.doesNotMatch(appContent.text, /<iframe|[.]innerHTML/);

  const malicious = "<script>parent.postMessage({stolen:true}, '*')</script><h1>Private</h1>";
  const published = await invoke("app-publish", "tools/call", {
    name: "publish_artifact",
    arguments: {
      html: malicious,
      title: "Secure review",
      description: "Metadata only",
      category: "Security"
    }
  });
  assert.equal(published.result.content[0].type, "text");
  assert.equal(published.result.content.at(-1).type, "resource_link");
  assert.equal(
    published.result._meta["com.agentshelf.artifact-mcp/review"].artifacts[0].title,
    "Secure review"
  );
  assert.doesNotMatch(appContent.text, /stolen:true|<h1>Private/);

  const thumbnail = await invoke(
    "app-thumbnail",
    "resources/read",
    {
      uri: `artifact://${published.result.structuredContent.id}/thumbnail`
    },
    appsMeta(),
    {
      preview: {
        readThumbnail: async () => Buffer.from([0x89, 0x50, 0x4e, 0x47]),
        placeholder: () => Buffer.from("<svg/>")
      }
    }
  );
  assert.equal(thumbnail.result.contents[0].mimeType, "image/png");
  assert.equal(thumbnail.result.contents[0].blob, "iVBORw==");
  assert.equal(
    thumbnail.result.contents[0]._meta["com.agentshelf.artifact-mcp/trustedThumbnail"],
    true
  );

  const fallbackCall = await invoke(
    "fallback-list",
    "tools/call",
    { name: "list_artifacts", arguments: {} },
    modernMeta()
  );
  assert.equal(
    fallbackCall.result._meta["com.agentshelf.artifact-mcp/review"],
    undefined
  );
  assert.ok(Array.isArray(fallbackCall.result.content));
  assert.ok(Array.isArray(fallbackCall.result.structuredContent.artifacts));
});

test("MCP App review actions enforce admin-or-uploader management", async () => {
  const owner = { clientId: "review-owner", org: "review-org", role: "author", label: "Owner" };
  const reader = { clientId: "review-reader", org: "review-org", role: "reader", label: "Reader" };
  const collaborator = {
    clientId: "review-collaborator",
    org: "review-org",
    role: "collaborator",
    label: "Collaborator"
  };
  const foreign = { clientId: "review-foreign", org: "other-org", role: "reader", label: "Foreign" };
  const admin = { clientId: "review-admin", org: "admin", role: "author", label: "Admin" };
  let requestId = 10_000;
  const invokeAs = (requestAuth, name, args, app = true) => handleMcp({
    jsonrpc: "2.0",
    id: requestId += 1,
    method: "tools/call",
    params: {
      name,
      arguments: args,
      _meta: app ? appsMeta() : modernMeta()
    }
  }, requestAuth, { protocolVersion: MODERN_PROTOCOL_VERSION });

  const published = await invokeAs(owner, "publish_artifact", {
    html: "<h1>Inline review actions</h1>",
    title: "Inline review actions"
  });
  const id = published.result.structuredContent.id;
  assert.equal(
    published.result._meta["com.agentshelf.artifact-mcp/review"].artifacts[0].canManage,
    true
  );

  const readerList = await invokeAs(reader, "list_artifacts", {});
  const readerReview = readerList.result._meta["com.agentshelf.artifact-mcp/review"]
    .artifacts.find((artifact) => artifact.id === id);
  assert.equal(readerReview.canManage, false);
  assert.equal(readerReview.canFeedback, true);

  const directFeedback = await invokeAs(reader, "submit_feedback", {
    id,
    body: "This should require an MCP App host."
  }, false);
  assert.equal(directFeedback.error.code, -32602);
  assert.equal(directFeedback.error.message, "Unknown tool: submit_feedback");

  const submitted = await invokeAs(reader, "submit_feedback", {
    id,
    body: "The review flow works."
  });
  assert.equal(submitted.result.structuredContent.artifact_id, id);
  assert.equal(submitted.result.structuredContent.submitted, true);

  const concealed = await invokeAs(foreign, "submit_feedback", {
    id,
    body: "Cross-org probe"
  });
  assert.equal(concealed.result.isError, true);
  assert.equal(concealed.result.content[0].text, `Unknown artifact: ${id}`);

  for (const [name, extra] of [
    ["set_visibility", { hidden: true }],
    ["create_share", { expires: "24h" }],
    ["delete_artifact", {}]
  ]) {
    const denied = await invokeAs(collaborator, name, { id, ...extra });
    assert.equal(denied.result.isError, true, `${name} rejects a non-owner collaborator`);
    assert.match(denied.result.content[0].text, /Permission denied/);
  }

  const revised = await invokeAs(owner, "update_artifact", {
    id,
    expected_revision: 1,
    html: "<h1>Inline review actions, revised</h1>"
  });
  assert.equal(revised.result.structuredContent.revision, 2);
  const revisions = await invokeAs(owner, "list_revisions", { id });
  assert.equal(revisions.result.structuredContent.current, 2);
  assert.ok(
    revisions.result.structuredContent.revisions.some((revision) => revision.revision === 1)
  );

  const hidden = await invokeAs(owner, "set_visibility", { id, hidden: true });
  assert.equal(hidden.result.structuredContent.hidden, true);
  assert.equal(
    hidden.result._meta["com.agentshelf.artifact-mcp/review"].artifacts[0].hidden,
    true
  );
  const shared = await invokeAs(owner, "create_share", { id, expires: "24h" });
  assert.equal(shared.result.structuredContent.id, id);
  assert.match(shared.result.structuredContent.url, /\/s\//);

  const removed = await invokeAs(owner, "delete_artifact", { id });
  assert.equal(removed.result.structuredContent.deleted, true);
  assert.deepEqual(
    removed.result._meta["com.agentshelf.artifact-mcp/audit"],
    {
      action: "delete",
      artifactId: id,
      actor: "agent:review-owner",
      outcome: "deleted"
    }
  );

  const adminTarget = await invokeAs(owner, "publish_artifact", {
    html: "<h1>Admin managed</h1>",
    title: "Admin managed"
  });
  const adminTargetId = adminTarget.result.structuredContent.id;
  const adminRead = await invokeAs(admin, "read_artifact", { id: adminTargetId });
  assert.equal(
    adminRead.result._meta["com.agentshelf.artifact-mcp/review"].artifacts[0].canManage,
    true
  );
  const adminRemoved = await invokeAs(admin, "delete_artifact", { id: adminTargetId });
  assert.equal(adminRemoved.result.structuredContent.deleted, true);
  assert.equal(
    adminRemoved.result._meta["com.agentshelf.artifact-mcp/audit"].actor,
    "agent:review-admin"
  );
});

test("MCP 2026 HTTP metadata rejects mismatches before dispatch", () => {
  const payload = {
    jsonrpc: "2.0",
    id: "call",
    method: "tools/call",
    params: {
      name: "list_artifacts",
      arguments: {},
      _meta: modernMeta()
    }
  };
  const valid = validateMcpHttpRequest(payload, {
    "mcp-protocol-version": MODERN_PROTOCOL_VERSION,
    "mcp-method": "tools/call",
    "mcp-name": "list_artifacts"
  });
  assert.deepEqual(valid, {
    ok: true,
    protocolVersion: MODERN_PROTOCOL_VERSION,
    modern: true
  });

  const mismatched = validateMcpHttpRequest(payload, {
    "mcp-protocol-version": MODERN_PROTOCOL_VERSION,
    "mcp-method": "tools/list",
    "mcp-name": "list_artifacts"
  });
  assert.equal(mismatched.status, 400);
  assert.equal(mismatched.response.error.code, -32020);

  const unsupportedPayload = structuredClone(payload);
  unsupportedPayload.params._meta = modernMeta("2099-01-01");
  const unsupported = validateMcpHttpRequest(unsupportedPayload, {
    "mcp-protocol-version": "2099-01-01",
    "mcp-method": "tools/call",
    "mcp-name": "list_artifacts"
  });
  assert.equal(unsupported.response.error.code, -32022);
  assert.deepEqual(unsupported.response.error.data.supported, [
    MODERN_PROTOCOL_VERSION,
    PROTOCOL_VERSION
  ]);

  const legacy = validateMcpHttpRequest(
    { jsonrpc: "2.0", id: 1, method: "tools/list" },
    {}
  );
  assert.deepEqual(legacy, { ok: true, protocolVersion: PROTOCOL_VERSION, modern: false });
});

test("MCP artifact events expose revision metadata through the notifier seam", async () => {
  const events = [];
  const notify = (...args) => events.push(args);
  const published = await handleMcp({
    jsonrpc: "2.0",
    id: 200,
    method: "tools/call",
    params: { name: "publish_artifact", arguments: { html: "<h1>Preview seam</h1>", category: "  Previews  " } }
  }, auth, { notify });
  const id = published.result.structuredContent.id;

  assert.equal(published.result.structuredContent.category, "Previews");
  assert.equal(events.length, 1);
  assert.equal(events[0][0], "published");
  assert.equal(events[0][3].artifactMeta.id, id);
  assert.equal(events[0][3].artifactMeta.revision, 1);
  assert.equal(events[0][3].artifactMeta.is_bundle, 0);

  const bundle = await handleMcp({
    jsonrpc: "2.0",
    id: 201,
    method: "tools/call",
    params: { name: "publish_bundle", arguments: { files: { "index.html": "<h1>Bundle</h1>" }, category: "  Bundles  " } }
  }, auth, { notify });
  assert.equal(bundle.result.structuredContent.category, "Bundles");
  assert.equal(events[1][0], "published");
  assert.equal(events[1][3].artifactMeta.is_bundle, 1);
});

test("admin publish targets are normalized and must name a registered organization", async () => {
  const orgs = await import("../lib/orgs.js");
  const admin = { clientId: "administrator", org: "admin", label: "Admin agent" };
  orgs.createOrg({ name: "publish-target" });

  const invoke = (requestId, name, targetOrg) => handleMcp({
    jsonrpc: "2.0",
    id: requestId,
    method: "tools/call",
    params: {
      name,
      arguments: name === "publish_bundle"
        ? { files: { "index.html": "<h1>Bundle</h1>" }, org: targetOrg }
        : { html: "<h1>Single</h1>", org: targetOrg }
    }
  }, admin);

  const single = await invoke(202, "publish_artifact", "  PUBLISH-TARGET  ");
  const bundle = await invoke(203, "publish_bundle", "Publish-Target");
  assert.equal(single.result.structuredContent.org, "publish-target");
  assert.equal(bundle.result.structuredContent.org, "publish-target");

  for (const [requestId, name] of [[204, "publish_artifact"], [205, "publish_bundle"]]) {
    const rejected = await invoke(requestId, name, "missing-target");
    assert.equal(rejected.result.isError, true);
    assert.equal(
      rejected.result.content[0].text,
      'Unknown organization "missing-target". Create it in the Organizations section first.'
    );
  }
});

test("update_artifact conceals existence and enforces expected revisions", async () => {
  const published = await call("publish_artifact", { html: "<h1>Guarded</h1>" }, 210);
  const id = published.result.structuredContent.id;
  const updateRequest = (requestId, artifactId, requestAuth, extra = {}) => handleMcp({
    jsonrpc: "2.0",
    id: requestId,
    method: "tools/call",
    params: { name: "update_artifact", arguments: { id: artifactId, title: "Updated", ...extra } }
  }, requestAuth);

  const denied = await updateRequest(211, id, { clientId: "other", org: "acme" });
  const missing = await updateRequest(212, "missing1", auth);
  assert.equal(denied.result.isError, true);
  assert.equal(missing.result.isError, true);
  assert.equal(denied.result.content[0].text, "Permission denied: this API key cannot modify this artifact");
  assert.equal(missing.result.content[0].text, "Unknown artifact: missing1");

  const updated = await updateRequest(213, id, auth, { expected_revision: 1 });
  assert.equal(updated.result.structuredContent.revision, 2);
  const stale = await updateRequest(214, id, auth, { expected_revision: 1 });
  assert.equal(stale.result.isError, true);
  assert.match(stale.result.content[0].text, /changed|conflict/i);
});

test("artifact_stats exposes named audience data only to the owner or an admin", async () => {
  const published = await call("publish_artifact", { html: "<h1>Stats</h1>" }, 3);
  const id = published.result.structuredContent.id;
  const { record } = await import("../lib/views.js");
  record(id, "acme", "viewer@example.test");

  const stats = await call("artifact_stats", { id }, 4);
  assert.equal(stats.result.structuredContent.views, 1);
  assert.deepEqual(stats.result.structuredContent.viewers.map((v) => v.email), ["viewer@example.test"]);

  // Same-org capability denials are explicit; only tenancy is concealed.
  const denied = await handleMcp({ jsonrpc: "2.0", id: 5, method: "tools/call", params: { name: "artifact_stats", arguments: { id } } }, { clientId: "other", org: "acme" });
  assert.equal(denied.result.isError, true);
  assert.equal(denied.result.content[0].text, "Permission denied: this API key cannot read this artifact");

  // Same key, different org (i.e. after an admin re-tenanted the artifact) must NOT retain
  // control just because client_id matches — ownership requires matching org too.
  const movedOut = await handleMcp({ jsonrpc: "2.0", id: 51, method: "tools/call", params: { name: "artifact_stats", arguments: { id } } }, { clientId: "publisher", org: "other" });
  assert.equal(movedOut.result.isError, true);
  assert.match(movedOut.result.content[0].text, /Unknown artifact/);
});

test("MCP read tools answer a foreign artifact exactly like a nonexistent one", async () => {
  const published = await call("publish_artifact", { html: "<h1>Concealed</h1>" }, 70);
  const id = published.result.structuredContent.id;
  const foreignAuth = { clientId: "intruder", org: "other", label: "Other org agent" };
  const probe = (requestId, tool, artifactId) => handleMcp({
    jsonrpc: "2.0",
    id: requestId,
    method: "tools/call",
    params: { name: tool, arguments: { id: artifactId } }
  }, foreignAuth);

  let requestId = 700;
  for (const tool of ["list_revisions", "list_shares", "artifact_stats", "list_feedback", "read_artifact"]) {
    const foreign = await probe(requestId += 1, tool, id);
    const missing = await probe(requestId += 1, tool, "missing1");
    assert.equal(foreign.result.isError, true, `${tool} denies a foreign artifact`);
    assert.equal(missing.result.isError, true, `${tool} denies an unknown artifact`);
    // The only thing that may differ is the id the caller itself supplied, never whether that
    // id exists in another tenant — substituting it makes the two responses identical.
    assert.equal(
      foreign.result.content[0].text.replace(id, "ID"),
      missing.result.content[0].text.replace("missing1", "ID"),
      `${tool} conceals cross-tenant existence`
    );
    assert.match(foreign.result.content[0].text, /Unknown artifact/);
  }
});

test("MCP artifact-id write tools conceal foreign existence exactly like nonexistence", async () => {
  const published = await call("publish_artifact", { html: "<h1>Write concealment</h1>" }, 710);
  const id = published.result.structuredContent.id;
  const foreignAuth = { clientId: "intruder", org: "other", label: "Other org agent" };
  const tools = [
    ["delete_artifact", {}],
    ["set_visibility", { hidden: true }],
    ["set_category", { category: "Probe" }],
    ["create_share", { expires: "never" }],
    ["restore_artifact", { revision: 1 }]
  ];
  const invoke = (requestId, name, extra) => handleMcp({
    jsonrpc: "2.0",
    id: requestId,
    method: "tools/call",
    params: { name, arguments: { id, ...extra } }
  }, foreignAuth);

  const foreign = [];
  for (const [name, extra] of tools) foreign.push(await invoke(739, name, extra));
  const removed = await call("delete_artifact", { id }, 720);
  assert.equal(removed.result.structuredContent.deleted, true);

  for (let index = 0; index < tools.length; index += 1) {
    const [name, extra] = tools[index];
    const missing = await invoke(739, name, extra);
    assert.deepEqual(missing, foreign[index], `${name} conceals foreign and missing ids identically`);
    assert.equal(missing.result.content[0].text, `Unknown artifact: ${id}`);
  }
});

test("share-token and feedback-id mutations conceal foreign records like unknown records", async () => {
  const foreignAuth = { clientId: "intruder", org: "other", label: "Other org agent" };
  const invoke = (requestId, name, args, requestAuth = foreignAuth) => handleMcp({
    jsonrpc: "2.0",
    id: requestId,
    method: "tools/call",
    params: { name, arguments: args }
  }, requestAuth);

  const shareArtifact = await call("publish_artifact", { html: "<h1>Share oracle</h1>" }, 740);
  const shareId = shareArtifact.result.structuredContent.id;
  const created = await call("create_share", { id: shareId, expires: "never" }, 741);
  const token = created.result.structuredContent.token;
  const foreignShare = await invoke(749, "revoke_share", { token });
  await call("revoke_share", { token }, 742);
  const unknownShare = await invoke(749, "revoke_share", { token });
  assert.deepEqual(foreignShare, unknownShare);
  assert.equal(unknownShare.result.content[0].text, "Unknown share");

  const feedbackArtifact = await call("publish_artifact", { html: "<h1>Feedback oracle</h1>" }, 750);
  const feedbackArtifactId = feedbackArtifact.result.structuredContent.id;
  const { addFeedback } = await import("../lib/feedback.js");
  const feedback = addFeedback({
    artifactId: feedbackArtifactId,
    org: "acme",
    viewerEmail: "viewer@acme.test",
    body: "Conceal me",
    artifactRevision: 1
  });
  const feedbackTools = ["resolve_feedback", "reopen_feedback"];
  const foreignFeedback = [];
  for (const name of feedbackTools) {
    foreignFeedback.push(await invoke(759, name, { feedback_id: feedback.id }));
  }
  await call("delete_artifact", { id: feedbackArtifactId }, 751);
  for (let index = 0; index < feedbackTools.length; index += 1) {
    const missing = await invoke(759, feedbackTools[index], { feedback_id: feedback.id });
    assert.deepEqual(foreignFeedback[index], missing);
    assert.equal(missing.result.content[0].text, `Unknown feedback: ${feedback.id}`);
  }
});

test("list_artifacts returns stored agent-facing metadata without credential or digest fields", async () => {
  const published = await call("publish_artifact", {
    html: "<h1>Listed metadata</h1>",
    title: "Metadata",
    description: "Round trip",
    category: "Reports"
  }, 720);
  const id = published.result.structuredContent.id;
  await call("set_visibility", { id, hidden: true }, 721);

  const listed = await call("list_artifacts", {}, 722);
  const row = listed.result.structuredContent.artifacts.find((artifact) => artifact.id === id);
  assert.deepEqual(
    Object.keys(row),
    [
      "id", "url", "title", "description", "created_at", "org", "category", "revision",
      "updated_at", "bytes", "is_bundle", "entry", "hidden", "uploader_label"
    ]
  );
  assert.equal(row.org, "acme");
  assert.equal(row.category, "Reports");
  assert.equal(row.revision, 1);
  assert.equal(row.bytes, Buffer.byteLength("<h1>Listed metadata</h1>"));
  assert.equal(row.is_bundle, 0);
  assert.equal(row.entry, "");
  assert.equal(row.hidden, 1);
  assert.equal(row.uploader_label, "Agent");
  assert.equal(typeof row.updated_at, "string");
  assert.equal(Object.hasOwn(row, "client_id"), false);
  assert.equal(Object.hasOwn(row, "body_sha256"), false);
});

test("API key roles enforce the own, same-org, and cross-org read/write/delete matrix", async () => {
  const { publish } = await import("../lib/store.js");
  const db = (await import("../lib/db.js")).default;
  const {
    PUBLISH_PERMISSION_ERROR,
    READ_PERMISSION_ERROR,
    WRITE_PERMISSION_ERROR,
    DELETE_PERMISSION_ERROR
  } = await import("../lib/access.js");

  let requestId = 800;
  const rpc = (requestAuth, name, toolArguments) => handleMcp({
    jsonrpc: "2.0",
    id: requestId += 1,
    method: "tools/call",
    params: { name, arguments: toolArguments }
  }, requestAuth);
  const publishFor = async (requestAuth, suffix) => {
    const response = await rpc(requestAuth, "publish_artifact", {
      html: "<p>seed</p>",
      title: suffix
    });
    return response.result.structuredContent.id;
  };
  const errorText = (response) => response.result.content[0].text;

  const identities = {
    reader: { clientId: "role-reader", org: "acme", label: "Reader key", role: "reader" },
    author: { clientId: "role-author", org: "acme", label: "Author key", role: "author" },
    collaborator: {
      clientId: "role-collaborator",
      org: "acme",
      label: "Collaborator key",
      role: "collaborator"
    }
  };
  const colleague = { clientId: "role-colleague", org: "acme", label: "Colleague", role: "author" };
  const foreign = { clientId: "role-foreign", org: "beta", label: "Foreign", role: "author" };

  const targets = {
    reader: {
      own: publish({
        clientId: identities.reader.clientId,
        org: "acme",
        uploaderLabel: identities.reader.label,
        html: "<p>seed</p>",
        title: "reader-own"
      }).id,
      same: await publishFor(colleague, "reader-same"),
      cross: await publishFor(foreign, "reader-cross")
    },
    author: {
      own: await publishFor(identities.author, "author-own"),
      same: await publishFor(colleague, "author-same"),
      cross: await publishFor(foreign, "author-cross")
    },
    collaborator: {
      own: await publishFor(identities.collaborator, "collaborator-own"),
      same: await publishFor(colleague, "collaborator-same"),
      cross: await publishFor(foreign, "collaborator-cross")
    }
  };

  const refusedPublish = await rpc(identities.reader, "publish_artifact", { html: "<p>no</p>" });
  assert.equal(refusedPublish.result.isError, true);
  assert.equal(errorText(refusedPublish), PUBLISH_PERMISSION_ERROR);

  const authorList = (await rpc(identities.author, "list_artifacts", {})).result.structuredContent.artifacts;
  assert.deepEqual(authorList.map((row) => row.id), [targets.author.own]);
  for (const role of ["reader", "collaborator"]) {
    const rows = (await rpc(identities[role], "list_artifacts", {})).result.structuredContent.artifacts;
    assert.ok(rows.some((row) => row.id === targets[role].same), role + " lists colleague artifacts");
    assert.ok(rows.every((row) => !Object.hasOwn(row, "client_id")), role + " listing conceals client_id");
    assert.ok(rows.every((row) => Object.hasOwn(row, "uploader_label")), role + " listing labels uploaders");
  }

  const expected = {
    reader: {
      own: { read: "ok", write: WRITE_PERMISSION_ERROR, delete: DELETE_PERMISSION_ERROR },
      same: { read: "ok", write: WRITE_PERMISSION_ERROR, delete: DELETE_PERMISSION_ERROR },
      cross: { read: "unknown", write: "unknown", delete: "unknown" }
    },
    author: {
      own: { read: "ok", write: "ok", delete: "ok" },
      same: { read: READ_PERMISSION_ERROR, write: WRITE_PERMISSION_ERROR, delete: DELETE_PERMISSION_ERROR },
      cross: { read: "unknown", write: "unknown", delete: "unknown" }
    },
    collaborator: {
      own: { read: "ok", write: "ok", delete: "ok" },
      same: { read: "ok", write: "ok", delete: DELETE_PERMISSION_ERROR },
      cross: { read: "unknown", write: "unknown", delete: "unknown" }
    }
  };

  for (const role of ["reader", "author", "collaborator"]) {
    for (const scope of ["own", "same", "cross"]) {
      const id = targets[role][scope];
      const read = await rpc(identities[role], "read_artifact", { id });
      const write = await rpc(identities[role], "patch_artifact", {
        id,
        expected_revision: 1,
        edits: [{ find: "seed", replace: role + "-" + scope }]
      });
      const deleted = await rpc(identities[role], "delete_artifact", { id });
      for (const [operation, response] of [["read", read], ["write", write], ["delete", deleted]]) {
        const outcome = expected[role][scope][operation];
        if (outcome === "ok") {
          assert.notEqual(response.result.isError, true, role + " " + scope + " " + operation);
        } else {
          assert.equal(response.result.isError, true, role + " " + scope + " " + operation + " is refused");
          assert.equal(
            errorText(response),
            outcome === "unknown" ? "Unknown artifact: " + id : outcome,
            role + " " + scope + " " + operation + " error"
          );
        }
      }
    }
  }

  // A second colleague edit makes the first collaborator-produced revision historical, proving
  // that list_revisions exposes the real actor rather than the artifact owner.
  const colleagueId = targets.collaborator.same;
  const secondPatch = await rpc(identities.collaborator, "patch_artifact", {
    id: colleagueId,
    expected_revision: 2,
    edits: [{ find: "collaborator-same", replace: "collaborator-second" }]
  });
  assert.equal(secondPatch.result.structuredContent.revision, 3);
  const revisions = (await rpc(identities.collaborator, "list_revisions", { id: colleagueId }))
    .result.structuredContent.revisions;
  assert.deepEqual(
    revisions.map((revision) => [revision.revision, revision.client_id]),
    [[2, identities.collaborator.clientId], [1, colleague.clientId]]
  );
  assert.equal(
    db.prepare("SELECT client_id FROM artifact_revisions WHERE artifact_id = ? AND revision = 3")
      .pluck().get(colleagueId),
    identities.collaborator.clientId,
    "the live revision also records its producer"
  );
});

test("read_artifact reads current and retained single-file content with UTF-8-safe byte paging", async () => {
  const first = "A🎉B";
  const published = await call("publish_artifact", { html: first }, 730);
  const id = published.result.structuredContent.id;

  const whole = await call("read_artifact", { id }, 731);
  const expectedWhole = {
    id,
    org: "acme",
    is_bundle: false,
    revision: 1,
    content_type: "text/html; charset=utf-8",
    bytes_total: 6,
    offset: 0,
    bytes_returned: 6,
    truncated: false,
    content: first
  };
  assert.deepEqual(whole.result.structuredContent, expectedWhole);
  assert.equal(whole.result.content[0].text, JSON.stringify(expectedWhole));

  const boundary = await call("read_artifact", { id, offset: 0, limit: 4 }, 732);
  assert.deepEqual(
    {
      offset: boundary.result.structuredContent.offset,
      bytes_returned: boundary.result.structuredContent.bytes_returned,
      truncated: boundary.result.structuredContent.truncated,
      content: boundary.result.structuredContent.content
    },
    { offset: 0, bytes_returned: 1, truncated: true, content: "A" }
  );
  const insideSequence = await call("read_artifact", { id, offset: 2, limit: 5 }, 733);
  assert.deepEqual(
    {
      offset: insideSequence.result.structuredContent.offset,
      bytes_returned: insideSequence.result.structuredContent.bytes_returned,
      content: insideSequence.result.structuredContent.content
    },
    { offset: 5, bytes_returned: 1, content: "B" }
  );

  await call("update_artifact", { id, html: "second" }, 734);
  const historical = await call("read_artifact", { id, revision: 1 }, 735);
  assert.equal(historical.result.structuredContent.revision, 1);
  assert.equal(historical.result.structuredContent.content, first);
});

test("read_artifact lists bundles, reads one file, and rejects traversal through the raw guard", async () => {
  const published = await call("publish_bundle", {
    files: {
      "index.html": "<h1>Bundle</h1>",
      "assets/note.txt": "hello 🎉"
    }
  }, 740);
  const id = published.result.structuredContent.id;

  const listing = await call("read_artifact", { id }, 741);
  assert.deepEqual(listing.result.structuredContent.files, [
    { path: "assets/note.txt", bytes: 10, entry: false },
    { path: "index.html", bytes: 15, entry: true }
  ]);
  assert.equal(listing.result.structuredContent.entry, "index.html");
  assert.equal(listing.result.structuredContent.content_type, "application/json");
  assert.equal(listing.result.structuredContent.bytes_returned, 0);

  const file = await call("read_artifact", { id, path: "assets/note.txt" }, 742);
  assert.equal(file.result.structuredContent.content_type, "text/plain; charset=utf-8");
  assert.equal(file.result.structuredContent.content, "hello 🎉");
  assert.equal(file.result.structuredContent.bytes_total, 10);

  const traversal = await call("read_artifact", { id, path: "../index.html" }, 743);
  assert.equal(traversal.result.isError, true);
  assert.equal(traversal.result.content[0].text, "Unknown bundle file: ../index.html");
});

test("patch_artifact applies one unique find as exactly one revision", async () => {
  const published = await call("publish_artifact", { html: "before unique after" }, 750);
  const id = published.result.structuredContent.id;

  const patched = await call("patch_artifact", {
    id,
    expected_revision: 1,
    edits: [{ find: "unique", replace: "changed" }]
  }, 751);
  const expected = {
    id,
    revision: 2,
    bytes_before: 19,
    bytes_after: 20,
    edits_applied: 1
  };
  assert.deepEqual(patched.result.structuredContent, expected);
  assert.equal(patched.result.content[0].text, JSON.stringify(expected));

  const read = await call("read_artifact", { id }, 752);
  assert.equal(read.result.structuredContent.content, "before changed after");
  const history = await call("list_revisions", { id }, 753);
  assert.equal(history.result.structuredContent.current, 2);
  assert.deepEqual(
    history.result.structuredContent.revisions.map((row) => row.revision),
    [1]
  );
});

test("patch_artifact applies a range batch against pre-edit UTF-8 byte offsets atomically", async () => {
  const published = await call("publish_artifact", { html: "A🎉B---C" }, 754);
  const id = published.result.structuredContent.id;

  const patched = await call("patch_artifact", {
    id,
    expected_revision: 1,
    edits: [
      { offset: 9, length: 1, replace: "see" },
      { offset: 5, length: 1, replace: "bee" }
    ]
  }, 755);
  assert.deepEqual(patched.result.structuredContent, {
    id,
    revision: 2,
    bytes_before: 10,
    bytes_after: 14,
    edits_applied: 2
  });

  const read = await call("read_artifact", { id }, 756);
  assert.equal(read.result.structuredContent.content, "A🎉bee---see");
  const history = await call("list_revisions", { id }, 757);
  assert.equal(history.result.structuredContent.current, 2);
  assert.deepEqual(history.result.structuredContent.revisions.map((row) => row.revision), [1]);
});

test("patch_artifact rejects zero and multiple find matches without writing", async () => {
  const published = await call("publish_artifact", { html: "alpha beta alpha" }, 758);
  const id = published.result.structuredContent.id;

  const missing = await call("patch_artifact", {
    id,
    expected_revision: 1,
    edits: [{ find: "gamma", replace: "changed" }]
  }, 759);
  assert.equal(missing.result.isError, true);
  assert.equal(missing.result.content[0].text, "edit 1 find matched 0 times; expected exactly once");

  const ambiguous = await call("patch_artifact", {
    id,
    expected_revision: 1,
    edits: [{ find: "alpha", replace: "changed" }]
  }, 760);
  assert.equal(ambiguous.result.isError, true);
  assert.equal(ambiguous.result.content[0].text, "edit 1 find matched 2 times; expected exactly once");

  const read = await call("read_artifact", { id }, 761);
  assert.equal(read.result.structuredContent.content, "alpha beta alpha");
  const history = await call("list_revisions", { id }, 762);
  assert.equal(history.result.structuredContent.current, 1);
  assert.deepEqual(history.result.structuredContent.revisions, []);
});

test("patch_artifact requires a fresh revision and UTF-8-aligned ranges", async () => {
  const published = await call("publish_artifact", { html: "A🎉B" }, 763);
  const id = published.result.structuredContent.id;

  const missingRevision = await call("patch_artifact", {
    id,
    edits: [{ find: "B", replace: "C" }]
  }, 764);
  assert.equal(missingRevision.error.code, -32602);
  assert.match(missingRevision.error.message, /expected_revision is required/);

  const stale = await call("patch_artifact", {
    id,
    expected_revision: 2,
    edits: [{ find: "B", replace: "C" }]
  }, 765);
  assert.equal(stale.result.isError, true);
  assert.equal(stale.result.content[0].text, "Artifact changed during update; fetch its current revision and retry");

  const splitOffset = await call("patch_artifact", {
    id,
    expected_revision: 1,
    edits: [{ offset: 2, length: 1, replace: "x" }]
  }, 766);
  assert.equal(splitOffset.result.isError, true);
  assert.equal(splitOffset.result.content[0].text, "edit 1 offset 2 is not a UTF-8 boundary");

  const splitEnd = await call("patch_artifact", {
    id,
    expected_revision: 1,
    edits: [{ offset: 1, length: 1, replace: "x" }]
  }, 767);
  assert.equal(splitEnd.result.isError, true);
  assert.equal(splitEnd.result.content[0].text, "edit 1 range end 2 is not a UTF-8 boundary");

  const read = await call("read_artifact", { id }, 768);
  assert.equal(read.result.structuredContent.content, "A🎉B");
  const history = await call("list_revisions", { id }, 769);
  assert.equal(history.result.structuredContent.current, 1);
  assert.deepEqual(history.result.structuredContent.revisions, []);
});

test("patch_artifact patches one sanitized bundle path and rejects traversal", async () => {
  const published = await call("publish_bundle", {
    files: {
      "index.html": "hello world",
      "assets/note.txt": "untouched"
    }
  }, 770);
  const id = published.result.structuredContent.id;

  const patched = await call("patch_artifact", {
    id,
    expected_revision: 1,
    path: "docs/../index.html",
    edits: [{ find: "world", replace: "Earth" }]
  }, 771);
  assert.deepEqual(patched.result.structuredContent, {
    id,
    revision: 2,
    bytes_before: 20,
    bytes_after: 20,
    edits_applied: 1
  });

  const entry = await call("read_artifact", { id, path: "index.html" }, 772);
  assert.equal(entry.result.structuredContent.content, "hello Earth");
  const untouched = await call("read_artifact", { id, path: "assets/note.txt" }, 773);
  assert.equal(untouched.result.structuredContent.content, "untouched");

  const traversal = await call("patch_artifact", {
    id,
    expected_revision: 2,
    path: "../index.html",
    edits: [{ find: "Earth", replace: "unsafe" }]
  }, 774);
  assert.equal(traversal.result.isError, true);
  assert.equal(traversal.result.content[0].text, "Unknown bundle file: ../index.html");

  const history = await call("list_revisions", { id }, 775);
  assert.equal(history.result.structuredContent.current, 2);
  assert.deepEqual(history.result.structuredContent.revisions.map((row) => row.revision), [1]);
});

test("patch_artifact rejects a result over the artifact size cap without writing", async () => {
  const { MAX_ARTIFACT_BYTES } = await import("../lib/config.js");
  const original = "x".repeat(MAX_ARTIFACT_BYTES);
  const published = await call("publish_artifact", { html: original }, 776);
  const id = published.result.structuredContent.id;

  const oversized = await call("patch_artifact", {
    id,
    expected_revision: 1,
    edits: [{ offset: MAX_ARTIFACT_BYTES, length: 0, replace: "y" }]
  }, 777);
  assert.equal(oversized.result.isError, true);
  assert.equal(
    oversized.result.content[0].text,
    `html exceeds ${MAX_ARTIFACT_BYTES} bytes (got ${MAX_ARTIFACT_BYTES + 1})`
  );

  const listed = await call("list_artifacts", {}, 778);
  const row = listed.result.structuredContent.artifacts.find((artifact) => artifact.id === id);
  assert.equal(row.revision, 1);
  assert.equal(row.bytes, MAX_ARTIFACT_BYTES);
  const history = await call("list_revisions", { id }, 779);
  assert.equal(history.result.structuredContent.current, 1);
  assert.deepEqual(history.result.structuredContent.revisions, []);
});

test("share tools are owner-or-admin and revocation immediately makes a link unresolved", async () => {
  const published = await call("publish_artifact", { html: "<h1>Shared</h1>" }, 40);
  const artifactId = published.result.structuredContent.id;
  const created = await call("create_share", { id: artifactId, expires: "never" }, 41);
  const token = created.result.structuredContent.token;
  assert.match(created.result.structuredContent.url, new RegExp(`/s/${token}$`));
  const listed = await call("list_shares", { id: artifactId }, 42);
  assert.equal(listed.result.structuredContent.shares.some((row) => row.token === token), true);
  const denied = await handleMcp({ jsonrpc: "2.0", id: 43, method: "tools/call", params: { name: "create_share", arguments: { id: artifactId, expires: "never" } } }, { clientId: "other", org: "acme" });
  assert.equal(denied.result.isError, true);
  const revoked = await call("revoke_share", { token }, 44);
  assert.equal(revoked.result.structuredContent.revoked, true);
  const { resolve } = await import("../lib/shares.js");
  assert.equal(resolve(token), null);
});

test("list_feedback exposes anchor reliability while keeping ownership and resolution scoped", async () => {
  const published = await call("publish_artifact", { html: "<h1>Feedback</h1>" }, 6);
  const artifactId = published.result.structuredContent.id;
  const { addFeedback } = await import("../lib/feedback.js");
  const row = addFeedback({
    artifactId, org: "acme", viewerEmail: "viewer@acme.test", body: "At the chart", artifactRevision: 1,
    anchor: { path: "body:nth-child(2)", x: 0.2, y: 0.8, w: 0.3, h: 0.1, approx: true }
  });

  const listed = await call("list_feedback", { id: artifactId }, 7);
  assert.deepEqual(listed.result.structuredContent.feedback[0].anchor_path, "body:nth-child(2)");
  assert.equal(listed.result.structuredContent.feedback[0].anchor_approx, 1);
  assert.equal(listed.result.structuredContent.feedback[0].anchor_w, 0.3);
  assert.equal(listed.result.structuredContent.feedback[0].anchor_h, 0.1);
  assert.equal(listed.result.structuredContent.feedback[0].artifact_revision, 1);
  assert.equal(listed.result.structuredContent.feedback[0].parent_id, null);

  const denied = await handleMcp({ jsonrpc: "2.0", id: 8, method: "tools/call", params: { name: "resolve_feedback", arguments: { feedback_id: row.id } } }, { clientId: "other", org: "acme" });
  assert.equal(denied.result.isError, true);
  const resolved = await call("resolve_feedback", { feedback_id: row.id }, 9);
  assert.equal(resolved.result.structuredContent.resolved, true);
  const reopened = await call("reopen_feedback", { feedback_id: row.id }, 10);
  assert.equal(reopened.result.structuredContent.reopened, true);
});

test("feedback anchors persist bounded coordinates while omitted anchors remain general comments", async () => {
  const published = await call("publish_artifact", { html: "<h1>Anchor storage</h1>" }, 10);
  const artifactId = published.result.structuredContent.id;
  const { addFeedback, listForArtifact } = await import("../lib/feedback.js");
  const anchored = addFeedback({
    artifactId, org: "acme", viewerEmail: "viewer@acme.test", body: "This heading", artifactRevision: 3,
    anchor: { path: "html:nth-child(1)>body:nth-child(2)", x: 0.25, y: 0.75 }
  });
  const general = addFeedback({ artifactId, org: "acme", viewerEmail: "viewer@acme.test", body: "General note", artifactRevision: 3 });

  assert.deepEqual(
    { path: anchored.anchor_path, x: anchored.anchor_x, y: anchored.anchor_y, approx: anchored.anchor_approx, revision: anchored.artifact_revision },
    { path: "html:nth-child(1)>body:nth-child(2)", x: 0.25, y: 0.75, approx: 0, revision: 3 }
  );
  assert.deepEqual(
    { path: general.anchor_path, x: general.anchor_x, y: general.anchor_y, approx: general.anchor_approx },
    { path: null, x: null, y: null, approx: 0 }
  );
  // Ordering ties on created_at (1s granularity) then random id, so look the row up by id
  // rather than assuming position 0.
  const listedAnchor = listForArtifact(artifactId).find((f) => f.id === anchored.id);
  assert.ok(listedAnchor, "anchored feedback is listed");
  assert.equal(listedAnchor.anchor_x, 0.25);
});

test("feedback anchor coordinates reject out-of-range values and cap paths", async () => {
  const published = await call("publish_artifact", { html: "<h1>Anchor bounds</h1>" }, 11);
  const artifactId = published.result.structuredContent.id;
  const { addFeedback } = await import("../lib/feedback.js");
  for (const anchor of [{ x: -0.01, y: 0.5 }, { x: 0.5, y: 1.01 }, { x: Infinity, y: 0.5 }]) {
    assert.throws(() => addFeedback({ artifactId, org: "acme", viewerEmail: "viewer@acme.test", body: "Nope", artifactRevision: 1, anchor }), /between 0 and 1/);
  }
  const row = addFeedback({
    artifactId, org: "acme", viewerEmail: "viewer@acme.test", body: "Capped", artifactRevision: 1,
    anchor: { path: "x".repeat(600), x: 0, y: 1, approx: true }
  });
  assert.equal(row.anchor_path.length, 512);
  assert.equal(row.anchor_approx, 1);
});

test("category tools list, set, create, and delete within the caller's org", async () => {
  const orgs = await import("../lib/orgs.js");
  try { orgs.createOrg({ name: "acme" }); } catch { /* already registered by an earlier test */ }

  const created = await call("create_category", { name: "Dashboards" }, 60);
  assert.deepEqual(created.result.structuredContent, { org: "acme", name: "Dashboards" });
  const listed = await call("list_categories", {}, 61);
  assert.ok(listed.result.structuredContent.categories.includes("Dashboards"));

  const pub = await call("publish_artifact", { html: "<h1>cat</h1>" }, 62);
  const id = pub.result.structuredContent.id;
  const moved = await call("set_category", { id, category: "Reports" }, 63);
  assert.equal(moved.result.structuredContent.category, "Reports");
  // set_category auto-registers the category into the org list.
  assert.ok((await call("list_categories", {}, 64)).result.structuredContent.categories.includes("Reports"));

  assert.equal((await call("delete_category", { name: "Dashboards" }, 65)).result.structuredContent.removed, true);
  assert.ok(!(await call("list_categories", {}, 66)).result.structuredContent.categories.includes("Dashboards"));

  // A foreign key (different client) may not recategorize this artifact.
  const denied = await handleMcp({ jsonrpc: "2.0", id: 67, method: "tools/call", params: { name: "set_category", arguments: { id, category: "Nope" } } }, { clientId: "other", org: "acme" });
  assert.equal(denied.result.isError, true);
  assert.equal(denied.result.content[0].text, "Permission denied: this API key cannot modify this artifact");
});
