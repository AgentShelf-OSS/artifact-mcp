# browser-tests

Tests for the JavaScript in `../assets` (viewer shell brokers, reader, cast bridge, portal,
anchor bridge, gallery layout, and so on), run in real Chrome for Testing
`chrome-headless-shell` and driven from Rust. Nothing here starts Node.

The crate is its own Cargo workspace, like `../preview-renderer`. Root `cargo test --workspace`
does not build it and never needs a browser. It shares the DevTools pipe transport and the
process-group launcher with the preview renderer through `../preview-renderer/cdp`
(`artifact-cdp`).

## Run

```sh
export CHROME_HEADLESS_SHELL=/path/to/chrome-headless-shell-linux64/chrome-headless-shell
cargo fmt --manifest-path browser-tests/Cargo.toml --check
cargo clippy --manifest-path browser-tests/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path browser-tests/Cargo.toml --locked
```

Use the Chrome for Testing version pinned in `../preview-renderer/Dockerfile` (`CHROME_VERSION`
and `CHROME_SHA256`). The CI job downloads exactly that archive.

Select cases by substring of their full name, `suite :: case`:

```sh
cargo test --manifest-path browser-tests/Cargo.toml --test js -- shell-geo
cargo test --manifest-path browser-tests/Cargo.toml --test js -- "portal :: shell opens" --nocapture
cargo test --manifest-path browser-tests/Cargo.toml --test js -- --list
```

| Variable | Meaning |
|---|---|
| `CHROME_HEADLESS_SHELL` | The browser. Without it the browser tests are skipped with a message. |
| `CI` | When set, a missing `CHROME_HEADLESS_SHELL` fails the run instead of skipping it. |
| `BROWSER_TESTS_JOBS` | How many test files run at once (default 1; `--test-threads N` also works). |
| `BROWSER_TESTS_CHROME_STDERR` | When set, the browser's own stderr is shown. |

The preview renderer's lifecycle tests scan for processes started from `CHROME_HEADLESS_SHELL`.
Do not run them at the same time as these tests with the same browser path.

## Layout

| Path | What it is |
|---|---|
| `js/*.test.js` | The JavaScript test files. One file is one suite. |
| `js/harness.js` | The in-page runner: `test`, `assert`, `readText`, `readBytes`, `fixture`, `runInNewContext`, `createContext`, `runInContext`, `setImmediate`. |
| `tests/js.rs` | `cargo test --test js`: runs every suite and reports each case by name. |
| `tests/harness.rs` | Tests of the harness itself: evaluation, console and exception capture, compile checks, routes, network policies, process cleanup, and the Node semantics of `js/harness.js`. |
| `src/` | The Rust harness API (below) and the suite runner (`src/suite.rs`). |

## How a suite runs

Each test file gets a fresh browser with a fresh profile and one page, as `node --test` gave each
file its own process. The page lives on the virtual origin `https://harness.test`, which the Rust
side answers inside the browser with DevTools `Fetch` interception: `/` is the repository root,
read-only, plus a blank start page and `/__fixtures__/<name>`. The browser runs with the preview
renderer's network lockdown, so a test cannot reach the network or this machine.

The runner imports the file as an ES module. Its `test(name, fn)` calls register cases. Then the
Rust side runs the cases one at a time, in order, in the same page. A case fails when:

* its function throws or its promise rejects, or
* while it runs, the page logs `console.error`, a failed `console.assert`, an uncaught exception,
  an unhandled rejection, a dialog, or a renderer crash.

Something that fails while the module loads, or after the last case, fails the whole file
(`suite <name> ... FAILED`).

`browser-assets-parse :: every browser asset parses` is a Rust-side case in the same report. It
parses every `assets/*.js` file with Chrome's parser, without running it, as a classic script
(`Runtime.compileScript`) and as an ES module.

## Write a test

Add `js/<name>.test.js`. The runner finds it; there is nothing to register in Rust.

```js
import { test, assert, readText, runInNewContext } from "./harness.js";

const shell = await readText("assets/shell.js");
const start = shell.indexOf("  function createViewerGeoBroker(options)");
const end = shell.indexOf("  // End viewer geolocation broker.", start);
const createBroker = runInNewContext(shell.slice(start, end) + "\ncreateViewerGeoBroker", {});

test("forget clears consent", async () => {
  const messages = [];
  const broker = createBroker({ /* stubs */ post: (message) => messages.push(message) });
  // ...
  assert.deepEqual(messages.at(-1), { type: "geo:ready", supported: true, remembered: false });
});
```

* `readText(path)` and `readBytes(path)` read repository files (paths relative to the root) and
  are async. Read at module level with `await`, or inside an `async` test.
* `fixture(name)` returns a value computed in Rust. `ANCHOR_BRIDGE` is the raw-string constant in
  `src/http/artifact_response.rs`. Add new fixtures in `src/suite.rs` (`fixture`).
* `assert` is the `node:assert/strict` subset: `ok`, `equal`, `notEqual`, `deepEqual`,
  `notDeepEqual`, `match`, `doesNotMatch`, `throws`, `rejects`, `fail`. `equal` is `Object.is`;
  `deepEqual` is deep-strict and compares prototypes.
* `runInNewContext(code, sandbox)` runs code the way `node:vm` does: in a separate realm (its own
  `Object`, `Array`, `Promise`) whose globals are the sandbox's properties, read at each access,
  plus the ECMAScript built-ins. There is no `window`, `document`, `fetch` or timer unless the
  sandbox provides one. Objects created inside are not `deepEqual` to plain literals outside,
  as in Node; compare `JSON.parse(JSON.stringify(value))`.
* The page is a real browser, so there is no `Buffer`, `process` or `require`. Browser timers
  reject a plain object as `this`: a stub such as `{ setTimeout }` that the code calls as
  `stub.setTimeout(...)` needs a wrapper, `{ setTimeout: (...args) => setTimeout(...args) }`.

## The Rust harness API

`src/lib.rs` is a small library for any browser test:

```rust
let browser = Browser::launch(&BrowserOptions::new(chrome).network(Network::LoopbackOnly)).await?;
let page = browser.new_page().await?;            // own browser context, 1280×720
page.set_viewport(390, 844, 3.0).await?;
page.goto("http://127.0.0.1:3483/").await?;      // waits for the load event
page.set_content("<p>hi</p>").await?;
let value = page.evaluate("document.title").await?;            // awaits promises, returns JSON
let sum = page.call("(a, b) => a + b", &[json!(1), json!(2)]).await?;
let error = page.compile_script(source, "file.js").await?;     // Option<ScriptError>
page.route("https://harness.test/", |request| Response::ok("text/plain", "hi")).await?;
let mut cursor = page.log_mark();
let entries = page.log_after(&mut cursor);       // console, exceptions, dialogs, crashes
let mut loads = page.subscribe("Page.loadEventFired");
page.command("Input.dispatchKeyEvent", json!({ /* … */ })).await?;  // raw DevTools escape hatch
browser.close().await;
```

* `Network::LockedDown` (default) is the renderer's lockdown: everything, loopback included, goes
  to an unreachable proxy and no name resolves. Only `Page::route` handlers answer.
* `Network::LoopbackOnly` allows direct connections to `127.0.0.1`, `[::1]`, `localhost` and
  `*.localhost`, for tests against a local server. Everything else stays blocked.
* Every call has a timeout (30 s by default; `Page::set_default_timeout`). Dialogs are dismissed
  and logged. A page crash fails the commands waiting on it.
* The browser is killed with its whole process group when `Browser` is dropped or closed, and if
  the thread that launched it exits.
