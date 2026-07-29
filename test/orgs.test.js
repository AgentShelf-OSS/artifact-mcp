import test, { after } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

// orgs.js binds to the default db (like keys.js), so point DATA_DIR at a temp dir first.
const dir = mkdtempSync(path.join(tmpdir(), "artifact-orgs-"));
process.env.DATA_DIR = dir;
const { default: db } = await import("../lib/db.js");
const orgs = await import("../lib/orgs.js");
const auth = await import("../lib/auth.js");

after(() => {
  db.close();
  rmSync(dir, { recursive: true, force: true });
});

test("creating an org with a domain makes it resolvable and listed", () => {
  const created = orgs.createOrg({ name: "acme", label: "Acme Inc", domain: "Acme.TEST" });
  assert.equal(created.name, "acme");
  assert.deepEqual(created.domains, ["acme.test"]); // normalized to lowercase
  assert.equal(orgs.orgForDomain("acme.test"), "acme");
  assert.equal(orgs.orgExists("acme"), true);
  assert.ok(orgs.listOrgNames().includes("acme"));
  const row = orgs.listOrgs().find((o) => o.name === "acme");
  assert.equal(row.label, "Acme Inc");
  assert.deepEqual(row.emails, []);
  assert.deepEqual(row.categories, []);
});

test("explicit email members normalize, resolve, list, and remove", () => {
  const added = orgs.addEmailMember("acme", "  Person@Example.com  ");
  assert.deepEqual(added, { org: "acme", email: "person@example.com" });
  assert.equal(orgs.orgForEmail("PERSON@EXAMPLE.COM"), "acme");
  assert.deepEqual(orgs.listOrgs().find((o) => o.name === "acme").emails, ["person@example.com"]);
  assert.equal(orgs.removeEmailMember("acme", " PERSON@example.com "), true);
  assert.equal(orgs.orgForEmail("person@example.com"), null);
  assert.equal(orgs.removeEmailMember("acme", "person@example.com"), false);
});

test("explicit email members reject invalid, unknown, duplicate, and conflicting ownership", () => {
  orgs.createOrg({ name: "email-one" });
  orgs.createOrg({ name: "email-two" });
  for (const email of ["", "not-an-email", "a@@example.com", "person@localhost", `${"a".repeat(65)}@example.com`, `${"a".repeat(245)}@example.com`]) {
    assert.throws(() => orgs.addEmailMember("email-one", email), /valid email address/i, email);
  }
  assert.throws(() => orgs.addEmailMember("ghost", "person@example.com"), /Unknown organization/);
  orgs.addEmailMember("email-one", "person@example.com");
  assert.throws(() => orgs.addEmailMember("email-one", "PERSON@example.com"), /already on this org/);
  assert.throws(() => orgs.addEmailMember("email-two", "person@example.com"), /already mapped to "email-one"/);
});

test("duplicate orgs, bad domains, reserved names, and taken domains are rejected", () => {
  orgs.createOrg({ name: "beta", domain: "beta.test" });
  assert.throws(() => orgs.createOrg({ name: "beta" }), /already exists/);
  assert.throws(() => orgs.createOrg({ name: "admin" }), /reserved/);
  assert.throws(() => orgs.createOrg({ name: "gamma", domain: "not a domain" }), /valid email domain/);
  // A domain can belong to only one org.
  assert.throws(() => orgs.createOrg({ name: "gamma", domain: "beta.test" }), /already mapped to "beta"/);
});

test("domain-shaped org names are rejected without weakening the implicit-domain fallback", () => {
  assert.throws(
    () => orgs.createOrg({ name: "tenant.example" }),
    { message: 'Org name must not be an email domain. Use a tenant id such as "acme" and add the domain separately.' }
  );
});

test("a legacy domain-named org cannot report a domain removal that leaves implicit access", () => {
  db.prepare("INSERT INTO orgs (name, label) VALUES (?, ?)").run("legacy.example", "Legacy");
  db.prepare("INSERT INTO org_domains (domain, org) VALUES (?, ?)").run("legacy.example", "legacy.example");

  assert.throws(
    () => orgs.removeDomain("legacy.example", "legacy.example"),
    {
      message: 'Cannot remove domain "legacy.example" from organization "legacy.example": implicit domain access would remain. Migrate to a non-domain organization first.'
    }
  );
  assert.equal(orgs.orgForDomain("legacy.example"), "legacy.example");
});

test("domains and categories can be added and removed independently", () => {
  orgs.createOrg({ name: "delta" });
  orgs.addDomain("delta", "delta.test");
  orgs.addDomain("delta", "delta.io");
  assert.throws(() => orgs.addDomain("delta", "delta.test"), /already on this org/);
  assert.equal(orgs.orgForDomain("delta.io"), "delta");

  orgs.addCategory("delta", "Dashboards");
  orgs.addCategory("delta", "Reports");
  orgs.addCategory("delta", "Dashboards"); // INSERT OR IGNORE — no duplicate
  assert.deepEqual(orgs.categoriesFor("delta"), ["Dashboards", "Reports"]);

  assert.equal(orgs.removeDomain("delta", "delta.io"), true);
  assert.equal(orgs.orgForDomain("delta.io"), null);
  assert.equal(orgs.removeCategory("delta", "Reports"), true);
  assert.deepEqual(orgs.categoriesFor("delta"), ["Dashboards"]);
});

test("deleting an org cascades its domains, email members, and categories", () => {
  orgs.createOrg({ name: "epsilon", domain: "epsilon.test" });
  orgs.addEmailMember("epsilon", "member@shared.test");
  orgs.addCategory("epsilon", "Specs");
  assert.equal(orgs.deleteOrg("epsilon"), true);
  assert.equal(orgs.orgExists("epsilon"), false);
  assert.equal(orgs.orgForDomain("epsilon.test"), null);
  assert.equal(orgs.orgForEmail("member@shared.test"), null);
  assert.deepEqual(orgs.categoriesFor("epsilon"), []);
});

test("org deletion refuses owned artifacts, then revokes keys atomically with deletion", () => {
  const secret = "offboard-secret";
  orgs.createOrg({ name: "offboard", domain: "offboard.test" });
  db.prepare("INSERT INTO api_keys (client_id, org, key_hash) VALUES (?, ?, ?)")
    .run("offboard-key", "offboard", auth.sha256Hex(secret));
  db.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES (?, ?, ?, ?)")
    .run("offboard-artifact", "offboard-key", "offboard", "Keep me");

  assert.throws(
    () => orgs.deleteOrg("offboard"),
    { message: 'Cannot delete organization "offboard" while it owns 1 artifact. Move its artifacts to another organization first.' }
  );
  assert.equal(orgs.orgExists("offboard"), true);
  assert.equal(db.prepare("SELECT revoked_at FROM api_keys WHERE client_id = ?").get("offboard-key").revoked_at, null);
  assert.equal(db.prepare("SELECT COUNT(*) AS n FROM artifacts WHERE org = ?").get("offboard").n, 1);
  assert.equal(auth.checkKey({ headers: { authorization: `Bearer ${secret}` } }).ok, true);

  db.prepare("DELETE FROM artifacts WHERE id = ?").run("offboard-artifact");
  assert.equal(orgs.deleteOrg("offboard"), true);
  assert.equal(orgs.orgExists("offboard"), false);
  assert.ok(db.prepare("SELECT revoked_at FROM api_keys WHERE client_id = ?").get("offboard-key").revoked_at);
  assert.deepEqual(auth.checkKey({ headers: { authorization: `Bearer ${secret}` } }), { ok: false });
});

test("org deletion rolls key revocation back when the registry delete fails", () => {
  const secret = "rollback-secret";
  orgs.createOrg({ name: "rollback-org" });
  db.prepare("INSERT INTO api_keys (client_id, org, key_hash) VALUES (?, ?, ?)")
    .run("rollback-key", "rollback-org", auth.sha256Hex(secret));
  db.exec(`
    CREATE TRIGGER block_rollback_org_delete
    BEFORE DELETE ON orgs WHEN OLD.name = 'rollback-org'
    BEGIN SELECT RAISE(ABORT, 'blocked delete'); END
  `);

  assert.throws(() => orgs.deleteOrg("rollback-org"), /blocked delete/);
  assert.equal(orgs.orgExists("rollback-org"), true);
  assert.equal(db.prepare("SELECT revoked_at FROM api_keys WHERE client_id = ?").get("rollback-key").revoked_at, null);
  assert.equal(auth.checkKey({ headers: { authorization: `Bearer ${secret}` } }).ok, true);
  db.exec("DROP TRIGGER block_rollback_org_delete");
});

test("org names are case-folded so a tenant cannot be split by casing", () => {
  const created = orgs.createOrg({ name: "MixedCase", domain: "mixed.test" });
  assert.equal(created.name, "mixedcase");
  assert.equal(orgs.orgExists("mixedcase"), true);
  assert.equal(orgs.orgForDomain("mixed.test"), "mixedcase");
  assert.throws(() => orgs.createOrg({ name: "MIXEDCASE" }), /already exists/);
});

test("adding a domain or category to an unknown org is rejected", () => {
  assert.throws(() => orgs.addDomain("ghost", "ghost.test"), /Unknown organization/);
  assert.throws(() => orgs.addCategory("ghost", "X"), /Unknown organization/);
});

test("org color: set hex, clear, reject invalid, and expose via colorMap", () => {
  orgs.createOrg({ name: "hued" });
  assert.deepEqual(orgs.setColor("hued", "#356B9F"), { name: "hued", color: "#356B9F" });
  assert.equal(orgs.colorMap().hued, "#356B9F");
  assert.equal(orgs.listOrgs().find((o) => o.name === "hued").color, "#356B9F");
  assert.throws(() => orgs.setColor("hued", "blue"), /hex/);
  assert.deepEqual(orgs.setColor("hued", ""), { name: "hued", color: null }); // clear -> derived
  assert.equal(orgs.colorMap().hued, null);
  assert.throws(() => orgs.setColor("ghost", "#000000"), /Unknown organization/);
});
