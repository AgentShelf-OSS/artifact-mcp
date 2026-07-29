// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// Admin org registry: organizations, explicit email members, email domains (which auto-tenant
// a signed-in viewer), and their category registry. Explicit email and domain mappings here are
// used by identity.js before its environment and implicit-domain fallbacks.
import db from "./db.js";

const listOrgsStmt = db.prepare("SELECT name, label, color, created_at FROM orgs ORDER BY name ASC");
const setColorStmt = db.prepare("UPDATE orgs SET color = ? WHERE name = ?");
const HEX_RE = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;
const domainsForStmt = db.prepare("SELECT domain FROM org_domains WHERE org = ? ORDER BY domain ASC");
const emailsForStmt = db.prepare("SELECT email FROM org_email_members WHERE org = ? ORDER BY email ASC");
const categoriesForStmt = db.prepare("SELECT name FROM org_categories WHERE org = ? ORDER BY name ASC");
const activeKeyCountsStmt = db.prepare(
  "SELECT org, COUNT(*) AS n FROM api_keys WHERE revoked_at IS NULL GROUP BY org"
);
const orgExistsStmt = db.prepare("SELECT 1 FROM orgs WHERE name = ?");
const insertOrgStmt = db.prepare("INSERT INTO orgs (name, label) VALUES (?, ?)");
const deleteOrgStmt = db.prepare("DELETE FROM orgs WHERE name = ?");
const artifactCountStmt = db.prepare("SELECT COUNT(*) AS n FROM artifacts WHERE org = ?");
const revokeOrgKeysStmt = db.prepare(
  "UPDATE api_keys SET revoked_at = datetime('now') WHERE org = ? AND revoked_at IS NULL"
);
const insertDomainStmt = db.prepare("INSERT INTO org_domains (domain, org) VALUES (?, ?)");
const deleteDomainStmt = db.prepare("DELETE FROM org_domains WHERE org = ? AND domain = ?");
const domainOwnerStmt = db.prepare("SELECT org FROM org_domains WHERE domain = ?");
const insertEmailMemberStmt = db.prepare("INSERT INTO org_email_members (email, org) VALUES (?, ?)");
const deleteEmailMemberStmt = db.prepare("DELETE FROM org_email_members WHERE org = ? AND email = ?");
const emailOwnerStmt = db.prepare("SELECT org FROM org_email_members WHERE email = ?");
const insertCategoryStmt = db.prepare("INSERT OR IGNORE INTO org_categories (org, name) VALUES (?, ?)");
const deleteCategoryStmt = db.prepare("DELETE FROM org_categories WHERE org = ? AND name = ?");

const ORG_RE = /^[a-z0-9][a-z0-9._-]{0,40}$/i;
const DOMAIN_RE = /^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/i;
const EMAIL_LOCAL_RE = /^[a-z0-9!#$%&'*+/=?^_`{|}~.-]+$/i;

export function isValidOrgName(name) {
  return ORG_RE.test(String(name || ""));
}

export function isValidDomain(domain) {
  return DOMAIN_RE.test(String(domain || ""));
}

function normDomain(s) {
  return String(s || "").trim().toLowerCase();
}
function normEmail(s) {
  return String(s || "").trim().toLowerCase();
}
function validEmail(email) {
  if (!email || email.length > 254 || /\s/.test(email)) return false;
  const at = email.indexOf("@");
  if (at <= 0 || at !== email.lastIndexOf("@")) return false;
  const local = email.slice(0, at);
  const domain = email.slice(at + 1);
  return local.length <= 64 && EMAIL_LOCAL_RE.test(local) &&
    !local.startsWith(".") && !local.endsWith(".") && !local.includes("..") &&
    domain.length <= 253 && DOMAIN_RE.test(domain);
}
function normCategory(s) {
  return String(s || "").trim().replace(/\s+/g, " ").slice(0, 60);
}

export function orgExists(name) {
  return !!orgExistsStmt.get(String(name || "").trim());
}

// domain -> org name, or null when the domain is not registered.
export function orgForDomain(domain) {
  const row = domainOwnerStmt.get(normDomain(domain));
  return row ? row.org : null;
}

// normalized email -> explicitly assigned org name, or null when normal domain routing applies.
export function orgForEmail(email) {
  const row = emailOwnerStmt.get(normEmail(email));
  return row ? row.org : null;
}

// Server-authoritative counterpart to the viewer identity routing used by identity.js.
// Owner assignment deliberately accepts only explicit membership or a registered org domain;
// it never derives authority from an arbitrary publisher label or client id.
export function verifiedOrgForEmail(email) {
  const normalized = normEmail(email);
  if (!validEmail(normalized)) return null;
  const explicit = orgForEmail(normalized);
  if (explicit) return explicit;
  return orgForDomain(normalized.slice(normalized.lastIndexOf("@") + 1));
}

export function categoriesFor(org) {
  return categoriesForStmt.all(String(org || "").trim()).map((r) => r.name);
}

export function listOrgNames() {
  return listOrgsStmt.all().map((o) => o.name);
}

export function listOrgs() {
  const counts = new Map(activeKeyCountsStmt.all().map((r) => [r.org, r.n]));
  return listOrgsStmt.all().map((o) => ({
    name: o.name,
    label: o.label,
    color: o.color || null,
    created_at: o.created_at,
    domains: domainsForStmt.all(o.name).map((r) => r.domain),
    emails: emailsForStmt.all(o.name).map((r) => r.email),
    categories: categoriesForStmt.all(o.name).map((r) => r.name),
    keyCount: counts.get(o.name) || 0
  }));
}

// Map of org name -> stored accent color (null when unset). Cheap; for gallery/shell rendering.
export function colorMap() {
  const map = {};
  for (const o of listOrgsStmt.all()) map[o.name] = o.color || null;
  return map;
}

// Set (hex like #356B9F or #abc) or clear (empty) an org's accent color. NULL = derived color.
export function setColor(name, color) {
  name = String(name || "").trim();
  color = String(color || "").trim();
  if (!orgExists(name)) throw new Error(`Unknown organization "${name}".`);
  if (color && !HEX_RE.test(color)) throw new Error("Color must be a hex value like #356B9F.");
  setColorStmt.run(color || null, name);
  return { name, color: color || null };
}

export function createOrg({ name, label, domain } = {}) {
  // Case-fold the org id: authorization compares org strings exactly and domains resolve to
  // lowercase, so accepting "Acme" alongside "acme" would silently split one tenant in two.
  name = String(name || "").trim().toLowerCase();
  label = String(label || "").trim().slice(0, 80);
  if (!isValidOrgName(name)) throw new Error("Org name must be letters, numbers, dot, dash, or underscore (max 41).");
  if (isValidDomain(name)) {
    throw new Error('Org name must not be an email domain. Use a tenant id such as "acme" and add the domain separately.');
  }
  if (name === "admin") throw new Error('"admin" is a reserved org name.');
  if (orgExists(name)) throw new Error(`Organization "${name}" already exists.`);
  const dom = domain ? normDomain(domain) : "";
  if (dom) {
    if (!DOMAIN_RE.test(dom)) throw new Error(`"${dom}" is not a valid email domain.`);
    const owner = domainOwnerStmt.get(dom);
    if (owner) throw new Error(`Domain "${dom}" is already mapped to "${owner.org}".`);
  }
  db.transaction(() => {
    insertOrgStmt.run(name, label);
    if (dom) insertDomainStmt.run(dom, name);
  })();
  return { name, label, domains: dom ? [dom] : [], emails: [], categories: [], keyCount: 0 };
}

const deleteOrgTransaction = db.transaction((name) => {
  if (!orgExists(name)) return false;
  const artifactCount = artifactCountStmt.get(name).n;
  if (artifactCount > 0) {
    throw new Error(
      `Cannot delete organization "${name}" while it owns ${artifactCount} artifact${artifactCount === 1 ? "" : "s"}. ` +
      "Move its artifacts to another organization first."
    );
  }
  revokeOrgKeysStmt.run(name);
  return deleteOrgStmt.run(name).changes > 0;
});

export function deleteOrg(name) {
  return deleteOrgTransaction(String(name || "").trim());
}

export function addDomain(org, domain) {
  org = String(org || "").trim();
  const dom = normDomain(domain);
  if (!orgExists(org)) throw new Error(`Unknown organization "${org}".`);
  if (!DOMAIN_RE.test(dom)) throw new Error(`"${dom}" is not a valid email domain.`);
  const owner = domainOwnerStmt.get(dom);
  if (owner) {
    throw new Error(owner.org === org ? `"${dom}" is already on this org.` : `Domain "${dom}" is already mapped to "${owner.org}".`);
  }
  insertDomainStmt.run(dom, org);
  return { org, domain: dom };
}

export function removeDomain(org, domain) {
  org = String(org || "").trim();
  const dom = normDomain(domain);
  const owner = domainOwnerStmt.get(dom);
  if (org === dom && owner?.org === org) {
    throw new Error(
      `Cannot remove domain "${dom}" from organization "${org}": implicit domain access would remain. Migrate to a non-domain organization first.`
    );
  }
  return deleteDomainStmt.run(org, dom).changes > 0;
}

export function addEmailMember(org, email) {
  org = String(org || "").trim();
  const normalized = normEmail(email);
  if (!orgExists(org)) throw new Error(`Unknown organization "${org}".`);
  if (!validEmail(normalized)) throw new Error(`"${normalized}" is not a valid email address.`);
  const owner = emailOwnerStmt.get(normalized);
  if (owner) {
    throw new Error(owner.org === org
      ? `"${normalized}" is already on this org.`
      : `Email "${normalized}" is already mapped to "${owner.org}".`);
  }
  insertEmailMemberStmt.run(normalized, org);
  return { org, email: normalized };
}

export function removeEmailMember(org, email) {
  return deleteEmailMemberStmt.run(String(org || "").trim(), normEmail(email)).changes > 0;
}

export function addCategory(org, name) {
  org = String(org || "").trim();
  const cat = normCategory(name);
  if (!orgExists(org)) throw new Error(`Unknown organization "${org}".`);
  if (!cat) throw new Error("Category name is required.");
  insertCategoryStmt.run(org, cat);
  return { org, name: cat };
}

export function removeCategory(org, name) {
  return deleteCategoryStmt.run(String(org || "").trim(), normCategory(name)).changes > 0;
}
