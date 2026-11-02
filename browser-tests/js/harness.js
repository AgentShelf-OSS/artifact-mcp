// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// The in-page side of browser-tests. Test files import what they need from here:
//
//   import { test, assert, readText, runInNewContext } from "./harness.js";
//
// * `test(name, fn)` registers a case; `fn` may be async. Cases run one at a time, in order, in
//   the same page, after the module has finished loading (`node:test` order).
// * `assert` is the subset of `node:assert/strict` the suites use, with the same pass/fail rules.
// * `readText`, `readBytes` and `fixture` fetch repository files and Rust-computed values from the
//   page's virtual origin. They are async: call them with `await`.
// * `runInNewContext`, `createContext` and `runInContext` replace `node:vm` (see below).
// * `setImmediate` and `clearImmediate` replace the Node timers of the same names.
//
// The Rust runner (browser-tests/src/suite.rs) calls `cases()` and then `run(index)` per case.

const registered = [];
let running = false;

export function test(name, fn) {
  if (typeof name !== "string" || typeof fn !== "function") {
    throw new TypeError("test(name, fn) expects a name and a function");
  }
  if (running) throw new Error(`test(${JSON.stringify(name)}) was called while a test ran; register tests at module level`);
  registered.push({ name, fn });
}

/** The registered case names, in order. */
export function cases() {
  return registered.map((entry) => entry.name);
}

/**
 * Run one case. Resolves to `{ ok: true }` or `{ ok: false, error }`; never rejects. After the
 * case settles, one more task turn passes before it resolves: browsers report an unhandled
 * rejection from a later task, and the runner attributes what the page logs to the case that was
 * running.
 */
export async function run(index) {
  const entry = registered[index];
  if (!entry) return { ok: false, error: `there is no test #${index}` };
  running = true;
  let outcome;
  try {
    await entry.fn();
    outcome = { ok: true };
  } catch (error) {
    outcome = { ok: false, error: describeError(error) };
  } finally {
    running = false;
  }
  await new Promise((resolve) => setTimeout(resolve, 0));
  return outcome;
}

function describeError(error) {
  if (error instanceof Error || (error && typeof error.stack === "string")) {
    const stack = String(error.stack || "");
    const head = `${error.name}: ${error.message}`;
    return stack.includes(error.message) ? stack : `${head}\n${stack}`;
  }
  return `non-Error thrown: ${inspect(error)}`;
}

// ---------------------------------------------------------------------------------------------
// Files

const ROOT = new URL("/", import.meta.url);

function checkPath(path) {
  if (typeof path !== "string" || path === "" || path.startsWith("/") || path.split("/").some((part) => part === ".." || part === ".")) {
    throw new TypeError(`expected a repository-relative path such as "assets/portal.css", got ${inspect(path)}`);
  }
  return path;
}

async function load(url, label) {
  const response = await fetch(url, { cache: "no-store" });
  if (!response.ok) throw new Error(`${label}: ${response.status} ${await response.text()}`);
  return new Uint8Array(await response.arrayBuffer());
}

// Like readFileSync(path, "utf8"): UTF-8, and a byte-order mark is kept, not stripped.
const utf8 = new TextDecoder("utf-8", { ignoreBOM: true });

/** A repository file as text. `path` is relative to the repository root. */
export async function readText(path) {
  return utf8.decode(await load(new URL(checkPath(path), ROOT), `readText(${path})`));
}

/** Join numbered script parts in the order their parts.txt lists. */
export async function readParts(dir) {
  const names = (await readText(`${dir}/parts.txt`)).split("\n").filter((name) => name !== "");
  return (await Promise.all(names.map((name) => readText(`${dir}/${name}`)))).join("");
}

/** The viewer shell script as the server ships it after dialogs.js. */
export async function readShell() {
  return readParts("assets/shell");
}

/** A repository file as bytes. */
export async function readBytes(path) {
  return load(new URL(checkPath(path), ROOT), `readBytes(${path})`);
}

/** A value the Rust side computes, such as the anchor bridge extracted from the server source. */
export async function fixture(name) {
  return utf8.decode(await load(new URL(`/__fixtures__/${encodeURIComponent(name)}`, ROOT), `fixture(${name})`));
}

// ---------------------------------------------------------------------------------------------
// Timers

/** Node's setImmediate: run `callback` in a later task, after pending promise jobs. */
export function setImmediate(callback, ...args) {
  const channel = new MessageChannel();
  const handle = { cancelled: false };
  channel.port1.onmessage = () => {
    channel.port1.close();
    if (!handle.cancelled) callback(...args);
  };
  channel.port2.postMessage(null);
  return handle;
}

export function clearImmediate(handle) {
  if (handle) handle.cancelled = true;
}

// ---------------------------------------------------------------------------------------------
// node:vm replacement
//
// `vm.runInNewContext(code, sandbox)` runs code in a fresh JavaScript realm whose global object
// is backed by `sandbox`: the code sees the sandbox's properties (read at the time of each
// access, so later changes to the sandbox are visible), the realm's own ECMAScript built-ins, and
// nothing else. In particular no `window`, `document`, `fetch` or timers unless the sandbox
// provides them. Here each context is a hidden same-origin iframe (a separate realm with its own
// `Object`, `Array`, `Promise` and so on), and the code is evaluated inside
// `with (scope) { eval(code) }`, where `scope` is a proxy over the sandbox that claims every
// name. The completion value of the code is returned, as `vm` does.
//
// Differences from Node that the suites do not depend on: a missing name reads as `undefined`
// instead of throwing a ReferenceError; top-level `var` and function declarations are not
// copied onto the sandbox; and `this` inside a plain function call is the iframe's window. A
// native function taken from the sandbox (for example a real `setTimeout`) is called with an
// undefined receiver, as a Node global call would be, because Web IDL operations reject a plain
// object as `this`.

// Every global of a fresh Node `vm` context (V8's built-ins), plus newer ECMAScript built-ins
// when this browser has them.
const ECMASCRIPT_GLOBALS = [
  "AggregateError", "Array", "ArrayBuffer", "Atomics", "BigInt", "BigInt64Array", "BigUint64Array",
  "Boolean", "DataView", "Date", "Error", "EvalError", "FinalizationRegistry", "Float32Array",
  "Float64Array", "Function", "Infinity", "Int16Array", "Int32Array", "Int8Array", "Intl", "Iterator",
  "JSON", "Map", "Math", "NaN", "Number", "Object", "Promise", "Proxy", "RangeError", "ReferenceError",
  "Reflect", "RegExp", "Set", "SharedArrayBuffer", "String", "Symbol", "SyntaxError", "TypeError",
  "URIError", "Uint16Array", "Uint32Array", "Uint8Array", "Uint8ClampedArray", "WeakMap", "WeakRef",
  "WeakSet", "WebAssembly", "console", "decodeURI", "decodeURIComponent", "encodeURI",
  "encodeURIComponent", "escape", "isFinite", "isNaN", "parseFloat", "parseInt", "undefined",
  "unescape",
  "AsyncDisposableStack", "DisposableStack", "Float16Array", "SuppressedError", "Temporal",
];

// Names the evaluator itself needs; the scope proxy lets them through to the real bindings.
const SCOPE = "__browserTestsScope";
const THIS = "__browserTestsThis";
const CODE = "__browserTestsCode";
const PASS_THROUGH = new Set([SCOPE, THIS, CODE, "eval"]);

const contexts = new WeakMap();

function isNativeNonConstructor(value) {
  return typeof value === "function"
    && !Object.prototype.hasOwnProperty.call(value, "prototype")
    && /\{\s*\[native code\]\s*\}\s*$/.test(Function.prototype.toString.call(value));
}

function newContext(sandbox) {
  const frame = document.createElement("iframe");
  frame.hidden = true;
  frame.setAttribute("aria-hidden", "true");
  (document.body || document.documentElement).appendChild(frame);
  const realm = frame.contentWindow;
  const builtins = Object.create(null);
  for (const name of ECMASCRIPT_GLOBALS) if (name in realm) builtins[name] = realm[name];

  const receiverless = new WeakMap();
  const lookup = (key) => {
    if (Reflect.has(sandbox, key)) {
      const value = Reflect.get(sandbox, key);
      if (!isNativeNonConstructor(value)) return value;
      let wrapper = receiverless.get(value);
      if (!wrapper) {
        wrapper = value.bind(undefined);
        receiverless.set(value, wrapper);
      }
      return wrapper;
    }
    if (key === "globalThis") return globalView;
    return builtins[key];
  };
  const write = {
    set: (target, key, value) => Reflect.set(target, key, value),
    deleteProperty: (target, key) => Reflect.deleteProperty(target, key),
  };
  const globalView = new Proxy(sandbox, {
    ...write,
    has: (target, key) => Reflect.has(target, key) || (typeof key === "string" && key in builtins),
    get: (target, key) => (typeof key === "string" ? lookup(key) : Reflect.get(target, key)),
  });
  const scope = new Proxy(sandbox, {
    ...write,
    has: (target, key) => typeof key === "string" && !PASS_THROUGH.has(key),
    get: (target, key) => (key === Symbol.unscopables ? undefined : lookup(key)),
  });
  // Built with the iframe's Function, so the code compiles in that realm. The inner function
  // gives sloppy-mode declarations their own scope, ahead of the `with` scope.
  const evaluate = realm.Function(SCOPE, THIS, CODE,
    `with (${SCOPE}) { return function () { return eval(${CODE}); }.call(${THIS}); }`);
  return { run: (code) => evaluate(scope, globalView, String(code)) };
}

/** `vm.createContext`: make `sandbox` usable with `runInContext`. Returns `sandbox`. */
export function createContext(sandbox = {}) {
  if (sandbox === null || typeof sandbox !== "object") throw new TypeError("createContext(sandbox) expects an object");
  if (!contexts.has(sandbox)) contexts.set(sandbox, newContext(sandbox));
  return sandbox;
}

/** `vm.runInContext`: evaluate `code` against a context made by `createContext`. */
export function runInContext(code, sandbox) {
  const context = contexts.get(sandbox);
  if (!context) throw new TypeError("runInContext(code, sandbox): call createContext(sandbox) first");
  return context.run(code);
}

/** `vm.runInNewContext`: evaluate `code` in a new context backed by `sandbox`. */
export function runInNewContext(code, sandbox = {}) {
  return runInContext(code, createContext(sandbox));
}

// ---------------------------------------------------------------------------------------------
// node:assert/strict subset: ok, equal, notEqual, deepEqual, notDeepEqual, match, doesNotMatch,
// throws, rejects, fail. "equal" is strict (Object.is) and "deepEqual" is deep-strict, as in
// node:assert/strict.

export class AssertionError extends Error {
  constructor({ message, actual, expected, operator }) {
    super(message);
    this.name = "AssertionError";
    this.code = "ERR_ASSERTION";
    this.actual = actual;
    this.expected = expected;
    this.operator = operator;
  }
}

function raise(message, details, generated) {
  if (message instanceof Error) throw message;
  throw new AssertionError({ ...details, message: message === undefined ? generated : message });
}

function ok(value, message) {
  if (!value) raise(message, { actual: value, expected: true, operator: "==" }, `The expression evaluated to a falsy value: ${inspect(value)}`);
}

export const assert = Object.assign((value, message) => ok(value, message), {
  AssertionError,
  ok,
  equal(actual, expected, message) {
    if (!Object.is(actual, expected)) {
      raise(message, { actual, expected, operator: "strictEqual" }, `Expected values to be strictly equal:\n\n${inspect(actual)} !== ${inspect(expected)}\n`);
    }
  },
  notEqual(actual, expected, message) {
    if (Object.is(actual, expected)) {
      raise(message, { actual, expected, operator: "notStrictEqual" }, `Expected "actual" to be strictly unequal to: ${inspect(expected)}`);
    }
  },
  deepEqual(actual, expected, message) {
    if (!isDeepStrictEqual(actual, expected)) {
      raise(message, { actual, expected, operator: "deepStrictEqual" }, `Expected values to be strictly deep-equal:\n+ actual - expected\n\n+ ${inspect(actual)}\n- ${inspect(expected)}\n`);
    }
  },
  notDeepEqual(actual, expected, message) {
    if (isDeepStrictEqual(actual, expected)) {
      raise(message, { actual, expected, operator: "notDeepStrictEqual" }, `Expected "actual" not to be strictly deep-equal to: ${inspect(expected)}`);
    }
  },
  match(string, regexp, message) {
    checkRegExp(regexp, "match");
    if (typeof string !== "string" || RegExp.prototype.exec.call(regexp, string) === null) {
      raise(message, { actual: string, expected: regexp, operator: "match" }, typeof string !== "string"
        ? `The "string" argument must be of type string. Received ${inspect(string)}`
        : `The input did not match the regular expression ${inspect(regexp)}. Input:\n\n${inspect(string)}\n`);
    }
  },
  doesNotMatch(string, regexp, message) {
    checkRegExp(regexp, "doesNotMatch");
    if (typeof string !== "string" || RegExp.prototype.exec.call(regexp, string) !== null) {
      raise(message, { actual: string, expected: regexp, operator: "doesNotMatch" }, typeof string !== "string"
        ? `The "string" argument must be of type string. Received ${inspect(string)}`
        : `The input was expected to not match the regular expression ${inspect(regexp)}. Input:\n\n${inspect(string)}\n`);
    }
  },
  throws(fn, expected, message) {
    if (typeof fn !== "function") throw new TypeError("assert.throws(fn) expects a function");
    let threw = false, error;
    try { fn(); } catch (caught) { threw = true; error = caught; }
    [expected, message] = splitExpected(expected, message);
    if (!threw) raise(message, { actual: undefined, expected, operator: "throws" }, "Missing expected exception.");
    validateError(error, expected, message, "throws");
  },
  async rejects(promiseOrFn, expected, message) {
    let promise = promiseOrFn;
    if (typeof promiseOrFn === "function") promise = promiseOrFn();
    if (!promise || typeof promise.then !== "function") throw new TypeError("assert.rejects expects a promise or a function returning one");
    let rejected = false, error;
    try { await promise; } catch (caught) { rejected = true; error = caught; }
    [expected, message] = splitExpected(expected, message);
    if (!rejected) raise(message, { actual: undefined, expected, operator: "rejects" }, "Missing expected rejection.");
    validateError(error, expected, message, "rejects");
  },
  fail(message = "Failed") {
    raise(message, { operator: "fail" }, "Failed");
  },
});

function checkRegExp(regexp, name) {
  if (Object.prototype.toString.call(regexp) !== "[object RegExp]") {
    throw new TypeError(`assert.${name}: the "regexp" argument must be a RegExp`);
  }
}

// assert.throws(fn, "message") treats the string as the message.
function splitExpected(expected, message) {
  return typeof expected === "string" ? [undefined, expected] : [expected, message];
}

function validateError(error, expected, message, operator) {
  if (expected === undefined) return;
  if (Object.prototype.toString.call(expected) === "[object RegExp]") {
    if (!RegExp.prototype.test.call(expected, String(error))) {
      raise(message, { actual: error, expected, operator }, `The error did not match the regular expression ${inspect(expected)}: ${inspect(error)}`);
    }
    return;
  }
  if (typeof expected === "function") {
    if (expected.prototype !== undefined && error instanceof expected) return;
    if (Error.isPrototypeOf(expected) || expected === Error) {
      raise(message, { actual: error, expected, operator }, `The error is expected to be an instance of "${expected.name}". Received ${inspect(error)}`);
    }
    const verdict = expected.call({}, error);
    if (verdict !== true) {
      raise(message, { actual: error, expected, operator }, `The validation function is expected to return "true". Received ${inspect(verdict)}\n\nCaught error:\n\n${inspect(error)}`);
    }
    return;
  }
  if (expected !== null && typeof expected === "object") {
    for (const key of Reflect.ownKeys(expected)) {
      const want = expected[key];
      const got = error == null ? undefined : error[key];
      const matches = Object.prototype.toString.call(want) === "[object RegExp]" && typeof got === "string"
        ? RegExp.prototype.test.call(want, got)
        : isDeepStrictEqual(got, want);
      if (!matches) {
        raise(message, { actual: error, expected, operator }, `The error's ${String(key)} differs: ${inspect(got)} !== ${inspect(want)}`);
      }
    }
    return;
  }
  throw new TypeError(`assert.${operator}: unsupported "expected" argument ${inspect(expected)}`);
}

const tagOf = (value) => Object.prototype.toString.call(value);

/** Node's deep-strict comparison: same prototypes, same own enumerable keys, Object.is leaves. */
export function isDeepStrictEqual(actual, expected) {
  return deepEqual(actual, expected, new Map());
}

function deepEqual(a, b, memo) {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
  const tag = tagOf(a);
  if (tag !== tagOf(b)) return false;
  // Cycles: assume equal while the same pair is being compared.
  let seen = memo.get(a);
  if (seen && seen.has(b)) return true;
  if (!seen) memo.set(a, (seen = new Set()));
  seen.add(b);

  if (Array.isArray(a) && a.length !== b.length) return false;
  if (tag === "[object Date]" && !Object.is(a.getTime(), b.getTime())) return false;
  if (tag === "[object RegExp]" && (a.source !== b.source || a.flags !== b.flags || a.lastIndex !== b.lastIndex)) return false;
  if (a instanceof Error || tag === "[object Error]") {
    if (a.message !== b.message || a.name !== b.name) return false;
  }
  for (const [boxed, unbox] of [
    ["[object Number]", Number.prototype.valueOf], ["[object String]", String.prototype.valueOf],
    ["[object Boolean]", Boolean.prototype.valueOf], ["[object BigInt]", BigInt.prototype.valueOf],
    ["[object Symbol]", Symbol.prototype.valueOf],
  ]) {
    if (tag === boxed) {
      try { if (!Object.is(unbox.call(a), unbox.call(b))) return false; } catch { /* not boxed */ }
    }
  }
  if (ArrayBuffer.isView(a)) {
    if (!ArrayBuffer.isView(b) || a.byteLength !== b.byteLength) return false;
    const left = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
    const right = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
    for (let i = 0; i < left.length; i++) if (left[i] !== right[i]) return false;
  } else if (tag === "[object ArrayBuffer]" || tag === "[object SharedArrayBuffer]") {
    if (a.byteLength !== b.byteLength) return false;
    const left = new Uint8Array(a), right = new Uint8Array(b);
    for (let i = 0; i < left.length; i++) if (left[i] !== right[i]) return false;
  }
  if (tag === "[object Map]") {
    if (a.size !== b.size) return false;
    const unmatched = [];
    for (const [key, value] of a) {
      if (b.has(key)) {
        if (!deepEqual(value, b.get(key), memo)) return false;
      } else if (key !== null && typeof key === "object") {
        unmatched.push([key, value]);
      } else {
        return false;
      }
    }
    if (unmatched.length) {
      const candidates = [...b].filter(([key]) => key !== null && typeof key === "object" && !a.has(key));
      for (const [key, value] of unmatched) {
        const index = candidates.findIndex(([otherKey, otherValue]) => deepEqual(key, otherKey, memo) && deepEqual(value, otherValue, memo));
        if (index < 0) return false;
        candidates.splice(index, 1);
      }
    }
  }
  if (tag === "[object Set]") {
    if (a.size !== b.size) return false;
    const unmatched = [];
    for (const value of a) {
      if (b.has(value)) continue;
      if (value !== null && typeof value === "object") unmatched.push(value);
      else return false;
    }
    if (unmatched.length) {
      const candidates = [...b].filter((value) => value !== null && typeof value === "object" && !a.has(value));
      for (const value of unmatched) {
        const index = candidates.findIndex((other) => deepEqual(value, other, memo));
        if (index < 0) return false;
        candidates.splice(index, 1);
      }
    }
  }
  const keysA = ownEnumerableKeys(a), keysB = ownEnumerableKeys(b);
  if (keysA.length !== keysB.length) return false;
  const keySetB = new Set(keysB);
  for (const key of keysA) {
    if (!keySetB.has(key)) return false;
    if (!deepEqual(a[key], b[key], memo)) return false;
  }
  return true;
}

function ownEnumerableKeys(value) {
  return Reflect.ownKeys(value).filter((key) => Object.prototype.propertyIsEnumerable.call(value, key));
}

/** A short, readable rendering for assertion messages. */
export function inspect(value, depth = 0, seen = new Set()) {
  if (typeof value === "string") return JSON.stringify(value);
  if (typeof value === "bigint") return `${value}n`;
  if (typeof value === "symbol" || typeof value === "undefined" || typeof value === "number" || typeof value === "boolean") {
    return Object.is(value, -0) ? "-0" : String(value);
  }
  if (typeof value === "function") return `[Function: ${value.name || "(anonymous)"}]`;
  if (value === null) return "null";
  if (seen.has(value)) return "[Circular]";
  if (depth > 4) return Array.isArray(value) ? "[Array]" : "[Object]";
  const tag = tagOf(value);
  if (tag === "[object RegExp]") return String(value);
  if (tag === "[object Date]") return Number.isNaN(value.getTime()) ? "Invalid Date" : value.toISOString();
  if (value instanceof Error || tag === "[object Error]") return `[${value.name}: ${value.message}]`;
  const next = new Set(seen).add(value);
  const inner = (item) => inspect(item, depth + 1, next);
  if (Array.isArray(value)) return `[${value.map(inner).join(", ")}]`;
  if (tag === "[object Map]") return `Map(${value.size}) {${[...value].map(([k, v]) => `${inner(k)} => ${inner(v)}`).join(", ")}}`;
  if (tag === "[object Set]") return `Set(${value.size}) {${[...value].map(inner).join(", ")}}`;
  if (ArrayBuffer.isView(value)) return `${value.constructor?.name || "TypedArray"}(${value.length}) [${Array.from(value).slice(0, 32).join(", ")}${value.length > 32 ? ", …" : ""}]`;
  const keys = ownEnumerableKeys(value);
  const prefix = Object.getPrototypeOf(value) === null ? "[Object: null prototype] " : "";
  return `${prefix}{${keys.map((key) => `${typeof key === "symbol" ? `[${String(key)}]` : /^[A-Za-z_$][\w$]*$/.test(key) ? key : JSON.stringify(key)}: ${inner(value[key])}`).join(", ")}}`;
}
