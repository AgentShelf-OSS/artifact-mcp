import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { Duplex } from "node:stream";
import { IncomingMessage, ServerResponse } from "node:http";
import { migrateDatabase } from "../lib/migrations.js";
import { createCollectionService } from "../lib/collections.js";
import { createApp } from "../lib/app.js";

function invoke(app, method, url, body, headers = {}) {
  const payload = headers["test-raw-json"] !== undefined ? headers["test-raw-json"] : body === undefined ? "" : JSON.stringify(body);
  const socket = new Duplex({ read() {}, write(_chunk, _encoding, callback) { callback(); } });
  const req = new IncomingMessage(socket); req.method = method; req.url = url;
  req.headers = { cookie: "session", "content-type": "application/json", "content-length": String(Buffer.byteLength(payload)), "x-artifact-mutation": "1", "sec-fetch-site": "same-origin", ...headers };
  const res = new ServerResponse(req);
  return new Promise((resolve, reject) => {
    res.end = (chunk) => { let parsed; try { parsed = JSON.parse(chunk?.toString() || ""); } catch { parsed = chunk?.toString() || undefined; } resolve({ status: res.statusCode, body: parsed }); socket.destroy(); return res; };
    req.on("error", reject); if (payload) req.push(payload); req.push(null); app.handle(req, res, reject);
  });
}

function fixture() {
  const db = new Database(":memory:"); db.pragma("foreign_keys = ON"); migrateDatabase(db);
  db.prepare("INSERT INTO orgs(name) VALUES ('acme')").run();
  db.prepare("INSERT INTO artifacts(id,client_id,org,title,owner_email) VALUES ('one','c','acme','One','alice@acme.test')").run();
  const artifacts = { getArtifactMeta: (id) => db.prepare("SELECT * FROM artifacts WHERE id=?").get(id) || null, listOrgArtifacts: (org, { includeHidden = false, ownerEmail = null } = {}) => db.prepare("SELECT * FROM artifacts WHERE org=? AND (hidden=0 OR ?=1 OR owner_email=?)").all(org, includeHidden ? 1 : 0, ownerEmail) };
  const collections = createCollectionService({ db, artifacts });
  const app = createApp({
    collections,
    resolveViewer: async (req) => ({ email: req.headers["test-email"] || "alice@acme.test", org: "acme", isAdmin: req.headers["test-admin"] === "1" }),
    artifacts,
    pages: { notFound: () => "not found", notSignedIn: () => "not signed in", gallery: () => "", shell: () => "", settings: () => "" },
    logger: { error() {}, warn() {}, info() {} }
  });
  return { app, db };
}

test("collection HTTP routes return projections and enforce same-origin mutations", async () => {
  const { app, db } = fixture();
  try {
    let result = await invoke(app, "POST", "/collections", { org: "acme", name: "Design", artifactIds: ["one"] });
    assert.equal(result.status, 201); const id = result.body.id;
    result = await invoke(app, "GET", "/collections?org=acme"); assert.equal(result.status, 200); assert.equal(result.body.collections[0].artifactIds[0], "one");
    result = await invoke(app, "POST", `/collections/${id}/memberships`, { artifactIds: ["one"] }, { "sec-fetch-site": "cross-site" }); assert.equal(result.status, 403);
    result = await invoke(app, "PUT", "/gallery/preferences?org=acme", { view: "ribbons" }); assert.equal(result.status, 200); assert.equal(result.body.view, "ribbons");
    result = await invoke(app, "DELETE", `/collections/${id}?org=acme`); assert.deepEqual(result.body, { id, deleted: true });
  } finally { db.close(); }
});


test("collection HTTP rejects malformed JSON and unsupported mutation queries with stable errors", async () => {
  const {app,db}=fixture();
  try {
    let result=await invoke(app,"POST","/collections",undefined,{"test-raw-json":"{"});
    assert.equal(result.status,400);assert.equal(result.body.error,"invalid_body");
    result=await invoke(app,"POST","/collections?include=no",{org:"acme",name:"Invalid"});
    assert.equal(result.status,400);assert.equal(result.body.error,"invalid_query");
    result=await invoke(app,"POST","/collections",{org:"acme",name:"Valid"});
    const id=result.body.id;
    result=await invoke(app,"POST",`/collections/${id}/memberships?unexpected=1`,{artifactIds:["one"]});
    assert.equal(result.status,400);assert.equal(result.body.error,"invalid_query");
    assert.equal(db.prepare("SELECT count(*) FROM collection_artifacts").pluck().get(),0);
  } finally {db.close();}
});
