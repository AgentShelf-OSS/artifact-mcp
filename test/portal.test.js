import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { renderArtifactShell, renderGallery } from "../lib/portal.js";

const meta = { id: "abc123", org: "acme", title: "Artifact", client_id: "owner", uploader_label: "", is_bundle: 0, revision: 3, bytes: 1, category: "" };
const nav = { prevId: null, nextId: null, index: 1, total: 1 };

function shellBrokerHarness(scriptOverride = null) {
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
    vframe: element("iframe")
  };
  elements["shell-config"].dataset = {
    artifactId: JSON.stringify("abc123"),
    prevId: JSON.stringify(""),
    nextId: JSON.stringify(""),
    bundleRawPrefix: JSON.stringify(""),
    versionQuery: JSON.stringify(""),
    favorite: "0",
    vote: "0",
    viewerEmail: JSON.stringify("viewer@acme.test"),
    viewerIsAdmin: "0",
    isBundle: "0",
    feedback: JSON.stringify("[]"),
    revision: "1",
    title: JSON.stringify("Artifact"),
    bytes: "1"
  };
  elements.vframe.contentWindow = {};
  const documentListeners = new Map();
  const windowListeners = new Map();
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
    location: { search: "" },
    addEventListener(type, listener) { windowListeners.set(type, listener); },
    open(href, target, features) { opens.push({ href, target, features, userActivation }); }
  };
  const html = renderArtifactShell(meta, nav, {}, []);
  const script = scriptOverride || [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].at(-1)[1];
  runInNewContext(script, {
    document, window, URL, URLSearchParams, JSON, Number, Array, String, Math,
    setTimeout() {}, clearTimeout() {}, fetch() {}, localStorage: {}
  });
  return {
    created,
    opens,
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
  assert.match(shell, /new URLSearchParams\(window\.location\.search\)\.get\('feedback'\)/);
  assert.match(shell, /focusFeedback\(requestedFeedback\)/);
});

test("viewer shell includes an escaped public-share inspector", () => {
  const dangerous = { ...meta, id: "abc123", title: "</script><img>" };
  const html = renderArtifactShell(dangerous, nav, {}, [], {}, { email: "member@acme.test", isAdmin: false });
  assert.match(html, /id="vshare-toggle"/);
  assert.match(html, /24 hours/);
  assert.match(html, /Until a date/);
  assert.match(html, /No expiration/);
  assert.match(html, /data-artifact-id="&quot;abc123&quot;"/);
  assert.doesNotMatch(html, /<script><\/script><img>/);
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
  assert.match(html, /pin\.page===null\|\|pin\.page===currentPage/);
  assert.match(html, /bridgeReady=false/);
  assert.match(html, /hideAllMarkers/);
  assert.match(html, /anchor_page:anchor&&anchor\.page/);
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
  assert.match(shell, /\/raw\/abc123\?anchor=1&v=deadbeefcafe/);

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

  assert.match(member, /class="artifact-grid"/);
  assert.doesNotMatch(member, /class="cat-track"/);
  assert.doesNotMatch(member, /data-ui="nav-administration"/);
  assert.equal(
    (member.match(/<button class="act icon-act visibility"[^>]*data-action="visibility"/g) || []).length,
    1,
  );
  assert.match(member, /data-id="owned123"[^>]*data-owned="1"/);
  assert.match(member, /data-id="other123"[^>]*data-owned="0"/);
  assert.doesNotMatch(member, /other@acme\.test/);
  assert.match(member, /My needs-work votes/);
  assert.match(member, /data-filter-category="Reports"/);
  assert.match(member, /data-filter-category="Dashboards"/);
  assert.ok(member.indexOf('data-id="owned123"') < member.indexOf('data-id="other123"'));
  assert.equal(
    (member.match(/<button class="act save[^"]*"[^>]*data-action="favorite"/g) || []).length,
    2,
  );
  assert.equal(
    (member.match(/<button class="act share"[^>]*data-action="share"/g) || []).length,
    2,
  );
  assert.equal(
    (member.match(/<button class="menu-action del"[^>]*data-action="delete"/g) || []).length,
    1,
  );
  assert.match(member, /HTML download unavailable for Teammate bundle/);

  const admin = renderGallery(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    [{ org: "acme", items: [teammate, owned] }],
    reactions
  );
  assert.match(admin, /data-ui="nav-administration"/);
  assert.equal(
    (admin.match(/<button class="act icon-act visibility"[^>]*data-action="visibility"/g) || []).length,
    2,
  );
  assert.equal(
    (admin.match(/<button class="menu-action del"[^>]*data-action="delete"/g) || []).length,
    2,
  );
  assert.match(admin, />Needs review <span>/);
  assert.doesNotMatch(admin, /My needs-work votes/);
});
