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
  home: `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m4 11 8-7 8 7"></path><path d="M6.5 9.5V20h11V9.5M10 20v-6h4v6"></path></svg>`,
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
          <span class="notif-meta"><strong>${esc(row.viewer_email)}</strong><time>${esc(relativeTime(row.created_at))}</time></span>
          <span class="notif-artifact">${esc(row.artifact_title)}</span>
          <span class="notif-snippet">${esc(snippet)}</span>
        </a>`;
      }).join("")
    : '<div class="notif-empty">No feedback yet.</div>';

  const chips = showChips
    ? `<div class="filter-group" id="org-filters" aria-label="Filter by organization">
        <p class="filter-title">Organization</p>
        <button class="filter-choice" data-filter-org="all" aria-pressed="true">All organizations <span>${total}</span></button>
        ${sections
          .map(
            (s) =>
              `<button class="filter-choice" data-filter-org="${esc(s.org)}" aria-pressed="false"><span><span class="dot" style="background:${orgColor(s.org, orgColors[s.org])}"></span>${esc(s.org)}</span><span>${s.items.length}</span></button>`
          )
          .join("")}
      </div>`
    : "";
  const categoryFilters = categoryNames.length
    ? `<div class="filter-group" id="category-filters" aria-label="Filter by category">
        <p class="filter-title">Category</p>
        <button class="filter-choice" data-filter-category="all" aria-pressed="true">All categories <span>${total}</span></button>
        ${categoryNames.map((category) => `<button class="filter-choice" data-filter-category="${esc(category)}" aria-pressed="false"><span>${esc(category || "Uncategorized")}</span><span>${allItems.filter((artifact) => (artifact.category && artifact.category.trim() ? artifact.category.trim() : "") === category).length}</span></button>`).join("")}
      </div>`
    : "";

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

    <div class="collection-tools">
      <label class="search">${ICONS.search}
        <input id="q" type="search" placeholder="Search artifacts, publishers, categories" aria-label="Search artifacts" autocomplete="off">
        <kbd aria-hidden="true">/</kbd>
      </label>
      <button class="filter-toggle" id="filter-toggle" type="button" aria-expanded="false" aria-controls="filter-rail">Filters</button>
      <div class="layout-toggle" aria-label="Collection layout">
        <button type="button" data-layout="grid" aria-pressed="true">Grid</button>
        <button type="button" data-layout="list" aria-pressed="false">List</button>
      </div>
    </div>

    <div class="collection-layout">
      <aside class="filter-rail" id="filter-rail" aria-label="Artifact filters">
        <div class="filter-group" id="view-filters">
          <p class="filter-title">View</p>
          <button class="filter-choice" data-filter-view="all" aria-pressed="true">All artifacts <span>${total}</span></button>
          <button class="filter-choice" data-filter-view="favorites" aria-pressed="false">Favorites <span>${favoriteTotal}</span></button>
          <button class="filter-choice" data-filter-view="review" aria-pressed="false">${viewer.isAdmin ? "Needs review" : "My needs-work votes"} <span>${needsReviewTotal}</span></button>
        </div>
        ${chips}
        ${categoryFilters}
      </aside>
      <section class="collection-results" aria-label="Artifact results">
        <div class="result-summary">
          <span>${viewer.isAdmin ? "All organizations" : esc(viewer.org || "Organization")} · Updated recently</span>
          <span class="count" id="count" aria-live="polite">Showing ${total} of ${total}</span>
        </div>
        ${body}
      </section>
    </div>
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

// --- Styled 404 for missing/deleted artifacts ---
export function notFoundPage(message) {
  const msg = message || "It may have been deleted, or the link is no longer valid.";
  return `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>Not found &middot; ${esc(APP_NAME)}</title>
<style>
:root{color-scheme:light dark;--bg:#E8EAED;--card:#fff;--ink:#142235;--dim:#506071;--line:#C8D0D8;--brass:#9B681F}
@media(prefers-color-scheme:dark){:root{--bg:#11161D;--card:#19212A;--ink:#EBEEF1;--dim:#A3ADB7;--line:#303A45;--brass:#D5A252}}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--ink);font-family:system-ui,-apple-system,"Segoe UI",sans-serif;padding:1.5rem}.nf{max-width:32rem;border-top:1px solid var(--ink);padding-top:1.25rem;position:relative}.nf::before{content:"";position:absolute;left:0;top:-3px;width:5rem;border-top:5px solid var(--brass)}.code{font:700 .68rem ui-monospace,Menlo,monospace;letter-spacing:.16em;color:var(--brass);text-transform:uppercase}h1{font:500 3rem/.95 ui-serif,"Iowan Old Style",Georgia,serif;letter-spacing:-.035em;margin:.75rem 0 .8rem}p{color:var(--dim);margin:0 0 1.6rem;line-height:1.6}.home{display:inline-flex;align-items:center;gap:.45rem;font:700 .75rem ui-monospace,Menlo,monospace;color:var(--card);background:var(--ink);text-decoration:none;border:1px solid var(--ink);border-radius:5px;padding:.65rem .9rem}.home:hover{background:var(--brass);border-color:var(--brass)}.home:focus-visible{outline:2px solid var(--brass);outline-offset:3px}
</style></head>
<body><main class="nf"><div class="code">404 · Missing folio</div><h1>This artifact isn’t in the index.</h1><p>${esc(msg)}</p><a class="home" href="/">← Back to ${esc(APP_NAME)}</a></main></body></html>`;
}

export function notSignedInPage() {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>Sign in &middot; ${esc(APP_NAME)}</title>
<style>
:root{color-scheme:light dark;--bg:#E8EAED;--card:#fff;--ink:#142235;--dim:#506071;--line:#C8D0D8;--brass:#9B681F}
@media(prefers-color-scheme:dark){:root{--bg:#11161D;--card:#19212A;--ink:#EBEEF1;--dim:#A3ADB7;--line:#303A45;--brass:#D5A252}}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--ink);font-family:system-ui,-apple-system,"Segoe UI",sans-serif;padding:1.5rem}
.si{max-width:33rem;width:100%;background:var(--card);border:1px solid var(--line);border-radius:14px;box-shadow:0 24px 60px -40px rgba(20,34,53,.6);padding:2.4rem 2.2rem;position:relative;overflow:hidden}
.si::before{content:"";position:absolute;inset:0 auto auto 0;height:4px;width:100%;background:linear-gradient(90deg,var(--brass),transparent 70%)}
.brand{display:flex;align-items:center;gap:.7rem;margin-bottom:1.6rem}
.mark{width:2.6rem;height:2.6rem;border-radius:8px;background:#142235;color:#D5A252;display:grid;place-items:center;flex:none;font:700 1rem ui-serif,"Iowan Old Style",Georgia,serif}
.brand strong{display:block;font:600 1.05rem/1 ui-serif,"Iowan Old Style",Georgia,serif;letter-spacing:-.01em}.brand small{color:var(--dim);font:.72rem ui-monospace,Menlo,monospace}
.code{font:700 .66rem ui-monospace,Menlo,monospace;letter-spacing:.16em;color:var(--brass);text-transform:uppercase}
h1{font:500 2.1rem/1.05 ui-serif,"Iowan Old Style",Georgia,serif;letter-spacing:-.03em;margin:.6rem 0 .7rem}
p{color:var(--dim);margin:0 0 1.7rem;line-height:1.65;font-size:.95rem}
.go{display:inline-flex;align-items:center;gap:.5rem;font:700 .78rem ui-monospace,Menlo,monospace;color:var(--card);background:var(--ink);text-decoration:none;border:1px solid var(--ink);border-radius:6px;padding:.72rem 1rem}
.go:hover{background:var(--brass);border-color:var(--brass);color:#fff}.go:focus-visible{outline:2px solid var(--brass);outline-offset:3px}
.hint{margin-top:1.3rem;font-size:.76rem;color:var(--dim);line-height:1.55}
</style></head>
<body><main class="si">
  <div class="brand"><span class="mark" aria-hidden="true">${esc(APP_BRAND)}</span><span><strong>${esc(APP_NAME)}</strong><small>${esc(SITE_HOST)}</small></span></div>
  <div class="code">Private · organization sign-in</div>
  <h1>Sign in to view your organization’s artifacts.</h1>
  <p>This index is private. Sign in with your organization account and you’ll see only the work published to your organization.</p>
  <a class="go" href="/">Sign in &rarr;</a>
  <p class="hint">If you just signed in and still see this, your session hasn’t propagated yet — reload the page. Access is granted by your email domain; ask an administrator if your organization isn’t set up.</p>
</main></body></html>`;
}

export function accessSessionRetryPage(target) {
  const retryTarget = target || "/?cf_access_retry=1";
  return `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>Completing sign in &middot; ${esc(APP_NAME)}</title>
<meta http-equiv="refresh" content="0;url=${esc(retryTarget)}">
<style>
:root{color-scheme:light dark;--bg:#E8EAED;--card:#fff;--ink:#142235;--dim:#506071;--line:#C8D0D8;--brass:#9B681F}
@media(prefers-color-scheme:dark){:root{--bg:#11161D;--card:#19212A;--ink:#EBEEF1;--dim:#A3ADB7;--line:#303A45;--brass:#D5A252}}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--ink);font-family:system-ui,-apple-system,"Segoe UI",sans-serif;padding:1.5rem}.retry{max-width:30rem;width:100%;background:var(--card);border:1px solid var(--line);border-radius:14px;padding:2rem}.code{font:700 .66rem ui-monospace,Menlo,monospace;letter-spacing:.14em;color:var(--brass);text-transform:uppercase}h1{font:500 2rem/1.08 ui-serif,"Iowan Old Style",Georgia,serif;margin:.6rem 0}.retry p{color:var(--dim);line-height:1.6}.retry a{color:var(--brass);font-weight:700}
</style></head><body><main class="retry"><div class="code">Secure session</div><h1>Completing sign in…</h1><p>Cloudflare Access is passing your verified session to ${esc(APP_NAME)}.</p><p><a href="${esc(retryTarget)}">Continue if the page does not reload.</a></p></main></body></html>`;
}

// --- Viewer shell: compact command bar around a served artifact. ---
const LEGACY_SHELL_CSS = `
:root{color-scheme:light dark;--bg:#fff;--bar:#F3F5F6;--panel:#fff;--txt:#142235;--line:#CDD4DB;--dim:#607080;--brass:#9B681F;--danger:#9E3441;--positive:#2F7450;--negative:#A14C38}
@media(prefers-color-scheme:dark){:root{--bg:#0C1117;--bar:#151C24;--panel:#1D2630;--txt:#EBEEF1;--line:#303A45;--dim:#98A4AF;--brass:#D5A252;--danger:#DF7882;--positive:#72BB91;--negative:#D78670}}
:root[data-theme="light"]{color-scheme:light;--bg:#fff;--bar:#F3F5F6;--panel:#fff;--txt:#142235;--line:#CDD4DB;--dim:#607080;--brass:#9B681F;--danger:#9E3441;--positive:#2F7450;--negative:#A14C38}
:root[data-theme="dark"]{color-scheme:dark;--bg:#0C1117;--bar:#151C24;--panel:#1D2630;--txt:#EBEEF1;--line:#303A45;--dim:#98A4AF;--brass:#D5A252;--danger:#DF7882;--positive:#72BB91;--negative:#D78670}
*{box-sizing:border-box}html,body{margin:0;height:100%;overflow:hidden}body{display:flex;flex-direction:column;font-family:system-ui,-apple-system,"Segoe UI",sans-serif;background:var(--bg);color:var(--txt)}button{font:inherit;color:inherit}svg{width:1rem;height:1rem;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}.sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0}
.vbar{flex:none;min-height:58px;display:flex;align-items:center;gap:.65rem;padding:.5rem .65rem;background:var(--bar);border-bottom:1px solid var(--line);box-shadow:inset 0 -2px 0 var(--k);position:relative;z-index:2}
.vhome{flex:none;width:2.35rem;height:2.35rem;display:grid;place-items:center;text-decoration:none;color:inherit;border:1px solid var(--txt);border-radius:5px;background:var(--panel);position:relative}.vhome::after{content:"";position:absolute;right:-3px;bottom:-3px;width:100%;height:100%;border-right:1px solid var(--k);border-bottom:1px solid var(--k);pointer-events:none}.vhome svg{width:1.05rem;height:1.05rem}
.vmid{flex:1;min-width:0;display:flex;flex-direction:column;line-height:1.15;padding-left:.2rem}.vtitle{font:600 .9rem/1.2 ui-serif,"Iowan Old Style",Georgia,serif;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.vmeta{display:flex;align-items:center;gap:.4rem;font:.62rem/1.3 ui-monospace,monospace;color:var(--dim);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;margin-top:.2rem}.vorg{color:var(--k);font-weight:700;text-transform:uppercase;letter-spacing:.04em}.vtype{border:1px solid var(--line);border-radius:3px;padding:.05rem .28rem;text-transform:uppercase;font-size:.55rem}
.vright{display:flex;align-items:center;justify-content:flex-end;gap:.45rem;min-width:0}.vgroup{display:flex;align-items:center;gap:.2rem}.vgroup+.vgroup{border-left:1px solid var(--line);padding-left:.45rem}.vreact,.vnav,.vtool{width:2.1rem;height:2.1rem;display:grid;place-items:center;border:1px solid transparent;border-radius:5px;background:transparent;color:var(--dim);cursor:pointer;text-decoration:none;position:relative}.vreact:hover,.vnav:hover,.vtool:hover{border-color:var(--line);background:var(--panel);color:var(--txt)}.vreact[aria-pressed="true"]{background:var(--panel)}.vreact.fav[aria-pressed="true"]{color:#D55373;border-color:#D55373}.vreact.fav[aria-pressed="true"] svg{fill:currentColor}.vreact.up[aria-pressed="true"]{color:var(--positive);border-color:var(--positive)}.vreact.down[aria-pressed="true"]{color:var(--negative);border-color:var(--negative)}.vreact:disabled{opacity:.45;cursor:wait}
.vpos{min-width:3rem;text-align:center;font:.65rem ui-monospace,monospace;color:var(--dim)}.vpos strong{color:var(--txt)}.vnav.off{opacity:.24;pointer-events:none}.vkey{position:absolute;right:3px;bottom:1px;font:500 .47rem ui-monospace,monospace;color:var(--dim)}.vtool.download{width:auto;padding:0 .55rem;grid-auto-flow:column;gap:.38rem;font:700 .62rem ui-monospace,monospace}.vlabel{white-space:nowrap}.vtheme{border:1px solid transparent}.vsignout{color:var(--brass)}
.vviewstat{border:1px solid var(--line);border-radius:4px;background:var(--panel);color:var(--dim);padding:.36rem .48rem;font:700 .61rem ui-monospace,monospace;white-space:nowrap}.vviewstat[type="button"]{cursor:pointer}.vviewstat[type="button"]:hover{border-color:var(--brass);color:var(--brass)}
.reaction-status{position:fixed;top:68px;right:.75rem;z-index:5;background:var(--txt);color:var(--panel);border-radius:5px;padding:.45rem .65rem;font:600 .65rem ui-monospace,monospace;box-shadow:0 8px 24px rgba(0,0,0,.2);opacity:0;transform:translateY(-5px);pointer-events:none;transition:opacity .15s,transform .15s}.reaction-status.show{opacity:1;transform:none}.reaction-status.error{background:var(--danger);color:#fff}
.vcat-wrap{display:inline-flex;align-items:center}
.vcat{font:600 .56rem ui-monospace,monospace;text-transform:uppercase;letter-spacing:.04em;border:1px dashed var(--line);border-radius:3px;padding:.1rem .35rem;background:transparent;color:var(--dim);cursor:pointer;white-space:nowrap}.vcat:hover{border-color:var(--brass);color:var(--brass)}.vcat[data-set="1"]{border-style:solid;color:var(--k);border-color:var(--k)}
.vcat-edit{display:inline-flex;align-items:center;gap:.25rem}.vcat-edit input{width:8rem;font:.62rem ui-monospace,monospace;background:var(--bg);border:1px solid var(--brass);border-radius:3px;padding:.14rem .3rem;color:var(--txt)}.vcat-edit input:focus{outline:0}.vcat-save{width:1.5rem;height:1.5rem;display:grid;place-items:center;border:1px solid var(--line);border-radius:3px;background:var(--panel);cursor:pointer;color:var(--positive)}.vcat-save:disabled{opacity:.5}
.vstage{flex:1;min-height:0;position:relative}.vframe{display:block;width:100%;height:100%;border:0;background:#fff}.vanchor-overlay{position:absolute;inset:0;z-index:1;pointer-events:none}.vanchor-overlay.fallback{pointer-events:auto;cursor:crosshair}.vanchor-marker{position:absolute;transform:translate(-50%,-50%);width:1.55rem;height:1.55rem;border-radius:50%;border:2px solid #fff;background:var(--brass);color:#fff;box-shadow:0 1px 6px rgba(0,0,0,.4);font:700 .62rem/1 ui-monospace,monospace;cursor:pointer;pointer-events:auto}.vanchor-marker.vanchor-box{transform:none;width:auto;height:auto;min-width:4px;min-height:4px;box-sizing:border-box;border-color:var(--brass);border-radius:2px;background:color-mix(in srgb,var(--brass) 12%,transparent)}.vanchor-marker.vanchor-box::after{content:attr(data-pin);position:absolute;top:-.72rem;left:-.72rem;width:1.3rem;height:1.3rem;display:grid;place-items:center;border:2px solid #fff;border-radius:50%;background:var(--brass);color:#fff;box-shadow:0 1px 6px rgba(0,0,0,.4);font:700 .58rem/1 ui-monospace,monospace}.vanchor-marker.stale{background:var(--dim);outline:1px dashed var(--brass);outline-offset:2px}.vanchor-marker.vanchor-box.stale{background:color-mix(in srgb,var(--dim) 18%,transparent)}.vanchor-marker.vanchor-box.stale::after{background:var(--dim)}.vanchor-marker[hidden]{display:none}.vanchor-selection{position:absolute;z-index:2;pointer-events:none;box-sizing:border-box;border:2px solid var(--brass);background:color-mix(in srgb,var(--brass) 14%,transparent)}.vcomment-toggle[aria-pressed="true"]{color:var(--brass);border-color:var(--brass);background:var(--panel)}.vbar :focus-visible{outline:2px solid var(--brass);outline-offset:2px}
@media(max-width:980px){.vlabel{display:none}.vtool.download{width:2.1rem;padding:0}.vmeta .publisher-label{display:none}}
@media(max-width:690px){
  .vbar{min-height:102px;flex-wrap:wrap;align-content:center;padding:.45rem}.vhome{width:2.15rem;height:2.15rem}.vmid{max-width:calc(100% - 3rem)}.vright{order:3;width:100%;justify-content:space-between;gap:.25rem}.vgroup{gap:.1rem}.vgroup+.vgroup{padding-left:.25rem}.vreact,.vnav,.vtool{width:2rem;height:2rem}.vpos{min-width:2.5rem}.vkey{display:none}.reaction-status{top:109px}.vmeta{font-size:.58rem}
}
@media(max-width:385px){.vright{overflow-x:auto;justify-content:flex-start}.vgroup{flex:none}.vtype{display:none}}
@media(prefers-reduced-motion:reduce){.reaction-status,.vfb-panel{transition:none}}
.vfb-toggle{position:relative}.vfb-count{position:absolute;top:-3px;right:-3px;min-width:1rem;height:1rem;padding:0 .18rem;border-radius:.5rem;background:var(--brass);color:#fff;font:700 .54rem/1rem ui-monospace,monospace;text-align:center}.vfb-count[hidden]{display:none}
.vfb-panel{position:fixed;top:0;right:0;height:100%;width:min(390px,94vw);background:var(--panel);border-left:1px solid var(--line);box-shadow:-14px 0 34px rgba(0,0,0,.2);z-index:6;display:flex;flex-direction:column;transform:translateX(100%);transition:transform .2s ease}.vfb-panel.open{transform:none}
.vfb-head{flex:none;display:flex;align-items:center;justify-content:space-between;padding:.85rem 1rem;border-bottom:1px solid var(--line)}.vfb-head h2{font:600 .98rem ui-serif,"Iowan Old Style",Georgia,serif;margin:0}
.vfb-close{width:2rem;height:2rem;display:grid;place-items:center;border:1px solid transparent;border-radius:5px;background:transparent;cursor:pointer;color:var(--dim)}.vfb-close:hover{border-color:var(--line);color:var(--txt)}
.vfb-list{flex:1;min-height:0;overflow-y:auto;padding:.8rem 1rem;display:flex;flex-direction:column;gap:.7rem}
.vfb-thread{display:flex;flex-direction:column;gap:.42rem}.vfb-item{border:1px solid var(--line);border-radius:6px;padding:.6rem .7rem;background:var(--bg)}.vfb-item.resolved{opacity:.55}.vfb-item .vfb-m{display:flex;justify-content:space-between;gap:.5rem;font:.59rem ui-monospace,monospace;color:var(--dim);margin-bottom:.32rem}.vfb-item .vfb-b{font-size:.82rem;line-height:1.45;white-space:pre-wrap;word-break:break-word}.vfb-res{font:700 .56rem ui-monospace,monospace;color:var(--positive);text-transform:uppercase;letter-spacing:.04em}.vfb-manage{display:flex;gap:.35rem;margin-top:.48rem}.vfb-manage button,.vfb-reply-form button{font:700 .56rem ui-monospace,monospace;border:1px solid var(--line);border-radius:4px;background:var(--panel);color:var(--dim);padding:.28rem .45rem;cursor:pointer}.vfb-manage button:hover,.vfb-reply-form button:hover{border-color:var(--brass);color:var(--brass)}.vfb-manage .vfb-delete:hover{border-color:var(--danger);color:var(--danger)}.vfb-replies{display:flex;flex-direction:column;gap:.4rem;margin-left:1rem;padding-left:.65rem;border-left:2px solid var(--line)}.vfb-reply-form{display:flex;gap:.35rem;margin-left:1rem;padding-left:.65rem}.vfb-reply-form textarea{flex:1;min-height:2.1rem;resize:vertical;font:inherit;font-size:.74rem;background:var(--bg);border:1px solid var(--line);border-radius:4px;padding:.35rem .45rem;color:var(--txt)}.vfb-reply-form textarea:focus{outline:0;border-color:var(--brass)}.vfb-reply-form button{align-self:flex-end;color:var(--brass);border-color:color-mix(in srgb,var(--brass) 40%,var(--line))}
.vfb-anchor-state{display:block;margin-top:.38rem;font:700 .55rem ui-monospace,monospace;text-transform:uppercase;letter-spacing:.04em;color:var(--brass)}.vfb-item.pin-focus{border-color:var(--brass);box-shadow:0 0 0 2px color-mix(in srgb,var(--brass) 22%,transparent)}.vanchor-marker.pin-focus{outline:3px solid color-mix(in srgb,var(--brass) 45%,transparent);outline-offset:3px}
.vfb-empty{color:var(--dim);font-size:.82rem;text-align:center;padding:2.2rem .5rem}
.vshare-form{padding:.8rem 1rem;border-top:1px solid var(--line);display:flex;flex-direction:column;gap:.55rem}.vshare-form label{font:700 .58rem ui-monospace,monospace;color:var(--dim);text-transform:uppercase;letter-spacing:.05em}.vshare-form select,.vshare-form input{width:100%;font:inherit;font-size:.78rem;background:var(--bg);border:1px solid var(--line);border-radius:4px;padding:.42rem .5rem;color:var(--txt)}.vshare-form button,.vshare-copy,.vshare-revoke{font:700 .62rem ui-monospace,monospace;border:1px solid var(--line);border-radius:4px;background:var(--panel);color:var(--brass);padding:.42rem .55rem;cursor:pointer}.vshare-form button:hover,.vshare-copy:hover{border-color:var(--brass)}.vshare-result{font:.7rem/1.4 ui-monospace,monospace;word-break:break-all;color:var(--dim);display:flex;align-items:flex-start;gap:.4rem}.vshare-result a{color:var(--brass)}.vshare-copy{flex:none;padding:.25rem .4rem}.vshare-row{display:flex;flex-direction:column;gap:.42rem}.vshare-row .vfb-m{margin:0}.vshare-revoke{align-self:flex-start;color:var(--danger)}.vshare-revoke:hover{border-color:var(--danger)}
.vhist-toggle{position:relative}.vhist-badge{position:absolute;top:-3px;right:-6px;padding:0 .22rem;height:1rem;border-radius:.5rem;background:var(--line);color:var(--dim);font:700 .5rem/1rem ui-monospace,monospace}
.vhist-item{border:1px solid var(--line);border-radius:8px;padding:.6rem .7rem;display:flex;flex-direction:column;gap:.35rem}
.vhist-item.current{border-color:var(--brass);background:color-mix(in srgb,var(--brass) 7%,transparent)}
.vh-m{display:flex;align-items:center;gap:.5rem;font:700 .8rem/1 ui-sans-serif,system-ui}.vh-m strong{font-size:.9rem}
.vh-cur{font:700 .52rem/1 ui-monospace,monospace;text-transform:uppercase;letter-spacing:.06em;color:var(--brass);border:1px solid var(--brass);border-radius:3px;padding:.12rem .3rem}
.vh-when{margin-left:auto;color:var(--dim);font:.62rem/1 ui-monospace,monospace}
.vh-t{font-size:.78rem;display:flex;gap:.5rem;align-items:baseline}.vh-size{margin-left:auto;color:var(--dim);font:.62rem ui-monospace,monospace;white-space:nowrap}
.vh-actions{display:flex;gap:.5rem;margin-top:.1rem}.vh-actions:empty{display:none}
.vh-view,.vh-restore{font:700 .64rem ui-monospace,monospace;border-radius:5px;padding:.32rem .6rem;cursor:pointer;border:1px solid var(--line);background:var(--panel);color:inherit;text-decoration:none;display:inline-flex;align-items:center}
.vh-view:hover{border-color:var(--brass);color:var(--brass)}
.vh-restore{color:var(--brass);border-color:color-mix(in srgb,var(--brass) 40%,var(--line))}.vh-restore:hover{background:var(--brass);color:#fff;border-color:var(--brass)}.vh-restore:disabled{opacity:.6;cursor:wait}
.vfb-form{flex:none;border-top:1px solid var(--line);padding:.75rem 1rem;display:flex;flex-direction:column;gap:.5rem}.vfb-form textarea{width:100%;min-height:4rem;resize:vertical;font:inherit;font-size:.82rem;background:var(--bg);border:1px solid var(--line);border-radius:5px;padding:.5rem .6rem;color:var(--txt)}.vfb-form textarea:focus{outline:0;border-color:var(--brass)}
.vfb-actions{display:flex;justify-content:space-between;align-items:center;gap:.5rem}.vfb-hint{font:.58rem ui-monospace,monospace;color:var(--dim)}.vfb-hint.error{color:var(--danger)}
.vfb-send{font:700 .67rem ui-monospace,monospace;border:1px solid var(--brass);color:#fff;background:var(--brass);border-radius:5px;padding:.5rem .9rem;cursor:pointer}.vfb-send:disabled{opacity:.55;cursor:wait}
`;
const SHELL_CSS = readFileSync(new URL("../assets/shell.css", import.meta.url), "utf8");
const SHELL_SCRIPT = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");

function feedbackItem(f, currentRevision, { viewerEmail, isAdmin } = {}) {
  const resolved = !!f.resolved_at;
  const anchored = f.anchor_x != null && f.anchor_y != null;
  const box = f.anchor_w != null && f.anchor_h != null;
  const manageable = isAdmin || f.viewer_email === viewerEmail;
  const state = anchored
    ? `<span class="vfb-anchor-state">${f.anchor_page_stale ? "Pinned page missing · stale" : f.artifact_revision !== currentRevision ? `Placed on v${esc(f.artifact_revision)} · stale` : box ? "Pinned section" : "Pinned comment"}</span>`
    : "";
  return `<div class="vfb-item ${resolved ? "resolved" : ""}" data-id="${esc(f.id)}">
    <div class="vfb-m"><span>${esc(f.viewer_email)}</span><span>${esc(fmtDate(f.created_at))}${resolved ? ' &middot; <span class="vfb-res">Resolved</span>' : ""}</span></div>
    <div class="vfb-b">${esc(f.body)}</div>${state}${manageable ? `<div class="vfb-manage"><button class="vfb-delete" type="button" data-feedback-action="delete">Delete</button>${resolved ? "" : '<button class="vfb-resolve" type="button" data-feedback-action="resolve">Resolve</button>'}</div>` : ""}
  </div>`;
}

function feedbackThread(parent, replies, currentRevision, viewer) {
  return `<section class="vfb-thread" data-thread-id="${esc(parent.id)}">${feedbackItem(parent, currentRevision, viewer)}<div class="vfb-replies">${replies.map((reply) => feedbackItem(reply, currentRevision, viewer)).join("")}</div><form class="vfb-reply-form" data-parent-id="${esc(parent.id)}"><textarea maxlength="4000" aria-label="Reply to feedback" placeholder="Reply to this thread…"></textarea><button type="submit">Reply</button></form></section>`;
}

// meta: the artifact row. nav: { prevId, nextId, index, total }. reaction: {favorite, vote}.
// feedback: array of feedback rows for this artifact (org-scoped, resolved shown last).
// analytics.viewers is intentionally supplied only for admins; counts are safe for same-org viewers.
function renderArtifactShellLegacy(meta, nav, reaction = { favorite: 0, vote: 0 }, feedback = [], analytics = {}, viewer = {}, orgAccent = null) {
  const hue = orgColor(meta.org, orgAccent);
  const who = meta.uploader_label || meta.client_id;
  const unresolved = feedback.filter((f) => !f.resolved_at).length;
  const viewerIdentity = { viewerEmail: viewer.email || "", isAdmin: !!viewer.isAdmin };
  const repliesByParent = new Map();
  for (const row of feedback) {
    if (row.parent_id != null) {
      const replies = repliesByParent.get(row.parent_id) || [];
      replies.push(row);
      repliesByParent.set(row.parent_id, replies);
    }
  }
  const feedbackHtml = feedback.length
    ? feedback.filter((f) => f.parent_id == null).map((parent) => feedbackThread(parent, repliesByParent.get(parent.id) || [], meta.revision, viewerIdentity)).join("")
    : '<div class="vfb-empty">No feedback yet. Leave the first note for the author.</div>';
  const counts = analytics.counts || null;
  const viewers = analytics.viewers || null;
  const viewStat = counts
    ? viewers
      ? `<button class="vviewstat" id="vview-toggle" type="button" title="View audience" aria-label="${Number(counts.views || 0)} views by ${Number(counts.unique_viewers || 0)} unique viewers" aria-expanded="false" aria-controls="vview-panel">👁 ${Number(counts.views || 0)} · ${Number(counts.unique_viewers || 0)}</button>`
      : `<span class="vviewstat" aria-label="${Number(counts.views || 0)} views by ${Number(counts.unique_viewers || 0)} unique viewers">👁 ${Number(counts.views || 0)} · ${Number(counts.unique_viewers || 0)}</span>`
    : "";
  const viewersHtml = viewers?.length
    ? viewers.map((v) => `<div class="vfb-item"><div class="vfb-m"><span>${esc(v.email)}</span><span>${Number(v.count || 0)} view${Number(v.count || 0) === 1 ? "" : "s"}</span></div><div class="vfb-b">Last seen ${esc(fmtDate(v.last_viewed_at))}</div></div>`).join("")
    : '<div class="vfb-empty">No audience views recorded yet.</div>';
  const rawSrc = meta.is_bundle ? `/raw/${esc(meta.id)}/` : `/raw/${esc(meta.id)}`;
  const verQ = bodyVersion(meta) ? `&v=${esc(bodyVersion(meta))}` : "";
  const anchorRawSrc = `${rawSrc}?anchor=1${verQ}`;
  const feedbackData = jsLiteral(JSON.stringify(feedback.map((f) => ({
    id: f.id, parent_id: f.parent_id, anchor_path: f.anchor_path, anchor_x: f.anchor_x, anchor_y: f.anchor_y, anchor_w: f.anchor_w, anchor_h: f.anchor_h,
    anchor_approx: f.anchor_approx, anchor_page: f.anchor_page, anchor_page_stale: !!f.anchor_page_stale, artifact_revision: f.artifact_revision
  }))));
  const dlLink = meta.is_bundle
    ? ""
    : `<a class="vtool download" href="/raw/${esc(meta.id)}?download" download title="Download HTML">${ICONS.download}<span class="vlabel">Download</span></a>`;
  const prev = nav.prevId
    ? `<a class="vnav" href="/${esc(nav.prevId)}" title="Newer artifact in ${esc(meta.org)}" rel="prev">${ICONS.back}<span class="vkey">←</span></a>`
    : `<span class="vnav off" aria-hidden="true">${ICONS.back}</span>`;
  const next = nav.nextId
    ? `<a class="vnav" href="/${esc(nav.nextId)}" title="Older artifact in ${esc(meta.org)}" rel="next">${ICONS.forward}<span class="vkey">→</span></a>`
    : `<span class="vnav off" aria-hidden="true">${ICONS.forward}</span>`;

  return `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>${esc(meta.title)} &middot; ${esc(APP_NAME)}</title>
<script>(function(){try{var t=localStorage.getItem('artifact-theme');if(t)document.documentElement.dataset.theme=t;}catch(e){}})();</script>
<style>${SHELL_CSS}</style></head>
<body>
  <header class="vbar" style="--org-k:${hue};--k:color-mix(in oklab,var(--org-k) 72%,var(--txt))">
    <a class="vhome" href="/" aria-label="Back to ${esc(APP_NAME)}" title="${esc(APP_NAME)}">${ICONS.home}</a>
    <div class="vmid">
      <span class="vtitle">${esc(meta.title)}</span>
      <span class="vmeta"><span class="vorg">${esc(meta.org)}</span><span>·</span><span><span class="publisher-label">Published by </span>${esc(who)}</span><span class="vtype">${meta.is_bundle ? "Bundle" : "HTML"}</span><span class="vcat-wrap"><button class="vcat" id="vcat" type="button" title="Set category" data-set="${meta.category ? "1" : "0"}">${meta.category ? esc(meta.category) : "Add category"}</button><form class="vcat-edit" id="vcat-edit" hidden><input id="vcat-input" type="text" maxlength="60" placeholder="Category" value="${esc(meta.category || "")}" aria-label="Artifact category"><button type="submit" class="vcat-save" aria-label="Save category">&#10003;</button></form></span></span>
    </div>
    <nav class="vright" aria-label="Artifact controls">
      <div class="vgroup vreacts" aria-label="Your reaction">
        <button class="vreact fav" data-act="fav" type="button" title="Save to favorites" aria-label="Save to favorites" aria-pressed="${reaction.favorite ? "true" : "false"}">${ICONS.heart}</button>
        <button class="vreact up" data-act="up" type="button" title="Mark as useful" aria-label="Mark as useful" aria-pressed="${reaction.vote > 0 ? "true" : "false"}">${ICONS.up}</button>
        <button class="vreact down" data-act="down" type="button" title="Mark as needing work" aria-label="Mark as needing work" aria-pressed="${reaction.vote < 0 ? "true" : "false"}">${ICONS.down}</button>
      </div>
      <div class="vgroup" aria-label="Browse ${esc(meta.org)} artifacts">
        ${prev}<span class="vpos"><strong>${nav.index}</strong> / ${nav.total}</span>${next}
      </div>
      ${viewStat ? `<div class="vgroup" aria-label="View analytics">${viewStat}</div>` : ""}
      <div class="vgroup">
        <button class="vtool vfb-toggle" id="vfb-toggle" type="button" title="Feedback" aria-label="View and add feedback" aria-expanded="false" aria-controls="vfb-panel"><svg viewBox="0 0 24 24"><path d="M21 15a2 2 0 0 1-2 2H8l-4 4V5a2 2 0 0 1 2-2h13a2 2 0 0 1 2 2Z"></path></svg><span class="vfb-count" ${unresolved ? "" : "hidden"}>${unresolved}</span></button>
        <button class="vtool vcomment-toggle" id="vcomment-toggle" type="button" title="Comment on a place" aria-label="Toggle comment mode" aria-pressed="false"><svg viewBox="0 0 24 24"><path d="M12 20v-8"></path><path d="M8 16l4 4 4-4"></path><circle cx="12" cy="7" r="3"></circle></svg></button>
        <button class="vtool vhist-toggle" id="vhist-toggle" type="button" title="Version history" aria-label="Version history" aria-expanded="false" aria-controls="vhist-panel"><svg viewBox="0 0 24 24"><path d="M12 8v4l3 2"></path><path d="M3.05 11a9 9 0 1 1 .5 4M3 16v-5h5"></path></svg><span class="vhist-badge">v${meta.revision}</span></button>
        <a class="vtool" href="${rawSrc}" target="_blank" rel="noopener" title="Open without viewer chrome" aria-label="Open raw artifact">${ICONS.open}</a>
        ${dlLink}
        <button class="vtool download" id="vshare-toggle" type="button" title="Create public share link" aria-label="Create public share link" aria-expanded="false" aria-controls="vshare-panel"><span aria-hidden="true">↗</span><span class="vlabel">Share</span></button>
        <button class="vtool vtheme" id="vtheme" type="button" title="Change color theme" aria-label="Change color theme">${ICONS.theme}</button>
        <a class="vtool vsignout" href="/cdn-cgi/access/logout" title="Sign out" aria-label="Sign out">${ICONS.signout}</a>
      </div>
    </nav>
  </header>
  <div class="reaction-status" id="reaction-status" role="status" aria-live="polite"></div>
  <div class="vstage" id="vstage"><iframe class="vframe" id="vframe" src="${anchorRawSrc}" title="${esc(meta.title)}"
          sandbox="allow-scripts allow-popups allow-forms allow-modals"></iframe><div class="vanchor-overlay" id="vanchor-overlay" aria-hidden="true"></div></div>
  <aside class="vfb-panel" id="vfb-panel" aria-label="Feedback" aria-hidden="true">
    <div class="vfb-head"><h2>Feedback</h2><button class="vfb-close" id="vfb-close" type="button" aria-label="Close feedback">&#10005;</button></div>
    <div class="vfb-list" id="vfb-list">${feedbackHtml}</div>
    <form class="vfb-form" id="vfb-form">
      <textarea id="vfb-body" placeholder="Leave feedback for the author…" maxlength="4000" aria-label="Your feedback"></textarea>
      <div class="vfb-actions"><span class="vfb-hint" id="vfb-hint"></span><button class="vfb-send" type="submit">Send feedback</button></div>
    </form>
  </aside>
  <aside class="vfb-panel vhist-panel" id="vhist-panel" aria-label="Version history" aria-hidden="true">
    <div class="vfb-head"><h2>History</h2><button class="vfb-close" id="vhist-close" type="button" aria-label="Close history">&#10005;</button></div>
    <div class="vfb-list" id="vhist-list"><div class="vfb-empty">Loading…</div></div>
  </aside>
  <aside class="vfb-panel" id="vshare-panel" aria-label="Public share links" aria-hidden="true">
    <div class="vfb-head"><h2>Share</h2><button class="vfb-close" id="vshare-close" type="button" aria-label="Close share links">&#10005;</button></div>
    <div class="vfb-list" id="vshare-list"><div class="vfb-empty">Loading active links…</div></div>
    <form class="vshare-form" id="vshare-form">
      <label for="vshare-expiry">Link expiry</label>
      <select id="vshare-expiry" aria-label="Link expiry"><option value="24h">24 hours</option><option value="date">Until a date</option><option value="never">No expiration</option></select>
      <input id="vshare-date" type="date" aria-label="Share expiration date" hidden>
      <button type="submit">Create link</button>
      <div class="vshare-result" id="vshare-result" aria-live="polite"></div>
    </form>
  </aside>
  ${viewers ? `<aside class="vfb-panel" id="vview-panel" aria-label="Audience views" aria-hidden="true">
    <div class="vfb-head"><h2>Audience views</h2><button class="vfb-close" id="vview-close" type="button" aria-label="Close audience views">&#10005;</button></div>
    <div class="vfb-list">${viewersHtml}</div>
  </aside>` : ""}
  <script>
    (function(){
      var theme=document.getElementById('vtheme');
      if(theme) theme.addEventListener('click',function(){
        var current=document.documentElement.dataset.theme;
        var dark=window.matchMedia&&window.matchMedia('(prefers-color-scheme: dark)').matches;
        var next=current==='dark'?'light':current==='light'?'dark':dark?'light':'dark';
        document.documentElement.dataset.theme=next;
        try{localStorage.setItem('artifact-theme',next);}catch(e){}
      });

      document.addEventListener('keydown',function(e){
        if(e.defaultPrevented||e.altKey||e.ctrlKey||e.metaKey) return;
        if(e.target.closest&&e.target.closest('a,button,input,textarea,select,[contenteditable]')) return;
        ${nav.prevId ? `if(e.key==='ArrowLeft'){location.href='/${esc(nav.prevId)}';}` : ""}
        ${nav.nextId ? `if(e.key==='ArrowRight'){location.href='/${esc(nav.nextId)}';}` : ""}
      });

      var R={favorite:${reaction.favorite ? 1 : 0},vote:${reaction.vote || 0}};
      var buttons=[].slice.call(document.querySelectorAll('.vreact'));
      var status=document.getElementById('reaction-status');
      var statusTimer;
      function announce(message,isError){
        clearTimeout(statusTimer);status.textContent=message;status.classList.toggle('error',!!isError);status.classList.add('show');
        statusTimer=setTimeout(function(){status.classList.remove('show');},1800);
      }
      function paintR(){
        var f=document.querySelector('.vreact.fav'),u=document.querySelector('.vreact.up'),d=document.querySelector('.vreact.down');
        f.setAttribute('aria-pressed',R.favorite?'true':'false');u.setAttribute('aria-pressed',R.vote>0?'true':'false');d.setAttribute('aria-pressed',R.vote<0?'true':'false');
        f.setAttribute('aria-label',R.favorite?'Remove from favorites':'Save to favorites');
      }
      buttons.forEach(function(b){
        b.addEventListener('click',function(){
          var act=b.dataset.act,body={};
          if(act==='fav')body.favorite=R.favorite?0:1;
          else if(act==='up')body.vote=R.vote>0?0:1;
          else body.vote=R.vote<0?0:-1;
          buttons.forEach(function(x){x.disabled=true;});
          fetch('/${esc(meta.id)}/react',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)})
            .then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Request failed');return d;});})
            .then(function(d){
              if(d&&typeof d.favorite!=='undefined'){R.favorite=d.favorite;R.vote=d.vote;paintR();announce(act==='fav'?(R.favorite?'Saved to favorites':'Removed from favorites'):'Feedback saved',false);}
            })
            .catch(function(){announce('Could not save feedback',true);})
            .finally(function(){buttons.forEach(function(x){x.disabled=false;});});
        });
      });

      // Feedback drawer
      function fesc(s){return String(s==null?'':s).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];});}
      var viewerEmail=${jsLiteral(viewerIdentity.viewerEmail)},viewerIsAdmin=${viewerIdentity.isAdmin ? "true" : "false"};
      var shareArtifactId=${jsLiteral(meta.id)};
      var shareToggle=document.getElementById('vshare-toggle'),sharePanel=document.getElementById('vshare-panel'),shareClose=document.getElementById('vshare-close'),shareList=document.getElementById('vshare-list'),shareForm=document.getElementById('vshare-form'),shareExpiry=document.getElementById('vshare-expiry'),shareDate=document.getElementById('vshare-date'),shareResult=document.getElementById('vshare-result'),shareLoaded=false;
      function copyShareUrl(url,button){function done(){button.textContent='Copied';setTimeout(function(){button.textContent='Copy';},1200);}if(navigator.clipboard&&navigator.clipboard.writeText){navigator.clipboard.writeText(url).then(done).catch(fallback);return;}fallback();function fallback(){var input=document.createElement('textarea');input.value=url;input.setAttribute('readonly','');input.style.position='fixed';input.style.opacity='0';document.body.appendChild(input);input.select();try{if(document.execCommand('copy'))done();}catch(_){}input.remove();}}
      function shareRow(row){var expiry=row.expires_at?'Expires '+fesc(String(row.expires_at).replace('T',' ').slice(0,16)):'No expiration',created=row.created_at?'Created '+fesc(String(row.created_at).replace('T',' ').slice(0,16)):'Created recently';return '<div class="vfb-item vshare-row" data-token="'+fesc(row.token)+'"><div class="vfb-m"><span>'+expiry+'</span><span>'+created+'</span></div><div class="vshare-result"><a href="/s/'+fesc(row.token)+'" target="_blank" rel="noopener">/s/'+fesc(row.token)+'</a><button class="vshare-copy" type="button" data-share-copy="'+fesc(row.token)+'">Copy</button></div><button class="vshare-revoke" type="button" data-share-revoke="'+fesc(row.token)+'">Revoke</button></div>';}
      function shareLoad(){shareList.innerHTML='<div class="vfb-empty">Loading active links…</div>';fetch('/'+shareArtifactId+'/shares').then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not load links');return d;});}).then(function(d){shareLoaded=true;var rows=Array.isArray(d.shares)?d.shares:[];shareList.innerHTML=rows.length?rows.map(shareRow).join(''):'<div class="vfb-empty">No active public links.</div>';}).catch(function(){shareLoaded=false;shareList.innerHTML='<div class="vfb-empty">Could not load share links.</div>';});}
      function shareOpen(open){if(open){if(typeof fbOpen==='function')fbOpen(false);if(typeof histOpen==='function')histOpen(false);if(typeof viewOpen==='function')viewOpen(false);}sharePanel.classList.toggle('open',open);sharePanel.setAttribute('aria-hidden',open?'false':'true');shareToggle.setAttribute('aria-expanded',open?'true':'false');if(open&&!shareLoaded)shareLoad();}
      if(shareToggle)shareToggle.addEventListener('click',function(){shareOpen(!sharePanel.classList.contains('open'));});
      if(shareClose)shareClose.addEventListener('click',function(){shareOpen(false);});
      if(shareExpiry)shareExpiry.addEventListener('change',function(){shareDate.hidden=shareExpiry.value!=='date';if(shareExpiry.value==='date')shareDate.focus();});
      if(shareForm)shareForm.addEventListener('submit',function(e){e.preventDefault();var expires=shareExpiry.value==='date'?shareDate.value:shareExpiry.value;if(!expires){shareResult.textContent='Choose a future date.';return;}var button=shareForm.querySelector('button[type="submit"]');button.disabled=true;shareResult.textContent='Creating…';fetch('/'+shareArtifactId+'/share',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({expires:expires})}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not create link');return d;});}).then(function(d){var url=String(d.url||'');shareResult.innerHTML='<a href="'+fesc(url)+'" target="_blank" rel="noopener">'+fesc(url)+'</a><button class="vshare-copy" type="button" data-share-url="'+fesc(url)+'">Copy</button>';shareLoaded=false;shareLoad();}).catch(function(err){shareResult.textContent=err.message||'Could not create link';}).finally(function(){button.disabled=false;});});
      if(shareResult)shareResult.addEventListener('click',function(e){var copy=e.target.closest('[data-share-url]');if(copy)copyShareUrl(copy.getAttribute('data-share-url'),copy);});
      if(shareList)shareList.addEventListener('click',function(e){var copy=e.target.closest('[data-share-copy],[data-share-url]');if(copy){var url=copy.getAttribute('data-share-url')||location.origin+'/s/'+copy.getAttribute('data-share-copy');copyShareUrl(url,copy);return;}var revoke=e.target.closest('[data-share-revoke]');if(!revoke)return;var token=revoke.getAttribute('data-share-revoke');revoke.disabled=true;fetch('/'+shareArtifactId+'/shares/'+encodeURIComponent(token),{method:'DELETE'}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not revoke link');return d;});}).then(function(){var row=revoke.closest('[data-token]');if(row)row.remove();if(!shareList.querySelector('[data-token]'))shareList.innerHTML='<div class="vfb-empty">No active public links.</div>';}).catch(function(){revoke.disabled=false;});});
      document.addEventListener('keydown',function(e){if(e.key==='Escape'&&sharePanel.classList.contains('open'))shareOpen(false);});
      var fbToggle=document.getElementById('vfb-toggle'),fbPanel=document.getElementById('vfb-panel'),fbClose=document.getElementById('vfb-close'),fbList=document.getElementById('vfb-list'),fbForm=document.getElementById('vfb-form'),fbBody=document.getElementById('vfb-body'),fbHint=document.getElementById('vfb-hint'),fbCount=document.querySelector('.vfb-count');
      function fbOpen(open){if(open){var hp=document.getElementById('vhist-panel'),vp=document.getElementById('vview-panel');[hp,vp].forEach(function(p){if(p&&p.classList.contains('open')){p.classList.remove('open');p.setAttribute('aria-hidden','true');}});['vhist-toggle','vview-toggle'].forEach(function(id){var t=document.getElementById(id);if(t)t.setAttribute('aria-expanded','false');});}fbPanel.classList.toggle('open',open);fbPanel.setAttribute('aria-hidden',open?'false':'true');fbToggle.setAttribute('aria-expanded',open?'true':'false');if(open){setTimeout(function(){fbBody.focus();},180);}}
      if(fbToggle){fbToggle.addEventListener('click',function(){fbOpen(!fbPanel.classList.contains('open'));});}
      if(fbClose){fbClose.addEventListener('click',function(){fbOpen(false);});}
      document.addEventListener('keydown',function(e){if(e.key==='Escape'&&fbPanel.classList.contains('open')){fbOpen(false);}});
      function canManage(row){return viewerIsAdmin||row.viewer_email===viewerEmail;}
      function itemHtml(row,justNow){var resolved=!!row.resolved_at,anchored=row.anchor_x!=null&&row.anchor_y!=null,box=row.anchor_w!=null&&row.anchor_h!=null,manage=canManage(row);return '<div class="vfb-item '+(resolved?'resolved':'')+'" data-id="'+fesc(row.id)+'"><div class="vfb-m"><span>'+fesc(row.viewer_email)+'</span><span>'+fesc(justNow?'Just now':'')+(resolved?' &middot; <span class="vfb-res">Resolved</span>':'')+'</span></div><div class="vfb-b">'+fesc(row.body)+'</div>'+(anchored?'<span class="vfb-anchor-state">'+(box?'Pinned section':'Pinned comment')+'</span>':'')+(manage?'<div class="vfb-manage"><button class="vfb-delete" type="button" data-feedback-action="delete">Delete</button>'+(resolved?'':'<button class="vfb-resolve" type="button" data-feedback-action="resolve">Resolve</button>')+'</div>':'')+'</div>';}
      function replyFormHtml(parentId){return '<form class="vfb-reply-form" data-parent-id="'+fesc(parentId)+'"><textarea maxlength="4000" aria-label="Reply to feedback" placeholder="Reply to this thread…"></textarea><button type="submit">Reply</button></form>';}

      // Positional comments are a postMessage-only boundary: this shell never inspects
      // the sandboxed iframe document. The bridge owns its document and reports pixels.
      var frame=document.getElementById('vframe'),overlay=document.getElementById('vanchor-overlay'),commentToggle=document.getElementById('vcomment-toggle'),commentMode=false,bridgeReady=false,currentPage=null,draftAnchor=null,bridgeTimer,fallbackDrag=null,isBundle=${meta.is_bundle ? "true" : "false"};
      var feedbackRows=JSON.parse(${feedbackData}),pins=[],pinById={},feedbackItems={},feedbackThreads={};
      [].slice.call(fbList.querySelectorAll('.vfb-item[data-id]')).forEach(function(item){feedbackItems[item.getAttribute('data-id')]=item;});
      [].slice.call(fbList.querySelectorAll('.vfb-thread[data-thread-id]')).forEach(function(thread){feedbackThreads[thread.getAttribute('data-thread-id')]=thread;});
      function finiteFraction(value){return typeof value==='number'&&Number.isFinite(value)?Math.max(0,Math.min(1,value)):null;}
      function positiveFraction(value){var n=finiteFraction(value);return n!==null&&n>0?n:null;}
      function pinFromRow(row){
        if(row.parent_id!=null)return null;var x=finiteFraction(row.anchor_x),y=finiteFraction(row.anchor_y);if(x===null||y===null)return null;
        var w=positiveFraction(row.anchor_w),h=positiveFraction(row.anchor_h),box=w!==null&&h!==null;
        if(box){w=Math.min(w,1-x);h=Math.min(h,1-y);if(w<=0||h<=0)box=false;}
        return {id:String(row.id),page:typeof row.anchor_page==='string'?row.anchor_page:null,path:typeof row.anchor_path==='string'?row.anchor_path.slice(0,512):null,x:x,y:y,w:box?w:null,h:box?h:null,approx:row.anchor_approx?1:0,stale:!!row.anchor_page_stale||Number(row.artifact_revision)!==${Number(meta.revision) || 1}};
      }
      feedbackRows.forEach(function(row){
        var pin=pinFromRow(row);if(pin){pins.push(pin);pinById[pin.id]=pin;}
      });
      function postToFrame(type,extra){try{if(frame&&frame.contentWindow)frame.contentWindow.postMessage(Object.assign({type:type},extra||{}),'*');}catch(_){}}
      function pinOnCurrentPage(pin){return !isBundle||pin.page===null||pin.page===currentPage;}
      function hideAllMarkers(){[].slice.call(overlay.querySelectorAll('.vanchor-marker')).forEach(function(marker){marker.hidden=true;});}
      function requestRepaint(){var pagePins=pins.filter(pinOnCurrentPage);postToFrame('anchor:repaint',{anchors:pagePins.map(function(pin){return {id:pin.id,path:pin.path,x:pin.x,y:pin.y,w:pin.w,h:pin.h};})});}
      function pinNumber(pin){return pins.indexOf(pin)+1;}
      function markerFor(pin){
        var marker=document.getElementById('vanchor-'+pin.id);if(marker)return marker;
        var box=pin.w!==null&&pin.h!==null,label=box?'Pinned section':'Pinned comment';marker=document.createElement('button');marker.type='button';marker.id='vanchor-'+pin.id;marker.className='vanchor-marker'+(box?' vanchor-box':'')+(pin.stale?' stale':'');marker.textContent=box?'':String(pinNumber(pin));if(box)marker.setAttribute('data-pin',String(pinNumber(pin)));marker.title=pin.stale?label+' '+pinNumber(pin)+' · placed on an older revision':label+' '+pinNumber(pin);
        marker.addEventListener('click',function(e){e.preventDefault();e.stopPropagation();fbOpen(true);var thread=feedbackThreads[pin.id],item=feedbackItems[pin.id];if(thread){if(item)item.classList.add('pin-focus');thread.scrollIntoView({block:'center'});setTimeout(function(){if(item)item.classList.remove('pin-focus');},1600);}fbHint.textContent=label+' '+pinNumber(pin)+(pin.stale?' · placed on an older revision.':'');});
        overlay.appendChild(marker);return marker;
      }
      function focusFeedback(id){
        fbOpen(true);var row=feedbackRows.find(function(entry){return String(entry.id)===id;}),item=feedbackItems[id],thread=row&&row.parent_id?feedbackThreads[row.parent_id]:feedbackThreads[id];
        if(item){item.classList.add('pin-focus');(thread||item).scrollIntoView({block:'center'});setTimeout(function(){item.classList.remove('pin-focus');},2200);}
        var pin=pinById[id];if(pin){var marker=markerFor(pin);marker.classList.add('pin-focus');setTimeout(function(){marker.classList.remove('pin-focus');},2200);if(isBundle&&pin.page){frame.src=${jsLiteral(`/raw/${meta.id}/`)}+pin.page.split('/').map(encodeURIComponent).join('/')+'?anchor=1${verQ}';}}
      }
      var requestedFeedback=new URLSearchParams(window.location.search).get('feedback');
      if(requestedFeedback)setTimeout(function(){focusFeedback(requestedFeedback);},0);
      function positionLost(pin){
        var marker=document.getElementById('vanchor-'+pin.id);if(marker)marker.hidden=true;
        var item=feedbackItems[pin.id];if(item&&!item.querySelector('.vfb-anchor-state[data-lost]')){var note=document.createElement('span');note.className='vfb-anchor-state';note.setAttribute('data-lost','1');note.textContent='Position lost · shown in this thread';item.appendChild(note);}
      }
      function paintPosition(pin,x,y,width,height,lost){if(lost){positionLost(pin);return;}var marker=markerFor(pin),box=pin.w!==null&&pin.h!==null;marker.hidden=false;marker.style.left=Math.round(x)+'px';marker.style.top=Math.round(y)+'px';if(box){marker.style.width=Math.max(1,Math.round(width))+'px';marker.style.height=Math.max(1,Math.round(height))+'px';}}
      function setCommentMode(next){commentMode=!!next;commentToggle.setAttribute('aria-pressed',commentMode?'true':'false');document.body.classList.toggle('vpinning',commentMode);if(!commentMode){overlay.classList.remove('fallback');postToFrame('anchor:pick-off');return;}if(bridgeReady){overlay.classList.remove('fallback');postToFrame('anchor:pick-on');requestRepaint();}else overlay.classList.add('fallback');}
      function startAnchoredComment(anchor){var x=finiteFraction(anchor&&anchor.x),y=finiteFraction(anchor&&anchor.y),width=positiveFraction(anchor&&anchor.w),height=positiveFraction(anchor&&anchor.h),box=width!==null&&height!==null;if(x===null||y===null)return;if(box){width=Math.min(width,1-x);height=Math.min(height,1-y);if(width<=0||height<=0)return;}draftAnchor={x:x,y:y,page:isBundle?(typeof anchor.page==='string'?anchor.page:currentPage):null,path:typeof anchor.path==='string'?anchor.path.slice(0,512):undefined,approx:anchor.approx?1:0};if(box){draftAnchor.w=width;draftAnchor.h=height;}setCommentMode(false);fbOpen(true);fbHint.classList.remove('error');fbHint.textContent=draftAnchor.approx?(box?'Approximate section selected.':'Approximate pin selected.'):(box?'Pinned section selected.':'Pinned location selected.');}
      if(commentToggle)commentToggle.addEventListener('click',function(){setCommentMode(!commentMode);});
      function fallbackPoint(e){var rect=overlay.getBoundingClientRect();if(!rect.width||!rect.height)return null;return {x:(e.clientX-rect.left)/rect.width,y:(e.clientY-rect.top)/rect.height};}
      function clearFallbackSelection(){var selection=overlay.querySelector('.vanchor-selection');if(selection)selection.remove();}
      function drawFallbackSelection(a,b){var selection=overlay.querySelector('.vanchor-selection');if(!selection){selection=document.createElement('div');selection.className='vanchor-selection';overlay.appendChild(selection);}selection.style.left=Math.min(a.x,b.x)+'px';selection.style.top=Math.min(a.y,b.y)+'px';selection.style.width=Math.abs(a.x-b.x)+'px';selection.style.height=Math.abs(a.y-b.y)+'px';}
      overlay.addEventListener('pointerdown',function(e){if(!commentMode||bridgeReady||e.button!==0||e.target.closest('.vanchor-marker'))return;var point=fallbackPoint(e);if(!point)return;e.preventDefault();e.stopPropagation();fallbackDrag={id:e.pointerId,x:e.clientX,y:e.clientY,moved:false};try{overlay.setPointerCapture(e.pointerId);}catch(_){};});
      overlay.addEventListener('pointermove',function(e){if(!fallbackDrag||e.pointerId!==fallbackDrag.id)return;e.preventDefault();e.stopPropagation();if(Math.abs(e.clientX-fallbackDrag.x)>4||Math.abs(e.clientY-fallbackDrag.y)>4){fallbackDrag.moved=true;drawFallbackSelection({x:fallbackDrag.x-overlay.getBoundingClientRect().left,y:fallbackDrag.y-overlay.getBoundingClientRect().top},{x:e.clientX-overlay.getBoundingClientRect().left,y:e.clientY-overlay.getBoundingClientRect().top});}});
      function finishFallbackDrag(e){if(!fallbackDrag||e.pointerId!==fallbackDrag.id)return;var start=fallbackDrag;fallbackDrag=null;clearFallbackSelection();e.preventDefault();e.stopPropagation();var end=fallbackPoint(e);if(!end)return;if(start.moved){var rect=overlay.getBoundingClientRect(),sx=(start.x-rect.left)/rect.width,sy=(start.y-rect.top)/rect.height;startAnchoredComment({x:Math.min(sx,end.x),y:Math.min(sy,end.y),w:Math.abs(end.x-sx),h:Math.abs(end.y-sy),approx:1});}else startAnchoredComment({x:end.x,y:end.y,approx:1});}
      overlay.addEventListener('pointerup',finishFallbackDrag);overlay.addEventListener('pointercancel',function(e){if(fallbackDrag&&e.pointerId===fallbackDrag.id){fallbackDrag=null;clearFallbackSelection();}});
      if(frame)frame.addEventListener('load',function(){clearTimeout(bridgeTimer);bridgeReady=false;currentPage=null;hideAllMarkers();overlay.classList.remove('fallback');bridgeTimer=setTimeout(function(){if(!bridgeReady&&commentMode)overlay.classList.add('fallback');},800);});
      var outboundPanel=null,outboundHost=null,outboundConfirm=null,outboundUrl=null;
      function parseOutboundHref(href){if(typeof href!=='string')return null;try{var url=new URL(href);return url.protocol==='http:'||url.protocol==='https:'?url:null;}catch(_){return null;}}
      function closeOutbound(){outboundUrl=null;if(!outboundPanel)return;outboundPanel.classList.remove('open');outboundPanel.setAttribute('aria-hidden','true');}
      function ensureOutboundPanel(){
        if(outboundPanel)return;
        outboundPanel=document.createElement('aside');outboundPanel.className='vfb-panel';outboundPanel.setAttribute('role','dialog');outboundPanel.setAttribute('aria-modal','true');outboundPanel.setAttribute('aria-label','Confirm external link');outboundPanel.setAttribute('aria-hidden','true');
        var head=document.createElement('div'),title=document.createElement('h2'),close=document.createElement('button'),content=document.createElement('div'),message=document.createElement('p'),actions=document.createElement('div'),cancel=document.createElement('button');
        head.className='vfb-head';title.textContent='Open external link?';close.type='button';close.className='vfb-close';close.textContent='×';close.setAttribute('aria-label','Close external link confirmation');head.appendChild(title);head.appendChild(close);
        content.className='vfb-list';message.textContent='You are being sent to ';outboundHost=document.createElement('strong');message.appendChild(outboundHost);content.appendChild(message);
        actions.className='vfb-actions';cancel.type='button';cancel.textContent='Cancel';outboundConfirm=document.createElement('button');outboundConfirm.type='button';outboundConfirm.className='vfb-send';outboundConfirm.textContent='Open link';actions.appendChild(cancel);actions.appendChild(outboundConfirm);
        outboundPanel.appendChild(head);outboundPanel.appendChild(content);outboundPanel.appendChild(actions);document.body.appendChild(outboundPanel);
        close.addEventListener('click',closeOutbound);cancel.addEventListener('click',closeOutbound);outboundConfirm.addEventListener('click',function(){var url=outboundUrl;closeOutbound();if(url)window.open(url.href,'_blank','noopener');});
      }
      function showOutbound(url){ensureOutboundPanel();outboundUrl=url;outboundHost.textContent=url.host;outboundPanel.classList.add('open');outboundPanel.setAttribute('aria-hidden','false');outboundConfirm.focus();}
      window.addEventListener('message',function(event){
        if(!frame||event.source!==frame.contentWindow)return;var data=event.data;if(!data||typeof data!=='object')return;
        if(data.type!=='anchor:ready'&&data.type!=='anchor:picked'&&data.type!=='anchor:positions'&&data.type!=='anchor:navigate')return;
        if(data.type==='anchor:navigate'){var url=parseOutboundHref(data.href);if(url)showOutbound(url);return;}
        if(data.type==='anchor:ready'){currentPage=isBundle&&typeof data.page==='string'?data.page:null;bridgeReady=true;hideAllMarkers();if(commentMode){overlay.classList.remove('fallback');postToFrame('anchor:pick-on');}requestRepaint();return;}
        if(data.type==='anchor:picked'){startAnchoredComment(data);return;}if(!Array.isArray(data.anchors))return;
        data.anchors.slice(0,200).forEach(function(pos){if(!pos||typeof pos!=='object'||typeof pos.id!=='string')return;var pin=pinById[pos.id];if(!pin||!pinOnCurrentPage(pin))return;if(pos.lost===true){paintPosition(pin,0,0,0,0,true);return;}if(typeof pos.x!=='number'||typeof pos.y!=='number'||!Number.isFinite(pos.x)||!Number.isFinite(pos.y))return;if(pin.w!==null&&pin.h!==null){if(typeof pos.w!=='number'||typeof pos.h!=='number'||!Number.isFinite(pos.w)||!Number.isFinite(pos.h)||pos.w<=0||pos.h<=0)return;paintPosition(pin,pos.x,pos.y,pos.w,pos.h,false);}else paintPosition(pin,pos.x,pos.y,0,0,false);});
      });
      function updateCount(delta){var n=Math.max(0,(parseInt(fbCount.textContent,10)||0)+delta);fbCount.textContent=n;fbCount.hidden=!n;}
      function appendFeedback(row){
        var empty=fbList.querySelector('.vfb-empty');if(empty)empty.remove();var item,thread;
        if(row.parent_id){thread=feedbackThreads[row.parent_id];if(!thread)return;var replies=thread.querySelector('.vfb-replies');var holder=document.createElement('div');holder.innerHTML=itemHtml(row,true);item=holder.firstChild;replies.appendChild(item);}
        else{thread=document.createElement('section');thread.className='vfb-thread';thread.setAttribute('data-thread-id',row.id);thread.innerHTML=itemHtml(row,true)+'<div class="vfb-replies"></div>'+replyFormHtml(row.id);fbList.appendChild(thread);feedbackThreads[row.id]=thread;item=thread.querySelector('.vfb-item');}
        feedbackItems[row.id]=item;feedbackRows.push(row);
        var pin=pinFromRow(row);if(pin){pins.push(pin);pinById[pin.id]=pin;requestRepaint();}
        updateCount(1);fbList.scrollTop=fbList.scrollHeight;
      }
      function sendFeedback(text,parentId,anchor){return fetch('/${esc(meta.id)}/feedback',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({body:text,parent_id:parentId||undefined,anchor:parentId?undefined:(anchor||undefined),anchor_page:anchor&&anchor.page})}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not send feedback');return d;});});}
      if(fbForm)fbForm.addEventListener('submit',function(e){e.preventDefault();var text=(fbBody.value||'').trim();if(!text){fbHint.textContent='Write something first.';fbHint.classList.add('error');return;}var btn=fbForm.querySelector('.vfb-send');btn.disabled=true;fbHint.classList.remove('error');fbHint.textContent='Sending…';sendFeedback(text,null,draftAnchor).then(function(d){appendFeedback(d);fbBody.value='';draftAnchor=null;fbHint.textContent='Sent to the author.';}).catch(function(err){fbHint.textContent=err.message||'Could not send feedback.';fbHint.classList.add('error');}).finally(function(){btn.disabled=false;});});
      fbList.addEventListener('submit',function(e){var form=e.target.closest('.vfb-reply-form');if(!form)return;e.preventDefault();var input=form.querySelector('textarea'),text=(input.value||'').trim(),button=form.querySelector('button');if(!text)return;button.disabled=true;sendFeedback(text,form.getAttribute('data-parent-id')).then(function(d){appendFeedback(d);input.value='';}).catch(function(err){fbHint.textContent=err.message||'Could not send reply.';fbHint.classList.add('error');}).finally(function(){button.disabled=false;});});
      function forgetPin(id){var marker=document.getElementById('vanchor-'+id);if(marker)marker.remove();delete pinById[id];pins=pins.filter(function(pin){return pin.id!==id;});}
      fbList.addEventListener('click',function(e){var button=e.target.closest('[data-feedback-action]');if(!button)return;var item=button.closest('.vfb-item[data-id]'),id=item&&item.getAttribute('data-id'),action=button.getAttribute('data-feedback-action');if(!id)return;button.disabled=true;var url='/${esc(meta.id)}/feedback/'+encodeURIComponent(id)+(action==='resolve'?'/resolve':'');fetch(url,{method:action==='resolve'?'POST':'DELETE'}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not update feedback');return d;});}).then(function(){if(action==='resolve'){if(!item.classList.contains('resolved')){item.classList.add('resolved');updateCount(-1);}var resolve=item.querySelector('.vfb-resolve');if(resolve)resolve.remove();var stamp=item.querySelector('.vfb-m span:last-child');if(stamp&&!stamp.querySelector('.vfb-res'))stamp.insertAdjacentHTML('beforeend',' &middot; <span class="vfb-res">Resolved</span>');return;}var thread=item.closest('.vfb-thread');var isTop=!!(thread&&item===feedbackItems[thread.getAttribute('data-thread-id')]);var items=isTop?[].slice.call(thread.querySelectorAll('.vfb-item[data-id]')):[item];items.forEach(function(node){var nodeId=node.getAttribute('data-id');if(!node.classList.contains('resolved'))updateCount(-1);forgetPin(nodeId);delete feedbackItems[nodeId];});if(isTop&&thread){delete feedbackThreads[thread.getAttribute('data-thread-id')];thread.remove();}else item.remove();if(!fbList.querySelector('.vfb-thread'))fbList.innerHTML='<div class="vfb-empty">No feedback yet. Leave the first note for the author.</div>';requestRepaint();}).catch(function(err){fbHint.textContent=err.message||'Could not update feedback.';fbHint.classList.add('error');button.disabled=false;});});

      // Category editor (top bar)
      var vcat=document.getElementById('vcat'),vcatEdit=document.getElementById('vcat-edit'),vcatInput=document.getElementById('vcat-input');
      function vcatShow(edit){vcat.hidden=edit;vcatEdit.hidden=!edit;if(edit){vcatInput.focus();vcatInput.select();}}
      if(vcat){vcat.addEventListener('click',function(){vcatShow(true);});}
      if(vcatEdit){
        vcatInput.addEventListener('keydown',function(e){if(e.key==='Escape'){e.preventDefault();vcatShow(false);}});
        vcatEdit.addEventListener('submit',function(e){
          e.preventDefault();
          var val=(vcatInput.value||'').trim();var save=vcatEdit.querySelector('.vcat-save');save.disabled=true;
          fetch('/${esc(meta.id)}/category',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({category:val})})
            .then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Failed');return d;});})
            .then(function(d){vcat.textContent=d.category?d.category:'Add category';vcat.setAttribute('data-set',d.category?'1':'0');vcatInput.value=d.category||'';vcatShow(false);})
            .catch(function(){vcatShow(false);})
            .finally(function(){save.disabled=false;});
        });
      }

      // Version history drawer
      var histToggle=document.getElementById('vhist-toggle'),histPanel=document.getElementById('vhist-panel'),histClose=document.getElementById('vhist-close'),histList=document.getElementById('vhist-list');
      var histLoaded=false,isBundle=${meta.is_bundle ? "true" : "false"},curTitle=${jsLiteral(meta.title)},curBytes=${Number(meta.bytes) || 0};
      function kb(b){return (Math.round((b||0)/102.4)/10)+' KB';}
      function histRow(r,isCurrent){
        var when=(r.created_at||'').replace('T',' ').slice(0,16);
        var view='/raw/${esc(meta.id)}/rev/'+r.revision+(isBundle?'/':'');
        return '<div class="vhist-item'+(isCurrent?' current':'')+'">'+
          '<div class="vh-m"><strong>v'+r.revision+'</strong>'+(isCurrent?'<span class="vh-cur">current</span>':'<span class="vh-when">'+fesc(when)+'</span>')+'</div>'+
          '<div class="vh-t">'+fesc(r.title)+'<span class="vh-size">'+kb(r.bytes)+'</span></div>'+
          '<div class="vh-actions">'+(isCurrent?'':'<a class="vh-view" href="'+view+'" target="_blank" rel="noopener">View</a><button class="vh-restore" type="button" data-rev="'+r.revision+'">Restore</button>')+'</div>'+
        '</div>';
      }
      function histLoad(){
        histList.innerHTML='<div class="vfb-empty">Loading…</div>';
        fetch('/${esc(meta.id)}/history').then(function(r){return r.json();}).then(function(d){
          histLoaded=true;
          var cur=d.current||1,revs=d.revisions||[];
          var html=histRow({revision:cur,title:curTitle,bytes:curBytes},true);
          if(!revs.length){html+='<div class="vfb-empty" style="border:0;margin-top:.4rem">No earlier versions yet. Each update adds one here.</div>';}
          revs.forEach(function(r){html+=histRow(r,false);});
          histList.innerHTML=html;
        }).catch(function(){histLoaded=false;histList.innerHTML='<div class="vfb-empty">Could not load history.</div>';});
      }
      function histOpen(open){
        if(open&&typeof fbOpen==='function')fbOpen(false);
        if(open&&typeof viewOpen==='function')viewOpen(false);
        histPanel.classList.toggle('open',open);histPanel.setAttribute('aria-hidden',open?'false':'true');histToggle.setAttribute('aria-expanded',open?'true':'false');
        if(open&&!histLoaded)histLoad();
      }
      if(histToggle){histToggle.addEventListener('click',function(){histOpen(!histPanel.classList.contains('open'));});}
      if(histClose){histClose.addEventListener('click',function(){histOpen(false);});}
      document.addEventListener('keydown',function(e){if(e.key==='Escape'&&histPanel.classList.contains('open')){histOpen(false);}});
      if(histList){histList.addEventListener('click',function(e){
        var b=e.target.closest('.vh-restore');if(!b)return;
        var rev=b.getAttribute('data-rev');
        if(!confirm('Restore v'+rev+'? It becomes a NEW revision at the same URL — nothing is lost.'))return;
        b.disabled=true;b.textContent='Restoring…';
        fetch('/${esc(meta.id)}/restore',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({revision:Number(rev)})})
          .then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Restore failed');return d;});})
          .then(function(){location.reload();})
          .catch(function(err){b.disabled=false;b.textContent='Restore';alert(err.message||'Restore failed');});
      });}

      // Admin-only audience drawer. Its markup is absent for regular org viewers.
      var viewToggle=document.getElementById('vview-toggle'),viewPanel=document.getElementById('vview-panel'),viewClose=document.getElementById('vview-close');
      function viewOpen(open){if(!viewPanel)return;if(open&&typeof fbOpen==='function')fbOpen(false);if(open&&histPanel&&histPanel.classList.contains('open'))histOpen(false);viewPanel.classList.toggle('open',open);viewPanel.setAttribute('aria-hidden',open?'false':'true');if(viewToggle)viewToggle.setAttribute('aria-expanded',open?'true':'false');}
      if(viewToggle)viewToggle.addEventListener('click',function(){viewOpen(!viewPanel.classList.contains('open'));});
      if(viewClose)viewClose.addEventListener('click',function(){viewOpen(false);});
      document.addEventListener('keydown',function(e){if(e.key==='Escape'&&viewPanel&&viewPanel.classList.contains('open'))viewOpen(false);});
    })();
  </script>
</body></html>`;
}

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
  const prev = nav.prevId ? `<a class="vnav" href="/${esc(nav.prevId)}" rel="prev">${ICONS.back}<span class="vkey">←</span></a>` : `<span class="vnav off" aria-hidden="true">${ICONS.back}</span>`;
  const next = nav.nextId ? `<a class="vnav" href="/${esc(nav.nextId)}" rel="next">${ICONS.forward}<span class="vkey">→</span></a>` : `<span class="vnav off" aria-hidden="true">${ICONS.forward}</span>`;
  const audience = viewers ? `<button type="button" id="vview-toggle" data-inspector-open="audience">Audience · ${Number(counts.views || 0)} views</button>` : "";
  const audiencePane = viewers ? `<section class="inspector-pane" id="inspector-audience" role="tabpanel" aria-labelledby="tab-audience" hidden><div class="vfb-list">${viewers.length ? viewers.map((row) => `<div class="vfb-item"><div class="vfb-m"><span>${esc(row.email)}</span><span>${Number(row.count || 0)} view${Number(row.count || 0) === 1 ? "" : "s"}</span></div><div class="vfb-b">Last seen ${esc(fmtDate(row.last_viewed_at))}</div></div>`).join("") : '<div class="vfb-empty">No audience views recorded yet.</div>'}</div></section>` : "";
  const feedbackData = jsLiteral(JSON.stringify(feedback.map((row) => ({ id: row.id, parent_id: row.parent_id, anchor_path: row.anchor_path, anchor_x: row.anchor_x, anchor_y: row.anchor_y, anchor_w: row.anchor_w, anchor_h: row.anchor_h, anchor_approx: row.anchor_approx, anchor_page: row.anchor_page, anchor_page_stale: !!row.anchor_page_stale, artifact_revision: row.artifact_revision }))));
  const attrLiteral = (value) => esc(jsLiteral(value));
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="light dark"><link rel="icon" href="${PORTAL_FAVICON}"><title>${esc(meta.title)} &middot; ${esc(APP_NAME)}</title><script>(function(){try{var t=localStorage.getItem('artifact-theme');if(t)document.documentElement.dataset.theme=t;}catch(e){}})();</script><style>${SHELL_CSS}</style></head><body>
<header class="vbar" style="--org-k:${hue};--k:color-mix(in oklab,var(--org-k) 72%,var(--txt))"><a class="vhome" href="/" aria-label="Back to ${esc(APP_NAME)}">${ICONS.home}</a><div class="vmid"><span class="vtitle">${esc(meta.title)}</span><span class="vmeta"><span class="vorg">${esc(meta.org)}</span><span>·</span><span><span class="publisher-label">Published by </span>${esc(who)}</span><span class="vtype">${meta.is_bundle ? "Bundle" : "HTML"}</span><span class="vrevision">v${Number(meta.revision || 1)}</span></span></div><nav class="vright" aria-label="Artifact controls"><div class="vgroup vreacts" aria-label="Your reaction"><button class="vreact fav" data-act="fav" type="button" aria-label="Save to favorites" aria-pressed="${reaction.favorite ? "true" : "false"}">${ICONS.heart}</button><button class="vreact up" data-act="up" type="button" aria-label="Mark as useful" aria-pressed="${reaction.vote > 0 ? "true" : "false"}">${ICONS.up}</button><button class="vreact down" data-act="down" type="button" aria-label="Mark as needing work" aria-pressed="${reaction.vote < 0 ? "true" : "false"}">${ICONS.down}</button></div><div class="vgroup" aria-label="Browse ${esc(meta.org)} artifacts">${prev}<span class="vpos"><strong>${Number(nav.index || 1)}</strong> / ${Number(nav.total || 0)}</span>${next}</div><div class="vgroup"><button class="vprimary" id="vshare-toggle" data-inspector-open="share" type="button" aria-controls="vinspector" aria-expanded="false">↗ <span>Share</span></button><button class="vprimary feedback" id="vfb-toggle" data-inspector-open="feedback" type="button" aria-controls="vinspector" aria-expanded="false">Feedback <span class="vfb-count" ${unresolved ? "" : "hidden"}>${unresolved}</span></button><button class="vmore-toggle" id="vmore-toggle" type="button" aria-expanded="false" aria-controls="vmore-menu" aria-label="More artifact actions">•••</button></div></nav><div class="vmore-menu" id="vmore-menu" aria-label="More artifact actions" hidden><button type="button" data-inspector-open="details">Details</button><button type="button" data-inspector-open="history">Version history</button>${audience}<button class="vcomment-toggle" id="vcomment-toggle" type="button" aria-pressed="false">Comment on a place</button><a href="${raw}" target="_blank" rel="noopener">Open raw artifact</a>${meta.is_bundle ? '<span class="sr-only">Bundles open in the viewer and cannot download as one HTML file.</span>' : `<a href="${raw}?download" download>Download HTML</a>`}<button id="vtheme" type="button">Change theme</button><a class="danger-link" href="/cdn-cgi/access/logout">Sign out</a>${canDelete ? '<button class="vdelete-trigger" id="vdelete-trigger" type="button">Delete artifact</button>' : ""}</div></header>
<div class="reaction-status" id="reaction-status" role="status" aria-live="polite"></div><div class="vstage" id="vstage"><iframe class="vframe" id="vframe" src="${raw}?anchor=1${versionQuery}" title="${esc(meta.title)}" sandbox="allow-scripts allow-popups allow-forms allow-modals"></iframe><div class="vanchor-overlay" id="vanchor-overlay" aria-hidden="true"></div></div>
<aside class="vinspector" id="vinspector" aria-label="Artifact inspector" aria-hidden="true" tabindex="-1"><div class="inspector-head"><h2 id="inspector-title">Artifact inspector</h2><button class="inspector-close" id="vinspector-close" type="button" aria-label="Close inspector">×</button></div><div class="inspector-tabs" role="tablist" aria-label="Artifact inspector"><button class="inspector-tab" id="tab-feedback" data-inspector-tab="feedback" type="button" role="tab" aria-selected="false" aria-controls="inspector-feedback" tabindex="-1">Feedback <span class="vfb-count" ${unresolved ? "" : "hidden"}>${unresolved}</span></button><button class="inspector-tab" id="tab-details" data-inspector-tab="details" type="button" role="tab" aria-selected="false" aria-controls="inspector-details" tabindex="-1">Details</button><button class="inspector-tab" id="tab-share" data-inspector-tab="share" type="button" role="tab" aria-selected="false" aria-controls="inspector-share" tabindex="-1">Share</button><button class="inspector-tab" id="tab-history" data-inspector-tab="history" type="button" role="tab" aria-selected="false" aria-controls="inspector-history" tabindex="-1">History</button>${viewers ? '<button class="inspector-tab" id="tab-audience" data-inspector-tab="audience" type="button" role="tab" aria-selected="false" aria-controls="inspector-audience" tabindex="-1">Audience</button>' : ""}</div><div class="inspector-body"><section class="inspector-pane" id="inspector-feedback" role="tabpanel" aria-labelledby="tab-feedback" hidden><div class="vfb-list" id="vfb-list">${feedbackHtml}</div><form class="vfb-form" id="vfb-form"><textarea id="vfb-body" placeholder="Leave feedback for the author…" maxlength="4000" aria-label="Your feedback"></textarea><div class="vfb-actions"><span class="vfb-hint" id="vfb-hint" role="status"></span><button class="vfb-send" type="submit">Send feedback</button></div></form></section><section class="inspector-pane" id="inspector-details" role="tabpanel" aria-labelledby="tab-details" hidden><div class="vdetails"><dl class="vdetails-grid"><dt>Organization</dt><dd>${esc(meta.org)}</dd><dt>Publisher</dt><dd>${esc(who)}</dd><dt>Format</dt><dd>${meta.is_bundle ? "Bundle" : "Single-page HTML"}</dd><dt>Version</dt><dd>v${Number(meta.revision || 1)}</dd><dt>Bytes</dt><dd>${Number(meta.bytes || 0)}</dd><dt>Category</dt><dd><span class="vcat-wrap"><button class="vcat" id="vcat" type="button" data-set="${meta.category ? "1" : "0"}">${meta.category ? esc(meta.category) : "Add category"}</button><form class="vcat-edit" id="vcat-edit" hidden><input id="vcat-input" type="text" maxlength="60" placeholder="Category" value="${esc(meta.category || "")}" aria-label="Artifact category"><button type="submit" class="vcat-save" aria-label="Save category">✓</button></form></span></dd></dl></div></section><section class="inspector-pane" id="inspector-share" role="tabpanel" aria-labelledby="tab-share" hidden><div class="vfb-list" id="vshare-list"><div class="vfb-empty">Loading active links…</div></div><form class="vshare-form" id="vshare-form"><label for="vshare-expiry">Link expiry</label><select id="vshare-expiry" aria-label="Link expiry"><option value="24h">24 hours</option><option value="date">Until a date</option><option value="never">No expiration</option></select><input id="vshare-date" type="date" aria-label="Share expiration date" hidden><button type="submit">Create link</button><div class="vshare-result" id="vshare-result" aria-live="polite"></div></form></section><section class="inspector-pane" id="inspector-history" role="tabpanel" aria-labelledby="tab-history" hidden><div class="vfb-list" id="vhist-list"><div class="vfb-empty">Open History to load versions.</div></div></section>${audiencePane}</div></aside>
${canDelete ? `<dialog class="delete-dialog" id="delete-dialog" aria-labelledby="delete-title" aria-describedby="delete-copy"><form method="dialog" class="delete-panel"><div class="delete-head"><div><p>Permanent action</p><h2 id="delete-title">Delete ${esc(meta.title)}?</h2></div><button class="delete-close" value="cancel" aria-label="Close delete dialog">×</button></div><div class="delete-body"><p class="delete-context">${esc(meta.org)} · /${esc(meta.id)}</p><p class="delete-copy" id="delete-copy">Deleting removes the artifact, revision history, feedback, reactions, audience history, and active public share links. This cannot be undone.</p><p class="delete-error" id="delete-error" role="status" aria-live="polite"></p></div><div class="delete-actions"><button class="delete-cancel" value="cancel" type="submit">Cancel</button><button class="delete-confirm" id="delete-confirm" type="button">Delete artifact</button></div></form></dialog>` : ""}
<div id="shell-config" hidden data-artifact-id="${attrLiteral(meta.id)}" data-prev-id="${attrLiteral(nav.prevId || "")}" data-next-id="${attrLiteral(nav.nextId || "")}" data-bundle-raw-prefix="${attrLiteral(`/raw/${meta.id}/`)}" data-version-query="${attrLiteral(versionQuery)}" data-viewer-email="${attrLiteral(identity.viewerEmail)}" data-viewer-is-admin="${identity.isAdmin ? "1" : "0"}" data-feedback="${esc(feedbackData)}" data-title="${attrLiteral(meta.title)}" data-favorite="${reaction.favorite ? "1" : "0"}" data-vote="${Number(reaction.vote || 0)}" data-is-bundle="${meta.is_bundle ? "1" : "0"}" data-revision="${Number(meta.revision || 1)}" data-bytes="${Number(meta.bytes || 0)}"></div><script>${SHELL_SCRIPT}</script></body></html>`;
}

const SCRIPT = readFileSync(new URL("../assets/portal.js", import.meta.url), "utf8");
