import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const requiredFiles = [
  "scripts/check-mcp-surface.mjs",
  "src/mcp/tool_defs.rs",
  "src/mcp/dispatch.rs",
  "src/mcp/resources.rs",
  "src/mcp/tasks.rs",
  "src/security/oauth.rs",
  "README.md",
  "Cargo.toml",
  "package.json",
  "tests/native/main.rs",
  "tests/native/u25_api_key_capabilities.rs",
  "tests/native/u26_mcp_2026.rs",
  "conformance/mcp.tool-output-schemas.json",
  "conformance/goldens/mcp.tools-list.json",
  "conformance/cases/mcp.tools-list.json",
  "conformance/cases/mcp.initialize.json",
  "conformance/cases/mcp.read-artifact.json",
];

async function fixture() {
  const target = mkdtempSync(join(tmpdir(), "artifact-mcp-surface-"));
  for (const path of requiredFiles) {
    const destination = join(target, path);
    await mkdir(dirname(destination), { recursive: true });
    await writeFile(destination, readFileSync(join(root, path)));
  }
  return target;
}

test("an unsynchronized modern declaration fails the derived surface gate", async () => {
  const target = await fixture();
  try {
    const toolDefinitions = join(target, "src/mcp/tool_defs.rs");
    const original = readFileSync(toolDefinitions, "utf8");
    writeFileSync(
      toolDefinitions,
      original.replace(
        'name: "regenerate_artifact_preview".to_owned(),',
        'name: "regenerate_artifact_preview_extra".to_owned(),',
      ),
    );
    assert.throws(
      () => execFileSync(process.execPath, ["scripts/check-mcp-surface.mjs"], { cwd: target, encoding: "utf8", stdio: "pipe" }),
      (error) => {
        const stderr = error.stderr.toString();
        return stderr.includes("regenerate_artifact_preview_extra has no typed output schema") &&
          stderr.includes("regenerate_artifact_preview_extra is advertised but missing from tools/call dispatch") &&
          stderr.includes("regenerate_artifact_preview_extra is advertised but absent from README MCP documentation");
      },
    );
  } finally {
    rmSync(target, { recursive: true, force: true });
  }
});

test("a duplicate modern declaration is rejected instead of being hidden by deduplication", async () => {
  const target = await fixture();
  try {
    const toolDefinitions = join(target, "src/mcp/tool_defs.rs");
    writeFileSync(
      toolDefinitions,
      `${readFileSync(toolDefinitions, "utf8")}\n    name: "regenerate_artifact_preview".to_owned(),\n`,
    );
    assert.throws(
      () => execFileSync(process.execPath, ["scripts/check-mcp-surface.mjs"], { cwd: target, encoding: "utf8", stdio: "pipe" }),
      (error) => error.stderr.toString().includes("modernDeclarations contain duplicate names: regenerate_artifact_preview"),
    );
  } finally {
    rmSync(target, { recursive: true, force: true });
  }
});
