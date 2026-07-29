// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import express from "express";
import path from "node:path";
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
  mcpTelemetry = null,
  limits = {}
}) {
  const app = express();
  const telemetry = mcpTelemetry || createMcpTelemetry({ logger });
  app.disable("x-powered-by");

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
      .send(telemetry.renderPrometheus());
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
    return res.json(feedback.listForArtifact(allowed.id));
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
        body: created.body,
        parent_id: created.parent_id,
        anchor_path: created.anchor_path,
        anchor_x: created.anchor_x,
        anchor_y: created.anchor_y,
        anchor_w: created.anchor_w,
        anchor_h: created.anchor_h,
        anchor_approx: created.anchor_approx,
        anchor_page: created.anchor_page,
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
