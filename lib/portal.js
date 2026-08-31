// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// Server-rendered gallery portal. Cards show authenticated static thumbnails; the full
// artifact viewer remains live and sandboxed. Light + dark themes.
import { APP_BRAND, APP_NAME } from "./config.js";
import { viewerCanManageArtifact } from "./access.js";
import { readFileSync } from "node:fs";

// Brand subtitle: the host of the configured public base URL, so it follows any deployment.
const SITE_HOST = (() => {
  try { return new URL(process.env.PUBLIC_BASE_URL || "http://localhost:3480").host; }
  catch { return "localhost:3480"; }
})();
const THEME_BOOT = readFileSync(new URL("../assets/theme-boot.js", import.meta.url), "utf8");
const NOT_FOUND_CSS = readFileSync(new URL("../assets/not-found.css", import.meta.url), "utf8");
const NOT_SIGNED_IN_CSS = readFileSync(new URL("../assets/not-signed-in.css", import.meta.url), "utf8");
const ACCESS_RETRY_CSS = readFileSync(new URL("../assets/access-retry.css", import.meta.url), "utf8");

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]
  );
}

// Encode a value as a JS literal safe to embed inside an inline <script> — escapes the
// characters that could break out of the script element or the JS string.
function jsLiteral(value) {
  return JSON.stringify(value == null ? "" : String(value))
    .replace(/</g, "\\u003c")
    .replace(/>/g, "\\u003e")
    .replace(/&/g, "\\u0026")
    .replace(/\u2028/g, "\\u2028")
    .replace(/\u2029/g, "\\u2029");
}

export const PORTAL_FAVICON = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'%3E%3Crect width='64' height='64' rx='7' fill='%23142235'/%3E%3Cpath d='M18 47 30 15h5l12 32h-7l-3-9H27l-3 9Zm11-15h6l-3-10Z' fill='%23D5A252'/%3E%3C/svg%3E";

// An org's accent color: an explicitly set hex (from Settings) wins; otherwise FNV-1a gives
// every org ID a stable hue without any hardcoded tenant list. Middle lightness keeps the
// accent visible against both application themes.
export function orgColor(name, color) {
  if (color && /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/.test(String(color))) return String(color);
  // "admin" is the built-in all-orgs pseudo-org (no registry row); give it a fixed accent.
  if (String(name) === "admin") return "#66578B";
  let hash = 2166136261;
  for (const char of String(name ?? "")) {
    hash ^= char.charCodeAt(0);
    hash = Math.imul(hash, 16777619);
  }
  return `hsl(${(hash >>> 0) % 360} 68% 52%)`;
}

const ICONS = {
  search: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="7"></circle><path d="m20 20-3.8-3.8"></path></svg>`,
  settings: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="3"></circle><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1-2.8 2.8-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.6v.2h-4V21a1.7 1.7 0 0 0-1-1.6 1.7 1.7 0 0 0-1.9.3l-.1.1L4.2 17l.1-.1a1.7 1.7 0 0 0 .3-1.9A1.7 1.7 0 0 0 3 14H2.8v-4H3a1.7 1.7 0 0 0 1.6-1 1.7 1.7 0 0 0-.3-1.9L4.2 7 7 4.2l.1.1a1.7 1.7 0 0 0 1.9.3 1.7 1.7 0 0 0 1-1.6v-.2h4V3a1.7 1.7 0 0 0 1 1.6 1.7 1.7 0 0 0 1.9-.3l.1-.1L19.8 7l-.1.1a1.7 1.7 0 0 0-.3 1.9 1.7 1.7 0 0 0 1.6 1h.2v4H21a1.7 1.7 0 0 0-1.6 1Z"></path></svg>`,
  bell: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M18 8a6 6 0 0 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9"></path><path d="M10 21h4"></path></svg>`,
  theme: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M20.5 15.2A8.5 8.5 0 0 1 8.8 3.5 8.5 8.5 0 1 0 20.5 15.2Z"></path></svg>`,
  signout: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M10 5H5v14h5"></path><path d="m14 8 4 4-4 4M8 12h10"></path></svg>`,
  open: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14 5h5v5M19 5l-8 8"></path><path d="M19 13v6H5V5h6"></path></svg>`,
  download: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4v11m0 0 4-4m-4 4-4-4"></path><path d="M5 19h14"></path></svg>`,
  heart: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.7l-1.1-1.1a5.5 5.5 0 0 0-7.8 7.8l1.1 1.1L12 21l7.8-7.5 1.1-1.1a5.5 5.5 0 0 0-.1-7.8Z"></path></svg>`,
  up: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 11 5-6 5 6"></path><path d="M12 5v14"></path></svg>`,
  down: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 13 5 6 5-6"></path><path d="M12 5v14"></path></svg>`,
  back: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m15 18-6-6 6-6"></path></svg>`,
  forward: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m9 18 6-6-6-6"></path></svg>`,
  eye: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"></path><circle cx="12" cy="12" r="2.5"></circle></svg>`,
  eyeOff: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m3 3 18 18"></path><path d="M10.6 6.2A10.5 10.5 0 0 1 12 6c6 0 9.5 6 9.5 6a17.7 17.7 0 0 1-3.1 3.8M6.1 6.1C3.8 7.7 2.5 10 2.5 12c0 0 3.5 6 9.5 6 1.4 0 2.7-.3 3.8-.8"></path><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2"></path></svg>`,
  share: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="18" cy="5" r="3"></circle><circle cx="6" cy="12" r="3"></circle><circle cx="18" cy="19" r="3"></circle><path d="m8.7 10.6 6.6-4.1M8.7 13.4l6.6 4.1"></path></svg>`,
  more: `<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="5" cy="12" r="1"></circle><circle cx="12" cy="12" r="1"></circle><circle cx="19" cy="12" r="1"></circle></svg>`
};

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

function fmtDate(s) {
  const m = String(s || "").match(/^(\d{4})-(\d{2})-(\d{2})/);
  if (!m) return "";
  return `${MONTHS[Number(m[2]) - 1]} ${Number(m[3])}, ${m[1]}`;
}

function relativeTime(s) {
  const date = new Date(String(s || "").replace(" ", "T") + "Z");
  const seconds = Math.max(0, Math.floor((Date.now() - date.getTime()) / 1000));
  if (!Number.isFinite(seconds) || seconds < 60) return "Just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  if (seconds < 604800) return `${Math.floor(seconds / 86400)}d ago`;
  return fmtDate(s);
}

function fmtBytes(n) {
  const bytes = Number(n || 0);
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

// Cache-bust digest-addressed thumbnails and viewer iframes by content version. Raw responses carry
// max-age=60, so without this a revised artifact could re-serve a browser-cached frame
// from a prior revision (e.g. an old inline <script> the current, stripped body no longer
// has). body_sha256 changes only when the body changes; revision is a pre-PBI-022 fallback.
function bodyVersion(a) {
  const v = a && (a.body_sha256 || a.revision);
  return v ? String(v).slice(0, 12) : "";
}

function card(a, reaction = {}, aggregate = {}, showAggregate = false, views = null, orgNames = [], categories = [], orgAccent = null) {
  const hue = orgColor(a.org, orgAccent);
  const who = a.uploader_label || a.client_id;
  const digest = a.body_sha256 ? String(a.body_sha256) : "";
  const thumbnailSrc = `/thumbnails/${esc(a.id)}${digest ? `?v=${esc(digest)}` : ""}`;
  const favorite = !!reaction.favorite;
  const vote = Number(reaction.vote || 0);
  const category = a.category && a.category.trim() ? a.category.trim() : "";
  const updatedAt = a.updated_at || a.created_at || "";
  const ownedByViewer = !!a.is_owned_by_viewer;
  const showVisibility = showAggregate || ownedByViewer;
  const showDelete = showVisibility;
  const needsReview = showAggregate ? Number(aggregate.down || 0) > 0 : vote < 0;
  const dlAct = a.is_bundle
    ? `<button class="act card-download" type="button" disabled title="Bundles open in the Viewer and do not have a single HTML download" aria-label="HTML download unavailable for ${esc(a.title)}">${ICONS.download}<span>HTML</span></button>`
    : `<a class="act card-download dl" href="/raw/${esc(a.id)}?download" download aria-label="Download ${esc(a.title)} as HTML" title="Download HTML">${ICONS.download}<span>HTML</span></a>`;
  const desc = a.description ? `<p class="desc">${esc(a.description)}</p>` : `<p class="desc desc-empty">No description supplied.</p>`;
  const aggregateStrip = showAggregate
    ? `<div class="sentiment" aria-label="Aggregate reactions: ${Number(aggregate.favorites || 0)} favorites, ${Number(aggregate.up || 0)} positive, ${Number(aggregate.down || 0)} negative">
        <span title="Favorites">${ICONS.heart}${Number(aggregate.favorites || 0)}</span>
        <span title="Positive votes">${ICONS.up}${Number(aggregate.up || 0)}</span>
        <span title="Negative votes">${ICONS.down}${Number(aggregate.down || 0)}</span>
      </div>`
    : vote
      ? `<span class="your-vote ${vote > 0 ? "positive" : "negative"}">${vote > 0 ? "Approved" : "Needs work"}</span>`
      : "";
  const categoryOptions = categories
    .filter((value) => value !== category)
    .map((value) => `<option value="${esc(value)}">${esc(value || "Uncategorized")}</option>`)
    .join("");
  const orgOptions = showAggregate
    ? orgNames
        .filter((org) => org !== a.org)
        .map((org) => `<option value="${esc(org)}">${esc(org)}</option>`)
        .join("")
    : "";
  const visibilityControl = showVisibility
    ? `<button class="act icon-act visibility" data-action="visibility" type="button" aria-label="${a.hidden ? "Show" : "Hide"} ${esc(a.title)} in the gallery" title="${a.hidden ? "Show in gallery" : "Hide from gallery"}">${a.hidden ? ICONS.eyeOff : ICONS.eye}</button>`
    : "";
  const menuId = `card-menu-${esc(a.id)}`;

  return `
  <article class="card${a.hidden ? " is-hidden" : ""}" data-ui="artifact-card" data-id="${esc(a.id)}" data-org="${esc(a.org)}" data-category="${esc(category)}" data-hidden="${a.hidden ? 1 : 0}" data-fav="${favorite ? 1 : 0}" data-vote="${vote}" data-needs-review="${needsReview ? 1 : 0}" data-owned="${ownedByViewer ? 1 : 0}"
           data-q="${esc((a.title + " " + a.org + " " + category + " " + who + " " + a.client_id + " " + (a.description || "")).toLowerCase())}"
           data-updated="${esc(String(updatedAt))}"
           style="--org-k:${hue};--k:color-mix(in oklab,var(--org-k) 72%,var(--ink))">
    <div class="preview">
      <img class="pv" src="${thumbnailSrc}" loading="lazy" decoding="async" width="1200" height="750"
           alt="" aria-hidden="true">
      <div class="preview-skeleton" aria-hidden="true"><span></span><span></span><span></span></div>
      <div class="glass" aria-hidden="true"></div>
      <a class="preview-hit" href="/${esc(a.id)}" aria-label="Open ${esc(a.title)}"></a>
      <span class="format-badge ${a.is_bundle ? "bundle" : "single"}">${a.is_bundle ? "Bundle" : "HTML"}</span>
      ${favorite ? `<span class="fav-badge" title="In your favorites">${ICONS.heart}<span class="sr-only">Favorited</span></span>` : ""}
      <span class="pid">/${esc(a.id)}</span>
    </div>
    <div class="label">
      <div class="card-overline">
        <span class="org-tag"><span class="org-dot"></span>${esc(a.org)} · ${esc(category || "Uncategorized")}</span>
        <time datetime="${esc(String(updatedAt).replace(" ", "T"))}">${relativeTime(updatedAt)}</time>
      </div>
      <h3 class="card-title"><a href="/${esc(a.id)}">${esc(a.title)}</a></h3>
      ${desc}
      <div class="facts">
        <span><span class="fact-label">Published by</span>${esc(who)}</span>
        <span><span class="fact-label">Format</span>${a.is_bundle ? "Multi-file" : fmtBytes(a.bytes)}</span>
        ${views ? `<span class="view-badge" title="${Number(views.unique_viewers || 0)} unique viewer${Number(views.unique_viewers || 0) === 1 ? "" : "s"}">👁 ${Number(views.views || 0)}</span>` : ""}
        ${aggregateStrip}
      </div>
      <div class="actions">
        <a class="act open" href="/${esc(a.id)}">${ICONS.open}<span>Open</span></a>
        <button class="act save${favorite ? " active" : ""}" data-action="favorite" type="button" aria-pressed="${favorite ? "true" : "false"}" aria-label="${favorite ? "Remove" : "Save"} ${esc(a.title)} ${favorite ? "from" : "to"} favorites">${ICONS.heart}<span>${favorite ? "Saved" : "Save"}</span></button>
        <button class="act share" data-action="share" type="button" aria-label="Share ${esc(a.title)}">${ICONS.share}<span>Share</span></button>
        ${dlAct}
        ${visibilityControl}
        <button class="act icon-act more" data-action="more" type="button" aria-label="More actions for ${esc(a.title)}" aria-expanded="false" aria-controls="${menuId}">${ICONS.more}</button>
        <div class="card-menu" id="${menuId}" data-ui="card-menu" hidden>
          <label>Category<select class="category-menu" data-action="category" aria-label="Change category for ${esc(a.title)}"><option value="">${esc(category || "Uncategorized")}</option>${categoryOptions}</select></label>
          ${showAggregate && orgOptions ? `<label>Organization<select class="org-menu" data-action="move-org" aria-label="Move ${esc(a.title)} to another organization"><option value="">${esc(a.org)}</option>${orgOptions}</select></label>` : ""}
          ${showDelete ? '<button class="menu-action del" data-action="delete" type="button">Delete artifact</button>' : ""}
          <div class="move-confirm" role="group" aria-label="Confirm organization move" hidden>
            <span class="move-question"></span><button class="move-yes" type="button">Move</button><button class="move-no" type="button">Cancel</button>
          </div>
        </div>
      </div>
    </div>
  </article>`;
}

// Group an org's items by category and render a horizontal carousel per category
// (3 visible, most-recently-modified first, paged by arrows). ctx: { reactionFor, sentiment, isAdmin, viewCounts }.
function renderCategorySections(items, ctx) {
  const groups = new Map();
  for (const a of items) {
    const key = a.category && a.category.trim() ? a.category.trim() : "";
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(a);
  }
  const keys = [...groups.keys()].sort((x, y) => {
    if (x === "") return 1; // Uncategorized last
    if (y === "") return -1;
    return x.toLowerCase().localeCompare(y.toLowerCase());
  });
  const modified = (a) => String(a.updated_at || a.created_at || "");
  return keys
    .map((key) => {
      const label = key || "Uncategorized";
      const rows = groups.get(key).sort((a, b) => modified(b).localeCompare(modified(a)));
      const cards = rows.map((a) => card(a, ctx.reactionFor(a.id), ctx.sentiment.get(a.id) || {}, ctx.isAdmin, ctx.viewCounts.get(a.id) || null, ctx.orgNames, keys, (ctx.orgColors || {})[a.org])).join("");
      return `<div class="cat" data-category="${esc(key)}">
        <div class="cat-head">
          <h3 class="cat-name">${esc(label)}</h3>
          <span class="cat-count">${rows.length}</span>
          <span class="cat-rule"></span>
          <span class="cat-pos" aria-hidden="true"></span>
          <div class="cat-nav">
            <button class="cat-arrow" data-dir="-1" type="button" aria-label="Previous in ${esc(label)}" disabled>${ICONS.back}</button>
            <button class="cat-arrow" data-dir="1" type="button" aria-label="Next in ${esc(label)}">${ICONS.forward}</button>
          </div>
        </div>
        <div class="cat-track">${cards}</div>
      </div>`;
    })
    .join("");
}

// sections: [{ org, items: [row,...] }]. viewer: { email, org, isAdmin }.
// reactions: Map<id, {favorite, vote}> for this viewer.
// sentiment: admin-only Map<id, {up,down,favorites}> aggregate insight.
// viewCounts: same-org/admin aggregate Map<id, {views,unique_viewers}>; topViewed is admin-only by org.
export function renderGallery(viewer, sections, reactions = new Map(), sentiment = new Map(), viewCounts = new Map(), topViewed = new Map(), orgColors = {}, notificationState = {}) {
  const reactionFor = (id) => reactions.get(id) || { favorite: 0, vote: 0 };
  const isFav = (id) => !!reactionFor(id).favorite;
  const total = sections.reduce((n, s) => n + s.items.length, 0);
  const favoriteTotal = sections.reduce((n, s) => n + s.items.filter((a) => isFav(a.id)).length, 0);
  const showChips = sections.length > 1;
  const role = viewer.isAdmin ? "All organizations" : viewer.org || "Member";
  const orgNames = sections.map((section) => section.org);
  const allItems = sections
    .flatMap((section) => section.items)
    .sort((left, right) => {
      const byModified = String(right.updated_at || right.created_at || "").localeCompare(String(left.updated_at || left.created_at || ""));
      return byModified || String(left.id).localeCompare(String(right.id));
    });
  const hasDeleteActions = allItems.some((artifact) => viewer.isAdmin || artifact.is_owned_by_viewer);
  const categoryNames = [...new Set(allItems.map((artifact) => artifact.category && artifact.category.trim() ? artifact.category.trim() : ""))]
    .sort((left, right) => {
      if (!left) return 1;
      if (!right) return -1;
      return left.toLowerCase().localeCompare(right.toLowerCase());
    });
  const needsReviewTotal = allItems.filter((artifact) => viewer.isAdmin
    ? Number(sentiment.get(artifact.id)?.down || 0) > 0
    : Number(reactionFor(artifact.id).vote || 0) < 0).length;
  const intro = viewer.isAdmin
    ? "Published work across every organization you can access, including hidden artifacts."
    : `Visible ${esc(viewer.org || "organization")} work, plus uploads you have hidden.`;
  const notificationItems = Array.isArray(notificationState.items) ? notificationState.items : [];
  const unreadNotifications = Math.max(0, Number(notificationState.unread) || 0);
  const notificationRows = notificationItems.length
    ? notificationItems.map((row) => {
        const snippet = String(row.body || "").replace(/\s+/g, " ").trim().slice(0, 120);
        return `<a class="notif-row${row.unread ? " unread" : ""}" href="/${esc(encodeURIComponent(row.artifact_id))}?feedback=${esc(encodeURIComponent(row.id))}">
          <span class="notif-meta"><strong>${esc(row.author?.source === "discord" ? `${row.author.external_author_display || "Discord user"} · Discord` : row.author?.viewer_email || row.viewer_email || "")}</strong><time>${esc(relativeTime(row.created_at))}</time></span>
          <span class="notif-artifact">${esc(row.artifact_title)}</span>
          <span class="notif-snippet">${esc(snippet)}</span>
        </a>`;
      }).join("")
    : '<div class="notif-empty">No feedback yet.</div>';

  const chips = showChips
    ? `<div class="filter-group" id="org-filters" aria-label="Filter by organization">
        <span class="toolbar-label">Organization</span>
        <div class="filter-choices">
          <button class="filter-choice" data-filter-org="all" aria-pressed="true">All orgs <span>${total}</span></button>
          ${sections
            .map(
              (s) =>
                `<button class="filter-choice" data-filter-org="${esc(s.org)}" aria-pressed="false"><span><span class="dot" style="background:${orgColor(s.org, orgColors[s.org])}"></span>${esc(s.org)}</span><span>${s.items.length}</span></button>`
            )
            .join("")}
        </div>
      </div>`
    : "";
  const categoryFilters = categoryNames.length
    ? `<div class="filter-group" id="category-filters" aria-label="Filter by category">
        <span class="toolbar-label">Category</span>
        <div class="filter-choices">
          <button class="filter-choice" data-filter-category="all" aria-pressed="true">All categories <span>${total}</span></button>
          ${categoryNames.map((category) => `<button class="filter-choice" data-filter-category="${esc(category)}" aria-pressed="false">${esc(category || "Uncategorized")} <span>${allItems.filter((artifact) => (artifact.category && artifact.category.trim() ? artifact.category.trim() : "") === category).length}</span></button>`).join("")}
        </div>
      </div>`
    : "";
  const viewFilters = `<div class="filter-group filter-group-quick" id="view-filters" aria-label="Quick filters">
    <span class="toolbar-label">View</span>
    <div class="filter-choices">
      <button class="filter-choice" data-filter-view="all" aria-pressed="true">All <span>${total}</span></button>
      <button class="filter-choice" data-filter-view="favorites" aria-pressed="false">Favorites <span>${favoriteTotal}</span></button>
      <button class="filter-choice" data-filter-view="review" aria-pressed="false">${viewer.isAdmin ? "Needs review" : "My needs-work votes"} <span>${needsReviewTotal}</span></button>
      ${viewer.isAdmin ? '<button class="filter-choice" data-filter-view="hidden" aria-pressed="false">Hidden</button>' : ""}
    </div>
  </div>`;

  const body =
    total === 0
      ? `<div class="empty-all">
          <div class="empty-mark" aria-hidden="true">${esc(APP_BRAND)}<span>00</span></div>
          <p class="empty-kicker">The index is ready</p>
          <h2>No artifacts have been published yet.</h2>
          <p>Use <code>publish_artifact</code> for a single HTML page or <code>publish_bundle</code> for a multi-file experience. The first publication will appear here automatically.</p>
        </div>`
      : `<div class="artifact-grid" id="artifact-grid" data-layout="grid">
          ${allItems.map((artifact) => card(artifact, reactionFor(artifact.id), sentiment.get(artifact.id) || {}, viewer.isAdmin, viewCounts.get(artifact.id) || null, orgNames, categoryNames, orgColors[artifact.org])).join("")}
        </div>`;

  return `<!doctype html><html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>${esc(APP_NAME)} &middot; ${esc(SITE_HOST)}</title>
<script>(function(){try{var t=localStorage.getItem('artifact-theme');if(t)document.documentElement.dataset.theme=t;}catch(e){}})();</script>
<style>${CSS}</style></head>
<body>
<a class="skip-link" href="#stage">Skip to artifacts</a>
<div class="wrap">
  <header class="masthead" data-ui="app-frame">
    <a class="brand" href="/" aria-label="${esc(APP_NAME)} home"><span class="brand-mark">${esc(APP_BRAND)}</span><span><strong>${esc(APP_NAME)}</strong><small>${esc(SITE_HOST)}</small></span></a>
    <nav class="primary-nav" aria-label="Primary">
      <a class="nav-link active" href="/" aria-current="page" data-ui="nav-artifacts">Artifacts</a>
      ${viewer.isAdmin ? `<a class="nav-link" href="/settings" data-ui="nav-administration">Administration</a>` : ""}
    </nav>
    <nav class="header-actions" aria-label="Account">
      <div class="notif-wrap">
        <button class="header-link notif-toggle" id="notif-toggle" type="button" aria-label="Notifications" aria-expanded="false" aria-controls="notif-panel">${ICONS.bell}<b class="notif-count" ${unreadNotifications ? "" : "hidden"}>${unreadNotifications}</b></button>
        <section class="notif-panel" id="notif-panel" aria-label="Notifications" hidden>
          <div class="notif-head"><strong>Feedback</strong><button id="notif-seen" type="button">Mark all read</button></div>
          <div class="notif-list">${notificationRows}</div>
        </section>
      </div>
      <button class="header-link theme-toggle" id="theme" type="button" aria-label="Change color theme">${ICONS.theme}<span>Theme</span></button>
      <span class="identity" style="--identity-k:${orgColor(viewer.isAdmin ? "admin" : viewer.org, orgColors[viewer.isAdmin ? "admin" : viewer.org])};--identity-accent:color-mix(in oklab,var(--identity-k) 72%,var(--ink))"><span class="identity-dot"></span><span class="identity-email">${esc(viewer.email)}</span><strong>${esc(role)}</strong></span>
      <a class="header-link signout" href="/cdn-cgi/access/logout">${ICONS.signout}<span>Sign out</span></a>
    </nav>
  </header>

  <main id="stage">
    <section class="collection-head" aria-labelledby="page-title">
      <div>
        <p class="eyebrow">Private artifact collection</p>
        <h1 id="page-title">${viewer.isAdmin ? "Artifacts" : `${esc(viewer.org || "Organization")} artifacts`}</h1>
        <p class="intro-copy">${intro}</p>
      </div>
      <div class="collection-stats" aria-label="${total} artifacts, ${favoriteTotal} favorites, ${unreadNotifications} unread notifications">
        <span><strong>${total}</strong>artifacts</span>
        <span><strong>${favoriteTotal}</strong>favorites</span>
        <span><strong>${unreadNotifications}</strong>unread</span>
      </div>
    </section>

    <section class="collection-tools" aria-label="Artifact library controls">
      <label class="search">${ICONS.search}
        <input id="q" type="search" placeholder="Search artifacts, publishers, categories" aria-label="Search artifacts" autocomplete="off">
        <kbd aria-hidden="true">/</kbd>
      </label>
      <div class="toolbar-filter-groups">
        ${viewFilters}
        <button class="reset-filters" type="button" data-reset-filters>Reset</button>
        ${chips}
        ${categoryFilters}
      </div>
      <label class="sort-control"><span class="toolbar-label">Sort</span><select id="sort" aria-label="Sort artifacts"><option value="recent">Updated recently</option><option value="views">Most viewed</option><option value="title">Title</option></select></label>
      <div class="layout-toggle" aria-label="Collection layout">
        <button type="button" data-layout="grid" aria-pressed="true">Grid</button>
        <button type="button" data-layout="list" aria-pressed="false">List</button>
      </div>
    </section>

    <section class="collection-results" aria-label="Artifact results">
        <div class="result-summary">
          <span>${viewer.isAdmin ? "All organizations" : esc(viewer.org || "Organization")} · <span id="sort-label">Updated recently</span></span>
          <span class="count" id="count" aria-live="polite">Showing ${total} of ${total}</span>
        </div>
        ${body}
    </section>
    <div class="toast" id="toast" role="status" aria-live="polite"></div>
    <div class="empty" id="empty" hidden>
      <p class="empty-kicker">No matches</p><h2>Nothing in the index fits that search.</h2><p>Try a title, publisher, description, or a different organization.</p>
    </div>
    <dialog class="share-dialog" id="share-dialog" aria-labelledby="share-title">
      <form method="dialog" class="dialog-head"><div><p class="eyebrow">Public access</p><h2 id="share-title">Share artifact</h2></div><button class="dialog-close" value="cancel" aria-label="Close share dialog">×</button></form>
      <p class="share-copy" id="share-copy">Create and revoke public links. Hiding an artifact from Gallery does not revoke these links.</p>
      <form class="share-form" id="share-form"><label>Expiry<select id="share-expiry"><option value="">No expiry</option><option value="1h">1 hour</option><option value="1d">1 day</option><option value="7d">7 days</option></select></label><button class="act open" type="submit">Create link</button></form>
      <div class="share-list" id="share-list" aria-live="polite"></div>
    </dialog>
    ${hasDeleteActions ? `<dialog class="delete-dialog" id="delete-dialog" aria-labelledby="delete-title" aria-describedby="delete-copy">
      <form method="dialog" class="delete-panel">
        <div class="dialog-head"><div><p class="eyebrow">Permanent action</p><h2 id="delete-title">Delete artifact?</h2></div><button class="dialog-close" value="cancel" aria-label="Close delete dialog">×</button></div>
        <div class="delete-body"><p class="delete-context" id="delete-context"></p><p class="delete-copy" id="delete-copy">Deleting removes the artifact, revision history, feedback, reactions, audience history, and active public share links. This cannot be undone.</p><p class="delete-error" id="delete-error" role="status" aria-live="polite"></p></div>
        <div class="delete-actions"><button class="delete-cancel" value="cancel" type="submit">Cancel</button><button class="delete-confirm" id="delete-confirm" type="button">Delete artifact</button></div>
      </form>
    </dialog>` : ""}
  <footer class="footer"><span>${esc(APP_NAME)}</span><span>Private · tenant-scoped · live HTML</span></footer>
</div>
<script>${SCRIPT}</script>
</body></html>`;
}

const CSS = readFileSync(new URL("../assets/portal.css", import.meta.url), "utf8");
export const PORTAL_CSS = CSS;
export { esc as escHtml };

// --- Standalone access pages share the same tokenized, saved-theme foundation as the portal. ---
export function notFoundPage(message) {
  const msg = message || "It may have been deleted, or the link is no longer valid.";
  return `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>Not found &middot; ${esc(APP_NAME)}</title>
<script>${THEME_BOOT}</script><style>${NOT_FOUND_CSS}</style></head>
<body><main class="nf"><div class="code">404 · Missing folio</div><h1>This artifact isn’t in the index.</h1><p>${esc(msg)}</p><a class="home" href="/">← Back to ${esc(APP_NAME)}</a></main></body></html>`;
}

export function notSignedInPage() {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>Sign in &middot; ${esc(APP_NAME)}</title><script>${THEME_BOOT}</script><style>${NOT_SIGNED_IN_CSS}</style></head><body><main class="si"><div class="brand"><span class="mark" aria-hidden="true">${esc(APP_BRAND)}</span><span><strong>${esc(APP_NAME)}</strong><small>${esc(SITE_HOST)}</small></span></div><div class="code">Private · organization sign-in</div><h1>Sign in to view your organization’s artifacts.</h1><p>This index is private. Sign in with your organization account and you’ll see only the work published to your organization.</p><a class="go" href="/">Sign in &rarr;</a><p class="hint">If you just signed in and still see this, your session has not propagated yet. Reload the page. Access is granted by your email domain; ask an administrator if your organization is not set up.</p></main></body></html>`;
}

export function accessSessionRetryPage(target) {
  const retryTarget = target || "/?cf_access_retry=1";
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>Completing sign in &middot; ${esc(APP_NAME)}</title><meta http-equiv="refresh" content="0;url=${esc(retryTarget)}"><script>${THEME_BOOT}</script><style>${ACCESS_RETRY_CSS}</style></head><body><main class="retry"><div class="code">Secure session</div><h1>Completing sign in…</h1><p>Cloudflare Access is passing your verified session to ${esc(APP_NAME)}.</p><p><a href="${esc(retryTarget)}">Continue if the page does not reload.</a></p></main></body></html>`;
}

// --- Viewer shell: compact command bar around a served artifact. ---
const SHELL_CSS = readFileSync(new URL("../assets/shell.css", import.meta.url), "utf8");
const SHELL_SCRIPT = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");

function feedbackItem(f, currentRevision, { viewerEmail, isAdmin } = {}) {
  const resolved = !!f.resolved_at;
  const anchored = f.anchor_x != null && f.anchor_y != null;
  const box = f.anchor_w != null && f.anchor_h != null;
  const discordAuthor = f.author_source === "discord" || f.author?.source === "discord";
  const authorLabel = discordAuthor
    ? `${f.external_author_display || f.author?.external_author_display || "Discord user"} · Discord`
    : f.viewer_email;
  const manageable = isAdmin || (!discordAuthor && f.viewer_email === viewerEmail);
  const state = anchored
    ? `<span class="vfb-anchor-state">${f.anchor_page_stale ? "Pinned page missing · stale" : f.artifact_revision !== currentRevision ? `Placed on v${esc(f.artifact_revision)} · stale` : box ? "Pinned section" : "Pinned comment"}</span>`
    : "";
  return `<div class="vfb-item ${resolved ? "resolved" : ""}" data-id="${esc(f.id)}">
    <div class="vfb-m"><span>${esc(authorLabel)}</span><span>${esc(fmtDate(f.created_at))}${resolved ? ' &middot; <span class="vfb-res">Resolved</span>' : ""}</span></div>
    <div class="vfb-b">${esc(f.body)}</div>${state}${anchored ? `<button class="vfb-copy-prompt" type="button" data-copy-prompt="${esc(f.id)}">Copy prompt</button>` : ""}${manageable ? `<div class="vfb-manage"><button class="vfb-delete" type="button" data-feedback-action="delete">Delete</button>${resolved ? "" : '<button class="vfb-resolve" type="button" data-feedback-action="resolve">Resolve</button>'}</div>` : ""}
  </div>`;
}

function feedbackThread(parent, replies, currentRevision, viewer) {
  return `<section class="vfb-thread" data-thread-id="${esc(parent.id)}">${feedbackItem(parent, currentRevision, viewer)}<div class="vfb-replies">${replies.map((reply) => feedbackItem(reply, currentRevision, viewer)).join("")}</div><form class="vfb-reply-form" data-parent-id="${esc(parent.id)}"><textarea maxlength="4000" aria-label="Reply to feedback" placeholder="Reply to this thread…"></textarea><button type="submit">Reply</button></form></section>`;
}

// meta: the artifact row. nav: { prevId, nextId, index, total }. reaction: {favorite, vote}.
// feedback: array of feedback rows for this artifact (org-scoped, resolved shown last).
// analytics.viewers is intentionally supplied only for admins; counts are safe for same-org viewers.
// Keep the Node renderer on the checked-in Viewer assets used by the Rust renderer. The
// configuration bridge is JSON-encoded in attributes so neither renderer interpolates user data
// into executable JavaScript.
export function renderArtifactShell(meta, nav, reaction = { favorite: 0, vote: 0 }, feedback = [], analytics = {}, viewer = {}, orgAccent = null) {
  const hue = orgColor(meta.org, orgAccent), who = meta.uploader_label || meta.client_id;
  const canDelete = viewerCanManageArtifact(viewer, meta);
  const unresolved = feedback.filter((row) => !row.resolved_at).length;
  const identity = { viewerEmail: viewer.email || "", isAdmin: !!viewer.isAdmin };
  const replies = new Map();
  feedback.forEach((row) => { if (row.parent_id != null) replies.set(row.parent_id, [...(replies.get(row.parent_id) || []), row]); });
  const feedbackHtml = feedback.filter((row) => row.parent_id == null).map((row) => feedbackThread(row, replies.get(row.id) || [], meta.revision, identity)).join("") || '<div class="vfb-empty">No feedback yet. Leave the first note for the author.</div>';
  const counts = analytics.counts || {}, viewers = Array.isArray(analytics.viewers) ? analytics.viewers : null;
  const raw = meta.is_bundle ? `/raw/${esc(meta.id)}/` : `/raw/${esc(meta.id)}`;
  const versionQuery = bodyVersion(meta) ? `&v=${esc(bodyVersion(meta))}` : "";
  const audiencePane = viewers ? `<section class="inspector-pane" id="inspector-audience" role="tabpanel" aria-labelledby="tab-audience" hidden><div class="vfb-list">${viewers.length ? viewers.map((row) => `<div class="vfb-item"><div class="vfb-m"><span>${esc(row.email)}</span><span>${Number(row.count || 0)} view${Number(row.count || 0) === 1 ? "" : "s"}</span></div><div class="vfb-b">Last seen ${esc(fmtDate(row.last_viewed_at))}</div></div>`).join("") : '<div class="vfb-empty">No audience views recorded yet.</div>'}</div></section>` : "";
  // Keep the entire persisted anchor envelope available to the isolated viewer shell.
  // The shell only paints bridge-supplied positions; these values are also used to make
  // a deterministic, copy-only reviewer handoff without touching the iframe DOM.
  const feedbackData = jsLiteral(JSON.stringify(feedback.map((row) => ({
    id: row.id, parent_id: row.parent_id, viewer_email: row.viewer_email,
    author_source: row.author_source || row.author?.source || null,
    external_author_display: row.external_author_display || row.author?.external_author_display || null,
    body: row.body, created_at: row.created_at, resolved_at: row.resolved_at,
    anchor_path: row.anchor_path, anchor_x: row.anchor_x, anchor_y: row.anchor_y,
    anchor_w: row.anchor_w, anchor_h: row.anchor_h, anchor_approx: row.anchor_approx,
    anchor_page: row.anchor_page, anchor_page_stale: !!row.anchor_page_stale,
    anchor_kind: row.anchor_kind, anchor_node_id: row.anchor_node_id,
    anchor_quote: row.anchor_quote, anchor_version: row.anchor_version,
    artifact_revision: row.artifact_revision
  }))));
  const attrLiteral = (value) => esc(jsLiteral(value));
  const titlePrevious = nav.prevId ? `<a class="vnav" role="menuitem" href="/${esc(nav.prevId)}" title="Newer artifact in ${esc(meta.org)}" rel="prev">${ICONS.back}<span>Newer artifact</span><kbd>←</kbd></a>` : "";
  const titleNext = nav.nextId ? `<a class="vnav" role="menuitem" href="/${esc(nav.nextId)}" title="Older artifact in ${esc(meta.org)}" rel="next"><span>Older artifact</span>${ICONS.forward}<kbd>→</kbd></a>` : "";
  const titleAudience = viewers ? `<button type="button" role="menuitem" id="vview-toggle" data-inspector-open="audience">Audience <span>${Number(counts.views || 0)} views</span></button>` : "";
  const shellChrome = `<header class="vbar" style="--org-k:${hue};--k:color-mix(in oklab,var(--org-k) 72%,var(--txt))"><a class="vhome" href="/" aria-label="Back to artifact library" title="Back to artifact library">${ICONS.back}<span>Back</span></a><div class="vmid"><button class="vtitle-toggle" id="vtitle-toggle" type="button" aria-expanded="false" aria-controls="vtitle-menu"><span class="vtitle">${esc(meta.title)}</span><span class="vdisclosure" aria-hidden="true">⌄</span></button><span class="vmeta"><span class="vorg">${esc(meta.org)}</span><span>·</span><span><span class="publisher-label">Published by </span>${esc(who)}</span><span class="vtype">${meta.is_bundle ? "Bundle" : "HTML"}</span><span class="vrevision">v${Number(meta.revision || 1)}</span></span></div><nav class="vright" aria-label="Artifact controls"><button class="vreact fav vdesktop-action" data-act="fav" type="button" title="Save to favorites" aria-label="Save to favorites" aria-pressed="${reaction.favorite ? "true" : "false"}">${ICONS.heart}</button><button class="vcomment-toggle vdesktop-action" id="vcomment-toggle" type="button" title="Comment on a place" aria-label="Comment on a place" aria-pressed="false">▣<span>Comment</span></button><button class="vprimary" id="vshare-toggle" data-inspector-open="share" type="button" aria-controls="vinspector" aria-expanded="false">${ICONS.share}<span>Share</span></button><button class="vmore-toggle" id="vmore-toggle" type="button" aria-expanded="false" aria-controls="vmore-menu" aria-label="More artifact actions">${ICONS.more}</button></nav><div class="vtitle-menu vmenu" id="vtitle-menu" role="menu" aria-label="Artifact overview" hidden><div class="vmenu-meta"><strong>${esc(meta.org)}</strong><span>Published by ${esc(who)}</span><span>${meta.is_bundle ? "Bundle" : "HTML"} · v${Number(meta.revision || 1)}</span></div><div class="vmenu-group"><button type="button" role="menuitem" data-inspector-open="details">Details</button><button type="button" role="menuitem" data-inspector-open="history">Version history</button>${titleAudience}<button type="button" role="menuitem" id="vfb-toggle" data-inspector-open="feedback">Feedback <span class="vfb-count" ${unresolved ? "" : "hidden"}>${unresolved}</span></button></div><div class="vmenu-group vmenu-nav" aria-label="Browse ${esc(meta.org)} artifacts">${titlePrevious}<span class="vpos"><strong>${Number(nav.index || 1)}</strong> / ${Number(nav.total || 0)}</span>${titleNext}</div><div class="vmenu-group vmenu-reactions" aria-label="Your reaction"><button class="vreact up" data-act="up" type="button" role="menuitemcheckbox" aria-label="Mark as useful" aria-checked="${reaction.vote > 0 ? "true" : "false"}">${ICONS.up}<span>Useful</span></button><button class="vreact down" data-act="down" type="button" role="menuitemcheckbox" aria-label="Mark as needing work" aria-checked="${reaction.vote < 0 ? "true" : "false"}">${ICONS.down}<span>Needs work</span></button></div></div><div class="vmore-menu vmenu" id="vmore-menu" role="menu" aria-label="More artifact actions" hidden><div class="vmenu-group"><a role="menuitem" href="${raw}" target="_blank" rel="noopener">Open raw artifact</a>${meta.is_bundle ? '<span class="sr-only">Bundles open in the viewer and cannot download as one HTML file.</span>' : `<a role="menuitem" href="${raw}?download" download>Download HTML</a>`}<button id="vtheme" type="button" role="menuitem">Change theme</button><a class="danger-link" role="menuitem" href="/cdn-cgi/access/logout">Sign out</a>${canDelete ? '<button class="vdelete-trigger" id="vdelete-trigger" type="button" role="menuitem">Delete artifact</button>' : ""}</div></div></header>`;
  const page = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>${esc(meta.title)} &middot; ${esc(APP_NAME)}</title><script>(function(){try{var t=localStorage.getItem('artifact-theme');if(t)document.documentElement.dataset.theme=t;}catch(e){}})();</script><style>${SHELL_CSS}</style></head><body>
${shellChrome}
<div class="reaction-status" id="reaction-status" role="status" aria-live="polite"></div><div class="vstage" id="vstage"><iframe class="vframe" id="vframe" src="${raw}?anchor=1${versionQuery}" title="${esc(meta.title)}" sandbox="allow-scripts allow-popups allow-forms allow-modals"></iframe><div class="vanchor-overlay" id="vanchor-overlay"></div><section class="vanchor-composer" id="vanchor-composer" role="dialog" aria-modal="false" aria-labelledby="vanchor-composer-title" hidden><div class="vanchor-composer-head"><div><p class="vanchor-kicker">Pinned feedback</p><h2 id="vanchor-composer-title">Add a comment</h2></div><button class="vanchor-dismiss" id="vanchor-dismiss" type="button" aria-label="Cancel anchored comment">×</button></div><p class="vanchor-summary" id="vanchor-summary" role="status"></p><p class="vanchor-identity">Commenting as <strong>${esc(identity.viewerEmail || "verified reviewer")}</strong> · verified reviewer</p><textarea id="vanchor-body" maxlength="4000" aria-label="Anchored feedback" placeholder="Describe what should change…"></textarea><div class="vanchor-actions"><button class="vanchor-copy" id="vanchor-copy" type="button">Copy prompt</button><button class="vanchor-save" id="vanchor-save" type="button">Add comment</button></div><p class="vanchor-status" id="vanchor-status" role="status" aria-live="polite"></p></section></div>
<aside class="vinspector" id="vinspector" aria-label="Artifact inspector" aria-hidden="true" tabindex="-1" inert><div class="inspector-head"><h2 id="inspector-title">Artifact inspector</h2><button class="inspector-close" id="vinspector-close" type="button" aria-label="Close inspector">×</button></div><div class="inspector-tabs" role="tablist" aria-label="Artifact inspector"><button class="inspector-tab" id="tab-feedback" data-inspector-tab="feedback" type="button" role="tab" aria-selected="false" aria-controls="inspector-feedback" tabindex="-1">Feedback <span class="vfb-count" ${unresolved ? "" : "hidden"}>${unresolved}</span></button><button class="inspector-tab" id="tab-details" data-inspector-tab="details" type="button" role="tab" aria-selected="false" aria-controls="inspector-details" tabindex="-1">Details</button><button class="inspector-tab" id="tab-share" data-inspector-tab="share" type="button" role="tab" aria-selected="false" aria-controls="inspector-share" tabindex="-1">Share</button><button class="inspector-tab" id="tab-history" data-inspector-tab="history" type="button" role="tab" aria-selected="false" aria-controls="inspector-history" tabindex="-1">History</button>${viewers ? '<button class="inspector-tab" id="tab-audience" data-inspector-tab="audience" type="button" role="tab" aria-selected="false" aria-controls="inspector-audience" tabindex="-1">Audience</button>' : ""}</div><div class="inspector-body"><section class="inspector-pane" id="inspector-feedback" role="tabpanel" aria-labelledby="tab-feedback" hidden><div class="vfb-list" id="vfb-list">${feedbackHtml}</div><form class="vfb-form" id="vfb-form"><textarea id="vfb-body" placeholder="Leave feedback for the author…" maxlength="4000" aria-label="Your feedback"></textarea><div class="vfb-actions"><span class="vfb-hint" id="vfb-hint" role="status"></span><button class="vfb-send" type="submit">Send feedback</button></div></form></section><section class="inspector-pane" id="inspector-details" role="tabpanel" aria-labelledby="tab-details" hidden><div class="vdetails"><dl class="vdetails-grid"><dt>Organization</dt><dd>${esc(meta.org)}</dd><dt>Publisher</dt><dd>${esc(who)}</dd><dt>Format</dt><dd>${meta.is_bundle ? "Bundle" : "Single-page HTML"}</dd><dt>Version</dt><dd>v${Number(meta.revision || 1)}</dd><dt>Bytes</dt><dd>${Number(meta.bytes || 0)}</dd><dt>Category</dt><dd><span class="vcat-wrap"><button class="vcat" id="vcat" type="button" data-set="${meta.category ? "1" : "0"}">${meta.category ? esc(meta.category) : "Add category"}</button><form class="vcat-edit" id="vcat-edit" hidden><input id="vcat-input" type="text" maxlength="60" placeholder="Category" value="${esc(meta.category || "")}" aria-label="Artifact category"><button type="submit" class="vcat-save" aria-label="Save category">✓</button></form></span></dd></dl></div></section><section class="inspector-pane" id="inspector-share" role="tabpanel" aria-labelledby="tab-share" hidden><div class="vfb-list" id="vshare-list"><div class="vfb-empty">Loading active links…</div></div><form class="vshare-form" id="vshare-form"><label for="vshare-expiry">Link expiry</label><select id="vshare-expiry" aria-label="Link expiry"><option value="24h">24 hours</option><option value="date">Until a date</option><option value="never">No expiration</option></select><input id="vshare-date" type="date" aria-label="Share expiration date" hidden><button type="submit">Create link</button><div class="vshare-result" id="vshare-result" aria-live="polite"></div></form></section><section class="inspector-pane" id="inspector-history" role="tabpanel" aria-labelledby="tab-history" hidden><div class="vfb-list" id="vhist-list"><div class="vfb-empty">Open History to load versions.</div></div></section>${audiencePane}</div></aside>
${canDelete ? `<dialog class="delete-dialog" id="delete-dialog" aria-labelledby="delete-title" aria-describedby="delete-copy"><form method="dialog" class="delete-panel"><div class="delete-head"><div><p>Permanent action</p><h2 id="delete-title">Delete ${esc(meta.title)}?</h2></div><button class="delete-close" value="cancel" aria-label="Close delete dialog">×</button></div><div class="delete-body"><p class="delete-context">${esc(meta.org)} · /${esc(meta.id)}</p><p class="delete-copy" id="delete-copy">Deleting removes the artifact, revision history, feedback, reactions, audience history, and active public share links. This cannot be undone.</p><p class="delete-error" id="delete-error" role="status" aria-live="polite"></p></div><div class="delete-actions"><button class="delete-cancel" value="cancel" type="submit">Cancel</button><button class="delete-confirm" id="delete-confirm" type="button">Delete artifact</button></div></form></dialog>` : ""}
<div id="shell-config" hidden data-artifact-id="${attrLiteral(meta.id)}" data-prev-id="${attrLiteral(nav.prevId || "")}" data-next-id="${attrLiteral(nav.nextId || "")}" data-bundle-raw-prefix="${attrLiteral(`/raw/${meta.id}/`)}" data-version-query="${attrLiteral(versionQuery)}" data-viewer-email="${attrLiteral(identity.viewerEmail)}" data-viewer-is-admin="${identity.isAdmin ? "1" : "0"}" data-feedback="${esc(feedbackData)}" data-title="${attrLiteral(meta.title)}" data-favorite="${reaction.favorite ? "1" : "0"}" data-vote="${Number(reaction.vote || 0)}" data-is-bundle="${meta.is_bundle ? "1" : "0"}" data-revision="${Number(meta.revision || 1)}" data-bytes="${Number(meta.bytes || 0)}"></div><script>${SHELL_SCRIPT}</script></body></html>`;
  const discussionDetail = `<dt>Discussion</dt><dd><section class="vdiscussion" id="vdiscussion"><strong class="vdiscussion-state" id="vdiscussion-state" tabindex="-1">Checking status…</strong><p class="vdiscussion-copy" id="vdiscussion-copy">Discussion uses the organization default unless this artifact is explicitly kept in Artifact MCP.</p>${canDelete ? '<div class="vdiscussion-actions" id="vdiscussion-actions" hidden></div>' : ""}<p class="vdiscussion-boundary">Artifact MCP remains canonical. Discord replies do not grant Artifact MCP access, and local feedback remains available during recovery or Discord failures.</p><p class="vdiscussion-status" id="vdiscussion-status" role="status" aria-live="polite"></p></section></dd>`;
  return page
    .replace(
      "</dd></dl></div></section><section class=\"inspector-pane\" id=\"inspector-share\"",
      `</dd>${discussionDetail}</dl></div></section><section class="inspector-pane" id="inspector-share"`,
    )
    .replace(
      '<div id="shell-config" hidden ',
      `<div id="shell-config" hidden data-can-manage-discussion="${canDelete ? "1" : "0"}" `,
    );
}

const SCRIPT = readFileSync(new URL("../assets/portal.js", import.meta.url), "utf8");
