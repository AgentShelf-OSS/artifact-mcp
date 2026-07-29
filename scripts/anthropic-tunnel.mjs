#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

export const TUNNEL_PROFILE = "anthropic-tunnel";
export const TUNNEL_SERVICES = [
  "anthropic-mcp-proxy",
  "anthropic-mcp-cloudflared"
];

const ANTHROPIC_MESSAGES_URL = "https://api.anthropic.com/v1/messages";
const MCP_CONNECTOR_BETA = "mcp-client-2025-11-20";

function compose(...args) {
  return ["compose", "--profile", TUNNEL_PROFILE, ...args];
}

export function operationPlan(action) {
  switch (action) {
    case "start":
      return [
        {
          name: "start Artifact MCP and the private tunnel profile",
          args: compose(
            "up",
            "-d",
            "--wait",
            "--wait-timeout",
            "60",
            "artifact-mcp",
            ...TUNNEL_SERVICES
          )
        }
      ];
    case "rollback":
      return [
        {
          name: "restore the default Artifact MCP service",
          args: ["compose", "up", "-d", "--wait", "--wait-timeout", "60", "artifact-mcp"]
        },
        {
          name: "stop the private tunnel profile",
          args: compose("stop", "anthropic-mcp-cloudflared", "anthropic-mcp-proxy")
        }
      ];
    default:
      throw new Error(`Unsupported tunnel action: ${action}`);
  }
}

export function localHealthPlan() {
  return [
    {
      name: "Artifact MCP application health",
      args: ["compose", "exec", "-T", "artifact-mcp", "/artifact-mcp", "healthcheck"]
    },
    {
      name: "Anthropic MCP proxy process",
      args: compose("ps", "--status", "running", "-q", "anthropic-mcp-proxy"),
      requireOutput: true
    },
    {
      name: "outbound tunnel edge connectivity",
      args: compose(
        "exec",
        "-T",
        "anthropic-mcp-cloudflared",
        "cloudflared",
        "tunnel",
        "--metrics",
        "127.0.0.1:2000",
        "ready"
      )
    }
  ];
}

function runDockerStep(step, { spawn = spawnSync } = {}) {
  const result = spawn("docker", step.args, {
    cwd: resolve(import.meta.dirname, ".."),
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"]
  });
  const output = String(result.stdout || "").trim();
  if (result.error || result.status !== 0 || (step.requireOutput && !output)) {
    const status = result.error ? "could not run Docker" : `exit ${result.status ?? "unknown"}`;
    throw new Error(`${step.name} failed (${status})`);
  }
}

export function runOperation(action, options = {}) {
  for (const step of operationPlan(action)) {
    runDockerStep(step, options);
    options.log?.(`[ok] ${step.name}`);
  }
}

export function checkLocalHealth(options = {}) {
  for (const step of localHealthPlan()) {
    runDockerStep(step, options);
    options.log?.(`[ok] ${step.name}`);
  }
}

function requiredEnv(env, name) {
  const value = String(env[name] || "").trim();
  if (!value) throw new Error(`${name} is required`);
  return value;
}

export function validateTunnelUrl(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error("ANTHROPIC_TUNNEL_MCP_URL must be a valid HTTPS URL");
  }
  if (
    url.protocol !== "https:"
    || !url.hostname.endsWith(".tunnel.anthropic.com")
    || url.username
    || url.password
    || url.search
    || url.hash
  ) {
    throw new Error(
      "ANTHROPIC_TUNNEL_MCP_URL must be an HTTPS *.tunnel.anthropic.com URL without credentials, query, or fragment"
    );
  }
  if (url.pathname.replace(/\/+$/, "") !== "/mcp") {
    throw new Error("ANTHROPIC_TUNNEL_MCP_URL must use Artifact MCP's /mcp path");
  }
  return url.toString();
}

async function readSecret(path) {
  const value = (await readFile(path, "utf8")).trim();
  if (!value || value.length > 16_384 || /[\r\n]/.test(value)) {
    throw new Error(`Secret file ${path} must contain one non-empty line`);
  }
  return value;
}

export function buildRemoteValidationRequest({ tunnelUrl, upstreamToken, model }) {
  return {
    model,
    max_tokens: 256,
    messages: [
      {
        role: "user",
        content: "Call list_artifacts exactly once with an empty input, then say validation complete."
      }
    ],
    mcp_servers: [
      {
        type: "url",
        url: tunnelUrl,
        name: "artifact-mcp-private",
        authorization_token: upstreamToken
      }
    ],
    tools: [
      {
        type: "mcp_toolset",
        mcp_server_name: "artifact-mcp-private",
        default_config: { enabled: false },
        configs: {
          list_artifacts: { enabled: true, defer_loading: false }
        }
      }
    ]
  };
}

export async function validateRemoteRegistration({
  env = process.env,
  fetchImpl = fetch,
  secretReader = readSecret
} = {}) {
  const tunnelUrl = validateTunnelUrl(requiredEnv(env, "ANTHROPIC_TUNNEL_MCP_URL"));
  const model = requiredEnv(env, "ANTHROPIC_TUNNEL_TEST_MODEL");
  const apiKey = await secretReader(requiredEnv(env, "ANTHROPIC_API_KEY_FILE"));
  const upstreamToken = await secretReader(
    requiredEnv(env, "ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE")
  );

  let response;
  try {
    response = await fetchImpl(ANTHROPIC_MESSAGES_URL, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": apiKey,
        "anthropic-version": "2023-06-01",
        "anthropic-beta": MCP_CONNECTOR_BETA
      },
      body: JSON.stringify(buildRemoteValidationRequest({ tunnelUrl, upstreamToken, model })),
      signal: AbortSignal.timeout(45_000)
    });
  } catch {
    throw new Error("Anthropic-side MCP tunnel validation could not reach the Messages API");
  }

  let payload;
  try {
    payload = await response.json();
  } catch {
    throw new Error(`Anthropic-side MCP tunnel validation returned non-JSON HTTP ${response.status}`);
  }
  if (!response.ok) {
    const errorType = String(payload?.error?.type || "unknown_error");
    throw new Error(
      `Anthropic-side MCP tunnel validation failed with HTTP ${response.status} (${errorType})`
    );
  }

  const blocks = Array.isArray(payload?.content) ? payload.content : [];
  const toolUse = blocks.find(
    (block) =>
      block?.type === "mcp_tool_use"
      && block?.server_name === "artifact-mcp-private"
      && block?.name === "list_artifacts"
  );
  const toolResult = blocks.find(
    (block) =>
      block?.type === "mcp_tool_result"
      && block?.tool_use_id === toolUse?.id
      && block?.is_error !== true
  );
  if (!toolUse || !toolResult) {
    throw new Error(
      "Anthropic reached the tunnel but did not complete the read-only list_artifacts probe"
    );
  }
}

function usage() {
  return `Usage: node scripts/anthropic-tunnel.mjs <start|status|validate|rollback>

start      Start the opt-in Anthropic MCP Tunnel profile and wait for local health.
status     Check Artifact MCP, proxy, and tunnel-edge health independently.
validate   Run local checks, then call list_artifacts from Anthropic's Messages API.
rollback   Stop the tunnel profile and preserve the default Artifact MCP deployment.

validate requires:
  ANTHROPIC_TUNNEL_MCP_URL
  ANTHROPIC_TUNNEL_TEST_MODEL
  ANTHROPIC_API_KEY_FILE
  ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE`;
}

async function main() {
  const [action, ...extra] = process.argv.slice(2);
  if (!action || action === "--help" || action === "-h") {
    console.log(usage());
    return;
  }
  if (extra.length || !["start", "status", "validate", "rollback"].includes(action)) {
    throw new Error(usage());
  }

  const options = { log: console.log };
  if (action === "start") {
    runOperation("start", options);
    checkLocalHealth(options);
    console.log("Tunnel profile is locally healthy. Run `npm run tunnel:validate` before cutover.");
    return;
  }
  if (action === "rollback") {
    runOperation("rollback", options);
    const appHealth = localHealthPlan()[0];
    runDockerStep(appHealth, options);
    options.log(`[ok] ${appHealth.name}`);
    console.log(
      "Default deployment restored. Re-enable the former public /mcp edge policy if it was removed after cutover."
    );
    return;
  }

  checkLocalHealth(options);
  if (action === "validate") {
    await validateRemoteRegistration();
    console.log("[ok] Anthropic-side registration, routing, authentication, and read-only tool call");
  }
}

const invokedPath = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : "";
if (import.meta.url === invokedPath) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
