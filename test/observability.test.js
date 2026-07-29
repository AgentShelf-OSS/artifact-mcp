// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import test from "node:test";
import assert from "node:assert/strict";
import {
  createMcpTelemetry,
  mcpMetricLabels,
  mcpResponseOutcome
} from "../lib/observability.js";

test("MCP telemetry emits safe bounded dimensions and never records unknown names", () => {
  const events = [];
  let clock = 10;
  const telemetry = createMcpTelemetry({
    logger: { info(message, fields) { events.push({ message, fields }); } },
    now: () => clock,
    createRequestId: () => "mcp_opaque"
  });
  const observation = telemetry.begin();
  observation.setLabels(mcpMetricLabels(
    "2026-07-28",
    "tools/call",
    "Bearer secret-and-artifact-content"
  ));
  clock = 35;
  observation.finish("success", 2_048);

  const metrics = telemetry.renderPrometheus();
  assert.match(metrics, /protocol="2026-07-28",operation="tool_call",method="tools\/call",name="unknown",outcome="success"/);
  assert.match(metrics, /size="le_16k"} 1/);
  assert.doesNotMatch(metrics, /secret-and-artifact-content|Bearer/);
  assert.equal(events.length, 1);
  assert.equal(events[0].fields.request_id, "mcp_opaque");
  assert.equal(events[0].fields.name, "unknown");
  assert.doesNotMatch(JSON.stringify(events), /secret-and-artifact-content|Bearer/);
});

test("MCP telemetry counts cancellation and representative failure outcomes", () => {
  const telemetry = createMcpTelemetry({ logger: { info() {} } });
  const cancelled = telemetry.begin();
  cancelled.setLabels(mcpMetricLabels("2025-06-18", "tools/call", "list_artifacts"));
  cancelled.cancel();

  for (const outcome of [
    "authentication_failure",
    "authorization_failure",
    "validation_failure",
    "server_failure",
    "output_validation_failure"
  ]) {
    const observation = telemetry.begin();
    observation.setLabels(mcpMetricLabels("2026-07-28", "server/discover"));
    observation.finish(outcome, 128);
  }

  const metrics = telemetry.renderPrometheus();
  for (const outcome of [
    "cancelled",
    "authentication_failure",
    "authorization_failure",
    "validation_failure",
    "server_failure",
    "output_validation_failure"
  ]) {
    assert.match(metrics, new RegExp(`outcome="${outcome}"`));
  }
});

test("MCP response classification separates output validation from other server failures", () => {
  assert.equal(mcpResponseOutcome({ result: {} }), "success");
  assert.equal(
    mcpResponseOutcome({ error: { code: -32603, message: "tool x output failed validation" } }),
    "output_validation_failure"
  );
  assert.equal(
    mcpResponseOutcome({ error: { code: -32603, message: "internal error" } }),
    "server_failure"
  );
  assert.equal(
    mcpResponseOutcome({ error: { code: -32602, message: "invalid params" } }),
    "validation_failure"
  );
});
