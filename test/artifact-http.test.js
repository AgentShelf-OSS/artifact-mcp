import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { rawArtifactHeaders, injectAnchorBridge, ANCHOR_BRIDGE_MARKER, ANCHOR_BRIDGE } from "../lib/artifact-http.js";

const ARTIFACT_CSP = "sandbox allow-scripts allow-popups allow-forms allow-modals; default-src 'none'; connect-src 'none'; script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' data: blob: https://fonts.gstatic.com; img-src 'self' data: blob:; media-src 'self' data: blob:; worker-src 'self' blob:";

function anchorBridgeHarness({
  querySelector,
  width = 1000,
  height = 1000,
  location = {
    pathname: "/raw/artifact",
    href: "https://artifact.example.test/raw/artifact",
    origin: "https://artifact.example.test"
  }
}) {
  const listeners = new Map();
  const messages = [];
  const parent = { postMessage(message) { messages.push(message); } };
  const document = {
    documentElement: { scrollWidth: width, clientWidth: width, scrollHeight: height, clientHeight: height },
    body: { scrollWidth: width, clientWidth: width, scrollHeight: height, clientHeight: height },
    querySelector,
    addEventListener(type, listener) { listeners.set(type, listener); },
    removeEventListener() {}
  };
  const window = {
    parent,
    location,
    scrollX: 0,
    scrollY: 0,
    addEventListener(type, listener) { listeners.set(type, listener); }
  };
  const source = ANCHOR_BRIDGE.replace(/^<script[^>]*>/, "").replace(/<\/script>$/, "");
  runInNewContext(source, { document, window, URL });
  return {
    messages,
    click(link) {
      let prevented = false;
      listeners.get("click")({
        target: { closest: () => link },
        preventDefault() { prevented = true; }
      });
      return prevented;
    },
    repaint(anchors) {
      listeners.get("message")({ source: parent, data: { type: "anchor:repaint", anchors } });
      return messages.at(-1);
    },
    resize() { listeners.get("resize")(); }
  };
}

test("anchor bridge injects before the real (last) </body>, not one inside a script string", () => {
  const html = '<html><body><script>var x = "</body>";</script><p>hi</p></body></html>';
  const out = injectAnchorBridge(html);
  const bridgeAt = out.indexOf(ANCHOR_BRIDGE_MARKER);
  assert.ok(bridgeAt > -1, "bridge injected");
  assert.equal(out.split(ANCHOR_BRIDGE_MARKER).length - 1, 1, "injected exactly once");
  // the script string's </body> stays ahead of the bridge; the bridge sits before the LAST </body>
  assert.ok(out.indexOf("</body>") < bridgeAt, "the earlier (in-script) </body> is untouched");
  assert.ok(bridgeAt < out.lastIndexOf("</body>"), "bridge is before the final </body>");
});

test("anchor bridge appends when there is no </body>", () => {
  const out = injectAnchorBridge("<p>no body tag here</p>");
  assert.ok(out.endsWith("</script>"));
  assert.ok(out.includes(ANCHOR_BRIDGE_MARKER));
});

test("anchor bridge handles pointer drag boxes as well as click points", () => {
  assert.match(ANCHOR_BRIDGE, /pointerdown/);
  assert.match(ANCHOR_BRIDGE, /pointermove/);
  assert.match(ANCHOR_BRIDGE, /pointerup/);
  assert.match(ANCHOR_BRIDGE, /w:bw,h:bh/);
  assert.match(ANCHOR_BRIDGE, /data-artifact-anchor-selection/);
});

test("anchor bridge brokers an external HTTPS link through its trusted parent", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  const link = {
    href: "https://admin.example.test/day-11",
    target: "",
    hasAttribute: () => false
  };

  assert.equal(bridge.click(link), true, "the sandbox must not open the link itself");
  assert.deepEqual({ ...bridge.messages.at(-1) }, {
    type: "anchor:navigate",
    href: "https://admin.example.test/day-11"
  });
});

test("anchor bridge retains its in-bundle anchor rewrite", () => {
  const bridge = anchorBridgeHarness({
    querySelector: () => null,
    location: {
      pathname: "/raw/artifact/index.html",
      href: "https://artifact.example.test/raw/artifact/index.html",
      origin: "https://artifact.example.test"
    }
  });
  const link = {
    href: "https://artifact.example.test/raw/artifact/pages/two.html?from=index",
    target: "_self",
    hasAttribute: () => false
  };

  assert.equal(bridge.click(link), false);
  assert.equal(
    link.href,
    "https://artifact.example.test/raw/artifact/pages/two.html?from=index&anchor=1"
  );
  assert.equal(bridge.messages.some((message) => message.type === "anchor:navigate"), false);
});

test("anchor bridge does not mistake another origin's raw-looking URL for an in-bundle link", () => {
  const bridge = anchorBridgeHarness({
    querySelector: () => null,
    location: {
      pathname: "/raw/artifact/index.html",
      href: "https://artifact.example.test/raw/artifact/index.html",
      origin: "https://artifact.example.test"
    }
  });
  const link = {
    href: "https://elsewhere.example.test/raw/artifact/pages/two.html",
    target: "_self",
    hasAttribute: () => false
  };

  assert.equal(bridge.click(link), true);
  assert.deepEqual({ ...bridge.messages.at(-1) }, {
    type: "anchor:navigate",
    href: "https://elsewhere.example.test/raw/artifact/pages/two.html"
  });
});

test("raw anchor-bridge golden freezes the current injected bridge bytes", () => {
  const golden = JSON.parse(readFileSync("conformance/goldens/raw.anchor-bridge.json", "utf8"));
  const body = injectAnchorBridge("<!doctype html><html><body><p>anchor me</p></body></html>");
  const expectedEtag = 'W/"' + Buffer.byteLength(body).toString(16) + "-" +
    createHash("sha1").update(body).digest("base64").replace(/=$/, "") + '"';

  assert.equal(golden.steps[2].body.data, body);
  assert.equal(golden.steps[2].headers.headers.etag, expectedEtag);
});

test("anchor bridge tracks a selector target when the document reflows", () => {
  const selector = "html:nth-child(1)>body:nth-child(2)>section:nth-child(1)";
  let rect = { left: 100, top: 200, width: 40, height: 60 };
  const bridge = anchorBridgeHarness({
    querySelector(path) {
      assert.equal(path, selector);
      return { getBoundingClientRect: () => rect };
    }
  });

  bridge.repaint([{ id: "comment-1", path: selector, x: 0.12, y: 0.23 }]);
  assert.deepEqual({ ...bridge.messages.at(-1).anchors[0] }, {
    id: "comment-1", x: 120, y: 230, lost: false
  });

  rect = { left: 500, top: 600, width: 40, height: 60 };
  bridge.resize();
  assert.deepEqual({ ...bridge.messages.at(-1).anchors[0] }, {
    id: "comment-1", x: 520, y: 630, lost: false
  });
});

test("anchor bridge reports a lost position when its selector target is missing", () => {
  const selector = "html:nth-child(1)>body:nth-child(2)>section:nth-child(2)";
  const bridge = anchorBridgeHarness({ querySelector: () => null });

  const message = bridge.repaint([
    { id: "comment-missing", path: selector, x: 0.4, y: 0.5 }
  ]);

  assert.deepEqual({ ...message.anchors[0] }, { id: "comment-missing", lost: true });
});

test("raw HTML responses keep inline content and measured presentation hosts but block other egress", () => {
  const headers = rawArtifactHeaders("text/html; charset=utf-8");

  assert.equal(headers["content-security-policy"], ARTIFACT_CSP);
  assert.doesNotMatch(headers["content-security-policy"], /allow-same-origin/);
  assert.equal(headers["content-security-policy-report-only"], undefined);
});

test("non-HTML bundle assets keep their content type but are still sandboxed", () => {
  // .svg / .xml execute scripts when navigated to directly, so the sandbox CSP is applied
  // to every content type, not just text/html. Content type itself is preserved.
  const headers = rawArtifactHeaders("image/svg+xml");

  assert.equal(headers["content-security-policy"], ARTIFACT_CSP);
  assert.doesNotMatch(headers["content-security-policy"], /allow-same-origin/);
  assert.equal(headers["content-type"], "image/svg+xml");
});

test("download responses retain sandboxing and attachment disposition", () => {
  const headers = rawArtifactHeaders("text/html; charset=utf-8", { downloadName: "report.html" });

  assert.equal(headers["content-disposition"], 'attachment; filename="report.html"');
  assert.equal(headers["content-security-policy"], ARTIFACT_CSP);
});
