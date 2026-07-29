import assert from "node:assert/strict";
import { mkdtemp, copyFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

import {
  buildRemoteValidationRequest,
  checkLocalHealth,
  localHealthPlan,
  operationPlan,
  runOperation,
  validateRemoteRegistration,
  validateTunnelUrl
} from "../scripts/anthropic-tunnel.mjs";

test("default, tunneled, degraded, and rollback deployment paths stay distinct", async () => {
  const project = await mkdtemp(join(tmpdir(), "artifact-mcp-tunnel-compose-"));
  await copyFile(new URL("../docker-compose.yml", import.meta.url), join(project, "docker-compose.yml"));
  await writeFile(join(project, ".env"), "ARTIFACT_API_KEYS=\n", { mode: 0o600 });

  const render = (profile = "") => {
    const args = ["compose"];
    if (profile) args.push("--profile", profile);
    args.push("config", "--format", "json");
    const result = spawnSync("docker", args, {
      cwd: project,
      encoding: "utf8",
      env: { ...process.env, COMPOSE_PROJECT_NAME: "artifact-mcp-tunnel-test" }
    });
    assert.equal(result.status, 0, "Compose configuration must render");
    return JSON.parse(result.stdout);
  };

  const defaultConfig = render();
  assert.deepEqual(Object.keys(defaultConfig.services), ["artifact-mcp"]);
  assert.deepEqual(defaultConfig.services["artifact-mcp"].ports[0], {
    mode: "ingress",
    target: 3480,
    published: "3480",
    protocol: "tcp",
    host_ip: "127.0.0.1"
  });

  const tunneledConfig = render("anthropic-tunnel");
  const proxy = tunneledConfig.services["anthropic-mcp-proxy"];
  const cloudflared = tunneledConfig.services["anthropic-mcp-cloudflared"];
  assert.ok(proxy);
  assert.ok(cloudflared);
  assert.equal(proxy.ports, undefined);
  assert.equal(cloudflared.ports, undefined);
  assert.equal(proxy.read_only, true);
  assert.deepEqual(proxy.cap_drop, ["ALL"]);
  assert.match(proxy.image, /@sha256:[a-f0-9]{64}$/);
  assert.match(cloudflared.image, /@sha256:[a-f0-9]{64}$/);
  assert.equal(cloudflared.environment, undefined);
  assert.deepEqual(cloudflared.secrets, [
    {
      source: "anthropic_tunnel_token",
      target: "/run/secrets/anthropic_tunnel_token"
    }
  ]);
  assert.ok(cloudflared.command.includes("--token-file"));
  assert.ok(!JSON.stringify(cloudflared).includes("TUNNEL_TOKEN="));

  const health = localHealthPlan();
  assert.equal(health.length, 3);
  assert.match(health[0].name, /application health/);
  assert.match(health[2].name, /tunnel edge connectivity/);

  const calls = [];
  assert.throws(
    () =>
      checkLocalHealth({
        spawn(_command, args) {
          calls.push(args);
          if (args.includes("anthropic-mcp-cloudflared")) {
            return { status: 1, stdout: "", stderr: "credential-that-must-not-leak" };
          }
          return { status: 0, stdout: "running", stderr: "" };
        }
      }),
    (error) => {
      assert.match(error.message, /outbound tunnel edge connectivity failed/);
      assert.doesNotMatch(error.message, /credential-that-must-not-leak/);
      return true;
    }
  );
  assert.equal(calls.length, 3);

  const rollback = operationPlan("rollback");
  assert.equal(rollback[0].args.includes("--profile"), false);
  assert.deepEqual(
    rollback[1].args.slice(-3),
    ["stop", "anthropic-mcp-cloudflared", "anthropic-mcp-proxy"]
  );

  const executed = [];
  runOperation("rollback", {
    spawn(_command, args) {
      executed.push(args);
      return { status: 0, stdout: "", stderr: "" };
    }
  });
  assert.deepEqual(executed, rollback.map((step) => step.args));
});

test("remote validation is read-only, Anthropic-side, and never logs its credentials", async () => {
  const env = {
    ANTHROPIC_TUNNEL_MCP_URL: "https://artifacts.example.tunnel.anthropic.com/mcp",
    ANTHROPIC_TUNNEL_TEST_MODEL: "test-model",
    ANTHROPIC_API_KEY_FILE: "/secure/anthropic",
    ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE: "/secure/upstream"
  };
  const secrets = new Map([
    ["/secure/anthropic", "api-secret"],
    ["/secure/upstream", "publisher-secret"]
  ]);
  let request;
  await validateRemoteRegistration({
    env,
    secretReader: async (path) => secrets.get(path),
    async fetchImpl(url, options) {
      request = { url, options };
      return {
        ok: true,
        status: 200,
        async json() {
          return {
            content: [
              {
                type: "mcp_tool_use",
                id: "mcptoolu_test",
                server_name: "artifact-mcp-private",
                name: "list_artifacts",
                input: {}
              },
              {
                type: "mcp_tool_result",
                tool_use_id: "mcptoolu_test",
                is_error: false,
                content: []
              }
            ]
          };
        }
      };
    }
  });

  assert.equal(request.url, "https://api.anthropic.com/v1/messages");
  assert.equal(request.options.headers["x-api-key"], "api-secret");
  assert.equal(request.options.headers["anthropic-beta"], "mcp-client-2025-11-20");
  const body = JSON.parse(request.options.body);
  assert.equal(body.mcp_servers[0].authorization_token, "publisher-secret");
  assert.deepEqual(body.tools[0].default_config, { enabled: false });
  assert.deepEqual(Object.keys(body.tools[0].configs), ["list_artifacts"]);

  const serializedPlan = JSON.stringify(operationPlan("start"));
  assert.doesNotMatch(serializedPlan, /api-secret|publisher-secret/);
});

test("remote validation rejects unsafe URLs and incomplete tunnel results", async () => {
  assert.throws(
    () => validateTunnelUrl("https://artifacts.example.com/mcp"),
    /tunnel\.anthropic\.com/
  );
  assert.throws(
    () => validateTunnelUrl("https://artifacts.example.tunnel.anthropic.com/not-mcp"),
    /\/mcp path/
  );

  const request = buildRemoteValidationRequest({
    tunnelUrl: "https://artifacts.example.tunnel.anthropic.com/mcp",
    upstreamToken: "secret",
    model: "test-model"
  });
  assert.deepEqual(request.mcp_servers[0].url, "https://artifacts.example.tunnel.anthropic.com/mcp");

  await assert.rejects(
    validateRemoteRegistration({
      env: {
        ANTHROPIC_TUNNEL_MCP_URL: "https://artifacts.example.tunnel.anthropic.com/mcp",
        ANTHROPIC_TUNNEL_TEST_MODEL: "test-model",
        ANTHROPIC_API_KEY_FILE: "/secure/anthropic",
        ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE: "/secure/upstream"
      },
      secretReader: async () => "secret",
      async fetchImpl() {
        return {
          ok: true,
          status: 200,
          async json() {
            return {
              content: [
                {
                  type: "mcp_tool_use",
                  id: "mcptoolu_test",
                  server_name: "artifact-mcp-private",
                  name: "list_artifacts",
                  input: {}
                },
                {
                  type: "mcp_tool_result",
                  tool_use_id: "mcptoolu_test",
                  is_error: true,
                  content: []
                }
              ]
            };
          }
        };
      }
    }),
    /did not complete the read-only list_artifacts probe/
  );
});
