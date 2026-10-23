import { test, assert, readText, readBytes, runInNewContext } from "./harness.js";

// Browser-side contracts of the viewer shell assets and the page templates. Server-rendered page
// markup is covered by the Rust render and route suites.
const SHELL_ASSET = await readText("assets/shell.js");

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

test("the shell focuses the feedback named by its fid deep-link parameter", () => {
  assert.match(SHELL_ASSET, /new URLSearchParams\(window\.location\.search\)\.get\('feedback'\)/);
  assert.match(SHELL_ASSET, /focusFeedback\(requestedFeedback\)/);
});

test("viewer shell menus keep the inspector inert when closed and support keyboard navigation", () => {
  assert.match(SHELL_ASSET, /inspector\.removeAttribute\('inert'\)/);
  assert.match(SHELL_ASSET, /inspector\.setAttribute\('inert',''\)/);
  assert.match(SHELL_ASSET, /function bindMenu/);
  assert.match(SHELL_ASSET, /e\.key==='ArrowDown'/);
  assert.match(SHELL_ASSET, /closeShellMenus/);
  assert.match(SHELL_ASSET, /input,textarea,select,\[contenteditable\],\[role="menu"\],dialog,\[role="dialog"\]/);
  assert.doesNotMatch(SHELL_ASSET, /page\.replace\(\/\<header/);
  assert.match(SHELL_ASSET, /commentMode\)\{e\.preventDefault\(\);setCommentMode\(false\)/);
});

test("bundle shell scopes anchors to the current page and resets bridge state on navigation", () => {
  assert.match(SHELL_ASSET, /pin\.page===null\|\|pin\.page===currentPage/);
  assert.match(SHELL_ASSET, /bridgeReady=false/);
  assert.match(SHELL_ASSET, /hideAllMarkers/);
  assert.match(SHELL_ASSET, /anchor_page:anchor&&anchor\.page/);
});

test("anchored-comment shell retains the v2 envelope and keeps prompt copy separate from saving", () => {
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

test("early state handshake accepts only the artifact frame and drains once", async () => {
  const frameWindow = {}, listeners = new Map();
  const window = {
    addEventListener(type, listener) { listeners.set(type, listener); },
    removeEventListener(type, listener) { if (listeners.get(type) === listener) listeners.delete(type); }
  };
  runInNewContext(await readText("assets/viewer-boot.js"), {
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

test("viewer shell template links the web app manifest and touch icon and carries the bell and prompt region", async () => {
  const template = await readText("templates/artifact-shell.html");
  for (const fragment of ['<link rel="manifest" href="/{{ artifact_id }}/manifest.webmanifest" crossorigin="use-credentials">', '<meta name="apple-mobile-web-app-title" content="{{ install_name }}">', '<link rel="apple-touch-icon" href="/icons/apple-touch-icon.png">', 'id="vpush-toggle"', 'id="vpush-banner"', 'id="vpush-reason"', 'id="vpush-enable"', 'id="vpush-dismiss"']) {
    assert.ok(template.includes(fragment), `viewer shell template keeps ${fragment}`);
  }
});

test("the library template links the site manifest and the generic Home Screen title", async () => {
  const template = await readText("templates/gallery.html");
  for (const fragment of ['<link rel="manifest" href="/manifest.webmanifest" crossorigin="use-credentials">', '<meta name="apple-mobile-web-app-title" content="Artifacts">']) {
    assert.ok(template.includes(fragment), `gallery template keeps ${fragment}`);
  }
});

test("manifest and service worker assets match the Web Push contract", async () => {
  const manifest = JSON.parse(await readText("assets/manifest.webmanifest"));
  assert.equal(manifest.start_url, "/");
  assert.equal(manifest.scope, "/");
  assert.equal(manifest.display, "standalone");
  assert.deepEqual(manifest.icons.map((icon) => [icon.src, icon.sizes, icon.purpose]), [
    ["/icons/app-192.png", "192x192", "any"], ["/icons/app-512.png", "512x512", "any"], ["/icons/maskable-512.png", "512x512", "maskable"]
  ]);
  for (const name of ["app-192.png", "app-512.png", "maskable-512.png", "apple-touch-icon.png", "badge-72.png"]) {
    const bytes = await readBytes(`assets/icons/${name}`);
    assert.equal(String.fromCharCode(...bytes.subarray(1, 4)), "PNG", name);
  }
  const worker = await readText("assets/push-sw.js");
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
