import test from "node:test";
import assert from "node:assert/strict";
import {
  artifactAccess,
  adminAccess,
  publisherCanDeleteArtifact,
  publisherCanReadArtifact,
  publisherCanWriteArtifact,
  viewerCanManageArtifact
} from "../lib/access.js";

test("artifact access expresses tenant and concealment policy consistently", () => {
  const artifact = { id: "abc123", org: "acme" };

  assert.deepEqual(artifactAccess({ email: "admin@example.com", isAdmin: true, org: "admin" }, artifact), { ok: true });
  assert.deepEqual(artifactAccess({ email: "a@acme.test", isAdmin: false, org: "acme" }, artifact), { ok: true });
  assert.deepEqual(artifactAccess({ email: null, isAdmin: false, org: null }, artifact), { ok: false, status: 401, error: "Not signed in" });
  assert.deepEqual(artifactAccess({ email: "b@other.test", isAdmin: false, org: "other" }, artifact), { ok: false, status: 403, error: "Forbidden" });
  assert.deepEqual(artifactAccess({ email: "b@other.test", isAdmin: false, org: "other" }, artifact, { conceal: true }), { ok: false, status: 404, error: "Not found" });
});

test("admin access distinguishes unsigned and non-admin viewers", () => {
  assert.deepEqual(adminAccess({ email: null, isAdmin: false }), { ok: false, status: 403, error: "Not signed in" });
  assert.deepEqual(adminAccess({ email: "member@example.com", isAdmin: false }), { ok: false, status: 403, error: "Admins only" });
  assert.deepEqual(adminAccess({ email: "admin@example.com", isAdmin: true }), { ok: true });
});

test("human artifact management is limited to administrators and recorded owners", () => {
  const owned = { id: "abc123", org: "acme", owner_email: "owner@acme.test" };
  const legacy = { id: "legacy123", org: "acme", owner_email: null };

  assert.equal(
    viewerCanManageArtifact({ email: "OWNER@ACME.TEST", org: "acme", isAdmin: false }, owned),
    true,
  );
  assert.equal(
    viewerCanManageArtifact({ email: "member@acme.test", org: "acme", isAdmin: false }, owned),
    false,
  );
  assert.equal(
    viewerCanManageArtifact({ email: "owner@acme.test", org: "other", isAdmin: false }, owned),
    false,
  );
  assert.equal(
    viewerCanManageArtifact({ email: "owner@acme.test", org: "acme", isAdmin: false }, legacy),
    false,
  );
  assert.equal(
    viewerCanManageArtifact({ email: "admin@example.com", org: "admin", isAdmin: true }, legacy),
    true,
  );
});

test("publisher capabilities distinguish own, same-org, and cross-org artifacts", () => {
  const own = { client_id: "key-a", org: "acme" };
  const colleague = { client_id: "key-b", org: "acme" };
  const foreign = { client_id: "key-c", org: "globex" };
  const auth = (role) => ({ clientId: "key-a", org: "acme", role });

  assert.deepEqual(
    [publisherCanReadArtifact(auth("reader"), own), publisherCanReadArtifact(auth("reader"), colleague), publisherCanReadArtifact(auth("reader"), foreign)],
    [true, true, false]
  );
  assert.deepEqual(
    [publisherCanWriteArtifact(auth("reader"), own), publisherCanWriteArtifact(auth("reader"), colleague), publisherCanWriteArtifact(auth("reader"), foreign)],
    [false, false, false]
  );
  assert.deepEqual(
    [publisherCanDeleteArtifact(auth("reader"), own), publisherCanDeleteArtifact(auth("reader"), colleague), publisherCanDeleteArtifact(auth("reader"), foreign)],
    [false, false, false]
  );

  assert.deepEqual(
    [publisherCanReadArtifact(auth("author"), own), publisherCanReadArtifact(auth("author"), colleague), publisherCanReadArtifact(auth("author"), foreign)],
    [true, false, false]
  );
  assert.deepEqual(
    [publisherCanWriteArtifact(auth("author"), own), publisherCanWriteArtifact(auth("author"), colleague), publisherCanWriteArtifact(auth("author"), foreign)],
    [true, false, false]
  );
  assert.deepEqual(
    [publisherCanDeleteArtifact(auth("author"), own), publisherCanDeleteArtifact(auth("author"), colleague), publisherCanDeleteArtifact(auth("author"), foreign)],
    [true, false, false]
  );

  assert.deepEqual(
    [publisherCanReadArtifact(auth("collaborator"), own), publisherCanReadArtifact(auth("collaborator"), colleague), publisherCanReadArtifact(auth("collaborator"), foreign)],
    [true, true, false]
  );
  assert.deepEqual(
    [publisherCanWriteArtifact(auth("collaborator"), own), publisherCanWriteArtifact(auth("collaborator"), colleague), publisherCanWriteArtifact(auth("collaborator"), foreign)],
    [true, true, false]
  );
  assert.deepEqual(
    [publisherCanDeleteArtifact(auth("collaborator"), own), publisherCanDeleteArtifact(auth("collaborator"), colleague), publisherCanDeleteArtifact(auth("collaborator"), foreign)],
    [true, false, false]
  );

  const admin = { clientId: "root", org: "admin", role: "reader" };
  assert.equal(publisherCanReadArtifact(admin, foreign), true);
  assert.equal(publisherCanWriteArtifact(admin, foreign), true);
  assert.equal(publisherCanDeleteArtifact(admin, foreign), true);
});
