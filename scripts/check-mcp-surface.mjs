#!/usr/bin/env node
/**
 * Derived MCP surface gate.
 *
 * This deliberately reads the contracts already owned by the server instead of maintaining a
 * second registry. It answers one release question: does every advertised MCP name still have a
 * validation path, an output schema, dispatch, OAuth scope coverage, documentation, and live
 * contract coverage?
 *
 * Usage:
 *   node scripts/check-mcp-surface.mjs
 *   node scripts/check-mcp-surface.mjs --base origin/master # additionally report changed paths
 */
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path) => readFileSync(resolve(root, path), "utf8");
const errors = [];
const check = (condition, message) => {
  if (!condition) errors.push(message);
};
const unique = (items) => [...new Set(items)];
const duplicates = (items) =>
  unique(items.filter((item, index) => items.indexOf(item) !== index)).sort();

function rustRawString(source, constant) {
  const marker = `${constant}: &str = r#"`;
  const start = source.indexOf(marker);
  if (start < 0) throw new Error(`could not find ${constant}`);
  const bodyStart = start + marker.length;
  const end = source.indexOf('"#;', bodyStart);
  if (end < 0) throw new Error(`could not find closing delimiter for ${constant}`);
  return source.slice(bodyStart, end);
}

function toolNamesFromDispatch(source) {
  const callStart = source.indexOf("let result = match name {");
  const callEnd = source.indexOf("    }?;", callStart);
  check(callStart >= 0 && callEnd >= 0, "could not locate tools/call dispatch match");
  if (callStart < 0 || callEnd < 0) return [];
  return unique(
    [...source.slice(callStart, callEnd).matchAll(/^\s*"([a-z_]+)"\s*(?:if [^{]+)?=>/gm)].map(
      (match) => match[1],
    ),
  ).sort();
}

function balancedBlock(source, openingBrace) {
  let depth = 0;
  for (let index = openingBrace; index < source.length; index += 1) {
    if (source[index] === "{") depth += 1;
    if (source[index] === "}") depth -= 1;
    if (depth === 0) return source.slice(openingBrace, index + 1);
  }
  throw new Error("could not find closing Rust block");
}

function namedToolDefinitionNames(source) {
  return [...source.matchAll(/^\s*name:\s*"([a-z_]+)"\.to_owned\(\),$/gm)].map(
    (match) => match[1],
  );
}

function namedToolDefinitions(source) {
  return unique(namedToolDefinitionNames(source)).sort();
}

function appOnlyToolDefinitions(source) {
  const start = source.indexOf("if supports_apps {\n        definitions.push");
  check(start >= 0, "could not locate MCP App-only tool declaration block");
  if (start < 0) return [];
  const openingBrace = source.indexOf("{", start);
  return namedToolDefinitions(balancedBlock(source, openingBrace));
}

function methodsFromDispatch(source, functionMarker, namespace) {
  const functionStart = source.indexOf(functionMarker);
  const matchStart = source.indexOf("match method {", functionStart);
  check(functionStart >= 0 && matchStart >= 0, `could not locate ${namespace} dispatch match`);
  if (functionStart < 0 || matchStart < 0) return [];
  const openingBrace = source.indexOf("{", matchStart);
  const block = balancedBlock(source, openingBrace);
  const methodPattern = new RegExp(`"(${namespace}/[a-z/]+)"`, "g");
  return unique([...block.matchAll(methodPattern)].map((match) => match[1])).sort();
}

function protocolVersionsFromDispatch(source) {
  return unique(
    [...source.matchAll(/^pub const (?:MODERN_)?PROTOCOL_VERSION: &str = "([^"]+)";$/gm)].map(
      (match) => match[1],
    ),
  ).sort();
}

function changedPaths(base) {
  if (!base) return [];
  try {
    return execFileSync("git", ["diff", "--name-only", `${base}...HEAD`], {
      cwd: root,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    })
      .split("\n")
      .filter(Boolean)
      .sort();
  } catch {
    errors.push(`could not derive changed paths from ${base}; fetch or pass a reachable base ref`);
    return [];
  }
}

const toolDefinitions = read("src/mcp/tool_defs.rs");
const dispatch = read("src/mcp/dispatch.rs");
const resources = read("src/mcp/resources.rs");
const tasks = read("src/mcp/tasks.rs");
const oauth = read("src/security/oauth.rs");
const mcpDocs = read("docs/mcp-api.md");
const cargo = read("Cargo.toml");
const packageJson = JSON.parse(read("package.json"));
const nativeTests = read("tests/native/main.rs");
const frozenTools = JSON.parse(rustRawString(toolDefinitions, "FROZEN_TOOL_DEFINITIONS_JSON"));
const outputSchemas = JSON.parse(read("conformance/mcp.tool-output-schemas.json"));
const golden = JSON.parse(read("conformance/goldens/mcp.tools-list.json"));
const goldenTools = golden.steps?.[0]?.body?.json?.result?.tools;

const legacyTools = frozenTools.map((tool) => tool.name);
const modernToolDeclarations = namedToolDefinitionNames(toolDefinitions);
const declaredModernTools = unique(modernToolDeclarations).sort();
const appOnlyTools = appOnlyToolDefinitions(toolDefinitions);
const modernBaseTools = declaredModernTools.filter((name) => !appOnlyTools.includes(name));
const modernTools = [...legacyTools, ...declaredModernTools];
const dispatchTools = toolNamesFromDispatch(dispatch);
const resourceMethods = methodsFromDispatch(resources, "pub async fn dispatch(", "resources");
const taskMethods = methodsFromDispatch(tasks, "pub fn dispatch_task_method(", "tasks");
const protocolVersions = protocolVersionsFromDispatch(dispatch);
const rejectedDuplicates = {
  legacyTools: duplicates(legacyTools),
  modernDeclarations: duplicates(modernToolDeclarations),
};

check(legacyTools.length === 21, `legacy advertised tool count is ${legacyTools.length}, expected 21`);
for (const [surface, names] of Object.entries(rejectedDuplicates)) {
  check(names.length === 0, `${surface} contain duplicate names: ${names.join(", ")}`);
}
check(
  JSON.stringify(goldenTools) === JSON.stringify(frozenTools),
  "frozen tool definitions drift from conformance/goldens/mcp.tools-list.json",
);
for (const tool of frozenTools) {
  check(typeof tool.description === "string" && tool.description.length > 0, `${tool.name} has no description`);
  check(tool.inputSchema?.type === "object", `${tool.name} input schema is not an object`);
}

for (const name of modernTools) {
  const schema = outputSchemas[name];
  check(schema, `${name} has no typed output schema`);
  check(schema?.type === "object", `${name} output schema is not an object`);
  check(dispatchTools.includes(name), `${name} is advertised but missing from tools/call dispatch`);
  check(oauth.includes(`"${name}"`), `${name} is advertised but absent from OAuth scope mapping`);
  check(mcpDocs.includes(`\`${name}`), `${name} is advertised but absent from MCP documentation`);
}
for (const name of Object.keys(outputSchemas)) {
  check(modernTools.includes(name), `output schema exists for non-advertised tool ${name}`);
}

check(
  dispatch.includes("modern_tool_definitions_for_client(allow_app_tools)") &&
    dispatch.includes("frozen_tool_definitions()") &&
    dispatch.includes("validate_schema_input(\n        &definition.input_schema"),
  "tools/call no longer validates input against the advertised definitions",
);
check(
  dispatch.includes("let schema = tool_output_schema(name)") && dispatch.includes("validate_schema_input(&schema"),
  "tools/call no longer validates structured output against the typed output schema",
);

for (const method of resourceMethods) {
  check(dispatch.includes(`"${method}"`), `${method} is missing from modern protocol dispatch`);
  check(resources.includes(`"${method}"`), `${method} is missing from resource dispatch`);
  check(oauth.includes(`"${method}"`), `${method} is missing OAuth scope coverage`);
  check(mcpDocs.includes(`\`${method}\``), `${method} is missing from MCP documentation`);
}
for (const method of taskMethods) {
  check(dispatch.includes(`"${method}"`), `${method} is missing from modern protocol dispatch`);
  check(tasks.includes(`"${method}"`), `${method} is missing from task dispatch`);
  check(oauth.includes(`"${method}"`), `${method} is missing OAuth scope coverage`);
  check(mcpDocs.includes(`\`${method}\``), `${method} is missing from MCP documentation`);
}
check(
  dispatch.includes('"server/discover"') && mcpDocs.includes("`server/discover`"),
  "server/discover implementation and MCP documentation are not aligned",
);
check(
  declaredModernTools.length > 0 && appOnlyTools.length > 0,
  "modern and MCP App tool declarations are no longer explicit",
);

const cargoVersion = cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
check(cargoVersion === packageJson.version, `Cargo.toml (${cargoVersion}) and package.json (${packageJson.version}) versions differ`);
check(dispatch.includes('pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION")'), "MCP serverInfo is not derived from Cargo package version");
for (const version of protocolVersions) {
  check(dispatch.includes(`"${version}"`), `${version} is missing from Rust protocol declarations`);
  check(mcpDocs.includes(`\`${version}\``), `${version} is missing from MCP protocol documentation`);
}
check(
  mcpDocs.includes(`The legacy catalog contains ${legacyTools.length} tools`) &&
    mcpDocs.includes(`for ${legacyTools.length + modernBaseTools.length}`),
  "MCP tool-count statement is stale or missing",
);

const requiredContractPaths = [
  "conformance/cases/mcp.tools-list.json",
  "conformance/cases/mcp.initialize.json",
  "conformance/cases/mcp.read-artifact.json",
  "tests/native/u25_api_key_capabilities.rs",
  "tests/native/u26_mcp_2026.rs",
];
for (const path of requiredContractPaths) {
  check(existsSync(resolve(root, path)), `required MCP contract test is missing: ${path}`);
}
check(nativeTests.includes("mod u25_api_key_capabilities;"), "API-key capability tests are not registered");
check(nativeTests.includes("mod u26_mcp_2026;"), "MCP 2026 tests are not registered");
const toolsListCase = read("conformance/cases/mcp.tools-list.json");
const initializeCase = read("conformance/cases/mcp.initialize.json");
const readArtifactCase = read("conformance/cases/mcp.read-artifact.json");
const modernContractTests = read("tests/native/u26_mcp_2026.rs");
check(/"method"\s*:\s*"tools\/list"/.test(toolsListCase), "tools/list conformance case no longer exercises tools/list");
check(/"method"\s*:\s*"initialize"/.test(initializeCase), "initialize conformance case no longer exercises initialize");
check(
  protocolVersions.some((version) => new RegExp(`"protocolVersion"\\s*:\\s*"${version}"`).test(initializeCase)),
  "initialize conformance case no longer exercises a declared protocol version",
);
check(/"name"\s*:\s*"read_artifact"/.test(readArtifactCase), "read-artifact conformance case no longer calls read_artifact");
for (const name of declaredModernTools) {
  check(modernContractTests.includes(`"${name}"`), `${name} has no reference in the MCP 2026 contract test`);
}
for (const method of [...resourceMethods, ...taskMethods]) {
  check(modernContractTests.includes(`"${method}"`), `${method} has no reference in the MCP 2026 contract test`);
}

const baseIndex = process.argv.indexOf("--base");
if (baseIndex >= 0 && !process.argv[baseIndex + 1]) {
  errors.push("--base requires a git ref");
}
const base = baseIndex >= 0 ? process.argv[baseIndex + 1] : undefined;
const changes = changedPaths(base);
const impactPaths = changes.filter((path) => /^(src\/mcp\/|src\/security\/oauth\.rs|src\/http\/routes\/mcp\.rs|conformance\/|scripts\/check-mcp-surface|README\.md|Cargo\.toml|package\.json|tests\/native\/u2[56]_)/.test(path));
const compatibilityDecision = impactPaths.length === 0 ? "not-applicable" : "review-required";
const report = {
  invariant: "PBI-059/mcp-surface-sync",
  compatibilityDecision,
  rejectedDuplicates,
  versions: { package: packageJson.version, protocols: protocolVersions },
  tools: {
    legacyCount: legacyTools.length,
    modernBaseCount: legacyTools.length + modernBaseTools.length,
    appsNegotiatedCount: modernTools.length,
    declaredModernOnly: declaredModernTools,
    appOnly: appOnlyTools,
    names: modernTools,
  },
  methods: { resources: resourceMethods, tasks: taskMethods },
  tests: requiredContractPaths,
  changedSurfacePaths: impactPaths,
  commands: [
    "node scripts/check-mcp-surface.mjs",
    "node conformance/runner.mjs --impl both",
    "cargo test --all-targets --locked",
  ],
};

if (errors.length > 0) {
  console.error("MCP surface synchronization failed:");
  for (const error of errors) console.error(`- ${error}`);
  process.exitCode = 1;
} else {
  console.log(JSON.stringify(report, null, 2));
}
