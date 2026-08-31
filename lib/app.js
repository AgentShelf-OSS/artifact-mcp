// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import express from "express";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { adminAccess, concealedArtifactAccess, viewerCanManageArtifact } from "./access.js";
import { injectAnchorBridge, isHtmlContentType, rawArtifactHeaders, stripScripts } from "./artifact-http.js";
import { parseReactionInput } from "./contracts.js";
import {
  createMcpTelemetry,
  mcpMetricLabels,
  mcpProtocolDimension,
  mcpRequestName,
  mcpResponseOutcome
} from "./observability.js";
import {
  OAUTH_SCOPES,
  hasRequiredScope,
  protectedResourceMetadata,
  requiredScopeForMcpRequest,
  resourceMetadataUrl
} from "./oauth.js";
import { AUDIT_CAPABILITIES, auditContextFromViewer } from "./audit.js";

function jsonError(res, decision) {
  return res.status(decision.status).json({ error: decision.error });
}

function validateAnchorPage(artifacts, meta, anchor, value) {
  if (anchor == null) {
    if (value != null && value !== "") throw new Error("anchor_page is only valid for anchored bundle feedback.");
    return null;
  }
  if (!meta.is_bundle) {
    if (value != null && value !== "") throw new Error("anchor_page is only valid for bundle feedback.");
    return null;
  }
  if (typeof value !== "string" || !value.trim()) {
    throw new Error("anchor_page is required for anchored bundle feedback.");
  }
  const raw = value.trim().replace(/\\/g, "/");
  if (raw.startsWith("/") || /^[A-Za-z]:\//.test(raw) || path.posix.isAbsolute(raw) || raw.split("/").includes("..")) {
    throw new Error("anchor_page must be a bundle-relative path without traversal.");
  }
  const normalized = path.posix.normalize(raw);
  if (!normalized || normalized === ".") throw new Error("anchor_page must identify a bundle HTML file.");
  const file = artifacts.readBundleFile(meta.id, normalized);
  if (!file || !isHtmlContentType(file.contentType)) {
    throw new Error("anchor_page must identify an existing bundle HTML file.");
  }
  return normalized;
}

const FEEDBACK_CONTENT_FIELDS = new Set(["body", "parent_id", "anchor", "anchor_page"]);

function validateFeedbackInput(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Feedback body must be a JSON object.");
  }
  const unknown = Object.keys(value).find((key) => !FEEDBACK_CONTENT_FIELDS.has(key));
  if (unknown) throw new Error(`Unknown feedback field: ${unknown}`);
  return value;
}

function anchorVersion(row) {
  if (Number.isInteger(row?.anchor_version)) return row.anchor_version;
  if (row?.anchor_kind != null || row?.anchor_node_id != null || row?.anchor_quote != null) return 2;
  return row?.anchor_x != null && row?.anchor_y != null ? 1 : 0;
}

// JSON.parse (and express.json) silently keeps the last duplicate object member. Discussion
// mutation bodies have exact, flat schemas, so inspect their raw top-level keys before parsing.
function hasDuplicateTopLevelJsonKey(source) {
  const first = source.search(/\S/);
  if (first < 0 || source[first] !== "{") return false;
  const keys = new Set();
  let depth = 0;
  let expectingKey = false;
  for (let index = first; index < source.length; index += 1) {
    const character = source[index];
    if (character === "\"") {
      let end = index + 1;
      let escaped = false;
      for (; end < source.length; end += 1) {
        const current = source[end];
        if (escaped) escaped = false;
        else if (current === "\\") escaped = true;
        else if (current === "\"") break;
      }
      const token = source.slice(index, end + 1);
      if (depth === 1 && expectingKey) {
        let cursor = end + 1;
        while (/\s/.test(source[cursor] || "")) cursor += 1;
        if (source[cursor] === ":") {
          const key = JSON.parse(token);
          if (keys.has(key)) return true;
          keys.add(key);
          expectingKey = false;
        }
      }
      index = end;
      continue;
    }
    if (character === "{" || character === "[") {
      depth += 1;
      if (depth === 1 && character === "{") expectingKey = true;
    } else if (character === "}" || character === "]") {
      depth -= 1;
    } else if (character === "," && depth === 1) {
      expectingKey = true;
    }
  }
  return false;
}

export function createApp({
  checkPublisherKey,
  handleMcp,
  validateMcpHttpRequest = () => ({ ok: true, protocolVersion: null, modern: false }),
  resolveViewer,
  artifacts,
  shares = { create: () => { throw new Error("Shares are unavailable"); }, resolve: () => null, listForArtifact: () => [], revoke: () => false },
  keys,
  orgs,
  webhooks = { listForOrg: () => [], create: () => undefined, remove: () => false, setEvents: () => undefined, get: () => undefined },
  discussions = { connection: () => ({ configured: false, label: "", destination: "", lastError: null }) },
  // PBI-081 owns organization credentials and effective policy separately from PBI-079's
  // legacy per-artifact discussion seam. The production implementation injects this narrow
  // port; keeping it optional preserves the compatibility oracle during staged rollout.
  organizationThreading = null,
  notify = { emit() {}, test: async () => ({ ok: false, error: "Notifications are unavailable." }) },
  reactions,
  views = { record() {}, countsFor: () => null, countsForOrg: () => new Map(), viewersFor: () => [], topForOrg: () => [] },
  feedback,
  notifications = { recentForViewer: () => [], unreadCount: () => 0, markSeen() {} },
  thumbnails = { readThumbnail: async () => null, removeArtifact: async () => {}, placeholder: () => Buffer.from("") },
  pages,
  logger = console,
  healthCheck = () => ({ status: "ok" }),
  publicBase = process.env.PUBLIC_BASE_URL || "http://localhost:3480",
  oauth = { enabled: false },
  audit = null,
  mcpTelemetry = null,
  securityMetrics = { record() {}, renderPrometheus: () => "" },
  limits = {}
}) {
  const app = express();
  const auditRequestIds = new WeakMap();
  const telemetry = mcpTelemetry || createMcpTelemetry({ logger });
  const publicOrigin = new URL(publicBase).origin;
  app.disable("x-powered-by");

  // Keep the Node compatibility twin aligned with Rust while PBI-041 still requires it. `/mcp`
  // intentionally uses explicit bearer credentials and is not a cookie-authenticated portal
  // surface, so it is exempt from this request-authenticity policy.
  app.use((req, res, next) => {
    if (req.path === "/mcp" || !["POST", "PUT", "PATCH", "DELETE"].includes(req.method)) {
      return next();
    }
    const hasViewerSession = Boolean(
      req.headers.cookie ||
      req.headers["cf-access-jwt-assertion"] ||
      req.headers["cf-access-authenticated-user-email"]
    );
    if (!hasViewerSession) {
      res.vary(["Sec-Fetch-Site", "Origin"]);
      return next();
    }

    const portalHeader = req.get("x-artifact-mutation") === "1";
    const fetchSite = req.get("sec-fetch-site");
    let sameOrigin = false;
    if (fetchSite !== undefined) {
      sameOrigin = fetchSite.toLowerCase() === "same-origin";
    } else {
      const origin = req.get("origin");
      try {
        sameOrigin = Boolean(origin) && new URL(origin).origin === publicOrigin;
      } catch {
        sameOrigin = false;
      }
    }
    if (!portalHeader || !sameOrigin) {
      return res
        .vary(["Sec-Fetch-Site", "Origin"])
        .status(403)
        .json({ error: "forbidden", code: "same_origin_required" });
    }
    res.vary(["Sec-Fetch-Site", "Origin"]);
    return next();
  });

  // Invariant 3, single gate. EVERY artifact-scoped human route resolves its artifact through
  // this helper, so a reserved id, an unknown id, an unsigned probe, and another organization's
  // id are indistinguishable: the same 404 status and the same body that a genuinely missing
  // artifact already returns. The viewer is resolved for every id (present or not) and no
  // subordinate read — body bytes, thumbnail, feedback, shares, views, history — is performed
  // before the decision, so neither the response nor the work done can be used as an oracle.
  async function artifactForViewer(req, respondNotFound) {
    const id = req.params.id;
    const meta = artifacts.isReserved?.(id) ? null : artifacts.getArtifactMeta(id);
    const viewer = await resolveViewer(req);
    if (!concealedArtifactAccess(viewer, meta).ok) {
      respondNotFound();
      return null;
    }
    return { id, meta, viewer };
  }

  // Two concealment shapes, one per existing not-found path: HTML pages keep the rendered
  // not-found page, JSON routes keep `{ error: "Not found" }`. Nothing new is invented.
  const artifactPageOr404 = (req, res) => artifactForViewer(req, () => res.status(404).send(pages.notFound()));
  const artifactApiOr404 = (req, res) => artifactForViewer(req, () => res.status(404).json({ error: "Not found" }));

  app.get("/health", (_req, res) => {
    try {
      return res.json(healthCheck());
    } catch (error) {
      logger.error?.("[artifact-mcp] health check failed", error);
      return res.status(503).json({ status: "error" });
    }
  });

  app.get("/metrics", (_req, res) => {
    return res
      .set("content-type", "text/plain; version=0.0.4; charset=utf-8")
      .set("cache-control", "no-store")
      .send(`${telemetry.renderPrometheus()}${securityMetrics.renderPrometheus()}`);
  });

  // Audit access is a deliberately separate OAuth-only surface. It never accepts a tenant,
  // actor, or capability from the request body: the verified OAuth projection supplies the
  // tenant and the ledger verifies the signed continuation cursor.
  function auditOptions(req) {
    const { tenant, cursor, limit } = req.query;
    if ([tenant, cursor, limit].some((value) => value !== undefined && typeof value !== "string")) {
      throw new Error("invalid audit query");
    }
    return { tenant, cursor, limit };
  }

  function auditChallenge(res, required, error) {
    if (oauth.enabled) {
      const parts = ["Bearer"];
      if (error) parts.push(`error="${error}"`);
      parts.push(`scope="${required}"`, `resource_metadata="${resourceMetadataUrl(publicBase)}"`);
      res.set("www-authenticate", parts.join(" "));
    }
  }

  async function authenticateAuditRequest(req, res, required) {
    let auth;
    try {
      auth = await checkPublisherKey(req);
    } catch {
      auth = null;
    }
    if (!auth?.ok) {
      securityMetrics.record("auth_failure");
      auditChallenge(res, required);
      res.status(401).json({ error: "unauthorized" });
      return null;
    }
    // Never let a legacy API key inherit an audit scope, and fail closed if OAuth has not been
    // configured. `createPublisherAuthenticator` stamps verified JWT identities as `oauth`.
    if (!oauth.enabled || auth.authType !== "oauth" || !(auth.scopes instanceof Set)) {
      auditChallenge(res, required, "insufficient_scope");
      res.status(403).json({ error: "forbidden" });
      return null;
    }
    const missing = required.split(" ").find((scope) => !hasRequiredScope(auth, scope));
    if (missing) {
      auditChallenge(res, missing, "insufficient_scope");
      res.status(403).json({ error: "forbidden" });
      return null;
    }
    return { tenant: auth.org, capabilities: auth.scopes };
  }

  function auditFailure(res, error) {
    if (/capability is required/i.test(String(error?.message || ""))) {
      return res.status(403).json({ error: "forbidden" });
    }
    // Cursor and query failures intentionally remain opaque: callers must not receive raw
    // cursor material or storage errors.
    return res.status(400).json({ error: "invalid_audit_request" });
  }

  app.get("/audit/events", async (req, res) => {
    const context = await authenticateAuditRequest(req, res, AUDIT_CAPABILITIES.READ);
    if (!context) return;
    if (!audit?.query) return res.status(503).json({ error: "audit_unavailable" });
    try {
      const page = audit.query(context, auditOptions(req));
      return res.set("cache-control", "no-store").json(page);
    } catch (error) {
      return auditFailure(res, error);
    }
  });

  app.get("/audit/export", async (req, res) => {
    const context = await authenticateAuditRequest(
      req,
      res,
      `${AUDIT_CAPABILITIES.READ} ${AUDIT_CAPABILITIES.EXPORT}`
    );
    if (!context) return;
    if (!audit?.export) return res.status(503).json({ error: "audit_unavailable" });
    try {
      const output = audit.export(context, auditOptions(req));
      if (output.next) res.set("x-audit-next", output.next);
      if (output.truncated) res.set("x-audit-truncated", "true");
      if (output.reason) res.set("x-audit-export-reason", output.reason);
      return res
        .set("content-type", "application/x-ndjson; charset=utf-8")
        .set("cache-control", "no-store")
        .send(output.ndjson);
    } catch (error) {
      return auditFailure(res, error);
    }
  });

  app.get([
    "/.well-known/oauth-protected-resource",
    "/.well-known/oauth-protected-resource/mcp"
  ], (_req, res) => {
    if (!oauth.enabled) return res.status(404).end();
    return res.json(protectedResourceMetadata(oauth, publicBase));
  });

  // Deliberately public: Cloudflare Access bypasses only /s/* and this token gate is
  // the authorization boundary. These routes never resolve a signed-in viewer.
  async function sharedArtifactOr404(req, res) {
    const share = shares.resolve(req.params.token);
    if (!share) { res.status(404).send(pages.notFound()); return null; }
    const meta = artifacts.getArtifactMeta(share.artifact_id);
    // Keep a stale/malformed row indistinguishable from every other invalid token.
    if (!meta || meta.org !== share.org) { res.status(404).send(pages.notFound()); return null; }
    return { share, meta };
  }

  function sharedHeaders(contentType) {
    // no-store, not the raw route's max-age=60: revocation/expiry must be immediate, so a
    // browser can't re-serve a killed link from private cache without re-hitting the token gate.
    return { ...rawArtifactHeaders(contentType), "cache-control": "no-store", "x-robots-tag": "noindex" };
  }

  // Register /s/:token/* BEFORE /s/:token so a bundle's trailing-slash entry (/s/:token/) hits
  // the wildcard and serves the entry, instead of matching /s/:token and 302-looping.
  app.get("/s/:token/*", async (req, res) => {
    const shared = await sharedArtifactOr404(req, res);
    if (!shared) return;
    if (!shared.meta.is_bundle) return res.status(404).send(pages.notFound());
    const file = artifacts.readBundleFile(shared.meta.id, req.params[0] || "");
    if (!file) return res.status(404).send(pages.notFound());
    return res.set(sharedHeaders(file.contentType)).send(file.content);
  });

  app.get("/s/:token", async (req, res) => {
    const shared = await sharedArtifactOr404(req, res);
    if (!shared) return;
    if (shared.meta.is_bundle) return res.set("x-robots-tag", "noindex").redirect(302, `/s/${encodeURIComponent(req.params.token)}/`);
    const found = artifacts.readArtifact(shared.meta.id);
    if (!found) return res.status(404).send(pages.notFound());
    return res.set(sharedHeaders("text/html; charset=utf-8")).send(found.html);
  });

  // Authenticate BEFORE express.json buffers/parses the (up to multi-MB) body, so an
  // unauthenticated caller can't spend memory/CPU on the parser. /mcp is Access-bypassed,
  // so this key check is the only gate in front of the body.
  const mcpAuthGate = async (req, res, next) => {
    const observation = telemetry.begin();
    observation.setLabels(mcpMetricLabels(mcpProtocolDimension(req.headers)));
    req.mcpObservation = observation;
    res.set("x-request-id", observation.requestId);
    req.once?.("aborted", () => observation.cancel());
    res.once?.("close", () => {
      if (!res.writableEnded) observation.cancel();
    });
    try {
      const auth = await checkPublisherKey(req);
      if (!auth.ok) {
        securityMetrics.record("auth_failure");
        if (oauth.enabled) {
          res.set("www-authenticate",
            `Bearer resource_metadata="${resourceMetadataUrl(publicBase)}", scope="${OAUTH_SCOPES.join(" ")}"`);
        }
        const body = { jsonrpc: "2.0", id: null, error: { code: -32001, message: "unauthorized" } };
        observation.finish("authentication_failure", Buffer.byteLength(JSON.stringify(body)));
        return res.status(401).json(body);
      }
      req.mcpAuth = auth;
      return next();
    } catch {
      const body = { jsonrpc: "2.0", id: null, error: { code: -32001, message: "unauthorized" } };
      observation.finish("authentication_failure", Buffer.byteLength(JSON.stringify(body)));
      return res.status(401).json(body);
    }
  };
  app.post("/mcp", mcpAuthGate, express.json({ limit: limits.mcpJson || "8mb" }), async (req, res) => {
    const observation = req.mcpObservation || telemetry.begin();
    if (!req.mcpObservation) {
      observation.setLabels(mcpMetricLabels(
        mcpProtocolDimension(req.headers),
        req.body?.method,
        mcpRequestName(req.body)
      ));
      res.set("x-request-id", observation.requestId);
    }
    const auth = req.mcpAuth || await checkPublisherKey(req);
    if (!auth.ok) {
      securityMetrics.record("auth_failure");
      const body = { jsonrpc: "2.0", id: null, error: { code: -32001, message: "unauthorized" } };
      observation.finish("authentication_failure", Buffer.byteLength(JSON.stringify(body)));
      return res.status(401).json(body);
    }
    try {
      const transport = validateMcpHttpRequest(req.body, req.headers);
      observation.setLabels(mcpMetricLabels(
        transport.protocolVersion || mcpProtocolDimension(req.headers),
        req.body?.method,
        mcpRequestName(req.body)
      ));
      if (!transport.ok) {
        observation.finish("validation_failure", Buffer.byteLength(JSON.stringify(transport.response)));
        return res.status(transport.status).json(transport.response);
      }
      const requiredScope = requiredScopeForMcpRequest(req.body);
      if (!hasRequiredScope(auth, requiredScope)) {
        res.set("www-authenticate",
          `Bearer error="insufficient_scope", scope="${requiredScope}", resource_metadata="${resourceMetadataUrl(publicBase)}"`);
        const body = {
          jsonrpc: "2.0",
          id: req.body?.id ?? null,
          error: {
            code: -32003,
            message: "insufficient_scope",
            data: { requiredScope }
          }
        };
        observation.finish("authorization_failure", Buffer.byteLength(JSON.stringify(body)));
        return res.status(403).json(body);
      }
      const output = await handleMcp(req.body, {
        clientId: auth.clientId,
        org: auth.org,
        label: auth.label,
        role: auth.role,
        ownerEmail: auth.ownerEmail,
        ...(auth.scopes !== undefined ? { scopes: auth.scopes } : {}),
        ...(auth.authType !== undefined ? { authType: auth.authType } : {})
      }, {
        protocolVersion: transport.protocolVersion,
        oauthEnabled: oauth.enabled
      });
      if (!output) {
        observation.finish("success", 0);
        return res.status(202).end();
      }
      observation.finish(mcpResponseOutcome(output), Buffer.byteLength(JSON.stringify(output)));
      return res.status(transport.modern && output?.error?.code === -32601 ? 404 : 200).json(output);
    } catch (error) {
      const body = {
        jsonrpc: "2.0",
        id: null,
        error: { code: -32700, message: String(error.message || error) }
      };
      observation.finish("validation_failure", Buffer.byteLength(JSON.stringify(body)));
      return res.status(400).json(body);
    }
  });
  app.use((error, req, res, next) => {
    if (req.path !== "/mcp" || !req.mcpObservation) return next(error);
    const status = error?.type === "entity.too.large" ? 413 : 400;
    // The frozen 2025-06-18 transport contract uses the same compact HTTP parser envelope as the
    // Rust server. A malformed body cannot be inspected for modern request metadata, so the
    // buffering boundary deliberately remains protocol-neutral.
    const body = { error: status === 413 ? "payload too large" : "invalid JSON" };
    req.mcpObservation.finish("validation_failure", Buffer.byteLength(JSON.stringify(body)));
    return res.status(status).json(body);
  });

  app.options("/mcp", (_req, res) =>
    res.set({
      "access-control-allow-origin": "*",
      "access-control-allow-headers": "authorization, content-type, accept, mcp-protocol-version, mcp-method, mcp-name"
    }).status(204).end()
  );

  // Thumbnails contain the same potentially-sensitive information as their artifacts.
  // Authenticate every read and only serve the digest of the current revision.
  app.get("/thumbnails/:id", async (req, res) => {
    const found = await artifactPageOr404(req, res);
    if (!found) return;
    const { meta } = found;

    const digest = typeof req.query.v === "string" ? req.query.v : "";
    const png = digest === meta.body_sha256 ? await thumbnails.readThumbnail(meta, digest) : null;
    if (png) {
      return res.set({
        "content-type": "image/png",
        "x-content-type-options": "nosniff",
        "cache-control": "private, max-age=31536000, immutable"
      }).send(png);
    }
    const accent = orgs.colorMap?.()[meta.org];
    return res.set({
      "content-type": "image/svg+xml; charset=utf-8",
      "x-content-type-options": "nosniff",
      "cache-control": "no-store"
    }).send(thumbnails.placeholder(meta, accent));
  });

  app.get("/", async (req, res) => {
    const viewer = await resolveViewer(req);
    // Never cache the identity-dependent page: a browser must not serve a stale pre-auth
    // "Not signed in" body after the Access session is established (hard-refresh bug).
    res.set("content-type", "text/html; charset=utf-8").set("cache-control", "no-store");
    if (!viewer.email) {
      return res.status(403).send(pages.notSignedIn ? pages.notSignedIn() : "Not signed in.");
    }

    let sections;
    if (viewer.isAdmin) {
      // Union the registry orgs with the orgs that have artifacts, so a newly created but
      // still-empty org appears as a (droppable) section AND in the "move to org" menu.
      const grouped = artifacts.listAllGroupedByOrg({ includeHidden: true });
      const names = [...new Set([...orgs.names(), ...grouped.keys()])];
      sections = names.map((org) => ({ org, items: grouped.get(org) || [] }));
    } else if (viewer.org) {
      const owned = artifacts.listOrgArtifacts(viewer.org, { ownerEmail: viewer.email });
      // The boolean is the only owner information a member-rendered projection may expose.
      sections = [{ org: viewer.org, items: owned.map(({ owner_email, ...item }) => ({
        ...item,
        is_owned_by_viewer: owner_email === String(viewer.email || "").toLowerCase()
      })) }];
    } else {
      sections = [];
    }
    const viewCounts = new Map();
    const topViewed = new Map();
    for (const { org } of sections) {
      try {
        for (const [id, counts] of views.countsForOrg(org)) viewCounts.set(id, counts);
        if (viewer.isAdmin) topViewed.set(org, views.topForOrg(org));
      } catch (error) {
        logger.error?.("[artifact-mcp] view analytics gallery read failed", error);
      }
    }
    const notificationState = {
      items: notifications.recentForViewer(viewer),
      unread: notifications.unreadCount(viewer)
    };
    return res.send(pages.gallery(
      viewer,
      sections,
      reactions.forViewer(viewer.email),
      viewer.isAdmin ? reactions.sentiment() : new Map(),
      viewCounts,
      topViewed,
      orgs.colorMap(),
      notificationState
    ));
  });

  app.post("/notifications/seen", async (req, res) => {
    const viewer = await resolveViewer(req);
    if (!viewer.email) return res.status(403).json({ error: "Not signed in." });
    notifications.markSeen(viewer.email);
    return res.json({ ok: true });
  });

  app.get("/settings", async (req, res) => {
    const viewer = await resolveViewer(req);
    const decision = adminAccess(viewer);
    if (!decision.ok) return res.status(decision.status).send(decision.error);
    const entries = keys.list();
    const orgList = orgs.list().map((org) => ({ ...org, webhooks: webhooks.listForOrg(org.name) }));
    return res.set("content-type", "text/html; charset=utf-8").set("cache-control", "no-store").send(pages.settings(viewer, entries, orgList));
  });

  app.post("/settings/keys", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await resolveViewer(req);
    const decision = adminAccess(viewer);
    if (!decision.ok) return jsonError(res, decision);
    const targetOrg = String(req.body?.org || "").trim();
    if (!orgs.has(targetOrg)) {
      return res.status(400).json({ error: `Unknown organization "${targetOrg}". Create it in the Organizations section first.` });
    }
    try {
      const { clientId, org, label, role, secret } = keys.create({
        clientId: req.body?.clientId,
        org: req.body?.org,
        label: req.body?.label,
        role: req.body?.role,
        ...(req.body?.ownerEmail === undefined ? {} : { ownerEmail: req.body.ownerEmail })
      });
      logger.info?.(`[artifact-mcp] key created ${clientId} (org=${org}, role=${role}) by ${viewer.email}`);
      return res.json({ clientId, org, label, role, secret, created_at: new Date().toISOString() });
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.post("/settings/keys/:id/revoke", async (req, res) => {
    const viewer = await resolveViewer(req);
    const decision = adminAccess(viewer);
    if (!decision.ok) return jsonError(res, decision);
    const revoked = keys.revoke(req.params.id);
    logger.info?.(`[artifact-mcp] key revoke ${req.params.id} by ${viewer.email} -> ${revoked}`);
    return res.json({ id: req.params.id, revoked });
  });

  app.patch("/settings/keys/:id", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await resolveViewer(req);
    const decision = adminAccess(viewer);
    if (!decision.ok) return jsonError(res, decision);
    try {
      const updated = keys.update(req.params.id, {
        label: req.body?.label,
        role: req.body?.role,
        ownerEmail: req.body?.ownerEmail
      });
      if (!updated) return res.status(404).json({ error: "Not found" });
      logger.info?.(`[artifact-mcp] key metadata ${updated.clientId} updated by ${viewer.email}`);
      return res.json(updated);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.post("/settings/keys/:id/owner", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await resolveViewer(req);
    const decision = adminAccess(viewer);
    if (!decision.ok) return jsonError(res, decision);
    try {
      const updated = keys.setOwner(req.params.id, req.body?.ownerEmail);
      if (!updated) return res.status(404).json({ error: "Not found" });
      logger.info?.(`[artifact-mcp] key owner ${updated.clientId} updated by ${viewer.email}`);
      return res.json(updated);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.post("/settings/keys/:id/owner/backfill", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await resolveViewer(req);
    const decision = adminAccess(viewer);
    if (!decision.ok) return jsonError(res, decision);
    try {
      const result = keys.backfillOwner(req.params.id, req.body?.ownerEmail, { confirm: req.body?.confirm === true });
      if (!result) return res.status(404).json({ error: "Not found" });
      logger.info?.(`[artifact-mcp] owner backfill ${result.clientId} ${result.confirmed ? `updated=${result.updated}` : `preview=${result.matched}`} by ${viewer.email}`);
      return res.json(result);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  // --- Organization registry (admin only) ---------------------------------------------
  async function requireAdmin(req, res) {
    const viewer = await resolveViewer(req);
    const decision = adminAccess(viewer);
    if (!decision.ok) {
      jsonError(res, decision);
      return null;
    }
    return viewer;
  }

  // Settings admins act on the selected organization; this tenant is server-derived from the
  // route and is never accepted from JSON. Artifact mutations inherit their proven artifact org.
  function auditFor(req, viewer, tenant) {
    let requestId = auditRequestIds.get(req);
    if (!requestId) { requestId = randomUUID(); auditRequestIds.set(req, requestId); }
    return { ...auditContextFromViewer(viewer, { requestId }), tenant };
  }
  function discussionFailure(res, error) {
    return res.status(error?.status || 500).json({ error: "discussion_unavailable" });
  }
  const THREADING_FAILURES = new Map([
    ["credential_validation_failed", "The credential could not access the selected Discord channel. The existing credential remains active."],
    ["credential_unavailable", "No organization credential is available. Save a credential before enabling Discord threads."],
    ["recovery_ambiguous", "More than one matching notification was found. Artifact MCP did not choose a thread anchor."],
    ["recovery_not_found", "The original notification could not be recovered. Discussion remains in Artifact MCP; no duplicate notification was posted."],
    ["recovery_permission_denied", "Discord history cannot be read for the selected channel. Check the bot's Read Message History permission."],
    ["recovery_rate_limited", "Discord is rate limiting history recovery. Artifact MCP will retry without posting a duplicate notification."],
    ["policy_disabled", "Organization Discord threading is disabled. Use organization settings to enable it."],
    ["threading_unavailable", "Discord threading is not available for this organization."]
  ]);
  function threadingFailure(res, error) {
    const code = typeof error?.code === "string" && THREADING_FAILURES.has(error.code) ? error.code : "threading_unavailable";
    const status = [400, 403, 404, 409, 422, 503].includes(error?.status) ? error.status : 503;
    return res.status(status).json({ error: THREADING_FAILURES.get(code), code });
  }
  function requireOrganizationThreading(res, operation) {
    if (!organizationThreading || typeof organizationThreading[operation] !== "function") {
      threadingFailure(res, { code: "threading_unavailable", status: 503 });
      return false;
    }
    return true;
  }
  function exactObject(body, fields) {
    return !!body && typeof body === "object" && !Array.isArray(body)
      && Object.keys(body).length === fields.length && fields.every((field) => Object.hasOwn(body, field));
  }
  async function discussionJson(req, res) {
    // The body parser is deliberately invoked after auth/concealment. That prevents a malformed
    // body from answering before an unauthorized caller receives the normal 403/404 decision.
    if (req.body !== undefined) return true;
    return new Promise((resolve) => {
      express.raw({ type: "application/json", limit: limits.keyJson || "64kb" })(req, res, (error) => {
        try {
          if (error || !Buffer.isBuffer(req.body)) throw error || new Error("missing JSON body");
          const source = req.body.toString("utf8");
          if (hasDuplicateTopLevelJsonKey(source)) throw new Error("duplicate JSON member");
          req.body = JSON.parse(source);
          resolve(true);
        } catch {
          res.status(400).json({ error: "invalid discussion request" });
          resolve(false);
        }
      });
    });
  }

  app.get("/settings/orgs/:org/discord-threading", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer || !requireOrganizationThreading(res, "status")) return;
    try { return res.json(await organizationThreading.status(req.params.org)); }
    catch (error) { return threadingFailure(res, error); }
  });

  app.put("/settings/orgs/:org/discord-threading", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer || !requireOrganizationThreading(res, "save")) return;
    if (!await discussionJson(req, res)) return;
    if (!exactObject(req.body, ["botToken", "enabled"]) || typeof req.body.botToken !== "string" || req.body.botToken.length > 512 || typeof req.body.enabled !== "boolean") {
      return res.status(400).json({ error: "invalid Discord threading request", code: "invalid_request" });
    }
    try {
      // `botToken` is intentionally passed straight into the server-side port. This route never
      // logs it, audits it, projects it, or keeps it after the awaited port call returns.
      return res.json(await organizationThreading.save({ org: req.params.org, botToken: req.body.botToken, enabled: req.body.enabled, audit, context: auditFor(req, viewer, req.params.org) }));
    } catch (error) { return threadingFailure(res, error); }
  });

  app.post("/settings/orgs/:org/discord-threading/test", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer || !requireOrganizationThreading(res, "test")) return;
    if (!await discussionJson(req, res) || !exactObject(req.body, [])) return res.status(400).json({ error: "invalid Discord threading request", code: "invalid_request" });
    try { return res.json(await organizationThreading.test({ org: req.params.org, audit, context: auditFor(req, viewer, req.params.org) })); }
    catch (error) { return threadingFailure(res, error); }
  });

  app.delete("/settings/orgs/:org/discord-threading", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer || !requireOrganizationThreading(res, "remove")) return;
    try { return res.json(await organizationThreading.remove({ org: req.params.org, audit, context: auditFor(req, viewer, req.params.org) })); }
    catch (error) { return threadingFailure(res, error); }
  });

  app.post("/settings/orgs/:org/discord-threading/recovery", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer || !requireOrganizationThreading(res, "queueRecovery")) return;
    if (!await discussionJson(req, res) || !exactObject(req.body, [])) return res.status(400).json({ error: "invalid Discord threading request", code: "invalid_request" });
    try { return res.status(202).json(await organizationThreading.queueRecovery({ org: req.params.org, audit, context: auditFor(req, viewer, req.params.org) })); }
    catch (error) { return threadingFailure(res, error); }
  });

  app.get("/:id/discussion/override", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed || !requireOrganizationThreading(res, "artifactStatus")) return;
    try { return res.json(await organizationThreading.artifactStatus({ artifact: allowed.meta })); }
    catch (error) { return threadingFailure(res, error); }
  });

  app.put("/:id/discussion/override", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    if (!viewerCanManageArtifact(allowed.viewer, allowed.meta)) return res.status(403).json({ error: "Forbidden" });
    if (!requireOrganizationThreading(res, "setArtifactOverride")) return;
    if (!await discussionJson(req, res)) return;
    if (!exactObject(req.body, ["override"]) || !["inherit", "artifact_only"].includes(req.body.override)) {
      return res.status(400).json({ error: "invalid discussion override request", code: "invalid_request" });
    }
    try {
      return res.json(await organizationThreading.setArtifactOverride({ artifact: allowed.meta, override: req.body.override, actor: allowed.viewer.email, audit, context: auditFor(req, allowed.viewer, allowed.meta.org) }));
    } catch (error) { return threadingFailure(res, error); }
  });

  app.get("/settings/orgs/:org/discord-discussion", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try { return res.json(discussions.connection(req.params.org)); }
    catch (error) { return discussionFailure(res, error); }
  });

  app.put("/settings/orgs/:org/discord-discussion", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    if (!await discussionJson(req, res)) return;
    if (!exactObject(req.body, ["url", "label"]) || typeof req.body.url !== "string" || typeof req.body.label !== "string") {
      return res.status(400).json({ error: "invalid discussion connection request" });
    }
    try {
      return res.json(discussions.configure({ org: req.params.org, url: req.body.url, label: req.body.label, audit, context: auditFor(req, viewer, req.params.org) }));
    } catch (error) { return discussionFailure(res, error); }
  });

  app.delete("/settings/orgs/:org/discord-discussion", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try { return res.json({ removed: discussions.remove({ org: req.params.org, audit, context: auditFor(req, viewer, req.params.org) }) }); }
    catch (error) { return discussionFailure(res, error); }
  });

  app.post("/settings/orgs/:org/discord-discussion/test", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    if (!await discussionJson(req, res)) return;
    if (!exactObject(req.body, [])) return res.status(400).json({ error: "invalid discussion test request" });
    try { return res.json({ tested: await discussions.testConnection({ org: req.params.org, audit, context: auditFor(req, viewer, req.params.org) }) }); }
    catch (error) { return discussionFailure(res, error); }
  });

  app.get("/:id/discussion", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    try { return res.json(discussions.status({ artifact: allowed.meta })); }
    catch (error) { return discussionFailure(res, error); }
  });

  app.put("/:id/discussion", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    if (!viewerCanManageArtifact(allowed.viewer, allowed.meta)) return res.status(403).json({ error: "Forbidden" });
    if (!await discussionJson(req, res)) return;
    if (!exactObject(req.body, ["mode"]) || !["artifact_only", "discord_mirror"].includes(req.body.mode)) {
      return res.status(400).json({ error: "invalid discussion mode request" });
    }
    try {
      return res.json(discussions.setMode({ artifact: allowed.meta, mode: req.body.mode, actor: allowed.viewer.email, audit, context: auditFor(req, allowed.viewer, allowed.meta.org) }));
    } catch (error) { return discussionFailure(res, error); }
  });

  app.post("/:id/discussion/retry", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    if (!viewerCanManageArtifact(allowed.viewer, allowed.meta)) return res.status(403).json({ error: "Forbidden" });
    if (!await discussionJson(req, res)) return;
    if (!exactObject(req.body, [])) return res.status(400).json({ error: "invalid discussion retry request" });
    try {
      return res.json(discussions.retry({ artifact: allowed.meta, actor: allowed.viewer.email, audit, context: auditFor(req, allowed.viewer, allowed.meta.org) }));
    } catch (error) { return discussionFailure(res, error); }
  });

  app.post("/settings/orgs", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const org = orgs.create({ name: req.body?.name, label: req.body?.label, domain: req.body?.domain });
      logger.info?.(`[artifact-mcp] org created ${org.name} by ${viewer.email}`);
      return res.json(org);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.delete("/settings/orgs/:name", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const removed = orgs.remove(req.params.name);
      logger.info?.(`[artifact-mcp] org delete ${req.params.name} by ${viewer.email} -> ${removed}`);
      return res.json({ name: req.params.name, removed });
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.post("/settings/orgs/:name/domains", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const result = orgs.addDomain(req.params.name, req.body?.domain);
      logger.info?.(`[artifact-mcp] domain +${result.domain} -> ${result.org} by ${viewer.email}`);
      return res.json(result);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.delete("/settings/orgs/:name/domains/:domain", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const removed = orgs.removeDomain(req.params.name, req.params.domain);
      return res.json({ org: req.params.name, domain: req.params.domain, removed });
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.post("/settings/orgs/:name/emails", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const result = orgs.addEmailMember(req.params.name, req.body?.email);
      logger.info?.(`[artifact-mcp] email member +${result.email} -> ${result.org} by ${viewer.email}`);
      return res.json(result);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.delete("/settings/orgs/:name/emails/:email", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    const email = String(req.params.email || "").trim().toLowerCase();
    const removed = orgs.removeEmailMember(req.params.name, email);
    logger.info?.(`[artifact-mcp] email member -${email} from ${req.params.name} by ${viewer.email} -> ${removed}`);
    return res.json({ org: req.params.name, email, removed });
  });

  app.post("/settings/orgs/:name/categories", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const result = orgs.addCategory(req.params.name, req.body?.name);
      return res.json(result);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.delete("/settings/orgs/:name/categories", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    const removed = orgs.removeCategory(req.params.name, req.body?.name);
    return res.json({ org: req.params.name, name: req.body?.name, removed });
  });

  app.post("/settings/orgs/:name/color", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const result = orgs.setColor(req.params.name, req.body?.color);
      logger.info?.(`[artifact-mcp] org color ${req.params.name} -> ${result.color || "auto"} by ${viewer.email}`);
      return res.json(result);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.post("/settings/orgs/:name/webhooks", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const webhook = webhooks.create({ org: req.params.name, url: req.body?.url, label: req.body?.label, events: req.body?.events });
      return res.json(webhook);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.delete("/settings/orgs/:name/webhooks/:id", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    return res.json({ org: req.params.name, id: req.params.id, removed: webhooks.remove(req.params.name, req.params.id) });
  });

  app.patch("/settings/orgs/:name/webhooks/:id", express.json({ limit: limits.keyJson || "64kb" }), async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    try {
      const webhook = webhooks.setEvents(req.params.name, req.params.id, req.body?.events);
      if (!webhook) return res.status(404).json({ error: "Webhook not found" });
      return res.json(webhook);
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.post("/settings/orgs/:name/webhooks/:id/test", async (req, res) => {
    const viewer = await requireAdmin(req, res);
    if (!viewer) return;
    const webhook = webhooks.get(req.params.id);
    if (!webhook || webhook.org !== req.params.name) return res.status(404).json({ error: "Webhook not found" });
    const result = await notify.test(webhook);
    return res.json(result);
  });

  // Past-revision raw delivery (version history). Registered BEFORE /raw/:id/* so the
  // /rev/:n path is not swallowed by the bundle wildcard.
  app.get("/raw/:id/rev/:n/*", async (req, res) => {
    if (!await artifactPageOr404(req, res)) return;
    const file = artifacts.readHistoryBundleFile(req.params.id, req.params.n, req.params[0] || "");
    if (!file) return res.status(404).send(pages.notFound());
    return res.set(rawArtifactHeaders(file.contentType)).send(file.content);
  });

  app.get("/raw/:id/rev/:n", async (req, res) => {
    const allowed = await artifactPageOr404(req, res);
    if (!allowed) return;
    const { meta } = allowed;
    const rev = Number(req.params.n);
    if (meta.is_bundle) return res.redirect(302, `/raw/${req.params.id}/rev/${rev}/`);
    const found = artifacts.readHistoryArtifact(req.params.id, rev);
    if (!found) return res.status(404).send(pages.notFound());
    return res.set(rawArtifactHeaders("text/html; charset=utf-8")).send(found.html);
  });

  app.get("/raw/:id/*", async (req, res) => {
    const allowed = await artifactPageOr404(req, res);
    if (!allowed) return;
    const { id, meta } = allowed;
    if (!meta.is_bundle) return res.status(404).send(pages.notFound());

    const file = artifacts.readBundleFile(id, req.params[0] || "");
    if (!file) return res.status(404).send(pages.notFound());
    const pagePath = req.params[0]
      ? path.posix.normalize(String(req.params[0]).replace(/\\/g, "/"))
      : meta.entry;
    let content = req.query.anchor === "1" && req.query.download === undefined && isHtmlContentType(file.contentType)
      ? injectAnchorBridge(file.content, { pagePath })
      : file.content;
    if (req.query.preview !== undefined && isHtmlContentType(file.contentType)) content = stripScripts(content);
    return res.set(rawArtifactHeaders(file.contentType)).send(content);
  });

  app.get("/raw/:id", async (req, res) => {
    const allowed = await artifactPageOr404(req, res);
    if (!allowed) return;
    const { id, meta } = allowed;
    if (meta.is_bundle) return res.redirect(302, `/raw/${id}/`);
    const found = artifacts.readArtifact(id);
    if (!found) return res.status(404).send(pages.notFound());

    let downloadName;
    if (req.query.download !== undefined) {
      const name = (meta.title || "artifact").replace(/[^\w.-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 80) || "artifact";
      downloadName = `${name}.html`;
    }
    let content = req.query.anchor === "1" && req.query.download === undefined
      ? injectAnchorBridge(found.html)
      : found.html;
    if (req.query.preview !== undefined) content = stripScripts(content);
    return res.set(rawArtifactHeaders("text/html; charset=utf-8", { downloadName })).send(content);
  });

  app.get("/:id", async (req, res) => {
    const allowed = await artifactPageOr404(req, res);
    if (!allowed) return;
    const { id, meta, viewer } = allowed;

    // Shell renders are the single attribution point; raw iframe/subresource requests
    // intentionally bypass analytics so they cannot double-count a visit.
    if (!viewer.isAdmin && viewer.email) {
      try {
        views.record(id, meta.org, viewer.email);
      } catch (error) {
        logger.error?.("[artifact-mcp] view analytics record failed", error);
      }
    }

    let counts = null;
    let viewers = null;
    try {
      counts = views.countsFor(id);
      if (viewer.isAdmin) viewers = views.viewersFor(id);
    } catch (error) {
      logger.error?.("[artifact-mcp] view analytics shell read failed", error);
    }

    const ids = artifacts.listOrgIds(meta.org, {
      includeHidden: viewer.isAdmin,
      ownerEmail: viewer.isAdmin ? null : viewer.email
    });
    const index = ids.indexOf(id);
    const nav = {
      prevId: index > 0 ? ids[index - 1] : null,
      nextId: index >= 0 && index < ids.length - 1 ? ids[index + 1] : null,
      index: index >= 0 ? index + 1 : 1,
      total: ids.length || 1
    };
    const shellFeedback = feedback.listForArtifact(id).map((row) => {
      if (!meta.is_bundle || row.anchor_page == null) return row;
      const page = artifacts.readBundleFile(id, row.anchor_page);
      return { ...row, anchor_page_stale: !page || !isHtmlContentType(page.contentType) };
    });
    return res.set("content-type", "text/html; charset=utf-8").set("cache-control", "no-store")
      .send(pages.shell(meta, nav, reactions.get(viewer.email, id), shellFeedback, { counts, viewers }, {
        email: viewer.email,
        org: viewer.org,
        isAdmin: viewer.isAdmin
      }, orgs.colorMap()[meta.org]));
  });

  app.delete("/:id", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, meta, viewer } = allowed;
    if (!viewerCanManageArtifact(viewer, meta)) {
      return res.status(403).json({ error: "Forbidden" });
    }
    const deleted = artifacts.deleteArtifactById(id);
    if (deleted) {
      try { void Promise.resolve(thumbnails.removeArtifact(id)).catch(() => {}); } catch {}
      notify.emit("deleted", meta.org, artifactPayload(meta));
    }
    logger.info?.(`[artifact-mcp] delete ${id} (org=${meta.org}) by ${viewer.email} -> ${deleted}`);
    return res.json({ id, deleted });
  });

  app.post("/:id/react", express.json({ limit: limits.reactionJson || "8kb" }), async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, viewer } = allowed;
    try {
      return res.json(reactions.set(viewer.email, id, parseReactionInput(req.body)));
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.get("/:id/feedback", async (req, res) => {
    res.set("cache-control", "no-store");
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    return res.json(feedback.listForArtifact(allowed.id).map((row) => {
      const author = row.author || (row.author_source === "discord"
        ? {
            source: "discord",
            external_author_id: row.external_author_id,
            external_author_display: row.external_author_display
          }
        : typeof row.viewer_email === "string"
          ? { source: "artifact", viewer_email: row.viewer_email }
          : null);
      const projected = {
        ...row,
        anchor_kind: row.anchor_kind ?? null,
        anchor_node_id: row.anchor_node_id ?? null,
        anchor_quote: row.anchor_quote ?? null,
        anchor_version: anchorVersion(row)
      };
      return author ? { ...projected, author } : projected;
    }));
  });

  app.post("/:id/feedback", express.json({ limit: limits.feedbackJson || "16kb" }), async (req, res) => {
    res.set("cache-control", "no-store");
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, meta, viewer } = allowed;
    try {
      const input = validateFeedbackInput(req.body);
      const anchorPage = validateAnchorPage(artifacts, meta, input.anchor, input.anchor_page);
      const created = feedback.add({
        artifactId: id,
        org: meta.org,
        viewerEmail: viewer.email,
        body: input.body,
        artifactRevision: meta.revision,
        anchor: input.anchor,
        anchorPage,
        parentId: input.parent_id
      });
      logger.info?.(`[artifact-mcp] feedback ${created.id} on ${id} (org=${meta.org}) by ${viewer.email}`);
      notify.emit("feedback", meta.org, {
        ...artifactPayload(meta), viewerEmail: viewer.email, body: created.body
      });
      return res.status(201).json({
        id: created.id,
        artifact_id: id,
        viewer_email: created.viewer_email,
        author: created.author || {
          source: "artifact",
          viewer_email: created.viewer_email
        },
        body: created.body,
        parent_id: created.parent_id,
        anchor_path: created.anchor_path,
        anchor_x: created.anchor_x,
        anchor_y: created.anchor_y,
        anchor_w: created.anchor_w,
        anchor_h: created.anchor_h,
        anchor_approx: created.anchor_approx,
        anchor_page: created.anchor_page,
        anchor_kind: created.anchor_kind ?? null,
        anchor_node_id: created.anchor_node_id ?? null,
        anchor_quote: created.anchor_quote ?? null,
        anchor_version: anchorVersion(created),
        artifact_revision: created.artifact_revision,
        created_at: created.created_at
      });
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.delete("/:id/feedback/:fid", async (req, res) => {
    res.set("cache-control", "no-store");
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, meta, viewer } = allowed;
    const row = feedback.getFeedback(req.params.fid);
    if (!row || row.artifact_id !== id || row.org !== meta.org) return res.status(404).json({ error: "Not found" });
    const result = feedback.deleteFeedback(req.params.fid, { viewerEmail: viewer.email, isAdmin: viewer.isAdmin });
    if (!result.ok) return res.status(result.reason === "forbidden" ? 403 : 404).json({ error: result.reason === "forbidden" ? "Forbidden" : "Not found" });
    logger.info?.(`[artifact-mcp] feedback delete ${req.params.fid} on ${id} by ${viewer.email}`);
    return res.json({ id: req.params.fid, deleted: true });
  });

  app.post("/:id/feedback/:fid/resolve", async (req, res) => {
    res.set("cache-control", "no-store");
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, meta, viewer } = allowed;
    const row = feedback.getFeedback(req.params.fid);
    if (!row || row.artifact_id !== id || row.org !== meta.org) return res.status(404).json({ error: "Not found" });
    const result = feedback.resolveByViewer(req.params.fid, { viewerEmail: viewer.email, isAdmin: viewer.isAdmin });
    if (!result.ok) return res.status(result.reason === "forbidden" ? 403 : 404).json({ error: result.reason === "forbidden" ? "Forbidden" : "Not found" });
    // Only notify on an actual resolve transition, not a retried resolve of an already-resolved item.
    if (result.changed) notify.emit("resolved", meta.org, { ...artifactPayload(meta), resolver: viewer.isAdmin ? `admin:${viewer.email}` : viewer.email });
    logger.info?.(`[artifact-mcp] feedback resolve ${req.params.fid} on ${id} by ${viewer.email}`);
    return res.json({ id: req.params.fid, resolved: true });
  });

  app.post("/:id/category", express.json({ limit: limits.categoryJson || "8kb" }), async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, viewer } = allowed;
    if (!req.body || !Object.hasOwn(req.body, "category")) {
      return res.status(400).json({ error: "category is required" });
    }
    const result = artifacts.setCategory(id, req.body?.category);
    // Best-effort: register the category on the org so it appears in the Settings picker, exactly
    // as the MCP set_category tool does. Without this a category assigned through the web UI never
    // reaches org_categories, so it is invisible in Settings.
    if (result.category) {
      const meta = artifacts.getArtifactMeta(id);
      if (meta) { try { orgs.addCategory(meta.org, result.category); } catch {} }
    }
    logger.info?.(`[artifact-mcp] category ${id} -> "${result.category}" by ${viewer.email}`);
    return res.json({ id, category: result.category });
  });

  app.post("/:id/share", express.json({ limit: limits.categoryJson || "8kb" }), async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, meta, viewer } = allowed;
    try {
      const share = shares.create({ artifactId: id, org: meta.org, createdBy: viewer.email, expires: req.body?.expires });
      return res.json({ ...share, url: `${publicBase}/s/${share.token}` });
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.get("/:id/shares", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    return res.json({ shares: shares.listForArtifact(allowed.id) });
  });

  app.delete("/:id/shares/:token", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id } = allowed;
    if (!shares.revoke(id, req.params.token)) return res.status(404).json({ error: "Not found" });
    return res.json({ token: req.params.token, revoked: true });
  });

  app.post("/:id/visibility", express.json({ limit: limits.categoryJson || "8kb" }), async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, meta, viewer } = allowed;
    if (typeof req.body?.hidden !== "boolean") return res.status(400).json({ error: "hidden must be a boolean" });
    // Same-org access remains concealed by artifactApiOr404. Delete and visibility deliberately
    // share one administrator-or-immutable-owner policy.
    if (!viewerCanManageArtifact(viewer, meta)) {
      return res.status(403).json({ error: "Forbidden" });
    }
    const result = artifacts.setHidden(id, req.body.hidden);
    logger.info?.(`[artifact-mcp] visibility ${id} -> ${result.hidden ? "hidden" : "visible"} by ${viewer.email}`);
    return res.json({ id, hidden: result.hidden });
  });

  app.post("/:id/move", express.json({ limit: limits.categoryJson || "8kb" }), async (req, res) => {
    // Conceal first, then check the role: a cross-org caller must get the plain 404 (the
    // artifact's existence is what has to stay secret), while a same-org non-admin — who can
    // already see the artifact — gets the role answer, 403.
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, viewer } = allowed;
    const decision = adminAccess(viewer);
    if (!decision.ok) return jsonError(res, decision);
    if (!req.body || (!Object.hasOwn(req.body, "org") && !Object.hasOwn(req.body, "category"))) {
      return res.status(400).json({ error: "org or category is required" });
    }
    try {
      const result = req.body?.org !== undefined
        ? artifacts.moveArtifactToOrg(id, req.body.org, req.body.category)
        : artifacts.setCategory(id, req.body?.category);
      if (!result.ok) return res.status(404).json({ error: "Not found" });
      const current = artifacts.getArtifactMeta(id);
      // Register the resulting category on the artifact's (possibly new) org, same as above.
      if (current?.category) { try { orgs.addCategory(current.org, current.category); } catch {} }
      logger.info?.(`[artifact-mcp] move ${id} -> ${current.org}/${current.category} by ${viewer.email}`);
      return res.json({ id, org: current.org, category: current.category });
    } catch (error) {
      return res.status(400).json({ error: String(error.message || error) });
    }
  });

  app.get("/:id/history", async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, meta } = allowed;
    return res.json(artifacts.listRevisions(id) || { current: meta.revision, revisions: [] });
  });

  app.post("/:id/restore", express.json({ limit: limits.categoryJson || "8kb" }), async (req, res) => {
    const allowed = await artifactApiOr404(req, res);
    if (!allowed) return;
    const { id, meta, viewer } = allowed;
    const revision = Number(req.body?.revision);
    if (!Number.isInteger(revision) || revision < 1) {
      return res.status(400).json({ error: "revision must be a positive integer" });
    }
    const result = artifacts.restoreArtifactRevision(id, revision);
    if (!result.ok) {
      const status = { not_found: 404, revision_not_found: 404, body_missing: 410, type_mismatch: 409 }[result.reason] || 400;
      return res.status(status).json({ error: result.reason || "restore failed" });
    }
    logger.info?.(`[artifact-mcp] restore ${id} -> rev ${result.revision} (from ${result.restoredFrom}) by ${viewer.email}`);
    const updatedMeta = artifacts.getArtifactMeta(id) || { ...meta, revision: result.revision, bytes: result.bytes };
    notify.emit("restored", meta.org, artifactPayload(updatedMeta), { artifactMeta: updatedMeta });
    return res.json({ id, revision: result.revision, restoredFrom: result.restoredFrom });
  });

  // Terminal error handler: return a small JSON error for body-parse failures instead of
  // Express's default (development) error page, which can expose internal paths / stack frames.
  app.use((err, _req, res, _next) => {
    if (err && err.type === "entity.too.large") return res.status(413).json({ error: "payload too large" });
    if (err instanceof SyntaxError && "body" in err) return res.status(400).json({ error: "invalid JSON" });
    return res.status(500).json({ error: "internal error" });
  });

  return app;
}

function artifactPayload(meta) {
  return {
    title: meta.title,
    url: `${process.env.PUBLIC_BASE_URL || "http://localhost:3480"}/${meta.id}`,
    description: meta.description,
    uploaderLabel: meta.uploader_label,
    category: meta.category,
    revision: meta.revision,
    bytes: meta.bytes
  };
}
