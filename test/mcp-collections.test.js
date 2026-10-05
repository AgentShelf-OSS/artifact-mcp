import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman

const dataDir = mkdtempSync(join(tmpdir(), "artifact-mcp-collections-rpc-"));
process.env.DATA_DIR = dataDir;
process.env.AUDIT_LEDGER_HMAC_KEY = Buffer.alloc(32, 19).toString("base64");

const { handleMcp } = await import("../lib/mcp.js");
const { createCollectionService } = await import("../lib/collections.js");
const dbModule = await import("../lib/db.js");
const artifacts = await import("../lib/store.js");
const { createOrg } = await import("../lib/orgs.js");

const db = dbModule.default;
const org = "mcp-collections";
createOrg({ name: org });
const auditEvents = [];
const audit = { appendInTransaction(context, event) { auditEvents.push({ context, event }); } };
const collections = createCollectionService({ db, artifacts, audit });

const author = { clientId: "folder-author", org, role: "author", authType: "api_key", ownerEmail: "owner@example.test" };
const collaborator = { clientId: "folder-collaborator", org, role: "collaborator", authType: "api_key" };
const reader = { clientId: "folder-reader", org, role: "reader", authType: "api_key" };
const admin = { clientId: "folder-admin", org: "admin", role: "collaborator", authType: "api_key" };

test.after(() => rmSync(dataDir, { recursive: true, force: true }));

async function call(name, arguments_, auth = author, options = {}) {
  const response = await handleMcp({ jsonrpc: "2.0", id: "client-request-id", method: "tools/call", params: { name, arguments: arguments_ } }, auth, { collections, ...options });
  if (response.result?.isError) throw new Error(response.result.content?.[0]?.text || "tool failed");
  return response.result.structuredContent;
}

async function callError(name, arguments_, auth = author, options = {}) {
  const response = await handleMcp({ jsonrpc: "2.0", id: "client-request-id", method: "tools/call", params: { name, arguments: arguments_ } }, auth, { collections, ...options });
  assert.equal(response.result?.isError, true);
  return response.result.content?.[0]?.text || "";
}

async function publish(auth, title, html = `<h1>${title}</h1>`) {
  return (await call("publish_artifact", { title, html }, auth)).id;
}

test("MCP collection tools complete the folder workflow and preserve artifacts", async () => {
  const id = await publish(author, "Folder member");
  const created = await call("create_collection", {
    name: "Alpha folder", description: "A description", color: "#123456", artifact_ids: [id], cover_artifact_id: id
  });
  assert.equal(created.collection.artifact_count, 1);
  assert.equal(created.collection.editable, true);
  assert.equal(created.collection.description, "A description");
  const collectionId = created.collection.id;
  const listed = await call("list_collections", {});
  assert.equal(listed.collections[0].name, "Alpha folder");
  const inspected = await call("get_collection", { id: collectionId });
  assert.deepEqual(inspected.artifacts.map((item) => item.id), [id]);
  const updated = await call("update_collection", { id: collectionId, description: "", color: "", cover_artifact_id: "" });
  assert.equal(updated.collection.description, "");
  assert.equal(updated.collection.color, "");
  assert.equal(updated.collection.cover_artifact_id, "");
  const preserved = await call("update_collection", { id: collectionId, name: "Renamed folder" });
  assert.equal(preserved.collection.name, "Renamed folder");
  const before = artifacts.getArtifactMeta(id);
  const deleted = await call("delete_collection", { id: collectionId });
  assert.deepEqual(deleted, { id: collectionId, deleted: true });
  assert.deepEqual(artifacts.getArtifactMeta(id), before);
  assert.equal(db.prepare("SELECT COUNT(*) FROM collection_artifacts WHERE artifact_id = ?").pluck().get(id), 0);
  assert.equal(db.prepare("SELECT COUNT(*) FROM gallery_preferences").pluck().get(), 0);
});

test("MCP collection writes enforce ownership, roles, organization pinning, and atomic batches", async () => {
  const first = await publish(author, "Atomic one");
  const second = await publish(author, "Atomic two");
  const folder = await call("create_collection", { name: "Ownership folder", artifact_ids: [first] });
  assert.match(await callError("update_collection", { id: folder.collection.id, name: "Nope" }, collaborator), /Only the collection creator/);
  assert.match(await callError("update_collection", { id: folder.collection.id, name: "Nope" }, reader), /Missing required|Only the collection creator|cannot manage collections/);
  assert.match(await callError("create_collection", { name: "Reader folder" }, reader), /cannot manage collections/);
  assert.match(await callError("add_artifacts_to_collection", { id: folder.collection.id, artifact_ids: [second, "foreign-artifact"] }), /Not found|Unknown artifact/);
  assert.equal((await call("get_collection", { id: folder.collection.id })).collection.artifact_count, 1);
  assert.match(await callError("list_collections", { org: "other-org" }), /Not found/);
  const administered = await call("update_collection", { org, id: folder.collection.id, name: "Admin renamed" }, admin);
  assert.equal(administered.collection.name, "Admin renamed");
  assert.match(await callError("get_collection", { org: "all", id: folder.collection.id }, admin), /org is required/);
});

test("MCP collection principals do not inherit owner email and compare service IDs exactly", async () => {
  const service = await call("create_collection", { name: "Service principal" }, author);
  const row = db.prepare("SELECT created_by, created_by_kind FROM collections WHERE id = ?").get(service.collection.id);
  assert.deepEqual(row, { created_by: author.clientId, created_by_kind: "api_key" });
  const caseVariant = { ...author, clientId: "FOLDER-AUTHOR" };
  assert.match(await callError("update_collection", { id: service.collection.id, name: "Spoof" }, caseVariant), /Only the collection creator/);
  const issuerA = { clientId: "same-client", issuer: "https://issuer-a.example", org, role: "author", authType: "oauth", scopes: new Set(["artifacts:read", "artifacts:publish"]) };
  const issuerB = { ...issuerA, issuer: "https://issuer-b.example" };
  const oauthFolder = await call("create_collection", { name: "OAuth principal" }, issuerA);
  assert.match(await callError("update_collection", { id: oauthFolder.collection.id, name: "Spoof issuer" }, issuerB), /Only the collection creator/);
  assert.match(await callError("create_collection", { name: "No OAuth scope" }, { ...issuerA, scopes: new Set(["artifacts:read"]) }), /Missing required scope/);
  assert.match(await callError("create_collection", { name: "No OAuth issuer" }, { ...issuerA, issuer: "" }), /OAuth issuer unavailable/);
});

test("MCP collection cursors, limits, and audit context are bounded and server-derived", async () => {
  for (const name of ["One", "Two", "Three"]) await call("create_collection", { name }, author, { requestId: "server-request-id" });
  assert.match(await callError("list_collections", { limit: 0 }), /between 1 and 100/);
  assert.match(await callError("list_collections", { limit: 101 }), /between 1 and 100/);
  assert.match(await callError("list_collections", { cursor: "not-a-cursor" }), /Invalid collection cursor/);
  const page = await call("list_collections", { limit: 1 }, author, { requestId: "server-request-id" });
  assert.equal(page.collections.length, 1);
  assert.ok(page.next_cursor);
  const pageTwo = await call("list_collections", { limit: 1, cursor: page.next_cursor }, author, { requestId: "server-request-id" });
  assert.equal(pageTwo.collections.length, 1);
  assert.match(await callError("list_collections", { limit: 1, cursor: page.next_cursor }, { ...author, clientId: "other-client" }), /Invalid collection cursor/);
  const event = auditEvents.find(({ context, event: item }) => item.operation === "collection.create" && context.requestId === "server-request-id");
  assert.equal(event.context.requestId, "server-request-id");
  assert.notEqual(event.context.requestId, "client-request-id");
  assert.equal(event.context.source, "mcp");
});

test("registered collection tools require read or publish scopes for OAuth callers", async () => {
  const oauth = {
    clientId: "scope-client", issuer: "https://issuer.example.test", org,
    role: "collaborator", authType: "oauth", scopes: new Set(["artifacts:publish"])
  };
  const folder = await call("create_collection", { name: "Scope matrix folder" }, oauth);
  const argsByTool = {
    list_collections: {}, get_collection: { id: folder.collection.id },
    create_collection: { name: "Forbidden scope folder" },
    update_collection: { id: folder.collection.id, description: "forbidden" },
    delete_collection: { id: folder.collection.id },
    add_artifacts_to_collection: { id: folder.collection.id, artifact_ids: [] },
    remove_artifacts_from_collection: { id: folder.collection.id, artifact_ids: [] }
  };
  for (const [name, args] of Object.entries(argsByTool)) {
    const required = ["list_collections", "get_collection"].includes(name) ? "artifacts:read" : "artifacts:publish";
    for (const scopes of [new Set(), new Set(["artifacts:delete"])]) {
      assert.equal(await callError(name, args, { ...oauth, scopes }), `Missing required scope: ${required}`);
    }
  }
  const readOnly = { ...oauth, scopes: new Set(["artifacts:read"]) };
  assert.ok((await call("list_collections", {}, readOnly)).collections.some((c) => c.id === folder.collection.id));
  assert.equal((await call("get_collection", { id: folder.collection.id }, readOnly)).collection.description, "");
  assert.equal((await call("get_collection", { id: folder.collection.id }, readOnly)).collection.editable, false);
  assert.equal((await call("list_collections", {}, readOnly)).collections.find((c) => c.id === folder.collection.id).editable, false);
  assert.equal(db.prepare("SELECT COUNT(*) FROM collection_artifacts WHERE collection_id=?").pluck().get(folder.collection.id), 0);
  await call("delete_collection", { id: folder.collection.id }, oauth);
});

test("MCP collection retries, duplicate names, legacy ownership, and key rotation stay stable", async () => {
  const first = await publish(author, "Retry member");
  const folder = await call("create_collection", { name: "Retry folder", artifact_ids: [first], cover_artifact_id: first });
  assert.match(await callError("create_collection", { name: "Retry folder" }), /already exists|duplicate/i);
  const retained = await call("update_collection", { id: folder.collection.id, description: "metadata only" });
  assert.equal(retained.collection.cover_artifact_id, first);
  const add = await call("add_artifacts_to_collection", { id: folder.collection.id, artifact_ids: [first, first] });
  assert.deepEqual(add.added, []);
  assert.deepEqual(add.already_present, [first]);
  const remove = await call("remove_artifacts_from_collection", { id: folder.collection.id, artifact_ids: [first, first] });
  assert.deepEqual(remove.removed, [first]);
  assert.deepEqual(remove.already_absent, []);
  const removeAgain = await call("remove_artifacts_from_collection", { id: folder.collection.id, artifact_ids: [first, first] });
  assert.deepEqual(removeAgain.removed, []);
  assert.deepEqual(removeAgain.already_absent, [first]);

  const browserFolder = collections.create({ email: "owner@example.test", org, isAdmin: false }, { org, name: "Browser folder" });
  assert.match(await callError("update_collection", { id: browserFolder.id, name: "Service cannot edit" }, author), /Only the collection creator/);
  assert.match(await callError("update_collection", { id: browserFolder.id, name: "Case cannot edit" }, { ...author, clientId: "owner@example.test" }), /Only the collection creator/);
  const rotated = await call("update_collection", { id: folder.collection.id, name: "Rotated stable" }, { ...author, label: "rotated secret" });
  assert.equal(rotated.collection.name, "Rotated stable");
  assert.match(await callError("update_collection", { id: folder.collection.id, name: "Different key" }, { ...author, clientId: "new-folder-key" }), /Only the collection creator/);
});

test("MCP collection visibility follows publisher ownership and OAuth folder ownership", async () => {
  const hiddenId = await publish(author, "Private author artifact");
  await call("set_visibility", { id: hiddenId, hidden: true });
  const hiddenFolder = await call("create_collection", { name: "Private folder", artifact_ids: [hiddenId] });
  assert.equal((await call("get_collection", { id: hiddenFolder.collection.id })).artifacts.length, 1);
  const foreignOrg = "mcp-foreign";
  createOrg({ name: foreignOrg });
  const foreign = await publish({ ...author, clientId: "foreign-client", org: foreignOrg }, "Foreign artifact");
  assert.match(await callError("add_artifacts_to_collection", { id: hiddenFolder.collection.id, artifact_ids: [foreign] }), /Not found|Unknown artifact/);

  const issuerA = { clientId: "oauth-member", issuer: "https://issuer-a.example", org, role: "author", authType: "oauth", scopes: new Set(["artifacts:read", "artifacts:publish"]) };
  const issuerB = { ...issuerA, issuer: "https://issuer-b.example" };
  const oauthArtifact = await publish(issuerA, "OAuth private artifact");
  const oauthFolder = await call("create_collection", { name: "OAuth visibility", artifact_ids: [oauthArtifact] }, issuerA);
  const foreignIssuerView = await call("get_collection", { id: oauthFolder.collection.id }, issuerB);
  assert.equal(foreignIssuerView.artifacts.length, 1);

  const adminFolder = await call("create_collection", { org, name: "Admin shared folder", artifact_ids: [hiddenId] }, admin);
  const authorView = await call("get_collection", { id: adminFolder.collection.id }, author);
  assert.equal(authorView.artifacts.length, 1);
  assert.equal(authorView.collection.editable, false);
  const sameReader = await call("get_collection", { id: adminFolder.collection.id }, { ...author, role: "reader" });
  assert.equal(sameReader.collection.editable, false);
});

test("folder metadata redacts members and covers after publisher read access narrows", async () => {
  const id = await publish(author, "Hidden shared reference");
  await call("set_visibility", { id, hidden: true });
  const folder = await call("create_collection", {
    name: "Redacted shared folder", artifact_ids: [id], cover_artifact_id: id
  }, collaborator);
  const collectionId = folder.collection.id;
  assert.equal(folder.collection.artifact_count, 1);
  assert.equal(folder.collection.cover_artifact_id, id);
  const narrowed = { ...collaborator, role: "author" };
  const updated = await call("update_collection", { id: collectionId, description: "metadata only" }, narrowed);
  const listed = (await call("list_collections", {}, narrowed)).collections.find((c) => c.id === collectionId);
  const inspected = await call("get_collection", { id: collectionId }, narrowed);
  assert.match(await callError("create_collection", { name: "Unreadable cover", artifact_ids: [id], cover_artifact_id: id }, narrowed), /Not found/);
  for (const summary of [updated.collection, listed, inspected.collection]) {
    assert.equal(summary.artifact_count, 0);
    assert.equal(summary.cover_artifact_id, "");
  }
  assert.deepEqual(inspected.artifacts, []);
  const adminView = await call("get_collection", { org, id: collectionId }, admin);
  assert.equal(adminView.collection.artifact_count, 1);
  assert.equal(adminView.collection.cover_artifact_id, id);
});

test("MCP collection caps and cursor context reject oversized and tampered requests", async () => {
  const bulkIds = [];
  const insertArtifact = db.prepare("INSERT INTO artifacts (id,client_id,org,title,owner_email,hidden) VALUES (?,?,?,?,?,0)");
  for (let i = 0; i < 101; i += 1) {
    const id = `bulk${String(i).padStart(3, "0")}`;
    insertArtifact.run(id, author.clientId, org, `Bulk ${i}`, "owner@example.test");
    bulkIds.push(id);
  }
  const folder = await call("create_collection", { name: "Capacity folder" });
  assert.match(await callError("add_artifacts_to_collection", { id: folder.collection.id, artifact_ids: bulkIds }), /at most 100|Invalid arguments/);
  const memberIds = [];
  for (let i = 0; i < 1000; i += 1) {
    const id = `member${String(i).padStart(4, "0")}`;
    insertArtifact.run(id, author.clientId, org, `Member ${i}`, "owner@example.test");
    memberIds.push(id);
  }
  const insertMember = db.prepare("INSERT INTO collection_artifacts (collection_id,artifact_id,org) VALUES (?,?,?)");
  for (const id of memberIds) insertMember.run(folder.collection.id, id, org);
  assert.equal(artifacts.getArtifactMeta(bulkIds[0])?.client_id, author.clientId);
  assert.match(await callError("add_artifacts_to_collection", { id: folder.collection.id, artifact_ids: [bulkIds[0]] }), /member limit/);
  const capacityOrg = "mcp-capacity";
  createOrg({ name: capacityOrg });
  const capacityAuth = { ...author, org: capacityOrg };
  for (let i = 0; i < 200; i += 1) {
    const id = `capacity-${i}`;
    db.prepare("INSERT INTO collections (id,org,name,name_key,description,color,created_by,created_by_kind) VALUES (?,?,?,?,?,?,?,?)")
      .run(id, capacityOrg, `Capacity ${i}`, `capacity ${i}`, "", null, capacityAuth.clientId, "api_key");
  }
  assert.match(await callError("create_collection", { name: "Over collection cap" }, capacityAuth), /collection limit/);

  const page = await call("list_collections", { limit: 1 });
  const decoded = JSON.parse(Buffer.from(page.next_cursor, "base64url").toString("utf8"));
  decoded[9] = 1001;
  const tamperedOffset = Buffer.from(JSON.stringify(decoded)).toString("base64url");
  assert.match(await callError("list_collections", { limit: 1, cursor: tamperedOffset }), /Invalid collection cursor/);
  const tamperedRole = JSON.parse(Buffer.from(page.next_cursor, "base64url").toString("utf8"));
  tamperedRole[6] = "reader";
  assert.match(await callError("list_collections", { limit: 1, cursor: Buffer.from(JSON.stringify(tamperedRole)).toString("base64url") }), /Invalid collection cursor/);
  const getPage = await call("get_collection", { id: folder.collection.id, limit: 1 });
  const tamperedFolder = JSON.parse(Buffer.from(getPage.next_cursor, "base64url").toString("utf8"));
  tamperedFolder[7] = "other-folder";
  assert.match(await callError("get_collection", { id: folder.collection.id, limit: 1, cursor: Buffer.from(JSON.stringify(tamperedFolder)).toString("base64url") }), /Invalid collection cursor/);
});
