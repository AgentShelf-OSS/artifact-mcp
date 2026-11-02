import { test, assert, readText, readParts } from "./harness.js";

test("Rust gallery template places every collection control in the top toolbar", async () => {
  const template = await readText("templates/gallery.html");

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
    readText("assets/portal.css"),
    readParts("assets/portal"),
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
  const css = await readText("assets/portal.css");

  assert.match(css, /\.brand-mark::after\{[^}]*inset:4px -5px -5px 4px;[^}]*z-index:-1;[^}]*background:var\(--brass\)[^}]*\}/);
  assert.doesNotMatch(css, /\.brand-mark::after\{[^}]*border-(?:right|bottom):/);
  assert.match(css, /\.card::before\{[^}]*width:4px;[^}]*background:var\(--org-k,var\(--brass\)\)[^}]*\}/);
  assert.doesNotMatch(css, /\.card::before\{[^}]*background:var\(--brass\)[^}]*\}/);
  assert.match(css, /@media\(forced-colors:active\)\{\.card::before,[^}]*background:CanvasText/);
});

test("active primary navigation paints one short accent indicator", async () => {
  const css = await readText("assets/portal.css");

  assert.doesNotMatch(css, /\.nav-link\.active\{[^}]*border-bottom-color:var\(--brass\)/);
  assert.match(css, /\.nav-link\.active::after\{background:var\(--brass\)\}/);
});
