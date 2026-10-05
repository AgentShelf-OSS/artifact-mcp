import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { migrateDatabase } from "../lib/migrations.js";
import { createAuditLedger } from "../lib/audit.js";
import { createCollectionService } from "../lib/collections.js";

function fixture() {
  const db = new Database(":memory:");
  db.pragma("foreign_keys = ON"); migrateDatabase(db);
  db.prepare("INSERT INTO orgs (name) VALUES ('acme'), ('beta')").run();
  const insert = db.prepare("INSERT INTO artifacts (id,client_id,org,title,owner_email,hidden) VALUES (?,?,?,?,?,?)");
  insert.run("one", "client", "acme", "One", "alice@acme.test", 0);
  insert.run("two", "client", "acme", "Two", "bob@acme.test", 0);
  insert.run("secret", "client", "acme", "Secret", "bob@acme.test", 1);
  insert.run("foreign", "client", "beta", "Foreign", "other@beta.test", 0);
  const artifacts = {
    getArtifactMeta: (id) => db.prepare("SELECT * FROM artifacts WHERE id = ?").get(id) || null,
    listOrgArtifacts: (org, { includeHidden = false, ownerEmail = null } = {}) => db.prepare(
      "SELECT * FROM artifacts WHERE org = ? AND (hidden = 0 OR ? = 1 OR owner_email = ?)"
    ).all(org, includeHidden ? 1 : 0, ownerEmail)
  };
  return { db, artifacts, service: createCollectionService({ db, artifacts }) };
}

test("collection service scopes reads, permits shared visible artifacts, and conceals foreign targets", () => {
  const { db, service } = fixture();
  try {
    const alice = { email: "alice@acme.test", org: "acme", isAdmin: false };
    const row = service.create(alice, { org: "acme", name: "Design", artifactIds: ["one"] });
    assert.equal(row.artifactCount, 1);
    assert.equal(service.addMemberships(alice, row.id, { artifactIds: ["two"] }, "acme").added[0], "two");
    assert.deepEqual(service.list(alice, "acme").collections[0].artifactIds, ["one", "two"]);
    assert.throws(() => service.addMemberships(alice, row.id, { artifactIds: ["foreign"] }, "acme"), (error) => error.status === 404);
    assert.throws(() => service.update({ email: "bob@acme.test", org: "acme", isAdmin: false }, row.id, { org: "acme", name: "Nope" }), (error) => error.status === 403);
    assert.equal(service.list({ email: "admin@example.test", org: "admin", isAdmin: true }, "beta").collections.length, 0);
    db.prepare("UPDATE artifacts SET org='beta' WHERE id='two'").run();
    assert.equal(db.prepare("SELECT COUNT(*) FROM collection_artifacts WHERE artifact_id='two'").pluck().get(), 0);
  } finally { db.close(); }
});

test("collection mutations validate the whole membership batch before writing and preferences prune stale ids", () => {
  const { db, service } = fixture();
  try {
    const alice = { email: "alice@acme.test", org: "acme", isAdmin: false };
    const row = service.create(alice, { org: "acme", name: "Research" });
    assert.throws(() => service.addMemberships(alice, row.id, { artifactIds: ["one", "foreign"] }, "acme"));
    assert.equal(db.prepare("SELECT COUNT(*) FROM collection_artifacts").pluck().get(), 0);
    const pref = service.setPreferences(alice, { view: "ribbons", collectionOrderByOrg: { acme: [row.id, "stale"] } }, "acme");
    assert.equal(pref.view, "ribbons"); assert.deepEqual(pref.collectionOrderByOrg.acme, [row.id]);
    assert.equal(service.getPreferences({ email: "other@acme.test", org: "acme", isAdmin: false }, "acme").view, "reel");
  } finally { db.close(); }
});

test("administrator projections support all organizations without leaking unknown preference keys", () => {
  const { db, service } = fixture();
  try {
    const admin = { email: "admin@example.test", org: "admin", isAdmin: true };
    service.create(admin, { org: "acme", name: "Admin folder", artifactIds: ["one"] });
    const all = service.list(admin, "all");
    assert.equal(all.collections.length, 1);
    assert.equal(all.uncollectedCount, 3);
    const prefs = service.setPreferences(admin, { collectionOrderByOrg: { acme: [all.collections[0].id], beta: ["foreign"] } }, "acme");
    assert.deepEqual(Object.keys(prefs.collectionOrderByOrg), ["acme"]);
  } finally { db.close(); }
});

test("administrator all-organization order and collapse state survives reload while members cannot use all", () => {
  const { db, service } = fixture();
  try {
    const admin = { email: "admin@example.test", org: "admin", isAdmin: true };
    const collection = service.create(admin, { org: "acme", name: "All view", artifactIds: ["one"] });
    const saved = service.setPreferences(admin, {
      collectionOrderByOrg: { all: [collection.id] },
      collapsedCollectionIdsByOrg: { all: [collection.id] }
    });
    assert.deepEqual(saved.collectionOrderByOrg.all, [collection.id]);
    assert.deepEqual(service.getPreferences(admin).collapsedCollectionIdsByOrg.all, [collection.id]);
    const member = { email: "alice@acme.test", org: "acme", isAdmin: false };
    const pruned = service.setPreferences(member, { collectionOrderByOrg: { all: [collection.id] } }, "acme");
    assert.deepEqual(pruned.collectionOrderByOrg, {});
  } finally { db.close(); }
});


test("collection audit uses the target tenant and a ledger failure rolls the mutation back", () => {
  const { db, artifacts } = fixture();
  try {
    const audit = createAuditLedger({ db, hmacKey: Buffer.alloc(32, 7).toString("base64") });
    const service = createCollectionService({ db, artifacts, audit });
    const admin = { email: "admin@example.test", org: "admin", isAdmin: true };
    const row = service.create(admin, { org: "acme", name: "Audited" });
    const event = db.prepare("SELECT tenant,operation,target_id FROM security_audit_events ORDER BY sequence DESC LIMIT 1").get();
    assert.deepEqual(event, {tenant:"acme",operation:"collection.create",target_id:row.id});
    assert.equal(audit.verify().ok, true);
    const failing = createCollectionService({ db, artifacts, audit: { appendInTransaction() { throw new Error("ledger unavailable"); } } });
    assert.throws(() => failing.addMemberships(admin, row.id, { artifactIds: ["one"] }, "acme"), /ledger unavailable/);
    assert.equal(db.prepare("SELECT count(*) FROM collection_artifacts WHERE collection_id=?").pluck().get(row.id), 0);
    assert.throws(() => failing.remove(admin, row.id, "acme"), /ledger unavailable/);
    assert.ok(db.prepare("SELECT id FROM collections WHERE id=?").get(row.id));
  } finally { db.close(); }
});

test("covers follow memberships and artifact moves without changing artifact contents", () => {
  const { db, service } = fixture();
  try {
    const alice = { email:"alice@acme.test",org:"acme",isAdmin:false };
    const first=service.create(alice,{org:"acme",name:"First",artifactIds:["one"],coverArtifactId:"one"});
    const second=service.create(alice,{org:"acme",name:"Second",artifactIds:["one"]});
    service.removeMemberships(alice,first.id,{artifactIds:["one"]},"acme");
    assert.equal(service.list(alice,"acme").collections.find(c=>c.id===first.id).coverArtifactId,null);
    assert.deepEqual(service.list(alice,"acme").collections.find(c=>c.id===second.id).artifactIds,["one"]);
    service.update(alice,second.id,{org:"acme",coverArtifactId:"one"});
    db.prepare("UPDATE artifacts SET org='beta' WHERE id='one'").run();
    assert.equal(db.prepare("SELECT count(*) FROM collection_artifacts WHERE artifact_id='one'").pluck().get(),0);
    assert.equal(db.prepare("SELECT cover_artifact_id FROM collections WHERE id=?").pluck().get(second.id),null);
    assert.equal(db.prepare("SELECT title FROM artifacts WHERE id='one'").pluck().get(),"One");
  } finally { db.close(); }
});
