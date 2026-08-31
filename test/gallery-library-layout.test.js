import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { renderGallery } from "../lib/portal.js";

const root = new URL("../", import.meta.url);

test("Rust gallery template places every collection control in the top toolbar", async () => {
  const template = await readFile(new URL("templates/gallery.html", root), "utf8");

  assert.match(template, /<section class="collection-tools" aria-label="Artifact library controls">/);
  assert.match(template, /data-filter-view="all"/);
  assert.match(template, /data-filter-org="all"/);
  assert.match(template, /data-filter-category="all"/);
  assert.match(template, /data-filter-view="hidden"/);
  assert.match(template, /<select id="sort" aria-label="Sort artifacts">/);
  assert.match(template, /data-layout="grid"/);
  assert.match(template, /data-layout="list"/);
  assert.match(template, /data-reset-filters/);
  assert.doesNotMatch(template, /filter-rail/);
  assert.doesNotMatch(template, /<aside/);
});

test("gallery foundation keeps three, two, then one responsive columns and stateful controls", async () => {
  const [css, script] = await Promise.all([
    readFile(new URL("assets/portal.css", root), "utf8"),
    readFile(new URL("assets/portal.js", root), "utf8"),
  ]);

  assert.match(css, /\.artifact-grid\{grid-template-columns:repeat\(3,minmax\(0,1fr\)\)/);
  assert.match(css, /@media\(max-width:1120px\)\{[\s\S]*?\.artifact-grid\{grid-template-columns:repeat\(2,minmax\(0,1fr\)\)/);
  assert.match(css, /@media\(max-width:760px\)\{[\s\S]*?\.artifact-grid\{grid-template-columns:1fr;/);
  assert.match(script, /activeView === "hidden"/);
  assert.match(script, /artifact-library-state/);
  assert.match(script, /scrollY: Math\.max\(0, window\.scrollY/);
  assert.match(script, /artifact-library-return/);
  assert.match(script, /window\.addEventListener\("pagehide", function \(\) \{ saveLibraryState\(\); \}\)/);
  assert.match(script, /window\.addEventListener\("pageshow", function \(event\) \{/);
  assert.match(script, /if \(!event\.persisted\) return;/);
  assert.match(script, /requestAnimationFrame\(function \(\) \{ window\.scrollTo\(0, returnScrollY\); \}\)/);
  assert.match(script, /localStorage\.setItem\("artifact-layout"/);
  assert.match(script, /x-artifact-mutation/);
});

test("Node gallery renderer mirrors the no-rail toolbar contract", () => {
  const html = renderGallery(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    [{ org: "acme", items: [{ id: "artifact-1", org: "acme", title: "Library test", client_id: "test", is_bundle: 0, bytes: 1, category: "Reports" }] }],
  );

  assert.match(html, /<section class="collection-tools" aria-label="Artifact library controls">/);
  assert.match(html, /data-filter-view="hidden"/);
  assert.match(html, /data-filter-category="Reports"/);
  assert.match(html, /id="sort" aria-label="Sort artifacts"/);
  assert.match(html, /data-reset-filters/);
  assert.doesNotMatch(html, /<aside[^>]*filter-rail/);
  assert.doesNotMatch(html, /<aside/);
});
