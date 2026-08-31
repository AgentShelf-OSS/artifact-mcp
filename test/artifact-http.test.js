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
    body: { scrollWidth: width, clientWidth: width, scrollHeight: height, clientHeight: height, appendChild() {} },
    querySelector,
    createElement() {
      return { style: {}, setAttribute() {}, remove() {} };
    },
    addEventListener(type, listener) { listeners.set(type, listener); },
    removeEventListener() {}
  };
  const window = {
    parent,
    location,
    scrollX: 0,
    scrollY: 0,
    requestAnimationFrame(callback) { callback(); },
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
    resize() { listeners.get("resize")(); },
    pickOn() { listeners.get("message")({ source: parent, data: { type: "anchor:pick-on" } }); },
    pickOff() { listeners.get("message")({ source: parent, data: { type: "anchor:pick-off" } }); },
    messageFrom(source, data) { listeners.get("message")({ source, data }); },
    pointerDown(ev) { listeners.get("pointerdown")?.(ev); },
    pointerMove(ev) { listeners.get("pointermove")?.(ev); },
    pointerUp(ev) { listeners.get("pointerup")?.(ev); },
    pointerCancel(ev) { listeners.get("pointercancel")?.(ev); }
  };
}

function artifactElement({
  tag = "DIV",
  text = "",
  attributes = {},
  rect = { left: 100, top: 100, width: 100, height: 50 },
  parentElement = null
} = {}) {
  return {
    nodeType: 1,
    tagName: tag,
    textContent: text,
    parentElement,
    previousElementSibling: null,
    getAttribute(name) { return Object.hasOwn(attributes, name) ? attributes[name] : null; },
    hasAttribute(name) { return Object.hasOwn(attributes, name); },
    getBoundingClientRect() { return rect; }
  };
}

function pointer(target, { id = 1, x = 100, y = 100 } = {}) {
  return {
    button: 0,
    pointerId: id,
    clientX: x,
    clientY: y,
    target,
    preventDefault() {},
    stopPropagation() {}
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
  assert.match(ANCHOR_BRIDGE, /requestAnimationFrame/);
  assert.doesNotMatch(ANCHOR_BRIDGE, /Date\.now/);
  assert.equal(ANCHOR_BRIDGE.includes("\u0000"), false, "evaluated bridge contains no NUL byte");
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

// PBI-083 Focused Tests

test("PBI-083: candidate selection prefers data-artifact-node over meaningful tags", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const target = {
    nodeType: 1,
    tagName: "SPAN",
    getAttribute: (name) => name === "data-artifact-node" ? "my-node" : null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: {
      nodeType: 1,
      tagName: "P",
      getAttribute: () => null,
      getBoundingClientRect: () => ({ left: 90, top: 90, width: 70, height: 70 }),
      parentElement: null
    }
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const candidateMsg = bridge.messages.find(m => m.type === "anchor:candidate");
  assert.ok(candidateMsg, "candidate message emitted");
  assert.equal(candidateMsg.anchor.nodeId, "my-node", "data-artifact-node wins over meaningful tag");
});

test("PBI-083: candidate selection falls back to meaningful tags", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const target = {
    nodeType: 1,
    tagName: "SPAN",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: {
      nodeType: 1,
      tagName: "P",
      getAttribute: () => null,
      getBoundingClientRect: () => ({ left: 90, top: 90, width: 70, height: 70 }),
      parentElement: null
    }
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const candidateMsg = bridge.messages.find(m => m.type === "anchor:candidate");
  assert.ok(candidateMsg, "candidate message emitted");
  assert.equal(candidateMsg.anchor.kind, "element", "meaningful tag selected");
  assert.equal(candidateMsg.anchor.nodeId, null, "no data-artifact-node");
});

test("PBI-083: v2 envelope normalizes nodeId to 128 code points", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const longId = "a".repeat(200);
  const target = {
    nodeType: 1,
    tagName: "DIV",
    getAttribute: (name) => name === "data-artifact-node" ? longId : null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerUp({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const pickedMsg = bridge.messages.find(m => m.type === "anchor:picked");
  assert.ok(pickedMsg, "picked message emitted");
  assert.equal(pickedMsg.nodeId.length, 128, "nodeId capped at 128 code points");
});

test("PBI-083: v2 envelope normalizes quote to 240 code points", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const longText = "b".repeat(300);
  const target = {
    nodeType: 1,
    tagName: "P",
    textContent: longText,
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerUp({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const pickedMsg = bridge.messages.find(m => m.type === "anchor:picked");
  assert.ok(pickedMsg, "picked message emitted");
  assert.equal(pickedMsg.quote.length, 240, "quote capped at 240 code points");
});

test("PBI-083: element click emits v2 envelope with kind=element and approx=false", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const target = {
    nodeType: 1,
    tagName: "P",
    textContent: "Hello world",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 200, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerUp({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const pickedMsg = bridge.messages.find(m => m.type === "anchor:picked");
  assert.ok(pickedMsg, "picked message emitted");
  assert.equal(pickedMsg.version, 2, "version is 2");
  assert.equal(pickedMsg.kind, "element", "kind is element");
  assert.equal(pickedMsg.approx, false, "approx is false");
  assert.equal(pickedMsg.quote, "Hello world", "quote from textContent");
});

test("PBI-083: region drag emits v2 envelope with kind=region and approx=true", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const target = {
    nodeType: 1,
    tagName: "P",
    textContent: "Drag me",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 200, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 150, clientY: 150, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerUp({ pointerId: 1, clientX: 150, clientY: 150, preventDefault: () => {}, stopPropagation: () => {} });

  const pickedMsg = bridge.messages.find(m => m.type === "anchor:picked");
  assert.ok(pickedMsg, "picked message emitted");
  assert.equal(pickedMsg.version, 2, "version is 2");
  assert.equal(pickedMsg.kind, "region", "kind is region");
  assert.equal(pickedMsg.approx, true, "approx is true");
});

test("PBI-083: candidate throttling prevents duplicate messages within 16ms", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const target = {
    nodeType: 1,
    tagName: "P",
    textContent: "Throttle test",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });
  const firstCount = bridge.messages.filter(m => m.type === "anchor:candidate").length;

  bridge.pointerMove({ pointerId: 1, clientX: 101, clientY: 101, preventDefault: () => {}, stopPropagation: () => {} });
  const secondCount = bridge.messages.filter(m => m.type === "anchor:candidate").length;

  assert.equal(firstCount, 1, "first candidate emitted");
  assert.equal(secondCount, 1, "second candidate throttled (same identity within 16ms)");
});

test("PBI-083: pick-off clears preview and stops candidate emission", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const target = {
    nodeType: 1,
    tagName: "P",
    textContent: "Pick off test",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });
  const beforeCount = bridge.messages.filter(m => m.type === "anchor:candidate").length;

  bridge.pickOff();
  const cleared = bridge.messages.filter(m => m.type === "anchor:candidate");
  bridge.pointerMove({ pointerId: 1, clientX: 101, clientY: 101, preventDefault: () => {}, stopPropagation: () => {} });
  const afterCount = bridge.messages.filter(m => m.type === "anchor:candidate").length;

  assert.equal(beforeCount, 1, "candidate emitted before pick-off");
  assert.equal(cleared.length, 2, "pick-off emits one bounded clear");
  assert.equal(cleared.at(-1).anchor, null, "pick-off clears the candidate identity");
  assert.equal(afterCount, 2, "no new candidate after pick-off clear");
});

test("PBI-083: no candidate or pick events when pick mode is off", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });

  const target = {
    nodeType: 1,
    tagName: "P",
    textContent: "Off mode test",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerUp({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const candidateCount = bridge.messages.filter(m => m.type === "anchor:candidate").length;
  const pickedCount = bridge.messages.filter(m => m.type === "anchor:picked").length;

  assert.equal(candidateCount, 0, "no candidate when pick mode off");
  assert.equal(pickedCount, 0, "no pick when pick mode off");
});

test("PBI-083: excluded tags (html, body, script, style, noscript) are not candidates", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const target = {
    nodeType: 1,
    tagName: "SCRIPT",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const candidateMsg = bridge.messages.find(m => m.type === "anchor:candidate");
  assert.equal(candidateMsg, undefined, "excluded tag not a candidate");
});

test("PBI-083: zero-area targets are excluded from candidate selection", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();

  const target = {
    nodeType: 1,
    tagName: "DIV",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 0, height: 0 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const candidateMsg = bridge.messages.find(m => m.type === "anchor:candidate");
  assert.equal(candidateMsg, undefined, "zero-area target not a candidate");
});

test("PBI-083: source check rejects messages from non-parent sources", () => {
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  const fakeParent = { postMessage: () => {} };

  bridge.messageFrom(fakeParent, { type: "anchor:pick-on" });

  const target = {
    nodeType: 1,
    tagName: "P",
    textContent: "Source check",
    getAttribute: () => null,
    getBoundingClientRect: () => ({ left: 100, top: 100, width: 50, height: 50 }),
    parentElement: null
  };

  bridge.pointerDown({ button: 0, pointerId: 1, clientX: 100, clientY: 100, target, preventDefault: () => {}, stopPropagation: () => {} });
  bridge.pointerMove({ pointerId: 1, clientX: 100, clientY: 100, preventDefault: () => {}, stopPropagation: () => {} });

  const candidateCount = bridge.messages.filter(m => m.type === "anchor:candidate").length;
  assert.equal(candidateCount, 0, "no candidate from non-parent source");
});

test("PBI-083: an explicit ancestor beats a nearer meaningful element and previews on hover", () => {
  const explicit = artifactElement({
    tag: "SECTION",
    text: "  Recommended\n package  ",
    attributes: { "data-artifact-node": "recommendation" },
    rect: { left: 50, top: 100, width: 400, height: 200 }
  });
  const meaningful = artifactElement({ tag: "P", text: "nearer paragraph", parentElement: explicit });
  const target = artifactElement({ tag: "SPAN", parentElement: meaningful });
  const bridge = anchorBridgeHarness({ querySelector: () => null });

  bridge.pickOn();
  bridge.pointerMove(pointer(target));

  const candidate = bridge.messages.find((message) => message.type === "anchor:candidate").anchor;
  assert.deepEqual({
    nodeId: candidate.nodeId,
    quote: candidate.quote,
    x: candidate.x,
    y: candidate.y,
    w: candidate.w,
    h: candidate.h
  }, {
    nodeId: "recommendation",
    quote: "Recommended package",
    x: 0.05,
    y: 0.1,
    w: 0.4,
    h: 0.2
  });
});

test("PBI-083: generic fallback, quote attributes, and null quote remain bounded", () => {
  const image = artifactElement({ tag: "IMG", attributes: { alt: "  Chart\n summary  " } });
  const imageBridge = anchorBridgeHarness({ querySelector: () => null });
  imageBridge.pickOn();
  imageBridge.pointerDown(pointer(image));
  imageBridge.pointerUp(pointer(image));
  assert.equal(
    imageBridge.messages.find((message) => message.type === "anchor:picked").quote,
    "Chart summary"
  );

  const generic = artifactElement({ tag: "SPAN", text: "   " });
  const genericBridge = anchorBridgeHarness({ querySelector: () => null });
  genericBridge.pickOn();
  genericBridge.pointerDown(pointer(generic));
  genericBridge.pointerUp(pointer(generic));
  const picked = genericBridge.messages.find((message) => message.type === "anchor:picked");
  assert.equal(picked.kind, "element");
  assert.equal(picked.nodeId, null);
  assert.equal(picked.quote, null);
});

test("PBI-083: node IDs remove controls and cap by Unicode code point", () => {
  const target = artifactElement({
    tag: "DIV",
    attributes: { "data-artifact-node": "\u0000\u0085  node\u0001\u009f-" + "😀".repeat(200) }
  });
  const bridge = anchorBridgeHarness({ querySelector: () => null });
  bridge.pickOn();
  bridge.pointerDown(pointer(target));
  bridge.pointerUp(pointer(target));

  const nodeId = bridge.messages.find((message) => message.type === "anchor:picked").nodeId;
  assert.equal(/[\u0000-\u001f\u007f-\u009f]/u.test(nodeId), false);
  assert.equal(Array.from(nodeId).length, 128);
  assert.match(nodeId, /^node-/);
});

test("PBI-083: region metadata is frozen at drag start", () => {
  const attributes = { "data-artifact-node": "drag-origin" };
  let rect = { left: 100, top: 100, width: 100, height: 50 };
  const target = artifactElement({
    tag: "P",
    text: "Original quote",
    attributes,
    rect
  });
  target.getBoundingClientRect = () => rect;
  const bridge = anchorBridgeHarness({ querySelector: () => null });

  bridge.pickOn();
  bridge.pointerDown(pointer(target));
  attributes["data-artifact-node"] = "changed-after-start";
  target.textContent = "Changed quote";
  rect = { left: 100, top: 100, width: 0, height: 0 };
  bridge.pointerMove(pointer(target, { x: 110, y: 110 }));
  bridge.pointerUp(pointer(target, { x: 110, y: 110 }));

  const picked = bridge.messages.find((message) => message.type === "anchor:picked");
  assert.equal(picked.kind, "region");
  assert.equal(picked.nodeId, "drag-origin");
  assert.equal(picked.quote, "Original quote");
});

test("PBI-083: four pixels remains a click, more than four is a region, and cancel clears", () => {
  const target = artifactElement({ tag: "P", text: "threshold" });
  const clickBridge = anchorBridgeHarness({ querySelector: () => null });
  clickBridge.pickOn();
  clickBridge.pointerDown(pointer(target));
  clickBridge.pointerMove(pointer(target, { x: 104, y: 104 }));
  clickBridge.pointerUp(pointer(target, { x: 104, y: 104 }));
  assert.equal(clickBridge.messages.find((message) => message.type === "anchor:picked").kind, "element");

  const regionBridge = anchorBridgeHarness({ querySelector: () => null });
  regionBridge.pickOn();
  regionBridge.pointerDown(pointer(target));
  regionBridge.pointerMove(pointer(target, { x: 105, y: 105 }));
  regionBridge.pointerUp(pointer(target, { x: 105, y: 105 }));
  assert.equal(regionBridge.messages.find((message) => message.type === "anchor:picked").kind, "region");

  const cancelBridge = anchorBridgeHarness({ querySelector: () => null });
  cancelBridge.pickOn();
  cancelBridge.pointerDown(pointer(target));
  cancelBridge.pointerCancel(pointer(target));
  const candidates = cancelBridge.messages.filter((message) => message.type === "anchor:candidate");
  assert.equal(candidates.at(-1).anchor, null);
});
