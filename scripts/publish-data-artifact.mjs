// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { readFile } from "node:fs/promises";
import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    mcp: { type: "string" },
    html: { type: "string" },
    bindings: { type: "string" },
    title: { type: "string" },
    description: { type: "string" },
    org: { type: "string" },
    "artifact-id": { type: "string" },
    help: { type: "boolean" }
  }
});

if (values.help) {
  console.log("Publish or update HTML and attach its live-data bindings.\n\n" +
    "node scripts/publish-data-artifact.mjs --mcp https://your-host/mcp " +
    "--html dashboard.html --bindings bindings.json --title 'PR Watch'\n\n" +
    "Set ARTIFACT_MCP_API_KEY through your secret manager. Optional --org selects an " +
    "organization for an admin key; --artifact-id updates an existing artifact.");
} else {
  await run().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}

async function run() {
  const token = process.env.ARTIFACT_MCP_API_KEY;
  if (!token) throw new Error("Set ARTIFACT_MCP_API_KEY through your secret manager.");
  if (!values.mcp || !values.html || !values.bindings || !values.title) {
    throw new Error("Required arguments: --mcp, --html, --bindings, --title. Use --help for an example.");
  }
  const endpoint = new URL(values.mcp);
  if (!["http:", "https:"].includes(endpoint.protocol) || endpoint.username || endpoint.password || endpoint.hash || endpoint.search) {
    throw new Error("--mcp must be an HTTP(S) endpoint without credentials, query or fragment.");
  }
  const [html, manifestText] = await Promise.all([
    readFile(values.html, "utf8"), readFile(values.bindings, "utf8")
  ]);
  const manifest = JSON.parse(manifestText);
  const bindings = manifest.bindings;
  if (!bindings || typeof bindings !== "object" || Array.isArray(bindings)) {
    throw new Error("The bindings file must contain a bindings object.");
  }
  let requestId = 0;
  async function rpc(method, params) {
    const response = await fetch(endpoint, {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
        accept: "application/json, text/event-stream"
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: ++requestId, method, params }),
      redirect: "error",
      signal: AbortSignal.timeout(30_000)
    });
    if (!response.ok) throw new Error(`MCP request failed with HTTP ${response.status}.`);
    const result = await response.json();
    if (result.error || result.result?.isError) {
      throw new Error(`MCP ${method === "tools/call" ? params.name : method} failed. Check source configuration and publisher permissions.`);
    }
    return result.result;
  }
  async function call(name, args) {
    const result = await rpc("tools/call", { name, arguments: args });
    if (result.structuredContent) return result.structuredContent;
    const text = result.content?.find((item) => item.type === "text")?.text;
    if (typeof text !== "string") throw new Error(`MCP ${name} returned no structured result.`);
    return JSON.parse(text);
  }

  await rpc("initialize", {
    protocolVersion: "2025-06-18", capabilities: {},
    clientInfo: { name: "artifact-data-publisher", version: "1.0.0" }
  });
  const discovery = await call("list_data_sources", values.org ? { org: values.org } : {});
  const sources = new Map(discovery.sources.map((source) => [source.id, source]));
  for (const binding of Object.values(bindings)) {
    const source = sources.get(binding.source);
    if (!source) throw new Error("A bound source is unavailable to this publisher. Check the source organization before publishing.");
    for (const operation of binding.operations ?? []) {
      if (!Object.hasOwn(source.operations, operation)) throw new Error("A binding requests an unavailable operation.");
    }
    for (const subscription of binding.subscriptions ?? []) {
      if (!Object.hasOwn(source.subscriptions, subscription)) throw new Error("A binding requests an unavailable subscription.");
    }
  }

  const publication = values["artifact-id"]
    ? await call("update_artifact", {
      id: values["artifact-id"], html, title: values.title,
      ...(values.description ? { description: values.description } : {})
    })
    : await call("publish_artifact", {
      html, title: values.title,
      ...(values.description ? { description: values.description } : {}),
      ...(values.org ? { org: values.org } : {})
    });
  const artifactId = publication.id ?? values["artifact-id"];
  try {
    await call("set_data_bindings", { id: artifactId, bindings });
  } catch (error) {
    throw new Error(`${error.message} HTML was published at artifact ${artifactId}; retry with --artifact-id ${artifactId} after correcting the bindings.`);
  }
  console.log(JSON.stringify({ id: artifactId, url: publication.url, bindings: Object.keys(bindings) }, null, 2));
}
