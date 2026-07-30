// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// Direct streamable-HTTP MCP (JSON-RPC over POST). A minimal compliant JSON-RPC server (no SDK transport):
// Express exposes plain req/res, so a minimal compliant JSON-RPC server is
// simpler and more robust than bridging the SDK transport.
import { readFileSync } from "node:fs";
import {
  publish,
  publishBundle,
  update,
  restore,
  listRevisions,
  listForClient,
  listOrgArtifacts,
  remove,
  getArtifactMeta,
  setHidden,
  setCategory,
  readArtifact,
  readBundleFile,
  readHistoryArtifact,
  readHistoryBundleFile,
  listBundleFiles,
  sanitizeBundlePath
} from "./store.js";
import {
  categoriesFor as orgCategoriesFor,
  addCategory as orgAddCategory,
  removeCategory as orgRemoveCategory,
  isValidOrgName,
  orgExists
} from "./orgs.js";
import {
  listForClient as feedbackForClient,
  listAll as feedbackListAll,
  addFeedback,
  getFeedback,
  resolveFeedback,
  reopenFeedback
} from "./feedback.js";
import {
  PUBLISH_PERMISSION_ERROR,
  concealedPublisherRead,
  publisherDeleteAccess,
  publisherWriteAccess
} from "./access.js";
import { applyUtf8Edits, pageUtf8, validateSchemaInput } from "./contracts.js";
import { emit as defaultNotify } from "./notify.js";
import { countsFor as viewCountsFor, viewersFor as viewViewersFor } from "./views.js";
import * as shares from "./shares.js";
import { TASKS_EXTENSION, taskAccessibleTo, taskWire } from "./tasks.js";

export const PROTOCOL_VERSION = "2025-06-18";
export const MODERN_PROTOCOL_VERSION = "2026-07-28";
export const SUPPORTED_PROTOCOL_VERSIONS = [MODERN_PROTOCOL_VERSION, PROTOCOL_VERSION];
const SERVER_INFO = { name: "artifact-mcp", version: "1.6.0" };
const TOOL_OUTPUT_SCHEMAS = JSON.parse(readFileSync(
  new URL("../conformance/mcp.tool-output-schemas.json", import.meta.url),
  "utf8"
));
const REVIEW_APP_HTML = readFileSync(
  new URL("../assets/mcp-review-app.html", import.meta.url),
  "utf8"
);
const MCP_APPS_EXTENSION = "io.modelcontextprotocol/ui";
const MCP_APP_MIME_TYPE = "text/html;profile=mcp-app";
const REVIEW_APP_URI = "ui://artifact-mcp/review";
const REVIEW_APP_VERSION = "1.0.0";
const REVIEW_META = "com.agentshelf.artifact-mcp/review";

const PUBLIC_BASE = process.env.PUBLIC_BASE_URL || "http://localhost:3480";

export const TOOL_DEFS = [
  {
    name: "publish_artifact",
    description:
      "Publish a self-contained HTML document. Returns a public URL that renders it at your configured domain, /<id>. Provide a title and a short description for the artifact index.",
    inputSchema: {
      type: "object",
      properties: {
        html: { type: "string", description: "Full self-contained HTML document to host." },
        title: { type: "string", description: "Short title shown on the artifact index." },
        description: { type: "string", description: "One-line description shown next to the link on the index." },
        category: { type: "string", description: "Optional category to group the artifact within its org (e.g. 'Dashboards'). Blank = Uncategorized." },
        org: { type: "string", description: "Target org (admin keys only; org keys are locked to their own org)." }
      },
      required: ["html"],
      additionalProperties: false
    }
  },
  {
    name: "publish_bundle",
    description:
      "Publish a multi-file artifact (e.g. several HTML pages that link to each other and a shared stylesheet). Provide files as a map of relative-path -> file contents; relative links between files resolve. Returns a public URL. Use this instead of publish_artifact when the HTML references other files like _shared.css or additional pages.",
    inputSchema: {
      type: "object",
      properties: {
        files: {
          type: "object",
          description: "Map of relative path to file contents, e.g. {\"index.html\":\"...\",\"_shared.css\":\"...\"}. Paths are relative; no leading slash or '..'.",
          additionalProperties: { type: "string" }
        },
        entry: { type: "string", description: "The HTML file to open first. Defaults to index.html, or the first .html file." },
        title: { type: "string", description: "Short title shown on the artifact index." },
        description: { type: "string", description: "One-line description shown on the index." },
        category: { type: "string", description: "Optional category to group the artifact within its org. Blank = Uncategorized." },
        org: { type: "string", description: "Target org (admin keys only)." }
      },
      required: ["files"],
      additionalProperties: false
    }
  },
  {
    name: "list_artifacts",
    description: "List artifacts available to this API key: organization-wide for reader/collaborator keys, own-only for author keys, with URLs and uploader labels.",
    inputSchema: { type: "object", properties: {}, additionalProperties: false }
  },
  {
    name: "delete_artifact",
    description: "Delete one of your artifacts by id.",
    inputSchema: {
      type: "object",
      properties: { id: { type: "string", description: "Artifact id to delete." } },
      required: ["id"],
      additionalProperties: false
    }
  },
  {
    name: "update_artifact",
    description:
      "Replace an existing artifact's content and/or metadata in place, keeping the SAME id and URL so existing links keep working. Pass `html` for a single-file artifact or `files` for a bundle — the artifact type cannot change. Omitted title/description are preserved. Each effective change increments the artifact's revision.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Artifact id to update." },
        html: { type: "string", description: "New HTML for a single-file artifact." },
        files: {
          type: "object",
          description: "New complete bundle snapshot (relative path -> content) for a bundle artifact; omitted files are removed.",
          additionalProperties: { type: "string" }
        },
        entry: { type: "string", description: "Entry file for a bundle (defaults to the current entry, then index.html)." },
        title: { type: "string", description: "New title (omit to keep the current one)." },
        description: { type: "string", description: "New description (omit to keep current; empty string clears it)." },
        category: { type: "string", description: "New category (omit to keep current; empty string moves it to Uncategorized)." },
        expected_revision: { type: "number", description: "Optional current revision; the update is rejected if the artifact has changed." }
      },
      required: ["id"],
      additionalProperties: false
    }
  },
  {
    name: "set_visibility",
    description: "Unlist or relist one of your artifacts. Hidden artifacts remain accessible by direct URL to organization members; this is not access control.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Artifact id." },
        hidden: { type: "boolean", description: "True unlists it from the gallery; false relists it." }
      },
      required: ["id", "hidden"],
      additionalProperties: false
    }
  },
  {
    name: "list_categories",
    description: "List the categories registered for your organization (used to group artifacts in the gallery). Admin keys may pass an org.",
    inputSchema: {
      type: "object",
      properties: { org: { type: "string", description: "Org to list (admin keys only; defaults to your org)." } },
      additionalProperties: false
    }
  },
  {
    name: "set_category",
    description: "Move one of your artifacts into a category (empty string = Uncategorized). Also adds the category to your org's list so it appears in the picker. Does NOT create a new revision.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Artifact id." },
        category: { type: "string", description: "Target category; empty string moves it to Uncategorized." }
      },
      required: ["id", "category"],
      additionalProperties: false
    }
  },
  {
    name: "create_category",
    description: "Add a category to your organization's category list. Admin keys may pass an org.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Category name." },
        org: { type: "string", description: "Org (admin keys only; defaults to your org)." }
      },
      required: ["name"],
      additionalProperties: false
    }
  },
  {
    name: "delete_category",
    description: "Remove a category from your organization's category list. Artifacts already tagged with it keep their tag. Admin keys may pass an org.",
    inputSchema: {
      type: "object",
      properties: {
        name: { type: "string", description: "Category name to remove." },
        org: { type: "string", description: "Org (admin keys only; defaults to your org)." }
      },
      required: ["name"],
      additionalProperties: false
    }
  },
  {
    name: "list_revisions",
    description:
      "List the version history of one of your artifacts — each retained revision's number, title, size, and timestamp. Use with restore_artifact to roll back.",
    inputSchema: {
      type: "object",
      properties: { id: { type: "string", description: "Artifact id." } },
      required: ["id"],
      additionalProperties: false
    }
  },
  {
    name: "create_share",
    description: "Create an unlisted public, read-only share link for one of your artifacts. It serves the live artifact until it expires or is revoked.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Artifact id." },
        expires: { type: "string", description: "'24h', 'never', or a future ISO date." }
      },
      required: ["id", "expires"],
      additionalProperties: false
    }
  },
  {
    name: "list_shares",
    description: "List active public share links for one of your artifacts.",
    inputSchema: {
      type: "object",
      properties: { id: { type: "string", description: "Artifact id." } },
      required: ["id"],
      additionalProperties: false
    }
  },
  {
    name: "revoke_share",
    description: "Revoke an active public share link you own. Revocation takes effect immediately.",
    inputSchema: {
      type: "object",
      properties: { token: { type: "string", description: "Share token returned by create_share or list_shares." } },
      required: ["token"],
      additionalProperties: false
    }
  },
  {
    name: "artifact_stats",
    description: "Get named audience-view analytics for one of your artifacts: total views, unique viewers, last viewed time, and each viewer's count and timestamps.",
    inputSchema: {
      type: "object",
      properties: { id: { type: "string", description: "Artifact id." } },
      required: ["id"],
      additionalProperties: false
    }
  },
  {
    name: "restore_artifact",
    description:
      "Restore a past revision of your artifact by number. Its content is re-published as a NEW revision at the same id/URL, so nothing is lost and the restore is itself undoable. Get revision numbers from list_revisions.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Artifact id." },
        revision: { type: "number", description: "Revision number to restore (from list_revisions)." }
      },
      required: ["id", "revision"],
      additionalProperties: false
    }
  },
  {
    name: "list_feedback",
    description: "List viewer feedback left on your artifacts. Pass an artifact id to scope to one; omit to list across all of your artifacts.",
    inputSchema: {
      type: "object",
      properties: { id: { type: "string", description: "Optional artifact id to scope the feedback to." } },
      additionalProperties: false
    }
  },
  {
    name: "resolve_feedback",
    description: "Mark a piece of viewer feedback as resolved once you've addressed it.",
    inputSchema: {
      type: "object",
      properties: { feedback_id: { type: "string", description: "Feedback id to resolve." } },
      required: ["feedback_id"],
      additionalProperties: false
    }
  },
  {
    name: "reopen_feedback",
    description: "Reopen previously resolved viewer feedback when more work is needed.",
    inputSchema: {
      type: "object",
      properties: { feedback_id: { type: "string", description: "Feedback id to reopen." } },
      required: ["feedback_id"],
      additionalProperties: false
    }
  },
  {
    name: "read_artifact",
    description:
      "Read an artifact or retained revision with byte-bounded UTF-8 paging. A bundle without path returns its file listing; pass path to read one bundle file.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Artifact id to read." },
        path: { type: "string", description: "Bundle file path. Omit to list bundle files." },
        revision: { type: "integer", description: "Optional retained revision number; defaults to the current revision." },
        offset: { type: "integer", description: "UTF-8 byte offset; defaults to 0." },
        limit: { type: "integer", description: "Maximum UTF-8 bytes to return; defaults to 65536." }
      },
      required: ["id"],
      additionalProperties: false
    }
  },
  {
    name: "patch_artifact",
    description:
      "Apply an atomic batch of UTF-8 byte-safe partial edits to an artifact. Find edits must match exactly once; range offsets refer to the pre-edit content.",
    inputSchema: {
      type: "object",
      properties: {
        id: { type: "string", description: "Artifact id to patch." },
        expected_revision: { type: "integer", description: "Required current revision; stale patches are rejected." },
        path: { type: "string", description: "Bundle file path. Required for bundle artifacts; omit for single-file artifacts." },
        edits: {
          type: "array",
          description: "Atomic edits, each using either find/replace or offset/length/replace.",
          minItems: 1,
          items: {
            type: "object",
            properties: {
              find: { type: "string", description: "Exact UTF-8 text to replace; must occur exactly once." },
              length: { type: "integer", description: "UTF-8 byte length in the pre-edit content." },
              offset: { type: "integer", description: "UTF-8 byte offset in the pre-edit content." },
              replace: { type: "string", description: "Replacement text." }
            },
            required: ["replace"],
            additionalProperties: false
          }
        }
      },
      required: ["id", "expected_revision", "edits"],
      additionalProperties: false
    }
  }
];

function clientSupportsApps(msg) {
  const extension = requestMeta(msg)
    ?.["io.modelcontextprotocol/clientCapabilities"]
    ?.extensions
    ?.[MCP_APPS_EXTENSION];
  return Array.isArray(extension?.mimeTypes)
    && extension.mimeTypes.includes(MCP_APP_MIME_TYPE);
}

function clientSupportsTasks(msg) {
  const extension = requestMeta(msg)
    ?.["io.modelcontextprotocol/clientCapabilities"]
    ?.extensions
    ?.[TASKS_EXTENSION];
  return extension && typeof extension === "object" && !Array.isArray(extension);
}

function modernToolDefs(supportsApps) {
  const definitions = TOOL_DEFS.map((tool) => {
    const appCallable = [
      "list_artifacts",
      "read_artifact",
      "list_revisions",
      "set_visibility",
      "delete_artifact",
      "create_share"
    ].includes(tool.name);
    const resourceLinked = [
      "publish_artifact",
      "publish_bundle",
      "list_artifacts",
      "read_artifact"
    ].includes(tool.name);
    return {
    ...tool,
    outputSchema: TOOL_OUTPUT_SCHEMAS[tool.name],
    ...(supportsApps
      ? {
          _meta: {
            ui: {
              visibility: appCallable ? ["model", "app"] : ["model"],
              ...(resourceLinked ? { resourceUri: REVIEW_APP_URI } : {})
            }
          }
        }
      : {})
    };
  });
  definitions.push({
    name: "regenerate_artifact_preview",
    description:
      "Regenerate the current thumbnail for an artifact you own. Administrators may target any artifact. Task-capable clients receive a durable task; other modern clients receive a bounded synchronous result.",
    inputSchema: {
      type: "object",
      properties: {
        id: {
          type: "string",
          description: "Artifact id whose current preview should be regenerated."
        }
      },
      required: ["id"],
      additionalProperties: false
    },
    outputSchema: TOOL_OUTPUT_SCHEMAS.regenerate_artifact_preview,
    ...(supportsApps ? { _meta: { ui: { visibility: ["model"] } } } : {})
  });
  if (supportsApps) {
    definitions.push({
      name: "submit_feedback",
      description: "Submit feedback on an authorized artifact from the trusted inline review app.",
      inputSchema: {
        type: "object",
        properties: {
          id: { type: "string", description: "Artifact id being reviewed." },
          body: { type: "string", description: "Feedback body." }
        },
        required: ["id", "body"],
        additionalProperties: false
      },
      outputSchema: TOOL_OUTPUT_SCHEMAS.submit_feedback,
      _meta: { ui: { visibility: ["app"] } }
    });
  }
  return definitions;
}

function feedbackJson(f) {
  return {
    id: f.id,
    artifact_id: f.artifact_id,
    parent_id: f.parent_id,
    viewer_email: f.viewer_email,
    body: f.body,
    artifact_revision: f.artifact_revision,
    anchor_path: f.anchor_path,
    anchor_x: f.anchor_x,
    anchor_y: f.anchor_y,
    anchor_w: f.anchor_w,
    anchor_h: f.anchor_h,
    anchor_approx: f.anchor_approx,
    anchor_page: f.anchor_page,
    created_at: f.created_at,
    resolved_at: f.resolved_at,
    resolved_by: f.resolved_by
  };
}

function urlFor(id) {
  return `${PUBLIC_BASE}/${id}`;
}

function artifactPayload(meta) {
  return {
    title: meta.title,
    url: urlFor(meta.id),
    description: meta.description,
    uploaderLabel: meta.uploader_label,
    category: meta.category,
    revision: meta.revision,
    bytes: meta.bytes
  };
}

function toolResult(obj) {
  return { content: [{ type: "text", text: JSON.stringify(obj) }], structuredContent: obj };
}

async function validatedToolCall(
  params,
  auth,
  notify,
  {
    modern = false,
    allowAppTools = false,
    allowTasks = false,
    preview,
    tasks
  } = {}
) {
  const result = await callTool(params, auth, notify, {
    modern,
    allowAppTools,
    allowTasks,
    preview,
    tasks
  });
  if (result?.resultType === "task") return result;
  const name = params?.name;
  const schema = TOOL_OUTPUT_SCHEMAS[name];
  if (!schema) {
    throw Object.assign(new Error(`missing output schema for tool: ${name}`), { rpcCode: -32603 });
  }
  const errors = validateSchemaInput(schema, result?.structuredContent)
    .map((error) => error.startsWith("arguments")
      ? error.replace("arguments", "structuredContent")
      : `structuredContent.${error}`);
  if (errors.length) {
    throw Object.assign(
      new Error(`tool ${name} output failed validation: ${errors.join("; ")}`),
      { rpcCode: -32603 }
    );
  }
  return result;
}

function rpcError(id, code, message, data) {
  return {
    jsonrpc: "2.0",
    id,
    error: { code, message, ...(data === undefined ? {} : { data }) }
  };
}

function publishOrg(args, auth) {
  if (auth.org !== "admin" || typeof args.org !== "string" || !args.org.trim()) return auth.org;
  const org = args.org.trim().toLowerCase();
  if (!isValidOrgName(org) || !orgExists(org)) {
    throw new Error(`Unknown organization "${org}". Create it in the Organizations section first.`);
  }
  return org;
}

function previewSource(id, auth) {
  const decision = publisherDeleteAccess(auth, getArtifactMeta(id), id);
  if (!decision.ok) throw new Error(decision.error);
  const meta = decision.artifact;
  if (meta.is_bundle) {
    return {
      meta,
      html: null,
      reason: "Preview regeneration supports single-file artifacts only"
    };
  }
  const artifact = readArtifact(id);
  if (!artifact) throw new Error("Artifact body is unavailable");
  return { meta, html: artifact.html, reason: null };
}

function previewToolResult(id, regenerated, digest, reason) {
  return toolResult({
    id,
    regenerated,
    digest,
    ...(reason ? { reason } : {})
  });
}

async function regeneratePreview(id, auth, preview, { task = false } = {}) {
  const source = previewSource(id, auth);
  const unavailable = source.reason || (!preview?.enabled ? "Preview renderer is disabled" : null);
  if (unavailable) {
    if (!task) return previewToolResult(id, false, source.meta.body_sha256 || "", unavailable);
    throw Object.assign(new Error(unavailable), { publicMessage: unavailable });
  }
  await preview.removeArtifact(id);
  const png = await preview.ensureThumbnail(source.meta, source.html);
  if (!png) {
    const message = "Preview renderer did not produce a valid PNG";
    if (!task) return previewToolResult(id, false, source.meta.body_sha256 || "", message);
    throw Object.assign(new Error(message), { publicMessage: message });
  }
  return previewToolResult(id, true, source.meta.body_sha256 || "");
}

export async function executePreviewTask(task, { preview }) {
  return regeneratePreview(
    task.artifactId,
    {
      clientId: task.clientId,
      org: task.org,
      role: task.role,
      label: "durable-preview-task"
    },
    preview,
    { task: true }
  );
}

async function callTool(
  params,
  auth,
  notify = defaultNotify,
  {
    modern = false,
    allowAppTools = false,
    allowTasks = false,
    preview,
    tasks
  } = {}
) {
  const name = params?.name;
  const definition = (modern ? modernToolDefs(allowAppTools) : TOOL_DEFS)
    .find((tool) => tool.name === name);
  if (!definition) {
    throw Object.assign(new Error(`Unknown tool: ${name}`), { rpcCode: -32602 });
  }
  const args = params?.arguments === undefined ? {} : params.arguments;
  const inputErrors = validateSchemaInput(definition.inputSchema, args);
  if (inputErrors.length) {
    throw Object.assign(new Error(`Invalid arguments: ${inputErrors.join("; ")}`), { rpcCode: -32602 });
  }
  const clientId = auth.clientId;
  const artifactOrRefuse = (id, decide) => {
    const decision = decide(auth, getArtifactMeta(id), id);
    if (!decision.ok) throw new Error(decision.error);
    return decision.artifact;
  };
  const readArtifactOrConceal = (id) => artifactOrRefuse(id, concealedPublisherRead);
  const writeArtifactOrRefuse = (id) => artifactOrRefuse(id, publisherWriteAccess);
  const deleteArtifactOrRefuse = (id) => artifactOrRefuse(id, publisherDeleteAccess);

  if (name === "regenerate_artifact_preview" && modern) {
    const source = previewSource(args.id, auth);
    if (!allowTasks || !tasks || source.reason || !preview?.enabled) {
      return regeneratePreview(args.id, auth, preview);
    }
    const task = tasks.create({ artifactId: args.id, auth });
    tasks.start(task.taskId, (record) => executePreviewTask(record, { preview }));
    return taskWire(task, { creation: true });
  }

  if (name === "publish_artifact") {
    if (auth.org !== "admin" && auth.role === "reader") throw new Error(PUBLISH_PERMISSION_ERROR);
    // Org is fixed by the key, except an 'admin' key may target any org explicitly.
    const org = publishOrg(args, auth);
    const { id, bytes } = publish({
      clientId,
      org,
      // Owner attribution is read only from the authenticated key record.  The MCP schema has
      // no owner field, so a caller cannot spoof or transfer this authorization snapshot.
      ownerEmail: auth.ownerEmail,
      uploaderLabel: auth.label || "",
      html: args.html,
      title: args.title,
      description: args.description,
      category: args.category
    });
    const meta = getArtifactMeta(id);
    if (meta) notify("published", meta.org, artifactPayload(meta), { artifactMeta: meta });
    return toolResult({ id, url: urlFor(id), org, bytes, category: meta?.category || "" });
  }

  if (name === "publish_bundle") {
    if (auth.org !== "admin" && auth.role === "reader") throw new Error(PUBLISH_PERMISSION_ERROR);
    const org = publishOrg(args, auth);
    const r = publishBundle({
      clientId,
      org,
      ownerEmail: auth.ownerEmail,
      uploaderLabel: auth.label || "",
      files: args.files,
      entry: args.entry,
      title: args.title,
      description: args.description,
      category: args.category
    });
    const meta = getArtifactMeta(r.id);
    if (meta) notify("published", meta.org, artifactPayload(meta), { artifactMeta: meta });
    return toolResult({ id: r.id, url: urlFor(r.id), org, entry: r.entry, files: r.files, bytes: r.bytes, category: meta?.category || "" });
  }

  if (name === "list_artifacts") {
    const available = auth.org !== "admin" && (auth.role === "reader" || auth.role === "collaborator")
      ? listOrgArtifacts(auth.org, { includeHidden: true })
      : listForClient(clientId, auth.org === "admin" ? undefined : auth.org);
    const rows = available.map((r) => ({
      id: r.id,
      url: urlFor(r.id),
      title: r.title,
      description: r.description,
      created_at: r.created_at,
      org: r.org,
      category: r.category,
      revision: r.revision,
      updated_at: r.updated_at,
      bytes: r.bytes,
      is_bundle: r.is_bundle,
      entry: r.entry,
      hidden: r.hidden,
      uploader_label: r.uploader_label
    }));
    return toolResult({ count: rows.length, artifacts: rows });
  }

  if (name === "read_artifact") {
    if (typeof args.id !== "string" || !args.id) {
      throw Object.assign(new Error("id is required"), { rpcCode: -32602 });
    }
    const revision = args.revision === undefined ? undefined : Number(args.revision);
    if (revision !== undefined && (!Number.isSafeInteger(revision) || revision < 1)) {
      throw Object.assign(new Error("revision must be a positive integer"), { rpcCode: -32602 });
    }
    const requestedOffset = args.offset === undefined ? 0 : Number(args.offset);
    if (!Number.isSafeInteger(requestedOffset) || requestedOffset < 0) {
      throw Object.assign(new Error("offset must be a non-negative integer"), { rpcCode: -32602 });
    }
    const limit = args.limit === undefined ? 65536 : Number(args.limit);
    if (!Number.isSafeInteger(limit) || limit < 1) {
      throw Object.assign(new Error("limit must be a positive integer"), { rpcCode: -32602 });
    }

    const current = readArtifactOrConceal(args.id);
    const useCurrent = revision === undefined || revision === current.revision;
    let selected = current;
    if (!useCurrent) {
      selected = (listRevisions(args.id)?.revisions || []).find((row) => row.revision === revision);
      if (!selected) throw new Error(`No such revision: ${revision}`);
    }

    if (selected.is_bundle) {
      if (args.path === undefined) {
        const listing = listBundleFiles(args.id, useCurrent ? undefined : revision);
        if (!listing) {
          throw new Error(useCurrent
            ? `Artifact body is unavailable: ${args.id}`
            : `Revision ${revision} is no longer retained`);
        }
        return toolResult({
          id: args.id,
          org: current.org,
          is_bundle: true,
          entry: listing.entry,
          revision: listing.revision,
          content_type: "application/json",
          bytes_total: listing.bytes,
          offset: 0,
          bytes_returned: 0,
          truncated: false,
          files: listing.files
        });
      }

      const file = useCurrent
        ? readBundleFile(args.id, args.path)
        : readHistoryBundleFile(args.id, revision, args.path);
      if (!file) throw new Error(`Unknown bundle file: ${args.path}`);
      const page = pageUtf8(file.content, requestedOffset, limit);
      return toolResult({
        id: args.id,
        org: current.org,
        is_bundle: true,
        entry: selected.entry,
        revision: selected.revision,
        content_type: file.contentType,
        ...page
      });
    }

    if (args.path !== undefined) throw new Error("path only applies to bundle artifacts");
    const artifact = useCurrent
      ? readArtifact(args.id)
      : readHistoryArtifact(args.id, revision);
    if (!artifact) {
      throw new Error(useCurrent
        ? `Artifact body is unavailable: ${args.id}`
        : `Revision ${revision} is no longer retained`);
    }
    const page = pageUtf8(artifact.html, requestedOffset, limit);
    return toolResult({
      id: args.id,
      org: current.org,
      is_bundle: false,
      revision: selected.revision,
      content_type: "text/html; charset=utf-8",
      ...page
    });
  }

  if (name === "patch_artifact") {
    if (typeof args.id !== "string" || !args.id) {
      throw Object.assign(new Error("id is required"), { rpcCode: -32602 });
    }
    const expectedRevision = Number(args.expected_revision);
    if (!Number.isSafeInteger(expectedRevision) || expectedRevision < 1) {
      throw Object.assign(new Error("expected_revision must be a positive integer"), { rpcCode: -32602 });
    }
    const unavailable = "Artifact not found or you are not authorized to update it";
    const pre = writeArtifactOrRefuse(args.id);
    if (expectedRevision !== pre.revision) {
      throw new Error("Artifact changed during update; fetch its current revision and retry");
    }
    let patched;
    let contentUpdate;
    if (pre.is_bundle) {
      if (args.path === undefined) throw new Error("path is required for bundle artifacts");
      const targetPath = args.path ? sanitizeBundlePath(args.path) : pre.entry;
      const target = readBundleFile(args.id, args.path);
      if (!targetPath || !target) throw new Error(`Unknown bundle file: ${args.path}`);
      const listing = listBundleFiles(args.id);
      if (!listing) throw new Error(`Artifact body is unavailable: ${args.id}`);
      patched = applyUtf8Edits(target.content, args.edits);
      const files = {};
      for (const file of listing.files) {
        const current = file.path === targetPath ? target : readBundleFile(args.id, file.path);
        if (!current) throw new Error(`Artifact body is unavailable: ${args.id}`);
        files[file.path] = file.path === targetPath ? patched.content : current.content.toString("utf8");
      }
      contentUpdate = { files, entry: pre.entry };
    } else {
      if (args.path !== undefined) throw new Error("path only applies to bundle artifacts");
      const artifact = readArtifact(args.id);
      if (!artifact) throw new Error(`Artifact body is unavailable: ${args.id}`);
      patched = applyUtf8Edits(artifact.html, args.edits);
      contentUpdate = { html: patched.content };
    }
    const result = update({
      id: args.id,
      clientId,
      org: auth.org === "admin" ? pre.org : auth.org,
      expectedRevision,
      isAdmin: auth.org === "admin" || auth.role === "collaborator",
      ...contentUpdate
    });
    if (!result.ok) {
      if (result.reason === "conflict") throw new Error("Artifact changed during update; fetch its current revision and retry");
      throw new Error(unavailable);
    }
    const meta = getArtifactMeta(result.id);
    if (result.changed && meta) notify("updated", meta.org, artifactPayload(meta), { artifactMeta: meta });
    return toolResult({
      id: result.id,
      revision: result.revision,
      bytes_before: pre.bytes,
      bytes_after: result.bytes,
      edits_applied: args.edits.length
    });
  }

  if (name === "delete_artifact") {
    if (typeof args.id !== "string" || !args.id) {
      throw Object.assign(new Error("id is required"), { rpcCode: -32602 });
    }
    const meta = deleteArtifactOrRefuse(args.id);
    const ok = remove({ id: args.id, clientId, isAdmin: auth.org === "admin" });
    if (ok && meta) notify("deleted", meta.org, artifactPayload(meta), { artifactMeta: meta });
    return toolResult({ id: args.id, deleted: ok });
  }

  if (name === "update_artifact") {
    if (typeof args.id !== "string" || !args.id) {
      throw Object.assign(new Error("id is required"), { rpcCode: -32602 });
    }
    const expectedRevision = args.expected_revision === undefined ? undefined : Number(args.expected_revision);
    if (expectedRevision !== undefined && (!Number.isInteger(expectedRevision) || expectedRevision < 1)) {
      throw Object.assign(new Error("expected_revision must be a positive integer"), { rpcCode: -32602 });
    }
    const pre = writeArtifactOrRefuse(args.id);
    const unavailable = "Artifact not found or you are not authorized to update it";
    const result = update({
      id: args.id,
      clientId,
      org: auth.org === "admin" ? pre.org : auth.org,
      expectedRevision: expectedRevision === undefined ? pre.revision : expectedRevision,
      isAdmin: auth.org === "admin" || auth.role === "collaborator",
      html: args.html,
      files: args.files,
      entry: args.entry,
      title: args.title,
      description: args.description,
      category: args.category
    });
    if (!result.ok) {
      if (result.reason === "conflict") throw new Error("Artifact changed during update; fetch its current revision and retry");
      throw new Error(unavailable);
    }
    const meta = getArtifactMeta(result.id);
    if (result.changed && meta) notify("updated", meta.org, artifactPayload(meta), { artifactMeta: meta });
    return toolResult({ id: result.id, url: urlFor(result.id), revision: result.revision, bytes: result.bytes, entry: result.entry, category: result.category });
  }

  if (name === "set_visibility") {
    deleteArtifactOrRefuse(args.id);
    const result = setHidden(args.id, args.hidden);
    return toolResult({ id: result.id, hidden: result.hidden });
  }

  if (name === "list_categories") {
    const targetOrg = auth.org === "admin" ? String(args.org || "").trim() : auth.org;
    if (!targetOrg) throw new Error("org is required for admin keys");
    return toolResult({ org: targetOrg, categories: orgCategoriesFor(targetOrg) });
  }

  if (name === "set_category") {
    const meta = writeArtifactOrRefuse(args.id);
    const result = setCategory(args.id, args.category);
    // Best-effort: register the normalized category on the org so it shows in the picker.
    if (result.category) { try { orgAddCategory(meta.org, result.category); } catch {} }
    return toolResult({ id: args.id, category: result.category });
  }

  if (name === "create_category") {
    const targetOrg = auth.org === "admin" ? String(args.org || "").trim() : auth.org;
    if (!targetOrg) throw new Error("org is required for admin keys");
    return toolResult(orgAddCategory(targetOrg, args.name));
  }

  if (name === "delete_category") {
    const targetOrg = auth.org === "admin" ? String(args.org || "").trim() : auth.org;
    if (!targetOrg) throw new Error("org is required for admin keys");
    return toolResult({ org: targetOrg, name: args.name, removed: orgRemoveCategory(targetOrg, args.name) });
  }

  if (name === "list_revisions") {
    if (typeof args.id !== "string" || !args.id) {
      throw Object.assign(new Error("id is required"), { rpcCode: -32602 });
    }
    const meta = readArtifactOrConceal(args.id);
    const history = listRevisions(args.id) || { current: meta.revision, revisions: [] };
    return toolResult({ id: args.id, current: history.current, revisions: history.revisions });
  }

  if (name === "create_share") {
    const meta = deleteArtifactOrRefuse(args.id);
    const share = shares.create({ artifactId: args.id, org: meta.org, createdBy: `agent:${clientId}`, expires: args.expires });
    return toolResult({ id: args.id, ...share, url: `${PUBLIC_BASE}/s/${share.token}` });
  }

  if (name === "list_shares") {
    readArtifactOrConceal(args.id);
    return toolResult({ id: args.id, shares: shares.listForArtifact(args.id) });
  }

  if (name === "revoke_share") {
    // resolve intentionally yields null for unknown, expired, and revoked links, so this
    // management action does not turn a token probe into an oracle either.
    const share = shares.resolve(args.token);
    if (!share) throw new Error("Unknown share");
    const meta = getArtifactMeta(share.artifact_id);
    if (!meta || meta.org !== share.org) throw new Error("Unknown share");
    const decision = publisherWriteAccess(auth, meta, meta.id);
    if (!decision.ok) {
      if (decision.error.startsWith("Unknown artifact:")) throw new Error("Unknown share");
      throw new Error(decision.error);
    }
    return toolResult({ token: args.token, revoked: shares.revoke(meta.id, args.token) });
  }

  if (name === "artifact_stats") {
    readArtifactOrConceal(args.id);
    return toolResult({ id: args.id, ...viewCountsFor(args.id), viewers: viewViewersFor(args.id) });
  }

  if (name === "restore_artifact") {
    if (typeof args.id !== "string" || !args.id) {
      throw Object.assign(new Error("id is required"), { rpcCode: -32602 });
    }
    const revision = Number(args.revision);
    if (!Number.isInteger(revision) || revision < 1) {
      throw Object.assign(new Error("revision must be a positive integer"), { rpcCode: -32602 });
    }
    writeArtifactOrRefuse(args.id);
    const result = restore({
      id: args.id,
      revision,
      clientId,
      isAdmin: auth.org === "admin" || auth.role === "collaborator"
    });
    if (!result.ok) {
      const msg =
        result.reason === "not_found" ? `Unknown artifact: ${args.id}`
          : result.reason === "forbidden" ? `Unknown artifact: ${args.id}`
          : result.reason === "revision_not_found" ? `No such revision: ${revision}`
          : result.reason === "body_missing" ? `Revision ${revision} is no longer retained`
          : result.reason === "type_mismatch" ? "Revision type does not match the current artifact"
          : "Restore failed";
      throw new Error(msg);
    }
    const meta = getArtifactMeta(result.id);
    if (meta) notify("restored", meta.org, artifactPayload(meta), { artifactMeta: meta });
    return toolResult({ id: result.id, url: urlFor(result.id), revision: result.revision, restoredFrom: result.restoredFrom, bytes: result.bytes });
  }

  if (name === "list_feedback") {
    const isAdmin = auth.org === "admin";
    if (typeof args.id === "string" && args.id) {
      readArtifactOrConceal(args.id);
      const items = feedbackListAll(args.id).map(feedbackJson);
      return toolResult({ artifact_id: args.id, count: items.length, feedback: items });
    }
    const rows = isAdmin
      ? feedbackListAll()
      : (auth.role === "reader" || auth.role === "collaborator")
        ? feedbackListAll().filter((row) => row.org === auth.org)
        : feedbackForClient(clientId, undefined, auth.org);
    return toolResult({ count: rows.length, feedback: rows.map(feedbackJson) });
  }

  if (name === "submit_feedback" && allowAppTools) {
    const meta = readArtifactOrConceal(args.id);
    const created = addFeedback({
      artifactId: meta.id,
      org: meta.org,
      viewerEmail: `agent:${clientId}`,
      body: args.body,
      artifactRevision: meta.revision
    });
    notify("feedback", meta.org, {
      ...artifactPayload(meta),
      viewerEmail: `agent:${clientId}`,
      body: created.body
    }, { artifactMeta: meta });
    return toolResult({
      feedback_id: created.id,
      artifact_id: meta.id,
      revision: created.artifact_revision,
      submitted: true
    });
  }

  if (name === "resolve_feedback") {
    if (typeof args.feedback_id !== "string" || !args.feedback_id) {
      throw Object.assign(new Error("feedback_id is required"), { rpcCode: -32602 });
    }
    const fb = getFeedback(args.feedback_id);
    if (!fb) throw new Error(`Unknown feedback: ${args.feedback_id}`);
    const meta = getArtifactMeta(fb.artifact_id);
    const decision = publisherWriteAccess(auth, meta, fb.artifact_id);
    if (!decision.ok) {
      if (decision.error.startsWith("Unknown artifact:")) throw new Error(`Unknown feedback: ${args.feedback_id}`);
      throw new Error(decision.error);
    }
    const resolved = resolveFeedback(args.feedback_id, `agent:${clientId}`);
    if (resolved && meta) notify("resolved", meta.org, {
      ...artifactPayload(meta),
      resolver: `agent:${clientId}`
    });
    return toolResult({ feedback_id: args.feedback_id, resolved });
  }

  if (name === "reopen_feedback") {
    if (typeof args.feedback_id !== "string" || !args.feedback_id) {
      throw Object.assign(new Error("feedback_id is required"), { rpcCode: -32602 });
    }
    const fb = getFeedback(args.feedback_id);
    if (!fb) throw new Error(`Unknown feedback: ${args.feedback_id}`);
    const meta = getArtifactMeta(fb.artifact_id);
    const decision = publisherWriteAccess(auth, meta, fb.artifact_id);
    if (!decision.ok) {
      if (decision.error.startsWith("Unknown artifact:")) throw new Error(`Unknown feedback: ${args.feedback_id}`);
      throw new Error(decision.error);
    }
    const reopened = reopenFeedback(args.feedback_id);
    return toolResult({ feedback_id: args.feedback_id, reopened });
  }

  throw Object.assign(new Error(`Tool is not implemented: ${name}`), { rpcCode: -32603 });
}

const RESOURCE_PAGE_SIZE = 50;
const RESOURCE_MAX_BYTES = 1_048_576;

function resourceInvalid(message) {
  return Object.assign(new Error(message), { rpcCode: -32602 });
}

function resourceRows(auth) {
  return auth.org !== "admin" && (auth.role === "reader" || auth.role === "collaborator")
    ? listOrgArtifacts(auth.org, { includeHidden: true })
    : listForClient(auth.clientId, auth.org === "admin" ? undefined : auth.org);
}

function resourceMeta(meta) {
  return {
    uri: `artifact://${meta.id}`,
    name: String(meta.title || "").trim() || meta.id,
    title: meta.title,
    description: meta.description,
    mimeType: meta.is_bundle
      ? "application/vnd.artifact-mcp.bundle+json"
      : "text/html",
    _meta: {
      "com.agentshelf.artifact-mcp/org": meta.org,
      "com.agentshelf.artifact-mcp/revision": meta.revision,
      "com.agentshelf.artifact-mcp/hidden": Boolean(meta.hidden),
      "com.agentshelf.artifact-mcp/updatedAt": meta.updated_at
    }
  };
}

function encodeResourceCursor(offset) {
  return Buffer.from(String(offset), "utf8").toString("base64url");
}

function parseResourceCursor(value) {
  if (typeof value !== "string") throw resourceInvalid("Invalid resource cursor");
  let decoded;
  try {
    decoded = Buffer.from(value, "base64url").toString("utf8");
  } catch {
    throw resourceInvalid("Invalid resource cursor");
  }
  const offset = /^\d+$/.test(decoded) ? Number(decoded) : NaN;
  if (!Number.isSafeInteger(offset) || offset < 0 || encodeResourceCursor(offset) !== value) {
    throw resourceInvalid("Invalid resource cursor");
  }
  return offset;
}

function parseArtifactResourceUri(uri) {
  if (typeof uri !== "string" || !uri.startsWith("artifact://")) {
    throw resourceInvalid("Unsupported resource URI scheme");
  }
  const raw = uri.slice("artifact://".length);
  const [id, ...segments] = raw.split("/");
  if (!id || /[?#]/.test(id)) throw resourceInvalid("Invalid artifact resource URI");
  let revision;
  let pathSegments = [];
  let thumbnail = false;
  if (segments.length === 0) {
    // Current artifact root.
  } else if (segments.length === 1 && segments[0] === "thumbnail") {
    thumbnail = true;
  } else if (segments[0] === "files" && segments.length > 1) {
    pathSegments = segments.slice(1);
  } else if (segments[0] === "revisions" && segments.length === 2) {
    revision = Number(segments[1]);
  } else if (
    segments[0] === "revisions"
    && segments[2] === "files"
    && segments.length > 3
  ) {
    revision = Number(segments[1]);
    pathSegments = segments.slice(3);
  } else {
    throw resourceInvalid("Invalid artifact resource URI");
  }
  if (revision !== undefined && (!Number.isSafeInteger(revision) || revision < 1)) {
    throw resourceInvalid("Invalid artifact revision");
  }
  let resourcePath;
  if (pathSegments.length) {
    try {
      resourcePath = pathSegments.map((segment) => decodeURIComponent(segment)).join("/");
    } catch {
      throw resourceInvalid("Invalid percent-encoding in resource URI");
    }
    resourcePath = sanitizeBundlePath(resourcePath);
    if (!resourcePath) throw resourceInvalid("Invalid artifact bundle path");
  }
  return { id, revision, path: resourcePath, thumbnail };
}

function authorizedResourceMeta(auth, id) {
  const decision = concealedPublisherRead(auth, getArtifactMeta(id), id);
  if (!decision.ok) throw resourceInvalid(decision.error);
  return decision.artifact;
}

function boundedResourceText(file) {
  const content = Buffer.isBuffer(file.content) ? file.content : Buffer.from(file.content, "utf8");
  if (content.length > RESOURCE_MAX_BYTES) {
    throw resourceInvalid(
      `Resource exceeds the ${RESOURCE_MAX_BYTES}-byte read limit; use the read_artifact tool for byte-paged access`
    );
  }
  return {
    mimeType: file.contentType,
    text: content.toString("utf8"),
    bytes: content.length
  };
}

function listResourceTemplates() {
  return {
    resourceTemplates: [
      {
        uriTemplate: "artifact://{id}",
        name: "Artifact",
        description: "Current authorized artifact content or bundle file listing."
      },
      {
        uriTemplate: "artifact://{id}/revisions/{revision}",
        name: "Artifact revision",
        description: "One retained revision of an authorized artifact."
      },
      {
        uriTemplate: "artifact://{id}/files/{+path}",
        name: "Artifact file",
        description: "One file from the current authorized bundle."
      },
      {
        uriTemplate: "artifact://{id}/revisions/{revision}/files/{+path}",
        name: "Artifact revision file",
        description: "One file from a retained authorized bundle revision."
      },
      {
        uriTemplate: "artifact://{id}/thumbnail",
        name: "Artifact thumbnail",
        description: "Authorized server-owned thumbnail or safe placeholder for an artifact."
      }
    ],
    ttlMs: 300_000,
    cacheScope: "private"
  };
}

function reviewAppDescriptor() {
  return {
    uri: REVIEW_APP_URI,
    name: "Artifact review",
    title: "Artifact review",
    description: "Trusted inline metadata and thumbnail review for authorized artifacts.",
    mimeType: MCP_APP_MIME_TYPE,
    _meta: {
      "com.agentshelf.artifact-mcp/appVersion": REVIEW_APP_VERSION,
      ui: {
        csp: {
          connectDomains: [],
          resourceDomains: [],
          frameDomains: [],
          baseUriDomains: []
        },
        prefersBorder: true
      }
    }
  };
}

function reviewAppResource() {
  return {
    contents: [{
      uri: REVIEW_APP_URI,
      mimeType: MCP_APP_MIME_TYPE,
      text: REVIEW_APP_HTML,
      _meta: reviewAppDescriptor()._meta
    }],
    ttlMs: 3_600_000,
    cacheScope: "public"
  };
}

function listResources(msg, auth) {
  const params = msg.params;
  const rows = resourceRows(auth);
  const offset = params?.cursor === undefined ? 0 : parseResourceCursor(params.cursor);
  if (offset > rows.length) throw resourceInvalid("Invalid resource cursor");
  const end = Math.min(offset + RESOURCE_PAGE_SIZE, rows.length);
  const resources = rows.slice(offset, end).map(resourceMeta);
  if (offset === 0 && clientSupportsApps(msg)) resources.unshift(reviewAppDescriptor());
  return {
    resources,
    ...(end < rows.length ? { nextCursor: encodeResourceCursor(end) } : {}),
    ttlMs: 60_000,
    cacheScope: "private"
  };
}

function fallbackThumbnail(meta) {
  const title = String(meta.title || meta.id || "Artifact")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
  return Buffer.from(
    `<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="750" viewBox="0 0 1200 750">`
    + `<rect width="1200" height="750" fill="#edf1f5"/><rect x="56" y="56" width="8" height="638" fill="#2e6da7"/>`
    + `<text x="104" y="336" fill="#142338" font-family="Georgia,serif" font-size="62" font-weight="700">${title}</text>`
    + `<text x="104" y="402" fill="#63738a" font-family="system-ui,sans-serif" font-size="25">Artifact preview</text></svg>`,
    "utf8"
  );
}

async function readResource(msg, auth, preview) {
  const params = msg.params;
  if (typeof params?.uri !== "string") throw resourceInvalid("uri is required");
  if (params.uri === REVIEW_APP_URI) {
    if (!clientSupportsApps(msg)) {
      throw resourceInvalid("MCP Apps support was not negotiated for this request");
    }
    return reviewAppResource();
  }
  const target = parseArtifactResourceUri(params.uri);
  const current = authorizedResourceMeta(auth, target.id);
  if (target.thumbnail) {
    let bytes = null;
    if (!current.is_bundle && current.body_sha256 && preview?.readThumbnail) {
      bytes = await preview.readThumbnail(current, current.body_sha256);
    }
    const mimeType = bytes ? "image/png" : "image/svg+xml";
    bytes ||= preview?.placeholder
      ? preview.placeholder(current)
      : fallbackThumbnail(current);
    bytes = Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes);
    return {
      contents: [{
        uri: params.uri,
        mimeType,
        blob: bytes.toString("base64"),
        _meta: {
          "com.agentshelf.artifact-mcp/org": current.org,
          "com.agentshelf.artifact-mcp/revision": current.revision,
          "com.agentshelf.artifact-mcp/bytes": bytes.length,
          "com.agentshelf.artifact-mcp/trustedThumbnail": true
        }
      }],
      ttlMs: 30_000,
      cacheScope: "private"
    };
  }
  const historical = target.revision !== undefined && target.revision !== current.revision
    ? target.revision
    : undefined;
  let selected = current;
  if (historical !== undefined) {
    selected = (listRevisions(target.id)?.revisions || [])
      .find((candidate) => candidate.revision === historical);
    if (!selected) throw resourceInvalid(`No such artifact revision: ${historical}`);
  }

  let content;
  if (selected.is_bundle) {
    if (target.path) {
      const file = historical === undefined
        ? readBundleFile(target.id, target.path)
        : readHistoryBundleFile(target.id, historical, target.path);
      if (!file) throw resourceInvalid(`Unknown artifact bundle file: ${target.path}`);
      content = boundedResourceText(file);
    } else {
      const listing = listBundleFiles(target.id, historical);
      if (!listing) {
        throw resourceInvalid(`Artifact revision content is unavailable: ${selected.revision}`);
      }
      const text = JSON.stringify({
        id: current.id,
        revision: selected.revision,
        entry: selected.entry,
        bytes: selected.bytes,
        files: listing.files.map(({ path, bytes }) => ({ path, bytes }))
      });
      content = {
        mimeType: "application/vnd.artifact-mcp.bundle+json",
        text,
        bytes: Buffer.byteLength(text)
      };
    }
  } else {
    if (target.path) throw resourceInvalid("Artifact file URIs apply only to bundle artifacts");
    const file = historical === undefined
      ? readArtifact(target.id)
      : readHistoryArtifact(target.id, historical);
    if (!file) {
      throw resourceInvalid(`Artifact revision content is unavailable: ${selected.revision}`);
    }
    content = boundedResourceText({
      content: file.html,
      contentType: "text/html; charset=utf-8"
    });
  }

  return {
    contents: [{
      uri: params.uri,
      mimeType: content.mimeType,
      text: content.text,
      _meta: {
        "com.agentshelf.artifact-mcp/org": current.org,
        "com.agentshelf.artifact-mcp/revision": selected.revision,
        "com.agentshelf.artifact-mcp/bytes": content.bytes
      }
    }],
    ttlMs: 30_000,
    cacheScope: "private"
  };
}

async function dispatchResource(msg, auth, preview) {
  try {
    switch (msg.method) {
      case "resources/list":
        return listResources(msg, auth);
      case "resources/templates/list":
        return listResourceTemplates();
      case "resources/read":
        return await readResource(msg, auth, preview);
      default:
        throw Object.assign(new Error(`Method not found: ${msg.method}`), { rpcCode: -32601 });
    }
  } catch (error) {
    if (error.rpcCode) throw error;
    throw Object.assign(new Error("resource operation failed"), { rpcCode: -32603 });
  }
}

function addArtifactResourceLink(msg, result) {
  if (![
    "publish_artifact",
    "publish_bundle",
    "update_artifact",
    "patch_artifact",
    "restore_artifact"
  ].includes(msg.params?.name)) {
    return result;
  }
  const id = result?.structuredContent?.id;
  if (typeof id !== "string" || !Array.isArray(result.content)) return result;
  return {
    ...result,
    content: [
      ...result.content,
      {
        type: "resource_link",
        uri: `artifact://${id}`,
        name: id,
        description: "Authorized artifact resource"
      }
    ]
  };
}

function reviewFromArtifact(artifact, auth) {
  const id = typeof artifact?.id === "string" ? artifact.id : "";
  if (!id) return null;
  return {
    id,
    url: typeof artifact.url === "string" ? artifact.url : urlFor(id),
    title: String(artifact.title || id),
    description: String(artifact.description || ""),
    org: String(artifact.org || ""),
    category: String(artifact.category || ""),
    publisher: String(artifact.publisher || artifact.uploader_label || ""),
    revision: Number.isSafeInteger(artifact.revision) ? artifact.revision : null,
    hidden: artifact.hidden === true || artifact.hidden === 1,
    isBundle: Boolean(artifact.is_bundle),
    canManage: publisherDeleteAccess(auth, artifact, id).ok,
    canFeedback: concealedPublisherRead(auth, artifact, id).ok,
    thumbnailResourceUri: `artifact://${id}/thumbnail`
  };
}

function addReviewAppData(msg, result, auth) {
  if (!clientSupportsApps(msg)) return result;
  const name = msg.params?.name;
  if (![
    "publish_artifact",
    "publish_bundle",
    "list_artifacts",
    "read_artifact",
    "list_revisions",
    "set_visibility",
    "delete_artifact",
    "create_share",
    "submit_feedback"
  ].includes(name)) {
    return result;
  }
  if (name === "delete_artifact") {
    const id = result?.structuredContent?.id || "";
    const deleted = result?.structuredContent?.deleted === true;
    return {
      ...result,
      _meta: {
        ...(result?._meta && typeof result._meta === "object" ? result._meta : {}),
        "com.agentshelf.artifact-mcp/audit": {
          action: "delete",
          artifactId: id,
          actor: `agent:${auth.clientId}`,
          outcome: deleted ? "deleted" : "unchanged"
        }
      }
    };
  }
  let artifacts;
  if (name === "list_artifacts") {
    artifacts = (result?.structuredContent?.artifacts || [])
      .map((artifact) => {
        const meta = typeof artifact?.id === "string" ? getArtifactMeta(artifact.id) : null;
        return meta ? reviewFromArtifact({ ...meta, url: artifact.url }, auth) : null;
      })
      .filter(Boolean);
  } else {
    const id = result?.structuredContent?.id;
    const meta = typeof id === "string" ? getArtifactMeta(id) : null;
    artifacts = meta
      ? [reviewFromArtifact({
          ...meta,
          url: result?.structuredContent?.url || urlFor(id)
        }, auth)]
      : [];
  }
  if (!artifacts.length) return result;
  return {
    ...result,
    _meta: {
      ...(result?._meta && typeof result._meta === "object" ? result._meta : {}),
      [REVIEW_META]: { artifacts }
    }
  };
}

function requestMeta(msg) {
  const meta = msg?.params?._meta;
  return meta && typeof meta === "object" && !Array.isArray(meta) ? meta : null;
}

function containsModernRequestMetadata(payload) {
  if (Array.isArray(payload)) return payload.some(containsModernRequestMetadata);
  const meta = requestMeta(payload);
  return Boolean(meta && Object.hasOwn(meta, "io.modelcontextprotocol/protocolVersion"));
}

function validateModernRequestMetadata(msg) {
  const meta = requestMeta(msg);
  if (!meta) {
    throw Object.assign(new Error("Missing required request metadata: params._meta"), { rpcCode: -32602 });
  }
  const requested = meta["io.modelcontextprotocol/protocolVersion"];
  if (typeof requested !== "string") {
    throw Object.assign(
      new Error("Missing required request metadata: io.modelcontextprotocol/protocolVersion"),
      { rpcCode: -32602 }
    );
  }
  if (requested !== MODERN_PROTOCOL_VERSION) {
    throw Object.assign(new Error("Unsupported protocol version"), {
      rpcCode: -32022,
      rpcData: { supported: SUPPORTED_PROTOCOL_VERSIONS, requested }
    });
  }
  const capabilities = meta["io.modelcontextprotocol/clientCapabilities"];
  if (!capabilities || typeof capabilities !== "object" || Array.isArray(capabilities)) {
    throw Object.assign(
      new Error("Missing required request metadata: io.modelcontextprotocol/clientCapabilities"),
      { rpcCode: -32602 }
    );
  }
}

function resultForEra(result, era) {
  if (era !== "modern" || !result || typeof result !== "object" || Array.isArray(result)) return result;
  return {
    ...result,
    resultType: result.resultType || "complete",
    _meta: {
      ...(result._meta && typeof result._meta === "object" && !Array.isArray(result._meta) ? result._meta : {}),
      "io.modelcontextprotocol/serverInfo": SERVER_INFO
    }
  };
}

function dispatchTask(msg, auth, tasks) {
  if (!clientSupportsTasks(msg)) {
    throw Object.assign(new Error("Missing required client capability"), {
      rpcCode: -32003,
      rpcData: {
        requiredCapabilities: {
          extensions: { [TASKS_EXTENSION]: {} }
        }
      }
    });
  }
  if (!tasks) throw Object.assign(new Error("Task service unavailable"), { rpcCode: -32603 });
  const taskId = msg.params?.taskId;
  if (typeof taskId !== "string" || !taskId) {
    throw Object.assign(new Error("taskId is required"), { rpcCode: -32602 });
  }
  const task = tasks.get(taskId);
  if (!task || !taskAccessibleTo(task, auth)) {
    throw Object.assign(new Error(`Unknown task: ${taskId}`), { rpcCode: -32602 });
  }
  if (msg.method === "tasks/get") return taskWire(task);
  if (msg.method === "tasks/update") {
    const responses = msg.params?.inputResponses;
    if (!responses || typeof responses !== "object" || Array.isArray(responses)) {
      throw Object.assign(new Error("inputResponses must be an object"), { rpcCode: -32602 });
    }
    return { resultType: "complete" };
  }
  if (msg.method === "tasks/cancel") {
    tasks.cancel(taskId);
    return { resultType: "complete" };
  }
  throw Object.assign(new Error(`Method not found: ${msg.method}`), { rpcCode: -32601 });
}

async function dispatch(msg, auth, notify, era, preview, tasks, oauthEnabled = false) {
  if (era === "modern") {
    validateModernRequestMetadata(msg);
    switch (msg.method) {
      case "server/discover":
        return {
          supportedVersions: SUPPORTED_PROTOCOL_VERSIONS,
          capabilities: {
            tools: { listChanged: false },
            resources: { listChanged: false, subscribe: false },
            extensions: {
              [TASKS_EXTENSION]: {},
              ...(oauthEnabled
                ? { "io.modelcontextprotocol/oauth-client-credentials": {} }
                : {})
            }
          },
          instructions: "Publish, organize, review, and manage authorized HTML artifacts.",
          ttlMs: 3_600_000,
          cacheScope: "private"
        };
      case "tools/list":
        return {
          tools: modernToolDefs(clientSupportsApps(msg)),
          ttlMs: 300_000,
          cacheScope: "private"
        };
      case "tools/call":
        return addReviewAppData(msg, addArtifactResourceLink(
          msg,
          await validatedToolCall(msg.params, auth, notify, {
            modern: true,
            allowAppTools: clientSupportsApps(msg),
            allowTasks: clientSupportsTasks(msg),
            preview,
            tasks
          })
        ), auth);
      case "resources/list":
      case "resources/templates/list":
      case "resources/read":
        return dispatchResource(msg, auth, preview);
      case "tasks/get":
      case "tasks/update":
      case "tasks/cancel":
        return dispatchTask(msg, auth, tasks);
      default:
        throw Object.assign(new Error(`Method not found: ${msg.method}`), { rpcCode: -32601 });
    }
  }

  switch (msg.method) {
    case "initialize":
      return {
        protocolVersion: msg.params?.protocolVersion || PROTOCOL_VERSION,
        // This stateless JSON-RPC-over-HTTP POST transport cannot push list-change notifications; clients must reconnect after updates.
        capabilities: { tools: { listChanged: false } },
        serverInfo: SERVER_INFO
      };
    case "ping":
      return {};
    case "notifications/initialized":
      return {};
    case "tools/list":
      return { tools: TOOL_DEFS };
    case "tools/call":
      return validatedToolCall(msg.params, auth, notify);
    default:
      throw Object.assign(new Error(`Method not found: ${msg.method}`), { rpcCode: -32601 });
  }
}

async function handleOne(msg, auth, notify, era, preview, tasks, oauthEnabled = false) {
  const isObj = msg && typeof msg === "object" && !Array.isArray(msg);
  if (!isObj || msg.jsonrpc !== "2.0" || typeof msg.method !== "string") {
    return rpcError(isObj && "id" in msg ? msg.id : null, -32600, "Invalid Request");
  }
  const expects = "id" in msg && msg.id !== null && msg.id !== undefined;
  try {
    const result = await dispatch(msg, auth, notify, era, preview, tasks, oauthEnabled);
    return expects ? { jsonrpc: "2.0", id: msg.id, result: resultForEra(result, era) } : null;
  } catch (err) {
    if (!expects) return null;
    // Tool execution failures surface as an isError tool result, not a protocol error,
    // unless they are protocol-level (rpcCode set).
    if (err.rpcCode) return rpcError(msg.id, err.rpcCode, err.message, err.rpcData);
    return {
      jsonrpc: "2.0",
      id: msg.id,
      result: resultForEra(
        { content: [{ type: "text", text: String(err.message || err) }], isError: true },
        era
      )
    };
  }
}

function requestId(payload) {
  return payload && typeof payload === "object" && !Array.isArray(payload) && Object.hasOwn(payload, "id")
    ? payload.id
    : null;
}

function transportFailure(payload, code, message, data) {
  return { ok: false, status: 400, response: rpcError(requestId(payload), code, message, data) };
}

function headerValue(headers, name) {
  const value = headers?.[name] ?? headers?.[name.toLowerCase()];
  return typeof value === "string" ? value : value == null ? null : undefined;
}

function decodeHeaderValue(value) {
  const match = /^=\?base64\?([A-Za-z0-9+/]*={0,2})\?=$/.exec(value);
  if (!match) return value.startsWith("=?base64?") || value.endsWith("?=") ? null : value;
  try {
    const bytes = Buffer.from(match[1], "base64");
    if (bytes.toString("base64") !== match[1]) return null;
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return null;
  }
}

/**
 * Validate the 2026 Streamable HTTP mirror headers without changing the legacy transport.
 * The JSON body remains authoritative; callers use `protocolVersion` to select the dispatcher.
 */
export function validateMcpHttpRequest(payload, headers = {}) {
  const versionHeader = headerValue(headers, "mcp-protocol-version");
  if (versionHeader === undefined) {
    return transportFailure(payload, -32020, "Header mismatch: mcp-protocol-version header is malformed");
  }
  const method = payload && typeof payload === "object" && !Array.isArray(payload)
    ? payload.method
    : undefined;
  const modernIntent = versionHeader === MODERN_PROTOCOL_VERSION
    || containsModernRequestMetadata(payload)
    || method === "server/discover";
  if (!modernIntent) return { ok: true, protocolVersion: PROTOCOL_VERSION, modern: false };
  if (Array.isArray(payload)) {
    return transportFailure(
      payload,
      -32600,
      "Batch requests are not supported by MCP 2026-07-28"
    );
  }
  if (typeof method !== "string") {
    return transportFailure(payload, -32600, "Invalid Request: method must be a string");
  }
  if (!versionHeader) {
    return transportFailure(
      payload,
      -32020,
      "Header mismatch: required MCP-Protocol-Version header is missing"
    );
  }

  const meta = requestMeta(payload);
  const bodyVersion = meta?.["io.modelcontextprotocol/protocolVersion"];
  if (typeof bodyVersion !== "string") {
    return transportFailure(
      payload,
      -32602,
      "Missing required request metadata: io.modelcontextprotocol/protocolVersion"
    );
  }
  if (versionHeader !== bodyVersion) {
    return transportFailure(
      payload,
      -32020,
      `Header mismatch: MCP-Protocol-Version header value '${versionHeader}' does not match body value '${bodyVersion}'`
    );
  }
  if (bodyVersion !== MODERN_PROTOCOL_VERSION) {
    return transportFailure(
      payload,
      -32022,
      "Unsupported protocol version",
      { supported: SUPPORTED_PROTOCOL_VERSIONS, requested: bodyVersion }
    );
  }
  const capabilities = meta["io.modelcontextprotocol/clientCapabilities"];
  if (!capabilities || typeof capabilities !== "object" || Array.isArray(capabilities)) {
    return transportFailure(
      payload,
      -32602,
      "Missing required request metadata: io.modelcontextprotocol/clientCapabilities"
    );
  }

  const methodHeader = headerValue(headers, "mcp-method");
  if (methodHeader === undefined) {
    return transportFailure(payload, -32020, "Header mismatch: mcp-method header is malformed");
  }
  if (!methodHeader) {
    return transportFailure(payload, -32020, "Header mismatch: required Mcp-Method header is missing");
  }
  if (methodHeader !== method) {
    return transportFailure(
      payload,
      -32020,
      `Header mismatch: Mcp-Method header value '${methodHeader}' does not match body value '${method}'`
    );
  }

  if ([
    "tools/call",
    "resources/read",
    "prompts/get",
    "tasks/get",
    "tasks/update",
    "tasks/cancel"
  ].includes(method)) {
    const field = method === "resources/read"
      ? "uri"
      : method.startsWith("tasks/")
        ? "taskId"
        : "name";
    const bodyName = payload.params?.[field];
    if (typeof bodyName !== "string") {
      return transportFailure(payload, -32602, `${method} requires a string params.${field}`);
    }
    const rawName = headerValue(headers, "mcp-name");
    if (rawName === undefined) {
      return transportFailure(payload, -32020, "Header mismatch: Mcp-Name header is malformed");
    }
    if (!rawName) {
      return transportFailure(payload, -32020, "Header mismatch: required Mcp-Name header is missing");
    }
    const decodedName = decodeHeaderValue(rawName);
    if (decodedName === null) {
      return transportFailure(payload, -32020, "Header mismatch: Mcp-Name header is malformed");
    }
    if (decodedName !== bodyName) {
      return transportFailure(
        payload,
        -32020,
        `Header mismatch: Mcp-Name header value '${decodedName}' does not match body value '${bodyName}'`
      );
    }
  }

  return { ok: true, protocolVersion: MODERN_PROTOCOL_VERSION, modern: true };
}

export async function handleMcp(
  payload,
  auth,
  { notify = defaultNotify, protocolVersion, preview, tasks, oauthEnabled = false } = {}
) {
  const era = protocolVersion === MODERN_PROTOCOL_VERSION
    || (protocolVersion && protocolVersion !== PROTOCOL_VERSION)
    || containsModernRequestMetadata(payload)
    ? "modern"
    : "legacy";
  if (Array.isArray(payload)) {
    if (era === "modern") {
      return rpcError(null, -32600, "Batch requests are not supported by MCP 2026-07-28");
    }
    if (payload.length === 0) return rpcError(null, -32600, "Invalid Request");
    const out = (await Promise.all(
      payload.map((m) => handleOne(m, auth, notify, era, preview, tasks, oauthEnabled))
    )).filter(Boolean);
    return out.length ? out : null;
  }
  return handleOne(payload, auth, notify, era, preview, tasks, oauthEnabled);
}
