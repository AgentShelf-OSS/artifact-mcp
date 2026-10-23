# browser-tests

Tests for the JavaScript in `../assets` (viewer shell brokers, reader, cast bridge, portal,
anchor bridge, gallery layout, and so on), and end-to-end browser journeys against the
artifact-mcp release binary, run in real Chrome for Testing `chrome-headless-shell` and driven
from Rust. Nothing here starts Node.

The crate is its own Cargo workspace, like `../preview-renderer`. Root `cargo test --workspace`
does not build it and never needs a browser. It shares the DevTools pipe transport and the
process-group launcher with the preview renderer through `../preview-renderer/cdp`
(`artifact-cdp`).

## Run

```sh
export CHROME_HEADLESS_SHELL=/path/to/chrome-headless-shell-linux64/chrome-headless-shell
cargo build --release --locked                       # the server, for the e2e target
export ARTIFACT_MCP_BIN=$PWD/target/release/artifact-mcp
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
cargo test --manifest-path browser-tests/Cargo.toml --test e2e -- 02-viewer
cargo test --manifest-path browser-tests/Cargo.toml --test e2e -- --list
```

| Variable | Meaning |
|---|---|
| `CHROME_HEADLESS_SHELL` | The browser. Without it the browser tests are skipped with a message. |
| `ARTIFACT_MCP_BIN` | The server binary for `--test e2e`. Without it the e2e target is skipped with a message. |
| `CI` | When set, a missing `CHROME_HEADLESS_SHELL` or `ARTIFACT_MCP_BIN` fails the run instead of skipping it. |
| `E2E_RUN_ID` | The e2e run id (organization `pwtest-<id>`); default: the last six digits of the start time. |
| `E2E_TEST_TIMEOUT_MS` | Per-case timeout of the e2e target (default 30000, Playwright's `timeout`). |
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
| `tests/locators.rs` | Tests of the locator, action and expectation layer on routed pages (no server). |
| `tests/e2e/` | `cargo test --test e2e`: browser journeys against the server, one module per former Playwright spec (`s01_gallery.rs` ...). |
| `src/` | The Rust harness API (below), the suite runner (`src/suite.rs`), and the e2e fixtures and runner (`src/e2e.rs`, `src/server.rs`, `src/http.rs`). |
| `src/injected.js` | The in-page locator engine (roles, accessible names, text, labels, states, matchers). |

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

### Locators, actions and expectations

`Page::locator`, `get_by_role`, `get_by_text`, `get_by_label`, `get_by_placeholder`,
`get_by_alt_text`, `get_by_title`, `get_by_test_id` and `frame_locator` return lazy `Locator`s
with Playwright's meaning: they are evaluated again on every use by an in-page engine ported from
Playwright's injected script (`src/injected.js`), so ARIA roles, accessible names, hidden
elements, text and label matching, and element states behave as in the Playwright suite.

```rust
use browser_tests::{Role, exact, expect, regex};

let card = page.locator(".card").filter_has_text(&title);           // { hasText }
card.get_by_role(Role::new("button").name(format!("More actions for {title}"))).click().await?;
page.get_by_role(Role::new("button").exact_name("Move")).click().await?;   // { exact: true }
page.get_by_label("Filter by organization").select_option(&["acme"]).await?;
let frame = page.frame_locator("#vframe");                          // in or out of process
frame.get_by_role(Role::new("textbox").exact_name("Note")).fill("Diary").await?;
expect(&frame.locator("body")).to_have_attribute("data-saved", "1").await?;
expect(&page.locator("#status")).to_contain_text(regex("not ready", "i")).await?;
expect(&page.locator(".filter-rail")).to_have_count(0).await?;
```

* **Patterns.** In locators a `&str` is a case-insensitive substring after whitespace
  normalisation; `exact("…")` is whole and case-sensitive; `regex(source, flags)` is a JavaScript
  `RegExp`. In expectations a string is whole (`to_have_*`) or contained (`to_contain_*`) and
  case-sensitive, as in Playwright.
* **Locator methods.** `locator`, `get_by_*`, `nth`, `first`, `last`, `filter_has_text`,
  `filter_has_not_text`, `filter_has`, `filter_has_not`, `filter_visible`, `content_frame`,
  `frame_locator`. CSS is standard CSS (open shadow roots are pierced) plus Playwright's
  `:visible` at the end of a compound selector; other Playwright selector extensions are not
  supported.
* **Actions** wait up to the page's default timeout (30 s) and are strict: more than one match is
  an immediate `Error::Strict`. `click`, `click_with(ClickOptions)`, `dblclick`, `hover` and `tap`
  wait for the element to be attached, visible, enabled (not for `hover`), stable for two
  animation frames, scrolled into view and the hit target at its centre (also in each parent
  frame), then dispatch trusted input. `fill` waits for visible, enabled and editable;
  `select_option` for visible, enabled and the options; `press` and `press_sequentially` focus
  first; `check`, `uncheck`, `focus`, `blur`, `dispatch_event`, `scroll_into_view_if_needed`.
* **Reads** (waiting for the element): `evaluate(function, arg)` (as `function(element, arg)`),
  `text_content`, `inner_text`, `input_value`, `get_attribute`, `bounding_box` (main-viewport CSS
  pixels, frames included), `accessible_name`. No waiting: `count`, `all_text_contents`,
  `is_visible`, `is_hidden`, `is_enabled`, `is_checked`. `wait_for(WaitState)`.
* **Expectations** (`expect(&locator)`, `.not()`, `.timeout(..)`) retry with Playwright's
  intervals until the expect timeout (5 s; `set_expect_timeout`): `to_be_visible`, `to_be_hidden`,
  `to_be_attached`, `to_be_enabled`, `to_be_disabled`, `to_be_editable`, `to_be_checked`,
  `to_be_focused`, `to_be_in_viewport`, `to_have_text`, `to_contain_text`, `to_have_texts`,
  `to_contain_texts`, `to_have_count`, `to_have_attribute`, `to_have_attribute_present`,
  `to_have_value`, `to_have_class`, `to_have_css`. A failure is an `Error::Assertion` with the
  locator, the expected and last received values and a screenshot path. `poll(what, probe,
  accept)` is `expect.poll`. Plain values use `assert!`.
* **Input.** `page.keyboard()` (`press("Shift+Tab")`, `down`, `up`, `type_text`,
  `insert_text`), `page.mouse()` (`move_to(x, y, steps)`, `down`, `up`, `click`, `dblclick`,
  `wheel`), `page.touchscreen().tap(x, y)` after `page.set_has_touch(true)`.
* **Pages.** `goto` (returns the main document's `NetworkResponse`), `reload`, `go_back`,
  `go_forward`, `set_base_url`, `set_extra_http_headers` and `add_init_script` (for the page and
  every iframe), `enable_browser_log` (resource errors and CSP reports become console entries, as
  Playwright reports them), `wait_for_function`, `url`, `frames`, `frame_url`, `screenshot`,
  `page_errors`, `set_evaluate_user_gesture` (Playwright evaluates with a user gesture).
* **Network.** `wait_for_request` and `wait_for_response` return waiters to create before the
  action and `.await` after it; they see every frame, out-of-process iframes included.
  `requests()` lists every request so far (a `page.on("request")` collector). `response_text`,
  `response_body`, `all_request_headers`. `route_glob(glob, handler)` and
  `route_where(predicate, handler)` take async handlers that `fulfill`, `continue_request`,
  `abort`, `fallback` or `fetch` the real response first; the newest route runs first, as in
  Playwright; `unroute(glob)`, `unroute_all`. Globs follow Playwright 1.60 (`glob_matches`).
* **Dialogs and popups.** `on_dialog(|dialog| DialogAction::Accept(None))` decides; without a
  handler dialogs are dismissed and logged as failures; `dialogs()` lists them all.
  `wait_for_popup()` returns the popup as a `Page`. The popup is attached after it opens, so its
  first request carries no extra headers and is not routed.
* **Frames.** Each page auto-attaches its out-of-process iframes (`Target.setAutoAttach`, flat
  sessions, paused until set up): their console output, exceptions, dialogs, traffic, routes,
  extra headers and init scripts belong to the page.

## End-to-end tests (`tests/e2e`)

`cargo test --test e2e` replaces the Playwright suite in `../playwright/tests`, spec by spec. The
runner (`browser_tests::e2e::main`) does what `playwright.config.mjs`, `global-setup.mjs`,
`global-teardown.mjs` and `fixtures.mjs` did:

1. **Global setup.** Start `ARTIFACT_MCP_BIN` on a free loopback port with a temporary
   `DATA_DIR` and the CI `browser` job's environment (`TRUST_ACCESS_HEADERS=1`,
   `ADMIN_EMAILS=admin@example.test`, raised `INGRESS_*` limits, test audit and webhook keys; see
   `src/server.rs`), wait for `/health`, create the throwaway organization `pwtest-<run id>`, and
   launch one browser with `Network::LoopbackOnly`.
2. **Cases**, one at a time (`workers: 1`), each with a 30 s timeout, reported as
   `spec :: title` with the Playwright titles. A case gets a `Ctx`: `t.page()` (the `page`
   fixture, signed in as the admin), `t.page_as(Identity::email(..))` / `Identity::Anonymous` /
   `Identity::Headers(..)` (`browser.newContext({ extraHTTPHeaders })`), `t.request()` and
   `t.request_as(..)` (the `request` fixture), `t.api(method, path, body)` (`fixtures.mjs`
   `api()`, with the mutation headers), `t.publisher_key()`, `t.publish(key, title, html,
   category)`, `t.mcp_call(key, tool, arguments)`, `t.org`, `t.run_id`, `t.base_url()`. Every page
   has its own browser context; the runner disposes of all of them after the case.
3. **Global teardown.** Delete the artifacts of the run's organizations and the organizations
   (failing the run if that fails), stop the server, remove its directory. On failure the last
   server log lines are printed.

Add a spec as `tests/e2e/sNN_name.rs` with `pub fn cases() -> Vec<Case>` and list it in
`tests/e2e/main.rs`:

```rust
use browser_tests::e2e::{Case, Ctx};
use browser_tests::{Error, Role, e2e_case, expect};

pub fn cases() -> Vec<Case> {
    vec![e2e_case!("10-collections", "the Playwright title, unchanged", renders)]
}

async fn renders(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t.publish(&key, "Title", "<!doctype html><h1>x</h1>", None).await?;
    page.goto(&format!("/{}", artifact["id"].as_str().unwrap_or(""))).await?;
    expect(&page.get_by_role(Role::new("button").name("Share"))).to_be_visible().await
}
```

Failure screenshots go to `$CARGO_TARGET_TMPDIR/e2e-failures/`.
