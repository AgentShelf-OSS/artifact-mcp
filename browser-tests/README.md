# browser-tests

Tests for the JavaScript in `../assets` (viewer shell brokers, reader, cast bridge, portal,
anchor bridge, gallery layout, and so on), end-to-end browser journeys against the artifact-mcp
release binary, and the reader regression suite, run in real Chrome for Testing
`chrome-headless-shell` and driven from Rust. Nothing here starts Node.

`cargo test` and the conformance oracle (`../conformance`) prove that the server emits the
expected bytes. They do not run page JavaScript. This crate does: a truncated `shell.js` was once
served byte for byte, passed every conformance case, and still broke every button because the
browser could not parse it. Add a journey here for every interactive flow before you trust it.

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
cargo test --manifest-path browser-tests/Cargo.toml --test reader -- reader-wav
cargo test --manifest-path browser-tests/Cargo.toml --test reader -- --list
```

| Variable | Meaning |
|---|---|
| `CHROME_HEADLESS_SHELL` | The browser. Without it the browser tests are skipped with a message. |
| `ARTIFACT_MCP_BIN` | The server binary for `--test e2e` and `--test reader`. Without it those targets are skipped with a message. |
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
| `tests/reader/` | `cargo test --test reader`: the reader regression suite, one module per former `reader-*.cjs` program. |
| `src/` | The Rust harness API (below), the suite runner (`src/suite.rs`), the e2e fixtures and runner (`src/e2e.rs`, `src/server.rs`, `src/http.rs`), and the reader fixture, stub speech worker and runner (`src/reader.rs`). |
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
import { test, assert, runInNewContext, readShell } from "./harness.js";

// The viewer shell is split across assets/shell/; readShell() joins the parts as the server does.
const shell = await readShell();
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
  `evaluate_all(function, arg)` (as `function(elements, arg)`, Playwright's `evaluateAll`; `[]`
  when nothing matches), `is_visible`, `is_hidden`, `is_enabled`, `is_checked`.
  `wait_for(WaitState)`.
* **Expectations** (`expect(&locator)`, `.not()`, `.timeout(..)`) retry with Playwright's
  intervals until the expect timeout (5 s; `set_expect_timeout`): `to_be_visible`, `to_be_hidden`,
  `to_be_attached`, `to_be_enabled`, `to_be_disabled`, `to_be_editable`, `to_be_checked`,
  `to_be_focused`, `to_be_in_viewport`, `to_have_text`, `to_contain_text`, `to_have_texts`,
  `to_contain_texts`, `to_have_count`, `to_have_attribute`, `to_have_attribute_present`,
  `to_have_value`, `to_have_class`, `to_have_css`. A failure is an `Error::Assertion` with the
  locator, the expected and last received values and a screenshot path. `poll(what, probe,
  accept)` is `expect.poll`; `poll_with_timeout(what, timeout, probe, accept)` is
  `expect.poll(fn, { timeout })`. Plain values use `assert!`.
* **Input.** `page.keyboard()` (`press("Shift+Tab")`, `down`, `up`, `type_text`,
  `insert_text`), `page.mouse()` (`move_to(x, y, steps)`, `down`, `up`, `click`, `dblclick`,
  `wheel`), `page.touchscreen().tap(x, y)` after `page.set_has_touch(true)`.
* **Drag and drop.** `chrome-headless-shell` has no system drag loop, so a native drag
  (`draggable`, `dragstart`) started with `page.mouse()` never drops. `page.drag_mouse()` is the
  same mouse with Playwright's Chromium drag handling: on a move with the left button down it
  turns on `Input.setInterceptDrags`, asks every frame whether a `dragstart` ran and was not
  cancelled, and then sends `dragEnter`, `dragOver` and, on `up()`, `drop` with the intercepted
  data (`Input.dispatchDragEvent`). Without a drag it sends plain mouse events.

  ```rust
  let mut mouse = page.drag_mouse();
  mouse.move_to(from.0, from.1, 1).await?;
  mouse.down().await?;
  mouse.move_to(to.0, to.1, 12).await?;
  mouse.up().await?; // drop
  ```
* **Pages.** `goto` (returns the main document's `NetworkResponse`), `reload`, `go_back`,
  `go_forward`, `set_base_url`, `set_extra_http_headers` and `add_init_script` (for the page and
  every iframe), `enable_browser_log` (resource errors and CSP reports become console entries, as
  Playwright reports them), `wait_for_function` and `wait_for_function_with_timeout`, `url`,
  `frames`, `frame_url`, `screenshot`, `page_errors`, `set_evaluate_user_gesture` (Playwright
  evaluates with a user gesture).
* **Exposed functions.** `page.expose_function(name, |args| Ok(value))` defines `window[name]`
  in the page (Playwright's `exposeFunction`): a call sends its JSON arguments to the Rust
  handler and returns a promise of its result (`Err(message)` rejects). It works in the current
  documents and every later one (navigations, `set_content`), through `Runtime.addBinding` and
  `Runtime.bindingCalled`. Out-of-process iframes that attach later do not get it.
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
  extra headers and init scripts belong to the page. A `FrameLocator` is also Playwright's
  `Frame`: `frame.evaluate(function, arg)` calls `function(arg)` in the frame's document,
  `frame.wait_for_function(function, arg)` (and `_with_timeout`) polls it until it is truthy,
  and `frame.set_content(html)` replaces the document (`document.open/write/close`) and waits
  for `load`. Each waits, like a locator, for the frame and its document.

## End-to-end tests (`tests/e2e`)

`cargo test --test e2e` replaces the Playwright suite in `playwright/tests`, spec by spec: each
`tests/e2e/sNN_*.rs` is the former `NN-*.spec.mjs`, with the same test titles. The runner
(`browser_tests::e2e::main`) does what `playwright.config.mjs`, `global-setup.mjs`,
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

### Safety and lessons from the Playwright suite

* Every mutation happens inside the throwaway `pwtest-<run id>` organization (and the companion
  organizations a case creates), on a server with a temporary `DATA_DIR`. Teardown removes the
  artifacts first, then the organizations. Never point these tests at production or at a
  database with real organizations.
* Regression guards found by manual testing, each verified non-vacuous by putting the bug back:
  reaction, vote, share and comment wiring (a click must send its request *and* change the UI;
  this catches the truncated-`shell.js` class of bug), and a category assigned in the web UI must
  reach the Settings picker (it registers on the organization, not only on the artifact).
* A signed-out case must send no identity header at all: use `Identity::Anonymous`. Playwright's
  `browser.newContext()` inherited the default `extraHTTPHeaders`, so a "signed-out" test once
  kept sending the admin header and got 200.
* Ids that must be unique per run come from `unique_suffix()` or the run id, never from a counter
  that restarts per file (every file once asked for `-1` and got "already exists").
* A server behind an identity-injecting proxy can never fail a signed-out assertion. Test the
  application directly when you check authentication boundaries.
* Chrome 156 fails the requests a page makes itself (for example `fetch`) with
  `net::ERR_INVALID_ARGUMENT` when DevTools adds a forbidden header such as `Sec-Fetch-Site`
  through `Network.setExtraHTTPHeaders`. Navigations still work. Give pages identity and
  `X-Artifact-Mutation` headers only; the browser sends an accurate `Sec-Fetch-Site` itself.
* HTML5 drag and drop needs `page.drag_mouse()` (see above). Spec 14 fails with `page.mouse()`:
  the drag starts but nothing is ever dropped.

## Reader regressions (`tests/reader`)

`cargo test --test reader` replaces `playwright/reader-runner.mjs` and its 14 standalone
`reader-*.cjs` programs, one module per program with the same name and assertions. The runner
(`browser_tests::reader::main`) runs the cases one at a time, each with a fresh browser launched
with `--autoplay-policy=no-user-gesture-required` and `Network::LoopbackOnly`, 90 s per case and
10 minutes per run. A case returns its old report line (`PASS: ...` or a JSON summary), which is
printed under its result:

```text
test reader-pacing [rust] ... ok (15696 ms)
    PASS: pacing 0.95/0.62/1.82 s, media session, concurrent request, pauses off
```

The binary is required even for a focused run of a `chromium` case, as before: a missing build
fails in CI and is never a pass.

* **`chromium` cases** load `assets/reader-bridge.js` (and `reader-ereader.js`) from the working
  tree into plain pages and talk to the bridge with `postMessage`.
* **`rust` cases** call `ReaderCtx::fixture(Audio::seconds(..).latency(..))`
  (`createReaderFixture`): a stub speech worker in the test process and the release binary with
  text to speech on and every provider (`TTS`, `POCKET_TTS`, `QWEN_TTS`, `RAVEN_TTS`,
  `MOSS_TTS`) pointed at it, `ADMIN_EMAILS=reader-fixture@example.test`, `REQUIRE_ACCESS_JWT=0`,
  a random `ARTIFACT_API_KEYS` publisher key and the old fixture's ingress limits. Only the
  worker's HTTP boundary is a stub; the application's speech routes are real.
  `assert_speech_routes(id)` probes the voices, `/speech` (WAV), `/speech/stream` (PCM) and
  `/speech/stream-timed` (timed PCM) routes and the reader injection in the viewer and the raw
  document, then resets the worker's log.
* **The stub worker** (`Worker`) answers `POST /speech` with a 24 kHz mono 16-bit WAV of a sine,
  `/speech/stream` with length-prefixed silent PCM frames (at most 96 000 bytes each) and
  `/speech/stream-timed` with a JSON word-timing frame per word (0.3 s each) and silent audio
  frames, after the configured latency (`set_latency`, or `set_latency_for(|body| ..)` per
  request). It needs the bearer token from `*_TOKEN_FILE`, logs every request with its body and
  arrival time (`requests()`, `reset()`), and counts the most requests it served at once
  (`max_load()`). A handler whose client disconnects is dropped, so an aborted fetch ends the
  request at the worker too.
* `StubServer` is the small HTTP/1.1 server behind the worker; cases also use it to serve a page.

Pages from `ReaderCtx::page_with(viewport, headers)` leave out `Sec-Fetch-*` headers (see the
Chrome 156 note above); the old fixture sent `sec-fetch-site: same-origin`, which the browser
now sends by itself. There are no response replacements, model downloads, paid API calls or
homelab dependencies: temporary data, random keys, loopback addresses and free ports only.

| Case | Runtime | Assertions |
| --- | --- | --- |
| `reader-mixed-content` | chromium | Document order; excluded controls, navigation, timers and hidden details; details invalidation; nested regions without duplication; visual and chart descriptions; grouped statistics and lists; small table rows and captions; large table and code summaries; literal code selection; chart ordering within lists. |
| `reader-scopes` | chromium | View, section and detail scopes; fingerprints and scoped resume; mutations inside and outside scope; hidden views and active tabs; table, code and visual detail selection; target ordinals. |
| `reader-sections` | chromium | Chapter seek and fingerprint validation; changed-document resume rejection; chapter, nested-section and heading-delimited boundaries. Messages reach Rust through `expose_function`. |
| `reader-word-highlights` | chromium | Long and multiline selections; nested markup; chunk offsets (UTF-16) and accented words; passage-to-word highlighting; native selection clearing; persistent playback maps; mismatched timing fallback; highlight clearing; page scope; mutation invalidation. The page comes from a `StubServer`. |
| `reader-controls` | rust | Section scope and chapter-control visibility; section-end prefetch boundary; checkpointed sleep; background sleep deadline; paused rewind without new synthesis; preview checkpoint isolation; completed-chunk resume to Finished. |
| `reader-player-scopes` | rust | Viewer injection and current-view scope; outside and inside mutations; literal code detail playback; scoped checkpoints and detail resume; keyboard chart navigation; desktop and mobile overflow. |
| `reader-prefetch` | rust | Prefetch before playback ends; read-ahead of a later block during the first; at most 2 concurrent worker requests; actual scheduled audio buffers; transition gap below 100 ms; Stop aborts current and prefetched requests. |
| `reader-pronunciation` | rust | Enabled real voices; spoken replacements without changing source DOM; source-word highlights; first-audio diagnostics; sentence replay; pronunciation mutation invalidation; partial literal and whole-word overridden selections. |
| `reader-pacing` | rust | Scheduled silences after a heading (0.9 s), between paragraphs (0.6 s) and at an `<hr>` scene break (1.8 s), each within 0.25 s; Pauses Off; Media Session metadata and artwork while reading; an overlapping speech request accepted under worker concurrency. |
| `reader-wav` | rust | First-click WAV playback through `<audio>` and Reading state; pause offset; rewind and checkpoint offset; reload resume; paused state; completion and checkpoint deletion. |
| `reader-voice-cast` | rust | Cast-declared Voices section; per-role voice on tagged blocks; inline dialogue split into role segments; ereader heading summary and intro read once in the narrator voice; segment-bounded highlights; untagged artifacts show no Voices section. |
| `reader-place-sync` | rust | Full speed list; a new speed kept in the checkpoint; paused place uploaded to viewer state (the server copy must equal, `savedAt` and place, the place saved at Pause or the copy the 2 s save tick stores while the audio clock settles); a second browser with empty storage shows and plays the synced place at the synced speed; Finished clears local and server copies. |
| `reader-forward` | rust | Forward 30 s button label and disabled state; paused seek inside the current block without a state change; remainder carried into the next block. |
| `reader-read-ahead` | rust | At least 3 and at most 8 blocks read ahead during the first block; no block synthesized twice; at most 2 concurrent worker requests. |

Every case also asserts that the page reports no uncaught errors. The audio cases measure the
scheduled `AudioBufferSourceNode` starts and the `<audio>` element's clock in
`chrome-headless-shell`, which runs muted (`--mute-audio`).

## Feature coverage

Every user-reachable feature and whether a browser journey exercises it, inherited from the
Playwright harness inventory (originally derived from the 40 HTTP routes of the retired Node
server and the 19 frozen MCP tools; the Rust server serves the same routes). Status: **✅
covered**, **🔸 partial**, **⬜ not covered**.

| Area | Feature | Status | Notes |
|---|---|---|---|
| Gallery | Renders artifacts for the viewer's org; card title and description | ✅ | |
| Gallery | Category grouping and filter chips; favorites filter | ✅ | |
| Gallery | Thumbnails (`GET /thumbnails/:id`) | 🔸 | renderer disabled in the tests; placeholder path only |
| Gallery | View counts and analytics surface | 🔸 | asserted present, not value-checked |
| Gallery | Notification badge and `POST /notifications/seen` | ✅ | |
| Gallery | Organization switcher (an admin sees all orgs) | ✅ | |
| Viewer | Shell with sandboxed iframe | ✅ | `sandbox` present, no `allow-same-origin` |
| Viewer | Previous and next; download; open raw (`/raw/:id`, CSP sandbox header) | ✅ | |
| Viewer | Bundle artifact and subpath (`/raw/:id/*`) | ✅ | |
| Viewer | Revision history (`/:id/history`); restore (`POST /:id/restore`); historical revision (`/raw/:id/rev/:n`) | ✅ | 17 confirms the restore dialog |
| Reactions | Favorite (heart), upvote and downvote, persistence across reload | ✅ | a click must send its request and flip `aria-pressed` |
| Feedback | Add, list, threaded reply, resolve and reopen, delete | ✅ | |
| Feedback | Anchored feedback | 🔸 | composer and anchor painting (02, 18); coordinates on a bundle page are not covered |
| Sharing | Create (`never`, `24h`, date), list, revoke; public page without auth (`/s/:token`) | ✅ | |
| Sharing | Revoked or invalid token gives an indistinguishable 404 | ✅ | |
| Categories | Assign, appears in Settings, create and delete in Settings | ✅ | regression guard for the registration bug |
| Tenancy | Hide and unhide (`POST /:id/visibility`); move between orgs (admin) | ✅ | |
| Tenancy | Cross-org artifact concealed as 404; signed-out sign-in page; non-admin cannot reach Settings | ✅ | invariant 3 |
| Settings | Organizations, domains, explicit email members, org colour | ✅ | 17 covers the deletion confirmation |
| Settings | Publisher keys (secret shown once, revoke with in-app confirmation and CSRF header) | ✅ | |
| Settings | Add and delete webhook | ✅ | |
| Settings | Send test webhook | ⬜ | would post to a real Discord endpoint |
| Collections | Folders in every library view, dialogs, popovers, inline folders, gestures, MCP folders, create preview | ✅ | specs 10 to 16 |
| Performance | No separate asset requests, no needless re-sort, deferred discussion fetch, voice capability states, late inline shell | ✅ | spec 18 |
| Reader | Extraction scopes, highlights, playback controls, pacing, WAV, cast voices, place sync, read-ahead | ✅ | `tests/reader` |

The MCP surface (19 tools) is covered byte for byte by `../conformance` (the recorded goldens,
including the frozen `tools/list` golden), not here; the journeys use `publish_artifact` and the
collection tools as setup and through spec 15. Out of scope: the live production instance (the
tests run the release binary against throwaway data; a separate read-only smoke checks
production) and sending a test webhook to a real endpoint.
