// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Privacy-safe, low-cardinality MCP metrics and completion logs. This module never accepts
// request arguments, identities, authorization headers, artifact contents, or raw resource URIs.
import { randomUUID } from "node:crypto";

const DURATION_BUCKETS_SECONDS = [0.01, 0.05, 0.1, 0.25, 0.5, 1, 5];
const KNOWN_METHODS = new Set([
  "initialize",
  "server/discover",
  "tools/list",
  "tools/call",
  "resources/list",
  "resources/templates/list",
  "resources/read",
  "tasks/get",
  "tasks/update",
  "tasks/cancel"
]);
const KNOWN_TOOLS = new Set([
  "publish_artifact",
  "publish_bundle",
  "list_artifacts",
  "delete_artifact",
  "update_artifact",
  "set_visibility",
  "list_categories",
  "set_category",
  "create_category",
  "delete_category",
  "list_revisions",
  "create_share",
  "list_shares",
  "revoke_share",
  "artifact_stats",
  "submit_feedback",
  "list_feedback",
  "resolve_feedback",
  "reopen_feedback",
  "read_artifact",
  "patch_artifact",
  "regenerate_artifact_preview"
]);

function operationFor(method) {
  if (method === "server/discover") return "discovery";
  if (method === "initialize") return "initialization";
  if (method === "tools/list") return "listing";
  if (method === "tools/call") return "tool_call";
  if (method === "resources/list" || method === "resources/templates/list" || method === "resources/read") {
    return "resource";
  }
  if (method === "tasks/get" || method === "tasks/update" || method === "tasks/cancel") return "task";
  return "unknown";
}

function safeName(method, name) {
  if (method === "resources/read") {
    if (name === "ui://artifact-mcp/review") return "review_app";
    if (typeof name === "string" && name.startsWith("artifact://") && name.endsWith("/thumbnail")) {
      return "artifact_thumbnail";
    }
    if (typeof name === "string" && name.startsWith("artifact://")) return "artifact_content";
    return "unknown";
  }
  if (method.startsWith("tasks/")) return "preview_regeneration";
  if (method !== "tools/call") return "none";
  return KNOWN_TOOLS.has(name) ? name : "unknown";
}

export function mcpMetricLabels(protocol, method, name) {
  const safeMethod = KNOWN_METHODS.has(method) ? method : "unknown";
  return {
    protocol: protocol === "2026-07-28" || protocol === "2025-06-18" || protocol === "unsupported"
      ? protocol
      : "unknown",
    operation: operationFor(safeMethod),
    method: safeMethod,
    name: safeName(safeMethod, name)
  };
}

export function mcpProtocolDimension(headers = {}) {
  const value = headers["mcp-protocol-version"];
  if (value === "2026-07-28") return value;
  if (value == null || value === "2025-06-18") return "2025-06-18";
  return "unsupported";
}

export function mcpRequestName(body) {
  const method = body?.method;
  if (method === "resources/read") return body?.params?.uri;
  if (method?.startsWith("tasks/")) return body?.params?.taskId;
  return body?.params?.name;
}

export function mcpResponseOutcome(output) {
  const code = output?.error?.code;
  const message = String(output?.error?.message || "");
  if (code === -32603 && (
    message.includes("output failed validation")
    || message.includes("output schema")
    || message.includes("structured content")
  )) return "output_validation_failure";
  if (code === -32603) return "server_failure";
  if (code === -32602 || code === -32600 || code === -32700) return "validation_failure";
  if (output?.error) return "protocol_error";
  return "success";
}

function resultSizeBucket(bytes) {
  if (bytes <= 1_024) return "le_1k";
  if (bytes <= 16_384) return "le_16k";
  if (bytes <= 262_144) return "le_256k";
  if (bytes <= 1_048_576) return "le_1m";
  return "gt_1m";
}

function metricKey(labels, outcome) {
  return JSON.stringify([
    labels.protocol,
    labels.operation,
    labels.method,
    labels.name,
    outcome
  ]);
}

function emptySeries() {
  return {
    calls: 0,
    durationSumSeconds: 0,
    durationBuckets: Array(DURATION_BUCKETS_SECONDS.length + 1).fill(0),
    resultBytes: 0,
    resultSizeBuckets: new Map([
      ["le_1k", 0],
      ["le_16k", 0],
      ["le_256k", 0],
      ["le_1m", 0],
      ["gt_1m", 0]
    ])
  };
}

export function createMcpTelemetry({
  logger = console,
  now = () => performance.now(),
  createRequestId = () => `mcp_${randomUUID().replaceAll("-", "").slice(0, 16)}`
} = {}) {
  const series = new Map();

  function record(requestId, labels, outcome, durationSeconds, resultBytes) {
    const key = metricKey(labels, outcome);
    const item = series.get(key) || emptySeries();
    item.calls += 1;
    item.durationSumSeconds += durationSeconds;
    const durationBucket = DURATION_BUCKETS_SECONDS.findIndex((bound) => durationSeconds <= bound);
    item.durationBuckets[durationBucket === -1 ? DURATION_BUCKETS_SECONDS.length : durationBucket] += 1;
    item.resultBytes += resultBytes;
    const sizeBucket = resultSizeBucket(resultBytes);
    item.resultSizeBuckets.set(sizeBucket, item.resultSizeBuckets.get(sizeBucket) + 1);
    series.set(key, item);

    logger.info?.("[artifact-mcp] MCP request completed", {
      request_id: requestId,
      ...labels,
      outcome,
      duration_ms: durationSeconds * 1_000,
      result_size_bucket: sizeBucket
    });
  }

  function begin() {
    const requestId = createRequestId();
    const started = now();
    let labels = mcpMetricLabels("unknown");
    let completed = false;
    return {
      requestId,
      setLabels(next) {
        labels = next;
      },
      finish(outcome, resultBytes = 0) {
        if (completed) return;
        completed = true;
        record(requestId, labels, outcome, Math.max(0, now() - started) / 1_000, resultBytes);
      },
      cancel() {
        this.finish("cancelled", 0);
      },
      get completed() {
        return completed;
      }
    };
  }

  function renderPrometheus() {
    let output =
      "# HELP artifact_mcp_requests_total MCP requests by safe protocol dimensions and outcome.\n"
      + "# TYPE artifact_mcp_requests_total counter\n"
      + "# HELP artifact_mcp_request_duration_seconds MCP request duration in seconds.\n"
      + "# TYPE artifact_mcp_request_duration_seconds histogram\n"
      + "# HELP artifact_mcp_result_bytes_total Total serialized MCP response bytes.\n"
      + "# TYPE artifact_mcp_result_bytes_total counter\n"
      + "# HELP artifact_mcp_result_size_bucket_total MCP responses in bounded size bands.\n"
      + "# TYPE artifact_mcp_result_size_bucket_total counter\n";
    for (const [rawKey, item] of [...series.entries()].sort(([left], [right]) => left.localeCompare(right))) {
      const [protocol, operation, method, name, outcome] = JSON.parse(rawKey);
      const labels = `protocol="${protocol}",operation="${operation}",method="${method}",name="${name}",outcome="${outcome}"`;
      output += `artifact_mcp_requests_total{${labels}} ${item.calls}\n`;
      let cumulative = 0;
      DURATION_BUCKETS_SECONDS.forEach((bound, index) => {
        cumulative += item.durationBuckets[index];
        output += `artifact_mcp_request_duration_seconds_bucket{${labels},le="${bound}"} ${cumulative}\n`;
      });
      cumulative += item.durationBuckets.at(-1);
      output += `artifact_mcp_request_duration_seconds_bucket{${labels},le="+Inf"} ${cumulative}\n`;
      output += `artifact_mcp_request_duration_seconds_sum{${labels}} ${item.durationSumSeconds}\n`;
      output += `artifact_mcp_request_duration_seconds_count{${labels}} ${item.calls}\n`;
      output += `artifact_mcp_result_bytes_total{${labels}} ${item.resultBytes}\n`;
      for (const [size, count] of item.resultSizeBuckets) {
        output += `artifact_mcp_result_size_bucket_total{${labels},size="${size}"} ${count}\n`;
      }
    }
    return output;
  }

  return { begin, renderPrometheus };
}
