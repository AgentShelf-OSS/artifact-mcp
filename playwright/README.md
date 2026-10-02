# Browser-level parity harness

The UI twin of `conformance/`. The conformance oracle compares Node and Rust **on the wire** — it
proved bytes match but could not prove the *page works*: a truncated `shell.js` was served
byte-faithfully, passed all 23 conformance cases, yet no button worked because the browser could
not parse the script. This harness drives a real browser against **both** implementations and
asserts behaviour a user would see.

## Run

    cd playwright
    npm install
    npx playwright install chrome   # once
    RUST_ARTIFACT_MCP_BIN=../target/release/artifact-mcp npm test

Two projects — `node` and `rust` — each boot their own server on a throwaway data dir with
header-trust identity and a seeded artifact (`server.mjs`), then run every spec against both.

## What it guards (both found by manual testing, both now fixed)

- **Reaction/vote/share/comment wiring** — a click must fire its request AND change UI state.
  Catches the truncated-`shell.js` class of bug (script parses on the server, dies in the browser).
- **Category → Settings visibility** — a category assigned through the web UI must reach the
  Settings picker (it must register on the org, not just the artifact).

Both regression guards were verified non-vacuous: reintroducing each bug makes the matching test
fail.

## The gap this closes

`cargo test` + `conformance --impl both` prove the servers emit identical bytes. They do NOT execute
page JavaScript. This is the only layer that does. Add a spec here for every interactive flow before
trusting it.

## Running against two live instances

The harness can drive two running, isolated servers rather than booting its own:

    PW_NODE_URL=http://127.0.0.1:3485 \
    PW_RUST_URL=http://127.0.0.1:3483 \
    PW_RUN_ID=$(date +%H%M%S) \
    PW_ADMIN_EMAIL=admin@example.test \
    npx playwright test -c playwright.config.mjs

Use separate data directories for the Node reference runtime and the Rust release candidate.
Both need a disposable `AUDIT_LEDGER_HMAC_KEY`; use a separate disposable
`WEBHOOK_ENC_KEY` when exercising encrypted delivery configuration. Raise the Rust `INGRESS_*`
per-window budgets only for this isolated browser run so the complete deterministic suite does
not exhaust production-oriented defaults.

## Safety

Every mutation happens inside a throwaway `pwtest-<runid>` organization, created in global setup and
deleted in teardown (artifacts removed first). Never point this harness at production or a database
containing real organizations.

## Gotchas this harness already hit

- `browser.newContext()` INHERITS `use.extraHTTPHeaders`, so a "signed-out" test silently kept
  sending the admin header and got 200. Pass `extraHTTPHeaders: {}` explicitly.
- A module-level counter for unique ids resets per spec file — every file asked for `-1` and got
  "already exists". Randomise instead.
- `BrowserContext.close()`, not `.dispose()` (that is `APIRequestContext`).
- An instance behind an identity-injecting proxy can never fail a signed-out assertion. Test the
  application directly when validating authentication boundaries.

## Reader regressions

The nine standalone `reader-*.cjs` programs have a separate runner. From the repository root:

```sh
npm ci
cargo build --release --locked
npm --prefix playwright ci
npm --prefix playwright exec -- playwright install --with-deps chromium
npm --prefix playwright run test:reader
```

Use Node 22 or newer and an executable Rust release binary. Set `RUST_ARTIFACT_MCP_BIN` to an
absolute path to test an existing build. A missing runtime fails the command instead of skipping
its checks. The runner uses bundled Chromium and requires Linux or macOS for process cleanup.
CI runs this command after the regular browser parity suite.

Inspect the complete execution matrix or focus on one program:

```sh
npm --prefix playwright run test:reader -- --list
npm --prefix playwright run test:reader -- --case reader-pronunciation
```

The full command runs 14 executions with no skips. Four programs test shared JavaScript assets
inside Chromium once. Five programs publish synthetic artifacts and exercise the real viewer and
speech routes against both Node and Rust. Extraction, playback and scope assertions stay in their
original programs. Repeated publish-ID and enabled-voice checks move into the shared fixture,
alongside duplicated server, browser and audio setup. No program is dropped or merged with another.
The two WAV-load readiness checks become one bounded first-click readiness and Reading-state wait.
Checkpoint checks use the current version-2 `current.offset` field.

| Program | Runtime | Assertions retained |
| --- | --- | --- |
| `reader-mixed-content` | Chromium | Document order; excluded controls, navigation, timers and hidden details; details invalidation; nested regions without duplication; visual and chart descriptions; grouped statistics and lists; small table rows and captions; large table and code summaries; literal code selection; chart ordering within lists. |
| `reader-scopes` | Chromium | View, section and detail scopes; fingerprints and scoped resume; mutations inside and outside scope; hidden views and active tabs; table, code and visual detail selection; target ordinals. |
| `reader-sections` | Chromium | Chapter seek and fingerprint validation; changed-document resume rejection; chapter, nested-section and heading-delimited boundaries. |
| `reader-word-highlights` | Chromium | Long and multiline selections; nested markup; chunk offsets and accented words; passage-to-word highlighting; native selection clearing; persistent playback maps; mismatched timing fallback; highlight clearing; page scope; mutation invalidation. |
| `reader-controls` | Node + Rust | Section scope and chapter-control visibility; section-end prefetch boundary; checkpointed sleep; background sleep deadline; paused rewind without new synthesis; preview checkpoint isolation; completed-chunk resume to Finished. |
| `reader-player-scopes` | Node + Rust | Viewer injection and current-view scope; outside and inside mutations; literal code detail playback; scoped checkpoints and detail resume; keyboard chart navigation; desktop and mobile overflow. |
| `reader-prefetch` | Node + Rust | Prefetch before playback ends; bounded lookahead; actual scheduled audio buffers; transition gap below 100 ms; Stop aborts current and prefetched requests. |
| `reader-pronunciation` | Node + Rust | Enabled real voices; spoken replacements without changing source DOM; source-word highlights; first-audio diagnostics; sentence replay; pronunciation mutation invalidation; partial literal and whole-word overridden selections. |
| `reader-wav` | Node + Rust | First-click WAV playback and Reading state; pause offset; rewind and checkpoint offset; reload resume; paused state; completion and checkpoint deletion. |

Every program also asserts that Chromium reports no uncaught page errors. The fixture probes
enabled voices, WAV, PCM and timed PCM through the application routes, and checks reader injection
in the viewer and artifact iframe. A local HTTP worker supplies deterministic audio. There are no
browser response replacements, speech-model downloads, paid API calls or homelab dependencies.

Each server uses temporary data, random disposable keys, loopback addresses and dynamically
allocated ports. The runner removes its temporary directory after success or failure. It bounds
each program to 90 seconds and the entire run to 10 minutes, and stops owned server and Chromium
processes on failure or interruption. Startup health checks and browser launch have separate
15-second deadlines. Assertion failures propagate to the npm command and fail the browser CI job.
