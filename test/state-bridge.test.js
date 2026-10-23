import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

// The state bridge example in GETTING_STARTED.md is browser code that artifact authors copy.
// Exact-byte raw, bundle and public-share delivery is covered by the conformance `raw.*` and
// `share.public-delivery` cases.

function documentedExample() {
  const guide = readFileSync(new URL("../GETTING_STARTED.md", import.meta.url), "utf8");
  const section = guide.split("## Persisting state from an artifact")[1];
  const source = section.match(/<script>([\s\S]*?)<\/script>/)[1];
  const note = { value: "" }, status = { textContent: "Notes are temporary on this page." };
  const messages = [];
  let receive, timeout;
  const parent = { postMessage(message, origin) {
    assert.equal(origin, "*"); messages.push(JSON.parse(JSON.stringify(message)));
  } };
  vm.runInNewContext(source, {
    parent,
    document: { querySelector: selector => selector === "#note" ? note : status },
    addEventListener(type, handler) { assert.equal(type, "message"); receive = handler; },
    setTimeout(callback, delay) { assert.equal(delay, 1000); timeout = callback; },
  });
  assert.equal(typeof timeout, "function", "the example must set the fallback timeout");
  return { note, status, messages, timeout,
    receive: (data, source = parent) => receive({ data, source }),
    input(value) { note.value = value; note.oninput(); },
  };
}

test("the documented example stays temporary after one second without a shell, including a late ready", () => {
  const example = documentedExample();
  example.input("keep this note");
  example.timeout();
  example.receive({ type: "state:ready", enabled: true });
  example.input("still temporary");
  assert.deepEqual(example.messages, [{ type: "state:hello" }]);
  assert.equal(example.note.value, "still temporary");
  assert.equal(example.status.textContent, "Notes are temporary on this page.");
});

test("the documented example reads and saves when the shell answers before the timeout", () => {
  const example = documentedExample();
  example.receive({ type: "state:ready", enabled: true });
  assert.deepEqual(example.messages.at(-1), { type: "state:get", key: "note" });
  example.receive({ type: "state:value", key: "note", value: "shared", revision: 2 });
  assert.equal(example.note.value, "shared");
  example.timeout();
  example.input("edited");
  assert.deepEqual(example.messages.at(-1), { type: "state:set", key: "note", value: "edited", ifRevision: 2 });
  example.receive({ type: "state:saved", key: "note", revision: 3 });
  assert.equal(example.status.textContent, "Saved for the organization.");
});

test("the documented example ignores foreign windows and respects an explicitly disabled shell", () => {
  const example = documentedExample();
  example.receive({ type: "state:ready", enabled: true }, {});
  example.input("temporary");
  example.receive({ type: "state:ready", enabled: false });
  example.timeout();
  example.input("still temporary");
  assert.deepEqual(example.messages, [{ type: "state:hello" }]);
});
