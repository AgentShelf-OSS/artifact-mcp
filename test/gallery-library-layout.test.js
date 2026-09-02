import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { renderGallery } from "../lib/portal.js";

const root = new URL("../", import.meta.url);

test("Rust gallery template places every collection control in the top toolbar", async () => {
  const template = await readFile(new URL("templates/gallery.html", root), "utf8");

  assert.match(template, /<section class="collection-tools" aria-label="Artifact library controls">/);
  assert.match(template, /data-filter-view="all"/);
  assert.match(template, /<select id="org-filter" aria-label="Filter by organization">/);
  assert.match(template, /<select id="category-filter" aria-label="Filter by category">/);
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
  assert.match(script, /orgFilter\.addEventListener\("change"/);
  assert.match(script, /categoryFilter\.addEventListener\("change"/);
  assert.match(script, /x-artifact-mutation/);
});

test("gallery keeps the approved brand block and organization-colored card stripe", async () => {
  const css = await readFile(new URL("assets/portal.css", root), "utf8");

  assert.match(css, /\.brand-mark::after\{[^}]*inset:4px -5px -5px 4px;[^}]*z-index:-1;[^}]*background:var\(--brass\)[^}]*\}/);
  assert.doesNotMatch(css, /\.brand-mark::after\{[^}]*border-(?:right|bottom):/);
  assert.match(css, /\.card::before\{[^}]*width:4px;[^}]*background:var\(--org-k,var\(--brass\)\)[^}]*\}/);
  assert.doesNotMatch(css, /\.card::before\{[^}]*background:var\(--brass\)[^}]*\}/);
  assert.match(css, /@media\(forced-colors:active\)\{\.card::before,[^}]*background:CanvasText/);
});

test("active primary navigation paints one short accent indicator", async () => {
  const css = await readFile(new URL("assets/portal.css", root), "utf8");

  assert.doesNotMatch(css, /\.nav-link\.active\{[^}]*border-bottom-color:var\(--brass\)/);
  assert.match(css, /\.nav-link\.active::after\{background:var\(--brass\)\}/);
});

test("Node gallery renderer mirrors the no-rail toolbar contract", () => {
  const html = renderGallery(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    [{ org: "acme", items: [{ id: "artifact-1", org: "acme", title: "Library test", client_id: "test", is_bundle: 0, bytes: 1, category: "Reports" }] }],
  );

  assert.match(html, /<section class="collection-tools" aria-label="Artifact library controls">/);
  assert.match(html, /data-filter-view="hidden"/);
  assert.match(html, /id="org-filter" aria-label="Filter by organization"/);
  assert.match(html, /id="category-filter" aria-label="Filter by category"/);
  assert.match(html, /<option value="Reports">Reports \(1\)<\/option>/);
  assert.match(html, /id="sort" aria-label="Sort artifacts"/);
  assert.match(html, /data-reset-filters/);
  assert.doesNotMatch(html, /<aside[^>]*filter-rail/);
  assert.doesNotMatch(html, /<aside/);
});

test("Node gallery cards expose configured organization colors to the shared stripe", () => {
  const html = renderGallery(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    [{ org: "acme", items: [{ id: "artifact-1", org: "acme", title: "Library test", client_id: "test", is_bundle: 0, bytes: 1 }] }],
    new Map(),
    new Map(),
    new Map(),
    new Map(),
    { acme: "#397b6f" },
  );

  assert.match(html, /<article class="card[^"]*"[^>]*style="--org-k:#397b6f;/);
});

test("gallery category controls carry organization-scoped options and creation actions", () => {
  const html = renderGallery(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    [
      { org: "agentshelf", items: [{ id: "artifact-1", org: "agentshelf", title: "Arty", client_id: "test", is_bundle: 0, bytes: 1, category: "UI/UX" }] },
      { org: "homelab", items: [{ id: "artifact-2", org: "homelab", title: "Dashboard", client_id: "test", is_bundle: 0, bytes: 1, category: "Dashboards" }] },
    ],
    new Map(),
    new Map(),
    new Map(),
    new Map(),
    {},
    {},
    { agentshelf: ["Specs", "UI/UX"], homelab: ["Dashboards", "Runbooks"] },
  );

  const artyCategory = html.match(/<select class="category-menu"[^>]*aria-label="Change category for Arty"[^>]*>([\s\S]*?)<\/select>/)?.[1] || "";
  assert.match(artyCategory, /<option value="UI\/UX" selected>UI\/UX<\/option>/);
  assert.match(artyCategory, /<option value="Specs">Specs<\/option>/);
  assert.doesNotMatch(artyCategory, /Dashboards|Runbooks/);
  assert.match(artyCategory, /<option value="__create_category__">\+ Category<\/option>/);
  assert.match(html, /id="category-filter"[^>]*>[\s\S]*?<option value="__create_category__">\+ Category<\/option>/);
  assert.match(html, /id="org-category-data"[^>]*data-json="[^"]*&quot;agentshelf&quot;[^"]*&quot;homelab&quot;/);
});
