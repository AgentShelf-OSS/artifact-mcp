// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { accessSync, constants } from "node:fs";
import { createServer } from "node:http";
import { createApp } from "./lib/app.js";
import { MCP_JSON_LIMIT } from "./lib/config.js";
import db, { ARTIFACT_DIR, seedKeysFromEnv } from "./lib/db.js";
import { sha256Hex, checkKey } from "./lib/auth.js";
import { executePreviewTask, handleMcp, validateMcpHttpRequest } from "./lib/mcp.js";
import * as artifactStore from "./lib/store.js";
import { ACCESS_IDENTITY_MODE, assertReady, resolveViewer } from "./lib/identity.js";
import { accessRetryTarget } from "./lib/access-retry.js";
import { accessSessionRetryPage, renderGallery, renderArtifactShell, notFoundPage, notSignedInPage } from "./lib/portal.js";
import { renderSettings } from "./lib/settings.js";
import { listKeys, createKey, revokeKey, updateKey, setKeyOwner, backfillKeyOwner } from "./lib/keys.js";
import * as orgs from "./lib/orgs.js";
import { getReaction, setReaction, reactionsFor, sentimentMap } from "./lib/reactions.js";
import * as views from "./lib/views.js";
import * as shares from "./lib/shares.js";
import { addFeedback, listForArtifact as feedbackForArtifact, getFeedback, deleteFeedback, resolveByViewer } from "./lib/feedback.js";
import * as webhooks from "./lib/webhooks.js";
import * as discussions from "./lib/discussions.js";
import * as notify from "./lib/notify.js";
import * as notifications from "./lib/notifications.js";
import { createArtifactPreviewNotifier, createPreviewRenderer } from "./lib/preview.js";
import { createThumbnailQueue, createThumbnailStore } from "./lib/thumbnails.js";
import {
  createPublisherAuthenticator,
  oauthConfigFromEnv
} from "./lib/oauth.js";
import { createPreviewTaskStore } from "./lib/tasks.js";
import { assertAuditReady, createAuditLedger, createAuditMetrics, systemAuditContext } from "./lib/audit.js";

const PORT = Number(process.env.PORT || 3480);
const PUBLIC_BASE = process.env.PUBLIC_BASE_URL || "http://localhost:3480";
const ACCESS_RETRY_PARAM = "cf_access_retry";
const oauthConfig = oauthConfigFromEnv();
const authenticatePublisher = createPublisherAuthenticator({
  config: oauthConfig,
  checkApiKey: checkKey
});

function withNoTransform(value) {
  const current = Array.isArray(value) ? value.join(", ") : String(value || "");
  const directives = current.split(",").map((part) => part.trim().toLowerCase());
  return directives.includes("no-transform") ? current : current ? `${current}, no-transform` : "no-transform";
}

function preventResponseTransforms(res) {
  const setHeader = res.setHeader;
  res.setHeader = function setNoTransformHeader(name, value) {
    const next = String(name).toLowerCase() === "cache-control" ? withNoTransform(value) : value;
    return setHeader.call(this, name, next);
  };
  res.setHeader("cache-control", "no-transform");

  // Node permits writeHead(..., headers) to bypass setHeader. Express currently uses
  // setHeader, but normalize the direct form too so this boundary cannot be skipped.
  const writeHead = res.writeHead;
  res.writeHead = function writeNoTransformHead(statusCode, statusMessage, headers) {
    const headerBag = typeof statusMessage === "object" && statusMessage !== null
      ? statusMessage
      : headers;
    if (Array.isArray(headerBag)) {
      for (let i = 0; i < headerBag.length - 1; i += 2) {
        if (String(headerBag[i]).toLowerCase() === "cache-control") {
          headerBag[i + 1] = withNoTransform(headerBag[i + 1]);
        }
      }
    } else if (headerBag && typeof headerBag === "object") {
      const key = Object.keys(headerBag).find((name) => name.toLowerCase() === "cache-control");
      if (key) headerBag[key] = withNoTransform(headerBag[key]);
    }
    return writeHead.apply(this, arguments);
  };
}

assertReady();
// Security mutations must never run without the durable, tamper-evident ledger key. The value is
// deliberately neither logged nor threaded through request diagnostics.
assertAuditReady();
const securityAudit = createAuditLedger({ db });
const auditIntegrity = securityAudit.verify();
if (!auditIntegrity.ok) {
  throw new Error("security audit ledger integrity verification failed; refusing to serve mutations");
}
const securityMetrics = createAuditMetrics();
const seededKeys = seedKeysFromEnv(sha256Hex);
console.log(`[artifact-mcp] seeded ${seededKeys} key(s) from env`);
if (seededKeys === 0 && listKeys().filter((k) => !k.revoked_at).length === 0) {
  console.warn(
    "[artifact-mcp] WARNING: no upload keys configured — no agent can publish yet. " +
    "Set ARTIFACT_API_KEYS in .env (see .env.example), or create a key in Settings once signed in. " +
    "See GETTING_STARTED.md Phase 2."
  );
}
if (ACCESS_IDENTITY_MODE === "jwt") {
  console.log("[artifact-mcp] Access identity: JWT-verified");
} else if (ACCESS_IDENTITY_MODE === "header-trust") {
  console.warn(
    "[artifact-mcp] WARNING: Access identity: HEADER-TRUST (unverified header; " +
    "TRUST_ACCESS_HEADERS=1 is unsafe outside loopback)"
  );
} else {
  console.log(
    "[artifact-mcp] Access identity: DISABLED (fail closed) — set CF_ACCESS_* for production " +
    "or TRUST_ACCESS_HEADERS=1 for local dev"
  );
}
const storageReport = artifactStore.auditStorage({ cleanTransient: true });
if (storageReport.recoveredPaths.length) {
  console.log(`[artifact-mcp] recovered ${storageReport.recoveredPaths.length} interrupted artifact operation(s)`);
}
if (storageReport.missingBodies.length || storageReport.orphanBodies.length) {
  console.warn(
    `[artifact-mcp] storage divergence: ${storageReport.missingBodies.length} missing body/bodies, ` +
    `${storageReport.orphanBodies.length} orphan body/bodies`
  );
}
// Reconciliation is an independently durable operational event. It has no body/path/token data;
// the count is intentionally classified only into fixed low-cardinality signal names.
if (storageReport.recoveredPaths.length) {
  securityAudit.append(systemAuditContext({ source: "reconciliation" }), {
    operation: "storage.reconcile", targetType: "storage", result: "recovered", classification: "durability_recovered"
  });
  securityMetrics.record("reconciliation_failure");
}
if (storageReport.missingBodies.length || storageReport.orphanBodies.length || storageReport.divergentBodies.length) {
  securityAudit.append(systemAuditContext({ source: "reconciliation" }), {
    operation: "storage.reconcile", targetType: "storage", result: "failure", classification: "integrity_mismatch"
  });
  securityMetrics.record("integrity_failure");
}

// Older artifacts predate the body_sha256 column and carry a blank digest. Backfill their
// content digests so they get stable thumbnail URLs instead of a permanent placeholder.
const digestBackfill = artifactStore.backfillBodyDigests();
if (digestBackfill.updated) {
  console.log(`[artifact-mcp] backfilled content digest for ${digestBackfill.updated} artifact(s)`);
}

const previewRenderer = createPreviewRenderer();
const thumbnails = createThumbnailStore({ renderer: previewRenderer });
const thumbnailQueue = createThumbnailQueue({ thumbnails });
const previewTasks = createPreviewTaskStore();
previewTasks.resume((task) => executePreviewTask(task, { preview: thumbnails }));
// The audit is best-effort cleanup and must never block core startup: a read-only or
// unwritable previews path degrades thumbnails, it does not take the service down.
try {
  const thumbnailAudit = await thumbnails.audit(artifactStore);
  if (thumbnailAudit.orphanDirs.length || thumbnailAudit.partialFiles.length || thumbnailAudit.invalidFiles.length) {
    console.log(
      `[artifact-mcp] thumbnail audit removed ${thumbnailAudit.orphanDirs.length} orphan path(s), ` +
      `${thumbnailAudit.partialFiles.length} stale/partial file(s), ${thumbnailAudit.invalidFiles.length} invalid PNG(s)`
    );
  }
} catch (error) {
  console.warn(`[artifact-mcp] thumbnail audit skipped: ${String(error?.message || error)}`);
}

const artifactNotifier = createArtifactPreviewNotifier({ artifacts: artifactStore, notify, thumbnails, thumbnailQueue });

// Queue existing single-file artifacts at low priority. The serial worker starts on a
// microtask and mutation events are always selected before remaining backfill jobs.
if (thumbnails.enabled) {
  void (async () => {
    for (const items of artifactStore.listAllGroupedByOrg({ includeHidden: true }).values()) {
      for (const meta of items) {
        if (!meta.is_bundle && meta.body_sha256 && !await thumbnails.readThumbnail(meta)) {
          // Await each low-priority job before admitting the next one: the backfill queue is
          // bounded to one pending artifact while high-priority mutation jobs can jump ahead.
          // Re-read the body at execution AND confirm the digest is still current: a concurrent
          // update between enqueue and run would otherwise pair this stale digest with newer HTML
          // and overwrite/delete the authoritative thumbnail. Returning undefined skips the job.
          await thumbnailQueue.enqueue(meta, () => {
            const current = artifactStore.getArtifactMeta(meta.id);
            if (!current || current.body_sha256 !== meta.body_sha256) return undefined;
            return artifactStore.readArtifact(meta.id)?.html;
          }, { priority: "low" });
        }
      }
    }
  })().catch((error) => console.warn(`[artifact-mcp] thumbnail backfill stopped: ${String(error?.message || error)}`));
}

const app = createApp({
  checkPublisherKey: authenticatePublisher,
  handleMcp: (payload, auth, options = {}) =>
    handleMcp(payload, auth, {
      ...options,
      notify: artifactNotifier.emit,
      preview: thumbnails,
      tasks: previewTasks
    }),
  validateMcpHttpRequest,
  resolveViewer,
  artifacts: artifactStore,
  thumbnails,
  shares,
  keys: { list: listKeys, create: createKey, revoke: revokeKey, update: updateKey, setOwner: setKeyOwner, backfillOwner: backfillKeyOwner },
  orgs: {
    list: orgs.listOrgs,
    names: orgs.listOrgNames,
    has: orgs.orgExists,
    create: orgs.createOrg,
    remove: orgs.deleteOrg,
    addDomain: orgs.addDomain,
    removeDomain: orgs.removeDomain,
    addEmailMember: orgs.addEmailMember,
    removeEmailMember: orgs.removeEmailMember,
    addCategory: orgs.addCategory,
    removeCategory: orgs.removeCategory,
    categoriesFor: orgs.categoriesFor,
    setColor: orgs.setColor,
    colorMap: orgs.colorMap
  },
  webhooks,
  discussions,
  notify: artifactNotifier,
  notifications,
  reactions: { get: getReaction, set: setReaction, forViewer: reactionsFor, sentiment: sentimentMap },
  views,
  feedback: { add: addFeedback, listForArtifact: feedbackForArtifact, getFeedback, deleteFeedback, resolveByViewer },
  pages: { gallery: renderGallery, shell: renderArtifactShell, notFound: notFoundPage, notSignedIn: notSignedInPage, settings: renderSettings },
  publicBase: PUBLIC_BASE,
  oauth: oauthConfig,
  audit: securityAudit,
  securityMetrics,
  healthCheck() {
    db.prepare("SELECT 1").get();
    accessSync(ARTIFACT_DIR, constants.R_OK | constants.W_OK);
    return { status: "ok" };
  },
  limits: {
    mcpJson: MCP_JSON_LIMIT
  }
});

const LISTEN_HOST = process.env.LISTEN_HOST || "0.0.0.0";
const server = createServer((req, res) => {
  // Cloudflare honors Cache-Control: no-transform for Email Address Obfuscation.
  // Applying it at the listener boundary preserves every body and every route's
  // existing no-store/private cache semantics without relying on route coverage.
  preventResponseTransforms(res);
  const retryTarget = accessRetryTarget(req, {
    mode: ACCESS_IDENTITY_MODE,
    param: ACCESS_RETRY_PARAM
  });
  if (retryTarget) {
    res.statusCode = 200;
    res.setHeader("content-type", "text/html; charset=utf-8");
    res.setHeader("cache-control", "no-store");
    res.setHeader("x-content-type-options", "nosniff");
    res.setHeader("referrer-policy", "no-referrer");
    res.end(accessSessionRetryPage(retryTarget));
    return;
  }
  app(req, res);
});
server.listen(PORT, LISTEN_HOST, () => console.log(`[artifact-mcp] listening on ${LISTEN_HOST}:${PORT}`));
