import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { artifactInstallName, artifactInstallShortName, renderArtifactShell, renderGallery } from "../lib/portal.js";

const SHELL_ASSET = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");

const meta = { id: "abc123", org: "acme", title: "Artifact", client_id: "owner", uploader_label: "", is_bundle: 0, revision: 3, bytes: 1, category: "" };
const nav = { prevId: null, nextId: null, index: 1, total: 1 };

function shellBrokerHarness(scriptOverride = null, { isBundle = false, revision = 3, fetchImpl = () => ({}), origin = "https://artifacts.example.test", popupBlocked = false, stateEnabled, windowExtras = {}, navigatorImpl = {}, search = "" } = {}) {
  const created = [];
  function element(tagName = "div") {
    const listeners = new Map();
    const attributes = new Map();
    const classes = new Set();
    const node = {
      tagName: tagName.toUpperCase(),
      dataset: {},
      style: {},
      children: [],
      hidden: false,
      textContent: "",
      className: "",
      classList: {
        add(...names) { names.forEach((name) => classes.add(name)); },
        remove(...names) { names.forEach((name) => classes.delete(name)); },
        contains(name) { return classes.has(name); },
        toggle(name, force) {
          const next = force === undefined ? !classes.has(name) : !!force;
          if (next) classes.add(name); else classes.delete(name);
          return next;
        }
      },
      setAttribute(name, value) { attributes.set(name, String(value)); },
      removeAttribute(name) { attributes.delete(name); },
      getAttribute(name) { return attributes.get(name) ?? null; },
      appendChild(child) { this.children.push(child); return child; },
      remove() {},
      addEventListener(type, listener) { listeners.set(type, listener); },
      querySelector() { return null; },
      querySelectorAll() { return []; },
      closest() { return null; },
      focus() {},
      trigger(type, event = {}) { listeners.get(type)?.(event); }
    };
    created.push(node);
    return node;
  }
  const body = element("body");
  const elements = {
    "shell-config": element("div"),
    "reaction-status": element("div"),
    "vfb-list": element("div"),
    "vanchor-overlay": element("div"),
    "vanchor-composer": element("section"),
    "vanchor-body": element("textarea"),
    "vanchor-save": element("button"),
    "vanchor-copy": element("button"),
    "vanchor-dismiss": element("button"),
    "vanchor-summary": element("p"),
    "vanchor-status": element("p"),
    vdiscussion: element("section"),
    "vdiscussion-state": element("span"),
    "vdiscussion-copy": element("p"),
    "vdiscussion-actions": element("div"),
    "vdiscussion-status": element("p"),
    vframe: element("iframe"),
    "vpush-toggle": element("button"),
    "vpush-banner": element("div"),
    "vpush-reason": element("p"),
    "vpush-enable": element("button"),
    "vpush-dismiss": element("button")
  };
  elements["vpush-toggle"].hidden = true;
  elements["vpush-banner"].hidden = true;
  elements["shell-config"].dataset = {
    artifactId: JSON.stringify("abc123"),
    prevId: JSON.stringify(""),
    nextId: JSON.stringify(""),
    bundleRawPrefix: JSON.stringify(""),
    versionQuery: JSON.stringify(""),
    favorite: "0",
    vote: "0",
    viewerEmail: JSON.stringify("viewer@acme.test"),
    viewerId: JSON.stringify("9f2c1e0a4b7d3c55"),
    viewerName: JSON.stringify("Viewer"),
    viewerIsAdmin: "0",
    isBundle: isBundle ? "1" : "0",
    feedback: JSON.stringify("[]"),
    revision: String(revision),
    title: JSON.stringify("Artifact"),
    bytes: "1",
    ...(stateEnabled === undefined ? {} : { stateEnabled })
  };
  const frameMessages = [];
  elements.vframe.contentWindow = { postMessage(message) { frameMessages.push(message); } };
  const documentListeners = new Map();
  const windowListeners = new Map();
  const animationFrames = [];
  const opens = [];
  let userActivation = false;
  const document = {
    body,
    documentElement: { dataset: {} },
    getElementById(id) { return elements[id] || null; },
    createElement: element,
    querySelector(selector) { return selector === ".vfb-count" ? element("span") : null; },
    querySelectorAll() { return []; },
    addEventListener(type, listener) { documentListeners.set(type, listener); }
  };
  const window = {
    location: { search, origin },
    __artifactMcpTestHooks: {},
    addEventListener(type, listener) { windowListeners.set(type, listener); },
    requestAnimationFrame(callback) { animationFrames.push(callback); return animationFrames.length; },
    open(href, target, features) { opens.push({ href, target, features, userActivation }); return popupBlocked ? null : {}; },
    ...windowExtras
  };
  const html = renderArtifactShell(meta, nav, {}, []);
  const script = scriptOverride || SHELL_ASSET;
  runInNewContext(script, {
    document, window, URL, URLSearchParams, JSON, Number, Array, String, Math,
    setTimeout() {}, clearTimeout() {}, fetch: fetchImpl, localStorage: {}, navigator: navigatorImpl
  });
  return {
    created,
    opens,
    elements,
    buildAnchorPrompt: window.__artifactMcpTestHooks.buildAnchorPrompt,
    composerPlacement: window.__artifactMcpTestHooks.composerPlacement,
    markerPreviewPlacement: window.__artifactMcpTestHooks.markerPreviewPlacement,
    draftAnchorFromSelection: window.__artifactMcpTestHooks.draftAnchorFromSelection,
    feedbackPayload: window.__artifactMcpTestHooks.feedbackPayload,
    requestRepaint: window.__artifactMcpTestHooks.requestRepaint,
    loadDiscussion: window.__artifactMcpTestHooks.loadDiscussion,
    frameMessages,
    flushAnimationFrames() { while (animationFrames.length) animationFrames.shift()(); },
    message(data) {
      windowListeners.get("message")({ source: elements.vframe.contentWindow, data });
    },
    confirm() {
      const button = created.find((node) => node.tagName === "BUTTON" && node.textContent === "Open link");
      assert.ok(button, "confirmation button is rendered");
      userActivation = true;
      button.trigger("click");
      userActivation = false;
    }
  };
}

test("feedback drawer nests one-level replies and only renders viewer management controls for allowed comments", () => {
  const feedback = [
    { id: "parent", viewer_email: "owner@acme.test", body: "Top", artifact_revision: 3, parent_id: null, created_at: "2026-07-12", anchor_x: 0.1, anchor_y: 0.2, anchor_w: 0.3, anchor_h: 0.4 },
    { id: "reply", viewer_email: "other@acme.test", body: "Reply", artifact_revision: 3, parent_id: "parent", created_at: "2026-07-12", anchor_x: null, anchor_y: null }
  ];
  const html = renderArtifactShell(meta, nav, {}, feedback, {}, { email: "owner@acme.test", isAdmin: false });
  assert.match(html, /data-thread-id="parent"/);
  assert.match(html, /anchor_w/);
  assert.match(html, /vanchor-box/);
  assert.match(html, /data-parent-id="parent"/);
  assert.ok(html.indexOf('data-id="parent"') < html.indexOf('data-id="reply"'));
  const drawer = html.slice(html.indexOf('<section class="inspector-pane" id="inspector-feedback"'), html.indexOf('<section class="inspector-pane" id="inspector-details"'));
  assert.equal((drawer.match(/data-feedback-action=/g) || []).length, 2);
  const escapedHtml = renderArtifactShell(meta, nav, {}, feedback, {}, { email: "</script><img>", isAdmin: false });
  assert.ok(escapedHtml.includes('data-viewer-email="&quot;\\u003c/script\\u003e\\u003cimg\\u003e&quot;"'));
  const adminHtml = renderArtifactShell(meta, nav, {}, feedback, {}, { email: "admin@acme.test", isAdmin: true });
  const adminDrawer = adminHtml.slice(adminHtml.indexOf('<section class="inspector-pane" id="inspector-feedback"'), adminHtml.indexOf('<section class="inspector-pane" id="inspector-details"'));
  assert.equal((adminDrawer.match(/data-feedback-action=/g) || []).length, 4);

  const discord = renderArtifactShell(meta, nav, {}, [{ id: "discord-1", viewer_email: null, author_source: "discord", external_author_display: "Díscord <reviewer>", body: "note", artifact_revision: 3, parent_id: null, anchor_x: 0.1, anchor_y: 0.2 }]);
  assert.match(discord, /author_source/);
  assert.match(discord, /external_author_display/);
  assert.match(discord, /Díscord/);
  assert.doesNotMatch(discord, /Díscord <reviewer>/);
});

test("notification rows link to a feedback deep link and the shell focuses its fid parameter", () => {
  const gallery = renderGallery(
    { email: "viewer@acme.test", org: "acme", isAdmin: false },
    [{ org: "acme", items: [] }], new Map(), new Map(), new Map(), new Map(), {},
    { unread: 1, items: [{ id: "feedback-1", artifact_id: "artifact-1", artifact_title: "Quarterly <report>", viewer_email: "author@acme.test", body: "Please review", created_at: "2026-07-14 10:00:00", unread: 1 }] }
  );
  assert.match(gallery, /href="\/artifact-1\?feedback=feedback-1"/);
  assert.match(gallery, /class="notif-count"[^>]*>1</);
  assert.match(gallery, /Quarterly &lt;report&gt;/);
  assert.doesNotMatch(gallery, /Quarterly <report>/);

  const shell = renderArtifactShell(
    { id: "artifact-1", org: "acme", title: "Report", client_id: "publisher", revision: 1, is_bundle: 0, category: "" },
    { prevId: null, nextId: null, index: 1, total: 1 }, {},
    [{ id: "feedback-1", viewer_email: "author@acme.test", body: "Please review", parent_id: null, resolved_at: null, artifact_revision: 1 }]
  );
  assert.match(SHELL_ASSET, /new URLSearchParams\(window\.location\.search\)\.get\('feedback'\)/);
  assert.match(SHELL_ASSET, /focusFeedback\(requestedFeedback\)/);
});

test("viewer shell includes an escaped public-share inspector", () => {
  const dangerous = { ...meta, id: "abc123", title: "</script><img>" };
  const html = renderArtifactShell(dangerous, nav, {}, [], {}, { email: "member@acme.test", isAdmin: false });
  assert.match(html, /id="vshare-toggle"/);
  assert.match(html, /24 hours/);
  assert.match(html, /Until a date/);
  assert.match(html, /No expiration/);
  assert.match(html, /data-artifact-id="&quot;abc123&quot;"/);
  assert.match(html, /data-cast-enabled="0"/);
  assert.doesNotMatch(html, /cast:host-init/);
  assert.doesNotMatch(html, /<script><\/script><img>/);
});

test("viewer shell gives review actions priority and keeps secondary actions in deterministic menus", () => {
  const html = renderArtifactShell(
    meta,
    { prevId: "newer", nextId: "older", index: 2, total: 3 },
    { favorite: 1, vote: -1 },
    [],
    { counts: { views: 7 }, viewers: [] },
  );
  const titleMenu = html.slice(html.indexOf('id="vtitle-menu"'), html.indexOf('id="vmore-menu"'));
  const moreMenu = html.slice(html.indexOf('id="vmore-menu"'), html.indexOf('</header>'));

  assert.match(html, /aria-label="Back to artifact library"/);
  assert.match(html, /id="vtitle-toggle"[^>]*aria-controls="vtitle-menu"/);
  assert.match(html, /id="vcomment-toggle"[^>]*aria-label="Comment on a place"/);
  assert.match(html, /id="vcomment-toggle"[^>]*><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M21 15/);
  assert.doesNotMatch(html, /id="vcomment-toggle"[^>]*>▣/);
  assert.match(html, /id="vshare-toggle"/);
  assert.match(html, /id="vmore-toggle"/);
  assert.match(html, /id="vinspector"[^>]*aria-hidden="true"[^>]*inert/);
  assert.match(SHELL_ASSET, /inspector\.removeAttribute\('inert'\)/);
  assert.match(SHELL_ASSET, /inspector\.setAttribute\('inert',''\)/);
  assert.match(titleMenu, /data-inspector-open="details"/);
  assert.match(titleMenu, /data-inspector-open="history"/);
  assert.match(titleMenu, /data-inspector-open="audience"/);
  assert.match(titleMenu, /id="vfb-toggle"[^>]*data-inspector-open="feedback"/);
  assert.match(titleMenu, /href="\/newer"[^>]*rel="prev"/);
  assert.match(titleMenu, /href="\/older"[^>]*rel="next"/);
  assert.match(titleMenu, /role="menuitemcheckbox"[^>]*aria-checked="true"/);
  assert.doesNotMatch(titleMenu, /role="menuitemcheckbox"[^>]*aria-pressed/);
  assert.doesNotMatch(moreMenu, /data-inspector-open/);
  assert.match(moreMenu, /Open raw artifact/);
  assert.match(moreMenu, /Download HTML/);
  assert.match(moreMenu, /Change theme/);
  assert.match(moreMenu, /Sign out/);
  assert.match(SHELL_ASSET, /function bindMenu/);
  assert.match(SHELL_ASSET, /e\.key==='ArrowDown'/);
  assert.match(SHELL_ASSET, /closeShellMenus/);
  assert.match(SHELL_ASSET, /input,textarea,select,\[contenteditable\],\[role="menu"\],dialog,\[role="dialog"\]/);
  assert.doesNotMatch(SHELL_ASSET, /page\.replace\(\/\<header/);
  assert.match(SHELL_ASSET, /commentMode\)\{e\.preventDefault\(\);setCommentMode\(false\)/);
});

test("delete controls render only for administrators and recorded owners", () => {
  const ownedMeta = { ...meta, owner_email: "owner@acme.test" };
  const ownerShell = renderArtifactShell(
    ownedMeta,
    nav,
    {},
    [],
    {},
    { email: "OWNER@ACME.TEST", org: "acme", isAdmin: false },
  );
  assert.match(ownerShell, /id="vdelete-trigger"/);
  assert.match(ownerShell, /id="delete-dialog"/);
  assert.match(ownerShell, /id="vdiscussion-state"/);
  assert.match(ownerShell, /id="vdiscussion-actions"/);
  assert.match(ownerShell, /data-can-manage-discussion="1"/);
  assert.doesNotMatch(ownerShell, /Open Thread/i);

  const memberShell = renderArtifactShell(
    ownedMeta,
    nav,
    {},
    [],
    {},
    { email: "member@acme.test", org: "acme", isAdmin: false },
  );
  assert.doesNotMatch(memberShell, /id="vdelete-trigger"/);
  assert.doesNotMatch(memberShell, /id="delete-dialog"/);
  assert.match(memberShell, /id="vdiscussion-state"/);
  assert.doesNotMatch(memberShell, /id="vdiscussion-actions"/);
  assert.match(memberShell, /data-can-manage-discussion="0"/);

  const adminShell = renderArtifactShell(
    { ...meta, owner_email: null },
    nav,
    {},
    [],
    {},
    { email: "admin@example.test", org: "admin", isAdmin: true },
  );
  assert.match(adminShell, /id="vdelete-trigger"/);
  assert.match(adminShell, /id="delete-dialog"/);
  assert.match(adminShell, /id="vdiscussion-actions"/);
});

test("bundle shell scopes anchors to the current page and resets bridge state on navigation", () => {
  const bundle = { ...meta, is_bundle: 1, entry: "index.html" };
  const feedback = [
    { id: "entry", parent_id: null, anchor_page: "index.html", anchor_x: 0.1, anchor_y: 0.2, artifact_revision: 3 },
    { id: "page-two", parent_id: null, anchor_page: "pages/two.html", anchor_x: 0.3, anchor_y: 0.4, artifact_revision: 3 },
    { id: "legacy", parent_id: null, anchor_page: null, anchor_x: 0.5, anchor_y: 0.6, artifact_revision: 3 }
  ];
  const html = renderArtifactShell(bundle, nav, {}, feedback);

  assert.match(html, /anchor_page/);
  assert.match(SHELL_ASSET, /pin\.page===null\|\|pin\.page===currentPage/);
  assert.match(SHELL_ASSET, /bridgeReady=false/);
  assert.match(SHELL_ASSET, /hideAllMarkers/);
  assert.match(SHELL_ASSET, /anchor_page:anchor&&anchor\.page/);
});

test("anchored-comment shell retains the v2 envelope and keeps prompt copy separate from saving", () => {
  const feedback = [{
    id: "saved-1", parent_id: null, viewer_email: "<author>", body: "<body>",
    artifact_revision: 2, anchor_path: "main > p", anchor_x: 0.1, anchor_y: 0.2,
    anchor_w: 0.3, anchor_h: 0.4, anchor_approx: 1, anchor_page: "pages/report.html",
    anchor_kind: "region", anchor_node_id: "revenue-table", anchor_quote: "Quarterly <revenue>", anchor_version: 2
  }];
  const html = renderArtifactShell({ ...meta, is_bundle: 1, revision: 3 }, nav, {}, feedback);
  assert.match(html, /id="vanchor-composer"/);
  assert.match(html, /id="vanchor-copy"[^>]*>Copy prompt/);
  assert.match(html, /id="vanchor-save"[^>]*>Add comment/);
  assert.match(html, /data-copy-prompt="saved-1"/);
  assert.match(html, /anchor_kind/);
  assert.match(html, /anchor_node_id/);
  assert.match(html, /anchor_quote/);
  assert.match(html, /anchor_version/);
  assert.match(SHELL_ASSET, /Artifact MCP review handoff/);
  assert.match(SHELL_ASSET, /list_feedback/);
  assert.match(SHELL_ASSET, /single 65,536-byte read may be incomplete/);
  assert.match(SHELL_ASSET, /__artifactMcpTestHooks/);
  assert.match(SHELL_ASSET, /pin\.stale\|\|!pinOnCurrentPage/);
  assert.match(SHELL_ASSET, /id:'__draft__'/);
});

test("bridge and fallback selections produce valid v2 feedback POST payloads", () => {
  const shell = shellBrokerHarness();
  const bridge = shell.draftAnchorFromSelection({
    version: 2, kind: "element", path: "main > p", nodeId: "target",
    quote: "anchor target", approx: false,
  }, 0.2, 0.3, 0.4, 0.1, true);
  const bridgePayload = shell.feedbackPayload("Bridge comment", null, bridge);
  assert.deepEqual(JSON.parse(JSON.stringify(bridgePayload.anchor)), {
    version: 2, kind: "element", x: 0.2, y: 0.3, page: null,
    path: "main > p", nodeId: "target", quote: "anchor target",
    approx: false, w: 0.4, h: 0.1,
  });
  assert.equal(typeof bridgePayload.anchor.approx, "boolean");

  const fallbackPoint = shell.draftAnchorFromSelection({ approx: true }, 0.4, 0.5, null, null, false);
  const fallbackPointPayload = shell.feedbackPayload("Fallback point", null, fallbackPoint);
  assert.deepEqual(JSON.parse(JSON.stringify(fallbackPointPayload.anchor)), {
    version: 2, kind: "element", x: 0.4, y: 0.5, page: null,
    path: "", approx: true,
  });

  const fallbackRegion = shell.draftAnchorFromSelection({ kind: "region", path: "", approx: true }, 0.1, 0.15, 0.2, 0.25, true);
  const fallbackRegionPayload = shell.feedbackPayload("Fallback region", null, fallbackRegion);
  assert.deepEqual(JSON.parse(JSON.stringify(fallbackRegionPayload.anchor)), {
    version: 2, kind: "region", x: 0.1, y: 0.15, page: null,
    path: "", approx: true, w: 0.2, h: 0.25,
  });
});

test("anchored prompt matrix is deterministic across draft/saved, single/bundle, semantic/legacy, and stale", () => {
  const cases = [
    { draft: true, isBundle: false, semantic: true, stale: false },
    { draft: false, isBundle: false, semantic: false, stale: false },
    { draft: true, isBundle: true, semantic: true, stale: false },
    { draft: false, isBundle: true, semantic: false, stale: true },
  ];
  for (const item of cases) {
    const shell = shellBrokerHarness(null, { isBundle: item.isBundle, revision: 7 });
    assert.equal(typeof shell.buildAnchorPrompt, "function");
    const anchor = {
      id: "feedback-9", artifact_revision: item.stale ? 6 : 7,
      anchor_page: item.isBundle ? "docs/report.html" : null,
      anchor_path: "main > p:nth-child(2)", anchor_x: 0.125, anchor_y: 0.25,
      anchor_w: 0.5, anchor_h: 0.125, anchor_approx: true,
      anchor_kind: item.semantic ? "region" : null,
      anchor_node_id: item.semantic ? "revenue-α" : null,
      anchor_quote: item.semantic ? "Quarterly <revenue>" : null,
      anchor_version: item.semantic ? 2 : 1,
      anchor_page_stale: item.stale, body: "Verbatim <saved> body"
    };
    const prompt = shell.buildAnchorPrompt({ anchor, draft: item.draft, body: item.draft ? "Verbatim <draft> body" : anchor.body, artifactId: "abc123", currentRevision: 7, isBundle: item.isBundle });
    assert.match(prompt, new RegExp(`State: ${item.draft ? "draft" : "saved"}`));
    assert.match(prompt, /Connector: Artifact MCP/);
    assert.match(prompt, /Artifact ID: abc123/);
    assert.match(prompt, new RegExp(`Canonical artifact revision: ${item.stale ? 6 : 7}`));
    assert.match(prompt, new RegExp(`Anchor version: ${item.semantic ? 2 : 1}`));
    assert.match(prompt, /Normalized bounds: x=0.125 y=0.25 w=0.5 h=0.125/);
    assert.match(prompt, item.semantic ? /Node ID: revenue-α/ : /Node ID: \(none\)/);
    assert.match(prompt, item.isBundle ? /Bundle file path: \/files\/docs\/report.html/ : /Bundle file path: \(none\)/);
    assert.match(prompt, new RegExp(`artifact://abc123/revisions/${item.stale ? 6 : 7}${item.isBundle ? "/files/docs/report.html" : ""}`));
    assert.match(prompt, /single 65,536-byte read may be incomplete/);
    if (item.draft) assert.match(prompt, /Feedback ID: \(draft; not persisted yet\)/);
    else assert.match(prompt, /Feedback ID: feedback-9/);
    if (item.stale) assert.match(prompt, /Do not switch this stale feedback to the latest revision/);
    if (!item.draft) assert.match(prompt, /Call Artifact MCP list_feedback/);
  }
});

test("composer placement chooses a side and clamps inside each desktop viewport", () => {
  const shell = shellBrokerHarness();
  for (const viewport of [{ width: 1440, height: 900 }, { width: 768, height: 1024 }]) {
    const right = shell.composerPlacement({ x: 120, y: 200, w: 20, h: 20 }, viewport, { width: 400, height: 330 });
    const left = shell.composerPlacement({ x: viewport.width - 30, y: viewport.height - 10, w: 20, h: 20 }, viewport, { width: 400, height: 330 });
    assert.equal(right.side, "right");
    assert.equal(left.side, "left");
    for (const placement of [right, left]) {
      assert.ok(placement.left >= 8 && placement.left <= viewport.width - 408);
      assert.ok(placement.top >= 8 && placement.top <= viewport.height - 338);
    }
  }
});

test("anchor repaint requests coalesce into one frame", () => {
  const shell = shellBrokerHarness();
  shell.requestRepaint(); shell.requestRepaint(); shell.requestRepaint();
  assert.equal(shell.frameMessages.filter((message) => message.type === "anchor:repaint").length, 0);
  shell.flushAnimationFrames();
  assert.equal(shell.frameMessages.filter((message) => message.type === "anchor:repaint").length, 1);
  shell.requestRepaint(); shell.flushAnimationFrames();
  assert.equal(shell.frameMessages.filter((message) => message.type === "anchor:repaint").length, 2);
});

test("early state handshake accepts only the artifact frame and drains once", () => {
  const frameWindow = {}, listeners = new Map();
  const window = {
    addEventListener(type, listener) { listeners.set(type, listener); },
    removeEventListener(type, listener) { if (listeners.get(type) === listener) listeners.delete(type); }
  };
  runInNewContext(readFileSync(new URL("../assets/viewer-boot.js", import.meta.url), "utf8"), {
    window, document: { getElementById: () => ({ contentWindow: frameWindow }) }, Array
  });
  const receive = listeners.get("message"), boot = window.__artifactMcpStateBoot;
  receive({ source: {}, data: { type: "state:hello" } });
  receive({ source: frameWindow, data: { type: "state:set", key: "note", value: "ignored" } });
  receive({ source: frameWindow, data: ["state:hello"] });
  receive({ source: frameWindow, data: { type: "state:hello", extra: "discarded" } });
  assert.deepEqual({ ...boot.take() }, { type: "state:hello" });
  assert.equal(boot.take(), null);
  assert.equal(listeners.has("message"), false);
  assert.equal(window.__artifactMcpStateBoot, undefined);
});

test("discussion status loads on demand and caches a successful response", async () => {
  let calls = 0, resolve;
  const shell = shellBrokerHarness(null, { fetchImpl: () => { calls += 1; return new Promise((done) => { resolve = done; }); } });
  shell.loadDiscussion(); shell.loadDiscussion();
  assert.equal(calls, 1);
  resolve({ ok: true, json: async () => ({ state: "local", overrideMode: "inherit" }) });
  await Promise.resolve(); await Promise.resolve();
  shell.loadDiscussion();
  assert.equal(calls, 1);
});

test("marker preview placement clamps horizontally and chooses above or below", () => {
  const shell = shellBrokerHarness();
  const stage = { width: 768, height: 1024 };
  const nearTop = shell.markerPreviewPlacement({ x: 4, y: 5 }, stage, { width: 208, height: 54 });
  const nearBottom = shell.markerPreviewPlacement({ x: 760, y: 900 }, stage, { width: 208, height: 54 });
  assert.equal(nearTop.vertical, "below");
  assert.equal(nearBottom.vertical, "above");
  assert.ok(nearTop.left >= 8 - 4);
  assert.ok(nearBottom.left <= 768 - 208 - 8 - 760);
  assert.ok(nearTop.top + 5 >= 8);
  assert.ok(nearBottom.top + 900 + 54 <= stage.height - 8);
  const html = renderArtifactShell(meta, nav, {}, []);
  assert.match(SHELL_ASSET, /beforeunload/);
  assert.match(SHELL_ASSET, /This discards the current draft comment/);
  assert.match(SHELL_ASSET, /draftAnchor=null;showDraftPosition\(0,0,0,0,true\);appendFeedback\(saved\)/);
});

test("shell brokers an iframe outbound link only after an explicit confirm click", () => {
  const shell = shellBrokerHarness();

  shell.message({ type: "anchor:navigate", href: "https://admin.example.test/day-11" });
  assert.deepEqual(shell.opens, [], "the iframe message never auto-opens a popup");
  assert.equal(
    shell.created.find((node) => node.tagName === "STRONG")?.textContent,
    "admin.example.test",
    "the confirmation names the parsed destination host"
  );

  shell.confirm();
  assert.deepEqual(shell.opens, [{
    href: "https://admin.example.test/day-11",
    target: "_blank",
    features: "noopener",
    userActivation: true
  }]);
});

test("shell opens same-origin artifact links directly without the external-link confirmation", () => {
  const shell = shellBrokerHarness();

  shell.message({ type: "anchor:navigate", href: "https://artifacts.example.test/c4byka0s4fey" });
  assert.equal(shell.created.some((node) => node.tagName === "ASIDE"), false, "no confirmation for a sibling artifact");
  assert.deepEqual(shell.opens, [{
    href: "https://artifacts.example.test/c4byka0s4fey",
    target: "_blank",
    features: undefined,
    userActivation: false
  }]);
});

test("shell falls back to the confirmation when a same-origin popup is blocked", () => {
  const shell = shellBrokerHarness(null, { popupBlocked: true });

  shell.message({ type: "anchor:navigate", href: "https://artifacts.example.test/c4byka0s4fey" });
  assert.equal(shell.created.find((node) => node.tagName === "STRONG")?.textContent, "artifacts.example.test");
  shell.confirm();
  assert.equal(shell.opens.at(-1).features, "noopener");
});

test("shell still confirms links to a different origin on the same site", () => {
  const shell = shellBrokerHarness();

  shell.message({ type: "anchor:navigate", href: "http://artifacts.example.test/c4byka0s4fey" });
  shell.message({ type: "anchor:navigate", href: "https://artifacts.example.test:8443/c4byka0s4fey" });
  assert.deepEqual(shell.opens, []);
});

test("shell rejects non-http(s) outbound hrefs before rendering a confirmation", () => {
  const shell = shellBrokerHarness();

  for (const href of [
    "javascript:alert(document.domain)",
    "data:text/html,<script>alert(1)</script>",
    "blob:https://admin.example.test/opaque",
    "file:///etc/passwd"
  ]) {
    shell.message({ type: "anchor:navigate", href });
  }

  assert.equal(shell.created.some((node) => node.tagName === "ASIDE"), false);
  assert.deepEqual(shell.opens, []);
});

test("both shell twins broker outbound links with the same trusted-context behavior", () => {
  const shellScripts = [
    null,
    readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8")
  ];

  for (const script of shellScripts) {
    const shell = shellBrokerHarness(script);
    shell.message({ type: "anchor:navigate", href: "https://admin.example.test/day-11" });
    shell.confirm();
    assert.deepEqual(shell.opens, [{
      href: "https://admin.example.test/day-11",
      target: "_blank",
      features: "noopener",
      userActivation: true
    }]);
  }
});

test("gallery cards use static digest-addressed images while the viewer iframe stays live", () => {
  const sha = "deadbeefcafebabe00112233445566778899aabbccddeeff0011223344556677";
  const item = { id: "abc123", org: "acme", title: "Artifact", client_id: "owner", uploader_label: "", is_bundle: 0, revision: 5, body_sha256: sha, bytes: 1, category: "" };
  const gallery = renderGallery({ email: "v@acme.test", org: "acme", isAdmin: false }, [{ org: "acme", items: [item] }]);
  assert.match(gallery, /<img class="pv" src="\/thumbnails\/abc123\?v=deadbeefcafebabe00112233445566778899aabbccddeeff0011223344556677" loading="lazy" decoding="async" width="1200" height="750"/);
  assert.doesNotMatch(gallery, /<iframe class="pv"/);
  assert.doesNotMatch(gallery, /\?preview/);

  // digest, not revision, drives the token when body_sha256 is present
  const shell = renderArtifactShell({ ...item }, nav, {}, []);
  assert.match(shell, /\/raw\/abc123\?anchor=1&reader=1&v=deadbeefcafe/);
  const castShell = renderArtifactShell({ ...item }, nav, {}, [], {}, {}, null, true);
  assert.match(castShell, new RegExp(`/raw/abc123\\?anchor=1&reader=1&cast-pin=5\\.${sha}`));

  // a changed body digest changes the token (cache is actually busted)
  const nextSha = "000000000000111122223333444455556666777788889999aaaabbbbccccdddd";
  const gallery2 = renderGallery({ email: "v@acme.test", org: "acme", isAdmin: false }, [{ org: "acme", items: [{ ...item, body_sha256: nextSha }] }]);
  assert.ok(gallery2.includes(`?v=${nextSha}`));
  assert.doesNotMatch(gallery2, /v=deadbeef/);

  // Missing legacy digests use the no-store placeholder route rather than a live iframe.
  const noDigest = renderGallery({ email: "v@acme.test", org: "acme", isAdmin: false }, [{ org: "acme", items: [{ ...item, body_sha256: null }] }]);
  assert.match(noDigest, /src="\/thumbnails\/abc123"/);
  assert.doesNotMatch(noDigest, /<iframe class="pv"/);

  const bundle = renderGallery({ email: "v@acme.test", org: "acme", isAdmin: false }, [{ org: "acme", items: [{ ...item, is_bundle: 1 }] }]);
  assert.match(bundle, /src="\/thumbnails\/abc123\?v=/);
  assert.match(bundle, />Bundle<\/span>/);
  assert.doesNotMatch(bundle, /<iframe class="pv"/);
});

test("gallery renders a flat role-aware collection and owner-scoped eyes", () => {
  const owned = {
    ...meta,
    id: "owned123",
    title: "Owned hidden upload",
    category: "Reports",
    hidden: 1,
    is_owned_by_viewer: true,
    owner_email: "viewer@acme.test",
    created_at: "2026-07-01 00:00:00",
    updated_at: "2026-07-03 00:00:00",
  };
  const teammate = {
    ...meta,
    id: "other123",
    title: "Teammate bundle",
    category: "Dashboards",
    is_bundle: 1,
    is_owned_by_viewer: false,
    owner_email: "other@acme.test",
    created_at: "2026-07-01 00:00:00",
    updated_at: "2026-07-02 00:00:00",
  };
  const reactions = new Map([["owned123", { favorite: 1, vote: -1 }]]);
  const member = renderGallery(
    { email: "viewer@acme.test", org: "acme", isAdmin: false },
    [{ org: "acme", items: [teammate, owned] }],
    reactions
  );

  const memberCards = member.slice(member.indexOf('<main id="stage">'), member.indexOf("</main>"));
  assert.match(member, /class="artifact-grid"/);
  assert.doesNotMatch(member, /class="cat-track"/);
  assert.doesNotMatch(member, /data-ui="nav-administration"/);
  assert.equal(
    (memberCards.match(/<button class="act icon-act visibility"[^>]*data-action="visibility"/g) || []).length,
    1,
  );
  assert.match(member, /data-id="owned123"[^>]*data-owned="1"/);
  assert.match(member, /data-id="other123"[^>]*data-owned="0"/);
  assert.doesNotMatch(member, /other@acme\.test/);
  assert.match(member, /My needs-work votes/);
  assert.match(member, /id="org-filter"[^>]*aria-label="Filter by organization"/);
  assert.match(member, /id="category-filter"[^>]*aria-label="Filter by category"/);
  assert.match(member, /<option value="Reports">Reports \(1\)<\/option>/);
  assert.match(member, /<option value="Dashboards">Dashboards \(1\)<\/option>/);
  assert.match(member, /Find every published artifact\./);
  assert.doesNotMatch(member, /data-filter-category=/);
  assert.ok(member.indexOf('data-id="owned123"') < member.indexOf('data-id="other123"'));
  assert.equal(
    (memberCards.match(/<button class="act save[^"]*"[^>]*data-action="favorite"/g) || []).length,
    2,
  );
  assert.equal(
    (memberCards.match(/<button class="act share"[^>]*data-action="share"/g) || []).length,
    2,
  );
  assert.equal(
    (memberCards.match(/<button class="menu-action del"[^>]*data-action="delete"/g) || []).length,
    1,
  );
  assert.match(member, /HTML download unavailable for Teammate bundle/);

  const admin = renderGallery(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    [{ org: "acme", items: [teammate, owned] }],
    reactions
  );
  const adminCards = admin.slice(admin.indexOf('<main id="stage">'), admin.indexOf("</main>"));
  assert.match(admin, /data-ui="nav-administration"/);
  assert.equal(
    (adminCards.match(/<button class="act icon-act visibility"[^>]*data-action="visibility"/g) || []).length,
    2,
  );
  assert.equal(
    (adminCards.match(/<button class="menu-action del"[^>]*data-action="delete"/g) || []).length,
    2,
  );
  assert.match(admin, />Needs review <span>/);
  assert.doesNotMatch(admin, /My needs-work votes/);
});

test("viewer shell links the web app manifest and touch icon and renders a hidden bell and prompt region", () => {
  const html = renderArtifactShell(meta, nav, {}, [], {}, { email: "viewer@acme.test" }, null, true);
  assert.match(html, /<link rel="manifest" href="\/abc123\/manifest\.webmanifest" crossorigin="use-credentials">/);
  assert.match(html, /<meta name="apple-mobile-web-app-title" content="Artifact">/);
  assert.doesNotMatch(html, /href="\/manifest\.webmanifest"/);
  assert.match(html, /<link rel="apple-touch-icon" href="\/icons\/apple-touch-icon\.png">/);
  assert.match(html, /<meta name="apple-mobile-web-app-capable" content="yes">/);
  assert.match(html, /<meta name="mobile-web-app-capable" content="yes">/);
  assert.match(html, /<button class="vpush-toggle" id="vpush-toggle" type="button" title="Notify me" aria-label="Notify me" aria-pressed="false" data-state="off" hidden>/);
  assert.match(html, /<div class="vpush-banner" id="vpush-banner" role="region" aria-label="Artifact notifications" hidden>/);
  assert.match(html, /<button class="vpush-enable" id="vpush-enable" type="button">Turn on notifications<\/button>/);
  assert.match(html, /id="vpush-dismiss" type="button" aria-label="Dismiss notification prompt"/);
  const template = readFileSync(new URL("../templates/artifact-shell.html", import.meta.url), "utf8");
  for (const fragment of ['<link rel="manifest" href="/{{ artifact_id }}/manifest.webmanifest" crossorigin="use-credentials">', '<meta name="apple-mobile-web-app-title" content="{{ install_name }}">', '<link rel="apple-touch-icon" href="/icons/apple-touch-icon.png">', 'id="vpush-toggle"', 'id="vpush-banner"', 'id="vpush-reason"', 'id="vpush-enable"', 'id="vpush-dismiss"']) {
    assert.ok(template.includes(fragment), `Rust template keeps parity: ${fragment}`);
  }
});

test("viewer shell names the per-artifact install after the escaped, bounded title", () => {
  const html = renderArtifactShell({ ...meta, title: ' \u0007Q3 <"Board"> & Rock\'n roll plan with a title that runs well past the sixty code point limit ' }, nav, {}, [], {}, { email: "viewer@acme.test" });
  assert.match(html, /<meta name="apple-mobile-web-app-title" content="Q3 &lt;&quot;Board&quot;&gt; &amp; Rock&#39;n roll plan with a title that runs well">/);
});

test("artifact install names strip controls, trim, bound code points, and fall back", () => {
  assert.equal(artifactInstallName("  Plain title  "), "Plain title");
  assert.equal(artifactInstallName("\u0000Tab\tand\u009fC1\u007f"), "TabandC1");
  assert.equal(artifactInstallName("x".repeat(80)), "x".repeat(60));
  assert.equal(artifactInstallName(" \u0001\n\u3000 "), "Artifact");
  assert.equal(artifactInstallName(null), "Artifact");
  assert.equal(artifactInstallShortName("Quarterly planning board"), "Quarterly pl");
  assert.equal(artifactInstallShortName("Daily  standup"), "Daily  stand");
  assert.equal(artifactInstallShortName("Weekly notes"), "Weekly notes");
  assert.equal(artifactInstallShortName("Ten chars  x"), "Ten chars  x");
  assert.equal(artifactInstallShortName("Ten chars   tail"), "Ten chars");
  assert.equal(artifactInstallShortName("\u{1F600}".repeat(20)), "\u{1F600}".repeat(12));
  assert.equal(artifactInstallShortName(""), "Artifact");
});

test("the library links the site manifest and the generic Home Screen title", () => {
  const gallery = renderGallery({ email: "v@acme.test", org: "acme", isAdmin: false }, []);
  assert.match(gallery, /<link rel="manifest" href="\/manifest\.webmanifest" crossorigin="use-credentials">/);
  assert.match(gallery, /<meta name="apple-mobile-web-app-title" content="Artifacts">/);
  const template = readFileSync(new URL("../templates/gallery.html", import.meta.url), "utf8");
  for (const fragment of ['<link rel="manifest" href="/manifest.webmanifest" crossorigin="use-credentials">', '<meta name="apple-mobile-web-app-title" content="Artifacts">']) {
    assert.ok(template.includes(fragment), `Rust gallery template keeps parity: ${fragment}`);
  }
});

test("manifest and service worker assets match the Web Push contract", () => {
  const manifest = JSON.parse(readFileSync(new URL("../assets/manifest.webmanifest", import.meta.url), "utf8"));
  assert.equal(manifest.start_url, "/");
  assert.equal(manifest.scope, "/");
  assert.equal(manifest.display, "standalone");
  assert.deepEqual(manifest.icons.map((icon) => [icon.src, icon.sizes, icon.purpose]), [
    ["/icons/app-192.png", "192x192", "any"], ["/icons/app-512.png", "512x512", "any"], ["/icons/maskable-512.png", "512x512", "maskable"]
  ]);
  for (const name of ["app-192.png", "app-512.png", "maskable-512.png", "apple-touch-icon.png", "badge-72.png"]) {
    const bytes = readFileSync(new URL(`../assets/icons/${name}`, import.meta.url));
    assert.equal(bytes.subarray(1, 4).toString("latin1"), "PNG", name);
  }
  const worker = readFileSync(new URL("../assets/push-sw.js", import.meta.url), "utf8");
  for (const event of ["push", "notificationclick", "pushsubscriptionchange"]) assert.match(worker, new RegExp(`addEventListener\\("${event}"`));
  assert.doesNotMatch(worker, /addEventListener\("(fetch|install)"|importScripts\(|caches\./);
  assert.match(worker, /"x-artifact-mutation": "1"/);
});

function reminderShell({ supported = true, permission = "default", config = { enabled: true, vapid_public_key: "BAAA" }, optedIn = false } = {}) {
  const requests = [];
  const respond = (body) => Promise.resolve({ status: 200, ok: true, json: async () => body });
  const windowExtras = {
    fetch(url) {
      requests.push(String(url));
      if (url === "/push/config") return respond(config);
      if (url === "/abc123/push") return respond({ enabled: true, opted_in: optedIn, devices: 0 });
      return respond({});
    },
    ...(supported ? { PushManager: function PushManager() {}, Notification: { permission } } : {})
  };
  const h = shellBrokerHarness(null, { stateEnabled: "1", windowExtras, navigatorImpl: supported ? { serviceWorker: {}, userAgent: "Mozilla/5.0 (X11; Linux x86_64)" } : { userAgent: "" } });
  return { ...h, requests };
}
const flushShell = async () => { for (let i = 0; i < 40; i++) await Promise.resolve(); };

test("viewer shell reveals the bell only when the server and browser support push", async () => {
  const on = reminderShell({ optedIn: true, permission: "granted" });
  await flushShell();
  assert.deepEqual(on.requests, ["/push/config", "/abc123/push"]);
  const bell = on.elements["vpush-toggle"];
  assert.equal(bell.hidden, false);
  assert.equal(bell.getAttribute("aria-label"), "Notifications on");
  assert.equal(bell.getAttribute("aria-pressed"), "true");
  assert.equal(bell.dataset.state, "on");

  const blocked = reminderShell({ permission: "denied" });
  await flushShell();
  assert.equal(blocked.elements["vpush-toggle"].getAttribute("aria-label"), "Blocked in browser settings");

  const off = reminderShell({ config: { enabled: false, vapid_public_key: null } });
  await flushShell();
  assert.equal(off.elements["vpush-toggle"].hidden, true);

  const unsupported = reminderShell({ supported: false });
  await flushShell();
  assert.equal(unsupported.elements["vpush-toggle"].hidden, true);
});

test("reminder:prompt renders artifact text as plain text in the shell banner and dismisses", async () => {
  const h = reminderShell();
  await flushShell();
  const hostile = '<img src=x onerror="parent.pwned=1"><b>Turn on</b>';
  h.message({ type: "reminder:prompt", reason: hostile });
  await flushShell();
  const banner = h.elements["vpush-banner"], reason = h.elements["vpush-reason"];
  assert.equal(banner.hidden, false);
  assert.equal(reason.textContent, hostile);
  assert.equal(reason.innerHTML, undefined, "the shell never assigns artifact text as HTML");
  assert.equal(h.elements["vpush-enable"].hidden, false);
  h.elements["vpush-dismiss"].trigger("click");
  assert.equal(banner.hidden, true);
  h.message({ type: "reminder:prompt", reason: "again" });
  await flushShell();
  assert.equal(banner.hidden, true, "a dismissed prompt stays dismissed for this page view");
});

test("viewer shell routes reminder:hello through the reminder broker", async () => {
  const h = reminderShell();
  await flushShell();
  h.message({ type: "reminder:hello" });
  await flushShell();
  assert.deepEqual(JSON.parse(JSON.stringify(h.frameMessages.filter((m) => String(m.type).startsWith("reminder:")))), [
    { type: "reminder:ready", enabled: true, optedIn: false, permission: "default", needsInstall: false }
  ]);
});

test("historical viewer shells never load push configuration", async () => {
  const requests = [];
  const h = shellBrokerHarness(null, { stateEnabled: "1", windowExtras: { fetch(url) { requests.push(url); return Promise.resolve({ ok: true, status: 200, json: async () => ({ enabled: true, vapid_public_key: "BAAA" }) }); }, PushManager() {}, Notification: { permission: "default" } }, navigatorImpl: { serviceWorker: {} }, search: "?v=2" });
  await flushShell();
  h.message({ type: "reminder:hello" });
  await flushShell();
  assert.deepEqual(requests.filter((url) => String(url).includes("push")), []);
  assert.equal(h.frameMessages.find((m) => m.type === "reminder:ready").enabled, false);
});
