// Guards that every shipped browser asset parses.
//
// `assets/shell.js` once shipped truncated: the closing `})();` of its IIFE was lost, so the browser
// could not parse the block and the reaction, share and comment controls were inert. Every
// server-side test and conformance case still passed, because nothing executed page JavaScript.
// A brace-balance check is not a substitute: regex literals such as `/[a-z{]/` contain unbalanced
// braces. Use the real parser.
//
// This check moved here from the Rust suite (tests/native/u15_assets.rs) when the Rust tests stopped
// starting Node (ADR-0024).
import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";

const assetsDir = fileURLToPath(new URL("../assets/", import.meta.url));

test("every browser asset parses", () => {
  const assets = readdirSync(assetsDir).filter((name) => name.endsWith(".js")).sort();
  assert.ok(assets.length > 0, "no browser assets were checked");
  const broken = [];
  for (const name of assets) {
    const result = spawnSync(process.execPath, ["--check", assetsDir + name], { encoding: "utf8" });
    if (result.status !== 0) {
      broken.push(`${name}: ${result.stderr.split("\n").slice(0, 3).join(" | ")}`);
    }
  }
  assert.deepEqual(broken, [], "browser assets the browser cannot parse");
});
