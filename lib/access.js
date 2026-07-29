// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman

// Invariant 3: cross-organization reads are concealed as `404`, so an artifact id never
// discloses tenant membership. The concealment decision lives here rather than in each route
// so every artifact-scoped surface answers a foreign id exactly like an unknown one.

export function artifactAccess(viewer, artifact, { conceal = false } = {}) {
  if (!artifact) return { ok: false, status: 404, error: "Not found" };
  if (!viewer?.email) {
    return conceal
      ? { ok: false, status: 404, error: "Not found" }
      : { ok: false, status: 401, error: "Not signed in" };
  }
  if (viewer.isAdmin || (viewer.org && viewer.org === artifact.org)) return { ok: true };
  return conceal
    ? { ok: false, status: 404, error: "Not found" }
    : { ok: false, status: 403, error: "Forbidden" };
}

// The single decision every artifact-scoped human route (read AND mutation) must use. A
// reserved id, an unknown id, an unsigned probe, and another organization's id are all
// indistinguishable: one `404` with the same body. Returning `403` for a foreign artifact
// would confirm the id exists in some other tenant, which is exactly the disclosure
// invariant 3 forbids. Role-only settings routes keep `adminAccess` and its `403`, because
// that answer is about the caller's role, not about which tenant owns a record.
export function concealedArtifactAccess(viewer, artifact) {
  return artifactAccess(viewer, artifact, { conceal: true });
}

// Human-side artifact management is intentionally narrower than same-tenant read access.
// Administrators may manage every readable artifact. Other viewers may manage only rows whose
// immutable publish-time owner matches their signed-in identity; legacy rows without an owner
// therefore remain administrator-only.
export function viewerCanManageArtifact(viewer, artifact) {
  if (!viewer?.email || !artifact) return false;
  if (viewer.isAdmin) return true;
  const viewerEmail = String(viewer.email).toLowerCase();
  const ownerEmail = String(artifact.owner_email || "").toLowerCase();
  return !!viewer.org
    && viewer.org === artifact.org
    && !!ownerEmail
    && ownerEmail === viewerEmail;
}

export const PUBLISH_PERMISSION_ERROR = "Permission denied: reader keys cannot publish artifacts";
export const READ_PERMISSION_ERROR = "Permission denied: this API key cannot read this artifact";
export const WRITE_PERMISSION_ERROR = "Permission denied: this API key cannot modify this artifact";
export const DELETE_PERMISSION_ERROR = "Permission denied: this API key cannot delete this artifact";

function publisherRole(auth) {
  return auth?.role === undefined ? "author" : auth.role;
}

function publisherOwns(auth, artifact) {
  return !!artifact && artifact.client_id === auth?.clientId && artifact.org === auth?.org;
}

function publisherSharesOrg(auth, artifact) {
  return !!artifact && artifact.org === auth?.org;
}

// Publisher capabilities are orthogonal to the admin pseudo-org. A non-admin decision always
// requires the artifact to remain in the key's current org, so control does not follow an admin
// re-tenant merely because client_id stayed unchanged.
export function publisherCanReadArtifact(auth, artifact) {
  if (auth?.org === "admin") return true;
  if (!publisherSharesOrg(auth, artifact)) return false;
  const role = publisherRole(auth);
  return role === "reader" || role === "collaborator" || (role === "author" && publisherOwns(auth, artifact));
}

export function publisherCanWriteArtifact(auth, artifact) {
  if (auth?.org === "admin") return true;
  if (!publisherSharesOrg(auth, artifact)) return false;
  const role = publisherRole(auth);
  return role === "collaborator" || (role === "author" && publisherOwns(auth, artifact));
}

export function publisherCanDeleteArtifact(auth, artifact) {
  if (auth?.org === "admin") return true;
  const role = publisherRole(auth);
  return (role === "author" || role === "collaborator") && publisherOwns(auth, artifact);
}

function publisherArtifactDecision(auth, artifact, id, allowed, refusal) {
  if (!artifact || (auth?.org !== "admin" && artifact.org !== auth?.org)) {
    return { ok: false, error: `Unknown artifact: ${id}` };
  }
  return allowed(auth, artifact)
    ? { ok: true, artifact }
    : { ok: false, error: refusal };
}

// Invariant 3 on the publisher read path: an artifact outside the caller's organization is
// reported with the identical error as one that does not exist, so ids cannot probe tenancy.
export function concealedPublisherRead(auth, artifact, id) {
  return publisherArtifactDecision(auth, artifact, id, publisherCanReadArtifact, READ_PERMISSION_ERROR);
}

export function publisherWriteAccess(auth, artifact, id) {
  return publisherArtifactDecision(auth, artifact, id, publisherCanWriteArtifact, WRITE_PERMISSION_ERROR);
}

export function publisherDeleteAccess(auth, artifact, id) {
  return publisherArtifactDecision(auth, artifact, id, publisherCanDeleteArtifact, DELETE_PERMISSION_ERROR);
}

export function adminAccess(viewer) {
  if (!viewer?.email) return { ok: false, status: 403, error: "Not signed in" };
  if (!viewer.isAdmin) return { ok: false, status: 403, error: "Admins only" };
  return { ok: true };
}
