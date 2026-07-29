// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Language-neutral conformance runner (blueprint B2/B3). Node stdlib only.
//
//   node conformance/runner.mjs --impl node|rust|both [--record] [--filter <tag>]
//
// For each case: copy the named fixture to a fresh temp DATA_DIR, start the target server on
// a random loopback port, wait for /health, execute the HTTP steps, normalize only declared-
// volatile fields, compare per the case's declared modes, stop the server cleanly, then run
// post-state SQL / file / directory assertions. Node and Rust NEVER share a data dir.
//
// Exit nonzero on any diff, failed assertion, or runner error.

import {
  cpSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import {
  CASES_DIR,
  GOLDENS_DIR,
  FIXTURES_DIR,
  baseEnv,
  constantSymbols,
  freePort,
  httpRequest,
  loadBetterSqlite,
  waitForHealth
} from "./testkit.mjs";
import {
  backsubString,
  deepEqual,
  describeDiff,
  normalizeBody,
  normalizeHeaders,
  sha256Hex
} from "./comparators.mjs";
import { createNodeDriver } from "./node-driver.mjs";
import { createRustDriver } from "./rust-driver.mjs";

// --- CLI -------------------------------------------------------------------------------
function parseArgs(argv) {
  const args = { impl: null, record: false, filter: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--impl") args.impl = argv[++i];
    else if (a === "--record") args.record = true;
    else if (a === "--filter") args.filter = argv[++i];
    else if (a === "--help" || a === "-h") args.help = true;
    else throw new Error(`Unknown argument: ${a}`);
  }
  return args;
}

const USAGE = `Usage: node conformance/runner.mjs --impl node|rust|both [--record] [--filter <tag>]

  --impl node    Run every recorded case against the real Node server (works today).
  --impl rust    Run against the Rust server (not built until M1+; fails clearly).
  --impl both    Run Node fully; degrade cleanly if the Rust binary is absent.
  --record       Regenerate goldens FROM the Node server. Forbidden with rust/both and in CI.
  --filter <t>   Only run cases whose id or tags contain <t>.`;

// --- Case & golden IO ------------------------------------------------------------------
function expandRoleMatrix(definition) {
  const expanded = [];
  for (const roleCase of definition.roleMatrix || []) {
    for (const target of roleCase.targets || []) {
      const operations = [
        ["read", "read_artifact", { id: target.id }],
        [
          "write",
          "patch_artifact",
          {
            id: target.id,
            expected_revision: 1,
            edits: [{ find: "seed", replace: roleCase.role + "-" + target.scope }]
          }
        ],
        ["delete", "delete_artifact", { id: target.id }]
      ];
      for (const [operation, name, toolArguments] of operations) {
        const outcome = target.outcomes[operation];
        expanded.push({
          name: roleCase.role + " " + target.scope + " " + operation,
          request: {
            method: "POST",
            path: "/mcp",
            headers: {
              authorization: "Bearer " + roleCase.key,
              "content-type": "application/json"
            },
            json: {
              jsonrpc: "2.0",
              id: expanded.length + 100,
              method: "tools/call",
              params: { name, arguments: toolArguments }
            }
          },
          expect: {
            status: 200,
            ...(outcome === "ok" ? { mcpSuccess: true } : { mcpError: outcome })
          }
        });
      }
    }
  }
  return expanded;
}

function loadCases() {
  if (!existsSync(CASES_DIR)) return [];
  return readdirSync(CASES_DIR)
    .filter((f) => f.endsWith(".json"))
    .sort()
    .map((f) => {
      const def = JSON.parse(readFileSync(join(CASES_DIR, f), "utf8"));
      def.__file = f;
      if (!def.id) def.id = basename(f, ".json");
      def.steps = [...(def.steps || []), ...expandRoleMatrix(def)];
      return def;
    });
}

function goldenPath(caseId) {
  return join(GOLDENS_DIR, `${caseId}.json`);
}
function loadGolden(caseId) {
  const p = goldenPath(caseId);
  if (!existsSync(p)) return null;
  return JSON.parse(readFileSync(p, "utf8"));
}
function writeGolden(caseId, normalized) {
  if (!existsSync(GOLDENS_DIR)) mkdirSync(GOLDENS_DIR, { recursive: true });
  writeFileSync(goldenPath(caseId), JSON.stringify(normalized, null, 2) + "\n");
}

// --- Substitution ----------------------------------------------------------------------
function substStr(str, symbols) {
  return String(str).replace(/\$\{([a-zA-Z0-9_]+)\}/g, (m, name) =>
    Object.hasOwn(symbols, name) ? String(symbols[name]) : m
  );
}
function substDeep(value, symbols) {
  if (typeof value === "string") return substStr(value, symbols);
  if (Array.isArray(value)) return value.map((v) => substDeep(v, symbols));
  if (value && typeof value === "object") {
    const out = {};
    for (const [k, v] of Object.entries(value)) out[k] = substDeep(v, symbols);
    return out;
  }
  return value;
}

function getByPath(obj, path) {
  return path.split(".").reduce((acc, key) => (acc == null ? undefined : acc[key]), obj);
}

// --- Request building ------------------------------------------------------------------
function buildRequest(reqSpec, symbols) {
  const headers = {};
  for (const [k, v] of Object.entries(reqSpec.headers || {})) headers[k.toLowerCase()] = substStr(v, symbols);

  let body;
  if (reqSpec.genBody) {
    body = generateBody(reqSpec.genBody);
    if (headers["content-type"] === undefined && reqSpec.genBody.kind !== "raw") {
      headers["content-type"] = "application/json";
    }
  } else if (reqSpec.json !== undefined) {
    body = Buffer.from(JSON.stringify(substDeep(reqSpec.json, symbols)));
    if (headers["content-type"] === undefined) headers["content-type"] = "application/json";
  } else if (reqSpec.rawBody !== undefined) {
    body = Buffer.from(substStr(reqSpec.rawBody, symbols));
  }

  return {
    method: reqSpec.method || "GET",
    path: substStr(reqSpec.path, symbols),
    headers,
    body
  };
}

function generateBody(gen) {
  if (gen.kind === "oversize-json") {
    // A syntactically-valid JSON object whose size exceeds the MCP json limit, to trip 413
    // AFTER auth but at the body-buffering boundary.
    const filler = "x".repeat(Math.max(1, (gen.bytes || 9_000_000)));
    return Buffer.from(`{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"publish_artifact","arguments":{"html":"${filler}"}}}`);
  }
  if (gen.kind === "raw") return Buffer.from(gen.text ?? "");
  throw new Error(`unknown genBody kind: ${gen.kind}`);
}

// --- Assertions (always evaluated, in record and compare) ------------------------------
function evalStepAssertions(step, res, captures, label) {
  const failures = [];

  if (step.expect && step.expect.status !== undefined && res.status !== step.expect.status) {
    failures.push(`${label}: status expected ${step.expect.status}, got ${res.status}`);
  }

  if (step.expect?.mcpSuccess || step.expect?.mcpError !== undefined) {
    let parsed;
    try {
      parsed = JSON.parse(res.body.toString("utf8"));
    } catch {
      failures.push(label + ": MCP assertion requires a JSON response body");
    }
    if (parsed) {
      const isError = parsed.result?.isError === true;
      const actual = parsed.result?.content?.[0]?.text;
      if (step.expect.mcpSuccess && isError) {
        failures.push(label + ": expected MCP success, got " + (actual ?? "tool error"));
      }
      if (step.expect.mcpError !== undefined) {
        const expected = substStr(step.expect.mcpError, captures);
        if (!isError) failures.push(label + ': expected MCP error "' + expected + '", got success');
        else if (actual !== expected) {
          failures.push(label + ': MCP error expected "' + expected + '", got "' + actual + '"');
        }
      }
    }
  }

  const headerSpec = step.expect?.headers;
  if (headerSpec) {
    const flat = {};
    for (const [k, v] of Object.entries(res.headers)) flat[k.toLowerCase()] = Array.isArray(v) ? v.join(", ") : String(v);
    for (const sub of headerSpec.forbid || []) {
      for (const [k, v] of Object.entries(flat)) {
        if (v.includes(sub)) failures.push(`${label}: forbidden header substring "${sub}" present in ${k}: ${v}`);
      }
    }
    for (const [name, expected] of Object.entries(headerSpec.require || {})) {
      const actual = flat[name.toLowerCase()];
      if (actual !== expected) failures.push(`${label}: header ${name} expected "${expected}", got "${actual ?? "(absent)"}"`);
    }
  }

  for (const spec of step.assertSymbols || []) {
    const val = captures[spec.symbol];
    if (val === undefined) {
      failures.push(`${label}: symbol \${${spec.symbol}} was never captured`);
      continue;
    }
    if (spec.length !== undefined && String(val).length !== spec.length) {
      failures.push(`${label}: symbol \${${spec.symbol}} length ${String(val).length} != ${spec.length} (value=${val})`);
    }
    if (spec.alphabet) {
      const set = new Set(spec.alphabet.split(""));
      const bad = [...String(val)].find((ch) => !set.has(ch));
      if (bad !== undefined) failures.push(`${label}: symbol \${${spec.symbol}} char "${bad}" not in declared alphabet (value=${val})`);
    }
  }

  return failures;
}

// --- Normalization (recorded / compared) ----------------------------------------------
function normalizeStep(step, res, captures) {
  const out = { status: res.status };
  if (step.expect?.headers && step.expect.headers.mode === "exact-header") {
    out.headers = normalizeHeaders(res.headers, { captures });
  }
  const bodySpec = step.expect?.body;
  if (bodySpec && bodySpec.mode) {
    out.body = normalizeBody(bodySpec.mode, res.body, {
      captures,
      volatileFields: bodySpec.volatileFields || []
    });
  }
  return out;
}

async function normalizePostState(postState, dataDir, symbols, captures) {
  const out = {};
  if (!postState) return out;

  // Files and directory entries FIRST — before any DB open creates -wal/-shm side effects.
  if (Array.isArray(postState.files) && postState.files.length) {
    out.files = {};
    for (const spec of postState.files) {
      const rel = substStr(spec.path, symbols);
      const full = join(dataDir, rel);
      if (!existsSync(full)) {
        out.files[rel] = { present: false };
        continue;
      }
      const st = statSync(full);
      out.files[rel] = st.isDirectory()
        ? { present: true, type: "dir" }
        : { present: true, type: "file", size: st.size, sha256: sha256Hex(readFileSync(full)) };
    }
  }

  if (Array.isArray(postState.dirEntries) && postState.dirEntries.length) {
    out.dirEntries = {};
    for (const spec of postState.dirEntries) {
      const rel = substStr(spec.path, symbols);
      const full = join(dataDir, rel);
      out.dirEntries[rel] = existsSync(full) ? readdirSync(full).sort() : null;
    }
  }

  if (Array.isArray(postState.sql) && postState.sql.length) {
    out.sql = {};
    const Database = await loadBetterSqlite();
    const dbPath = join(dataDir, "artifacts.db");
    const db = new Database(dbPath, { fileMustExist: true });
    try {
      for (const spec of postState.sql) {
        const query = substStr(spec.query, symbols);
        let rows = db.prepare(query).all();
        // Blank declared-volatile columns, then serialize deterministically.
        const volatile = new Set(spec.volatileFields || []);
        rows = rows.map((row) => {
          const o = {};
          for (const k of Object.keys(row).sort()) o[k] = volatile.has(k) ? "<volatile>" : row[k];
          return o;
        });
        out.sql[spec.name] = rows;
      }
    } finally {
      db.close();
    }
  }
  // Back-substitute captured ids/tokens across the WHOLE post-state tree — keys (file paths,
  // directory entries) as well as values (SQL columns) — so goldens are run-independent.
  return JSON.parse(backsubString(JSON.stringify(out), captures));
}

async function applyAfterSql(statements, dataDir, symbols) {
  if (!Array.isArray(statements) || statements.length === 0) return;
  const Database = await loadBetterSqlite();
  const dbPath = join(dataDir, "artifacts.db");
  const db = new Database(dbPath, { fileMustExist: true });
  try {
    for (const statement of statements) db.exec(substStr(statement, symbols));
  } finally {
    db.close();
  }
}

// --- Fixtures --------------------------------------------------------------------------
function materializeFixture(name, destDataDir) {
  const src = join(FIXTURES_DIR, name);
  if (!existsSync(src)) {
    throw new Error(`Unknown fixture "${name}" (expected directory ${src})`);
  }
  // Copy fixture contents into the fresh data dir, skipping the .gitkeep placeholder. For
  // the empty-v21 fixture this copies nothing; the server migrates the empty dir to v21 on
  // boot, which is exactly the "freshly-migrated empty DB" starting state.
  cpSync(src, destDataDir, {
    recursive: true,
    filter: (s) => basename(s) !== ".gitkeep"
  });
}

// --- Per-case execution ----------------------------------------------------------------
async function runCaseAgainst(driver, caseDef) {
  const dataDir = mkdtempSync(join(tmpdir(), `conf-data-${driver.name}-`));
  const port = await freePort();
  const env = { ...baseEnv(), ...(caseDef.env || {}) };
  const symbols = { ...constantSymbols() }; // forward-substitution table (constants + captures)
  const captures = {};                       // high-entropy runtime symbols (back-substituted)
  const assertionFailures = [];
  let handle = null;

  try {
    materializeFixture(caseDef.fixture, dataDir);
    handle = await driver.start({ dataDir, port, env });
    await waitForHealth(handle.baseUrl);

    const steps = [];
    for (let i = 0; i < caseDef.steps.length; i++) {
      const step = caseDef.steps[i];
      const label = `step[${i}]${step.name ? ` ${step.name}` : ""}`;
      const req = buildRequest(step.request, symbols);
      const res = await httpRequest({ baseUrl: handle.baseUrl, ...req });

      // Captures feed both later substitutions and back-substitution in goldens.
      for (const [name, path] of Object.entries(step.capture || {})) {
        let parsed;
        try {
          parsed = JSON.parse(res.body.toString("utf8"));
        } catch {
          assertionFailures.push(`${label}: cannot capture \${${name}} — response body is not JSON`);
          continue;
        }
        const value = getByPath(parsed, path);
        if (value === undefined) {
          assertionFailures.push(`${label}: capture path "${path}" for \${${name}} resolved to undefined`);
          continue;
        }
        captures[name] = value;
        symbols[name] = value;
      }

      await applyAfterSql(step.afterSql, dataDir, symbols);
      assertionFailures.push(...evalStepAssertions(step, res, captures, label));
      const normalizedStep = normalizeStep(step, res, captures);

      // Uniformity contract: this response must be byte-for-byte indistinguishable from an
      // earlier step's (status + headers + body). This is how "invalid, revoked and expired
      // are the SAME 404" and the invariant-3 concealment matrix are proven inside one run,
      // independently of what any golden happens to hold.
      const sameAs = step.expect?.sameAsStep;
      if (sameAs !== undefined) {
        const other = steps[sameAs];
        if (!other) {
          assertionFailures.push(`${label}: sameAsStep ${sameAs} does not refer to an earlier step`);
        } else if (!deepEqual(other, normalizedStep)) {
          assertionFailures.push(
            `${label}: response is distinguishable from step[${sameAs}] (concealment/uniformity broken):\n` +
            describeDiff(other, normalizedStep)
          );
        }
      }

      steps.push(normalizedStep);
    }

    // Stop cleanly BEFORE inspecting persistent state.
    await handle.stop();
    handle = null;

    const postState = await normalizePostState(caseDef.postState, dataDir, symbols, captures);
    return { normalized: { steps, postState }, assertionFailures };
  } finally {
    if (handle) await handle.stop();
    rmSync(dataDir, { recursive: true, force: true });
  }
}

// --- Golden comparison -----------------------------------------------------------------
function diffNormalized(golden, actual) {
  const diffs = [];
  const gSteps = golden.steps || [];
  const aSteps = actual.steps || [];
  if (gSteps.length !== aSteps.length) {
    diffs.push(`step count: golden ${gSteps.length} vs actual ${aSteps.length}`);
  }
  const n = Math.max(gSteps.length, aSteps.length);
  for (let i = 0; i < n; i++) {
    const g = gSteps[i] || {};
    const a = aSteps[i] || {};
    if (g.status !== a.status) diffs.push(`step[${i}].status: golden ${g.status} vs actual ${a.status}`);
    if (!deepEqual(g.headers ?? null, a.headers ?? null)) {
      diffs.push(`step[${i}].headers differ:\n${describeDiff(g.headers ?? null, a.headers ?? null)}`);
    }
    if (!deepEqual(g.body ?? null, a.body ?? null)) {
      diffs.push(`step[${i}].body differ:\n${describeDiff(g.body ?? null, a.body ?? null)}`);
    }
  }
  if (!deepEqual(golden.postState ?? {}, actual.postState ?? {})) {
    diffs.push(`postState differ:\n${describeDiff(golden.postState ?? {}, actual.postState ?? {})}`);
  }
  return diffs;
}

// --- Orchestration ---------------------------------------------------------------------
async function runImpl(driver, cases, { record }) {
  const results = [];
  for (const caseDef of cases) {
    if (caseDef.pending) {
      results.push({ id: caseDef.id, status: "pending", detail: caseDef.pending });
      continue;
    }
    try {
      const { normalized, assertionFailures } = await runCaseAgainst(driver, caseDef);

      if (record && driver.name === "node") {
        if (assertionFailures.length) {
          results.push({ id: caseDef.id, status: "fail", detail: `refusing to record (assertions failed):\n  ${assertionFailures.join("\n  ")}` });
          continue;
        }
        writeGolden(caseDef.id, normalized);
        results.push({ id: caseDef.id, status: "recorded" });
        continue;
      }

      const golden = loadGolden(caseDef.id);
      if (!golden) {
        results.push({ id: caseDef.id, status: "fail", detail: "no golden recorded (run with --record --impl node)" });
        continue;
      }
      const diffs = diffNormalized(golden, normalized);
      const problems = [...assertionFailures, ...diffs];
      results.push(problems.length
        ? { id: caseDef.id, status: "fail", detail: problems.join("\n  ") }
        : { id: caseDef.id, status: "pass" });
    } catch (err) {
      results.push({ id: caseDef.id, status: "error", detail: err.stack || String(err) });
    }
  }
  return results;
}

function printResults(implName, results) {
  console.log(`\n=== impl: ${implName} ===`);
  for (const r of results) {
    const tag =
      r.status === "pass" ? "PASS " :
      r.status === "recorded" ? "REC  " :
      r.status === "pending" ? "PEND " :
      r.status === "fail" ? "FAIL " : "ERROR";
    console.log(`  [${tag}] ${r.id}${r.detail ? "\n    " + r.detail.replace(/\n/g, "\n    ") : ""}`);
  }
  const count = (s) => results.filter((r) => r.status === s).length;
  console.log(
    `  -- ${implName}: ${count("pass")} pass, ${count("recorded")} recorded, ` +
    `${count("fail")} fail, ${count("error")} error, ${count("pending")} pending`
  );
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help || !args.impl) {
    console.log(USAGE);
    process.exit(args.help ? 0 : 2);
  }
  if (!["node", "rust", "both"].includes(args.impl)) {
    console.error(`--impl must be node|rust|both (got "${args.impl}")`);
    process.exit(2);
  }
  if (args.record && args.impl !== "node") {
    console.error("--record is only valid with --impl node (goldens are recorded from the Node reference).");
    process.exit(2);
  }

  let cases = loadCases();
  if (args.filter) {
    cases = cases.filter((c) => c.id.includes(args.filter) || (c.tags || []).some((t) => t.includes(args.filter)));
  }
  if (cases.length === 0) {
    console.error("No cases matched.");
    process.exit(2);
  }

  const impls = args.impl === "both" ? ["node", "rust"] : [args.impl];
  let anyFailure = false;

  for (const impl of impls) {
    if (impl === "node") {
      const driver = createNodeDriver();
      try {
        const results = await runImpl(driver, cases, { record: args.record });
        printResults("node", results);
        if (results.some((r) => r.status === "fail" || r.status === "error")) anyFailure = true;
      } finally {
        driver.dispose();
      }
    } else {
      const driver = createRustDriver();
      if (!driver.available()) {
        console.log(`\n=== impl: rust ===`);
        console.log(`  [SKIP ] rust unavailable — ${driver.unavailableReason()}`);
        if (args.impl === "rust") {
          anyFailure = true; // an explicit --impl rust with no binary is a hard failure
        }
        continue;
      }
      const results = await runImpl(driver, cases, { record: false });
      printResults("rust", results);
      if (results.some((r) => r.status === "fail" || r.status === "error")) anyFailure = true;
      driver.dispose();
    }
  }

  process.exit(anyFailure ? 1 : 0);
}

main().catch((err) => {
  console.error("runner crashed:", err.stack || err);
  process.exit(1);
});
