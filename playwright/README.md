# Browser-level harness

The UI companion to `conformance/`. The conformance oracle checks the server **on the wire**: it
proves bytes match the goldens but cannot prove the *page works*. A truncated `shell.js` was once
served byte-faithfully and passed every conformance case, yet no button worked because the
browser could not parse the script. This harness drives real Chromium against a running Rust
server and asserts behaviour a user would see.

It started as a parity harness that ran every spec against both the retired Node reference server
and the Rust server. It now has one Playwright project, `rust`.

## Run

Start an isolated Rust server, then point the harness at it. CI uses this shape (see the
`browser` job in `.github/workflows/ci.yml`):

    cargo build --release --locked
    data="$(mktemp -d)"
    test_key=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
    env DATA_DIR="$data" PORT=3483 LISTEN_HOST=127.0.0.1 \
      TRUST_ACCESS_HEADERS=1 ADMIN_EMAILS=admin@example.test NODE_ENV=test \
      PUBLIC_BASE_URL=http://127.0.0.1:3483 PREVIEW_RENDERER_URL= \
      AUDIT_LEDGER_HMAC_KEY="$test_key" WEBHOOK_ENC_KEY="$test_key" \
      INGRESS_READS_PER_WINDOW=10000 INGRESS_MUTATIONS_PER_WINDOW=10000 \
      INGRESS_MCP_PER_WINDOW=10000 INGRESS_UPLOADS_PER_WINDOW=10000 \
      INGRESS_FEEDBACK_PER_WINDOW=10000 INGRESS_STATE_PER_WINDOW=10000 \
      INGRESS_ADMIN_PER_WINDOW=10000 INGRESS_VERIFIED_VIEWERS_PER_WINDOW=10000 \
      INGRESS_SHARES_PER_WINDOW=10000 INGRESS_AUTH_FAILURES_PER_WINDOW=10000 \
      target/release/artifact-mcp &

    cd playwright
    npm ci
    npx playwright install chromium   # once
    PW_RUST_URL=http://127.0.0.1:3483 \
    PW_RUN_ID=$(date +%H%M%S) \
    PW_ADMIN_EMAIL=admin@example.test \
    PW_USE_BUNDLED_CHROMIUM=1 \
    npm test

`PW_RUST_URL` defaults to `http://127.0.0.1:3483`. Without `PW_USE_BUNDLED_CHROMIUM=1` the
harness uses the installed Chrome channel. Use a fresh data directory and disposable
`AUDIT_LEDGER_HMAC_KEY` and `WEBHOOK_ENC_KEY` values. Raise the `INGRESS_*` per-window budgets
only for this isolated browser run so the complete deterministic suite does not exhaust
production-oriented defaults.

## What it guards (both found by manual testing, both now fixed)

- **Reaction/vote/share/comment wiring** — a click must fire its request AND change UI state.
  Catches the truncated-`shell.js` class of bug (script parses on the server, dies in the browser).
- **Category → Settings visibility** — a category assigned through the web UI must reach the
  Settings picker (it must register on the org, not just the artifact).

Both regression guards were verified non-vacuous: reintroducing each bug makes the matching test
fail.

## The gap this closes

`cargo test` and the conformance oracle prove the server emits the expected bytes. They do NOT
execute page JavaScript. This is the only layer that does. Add a spec here for every interactive
flow before trusting it.

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

The standalone `reader-*.cjs` programs have a separate runner. From the repository root:

```sh
cargo build --release --locked
npm --prefix playwright ci
npm --prefix playwright exec -- playwright install --with-deps chromium
npm --prefix playwright run test:reader
```

Use Node 22 or newer and an executable Rust release binary. Set `RUST_ARTIFACT_MCP_BIN` to an
absolute path to test an existing build. A missing binary fails the command instead of skipping
its checks. The runner uses bundled Chromium and requires Linux or macOS for process cleanup.
CI runs this command after the regular browser suite.

Inspect the complete execution matrix or focus on one program:

```sh
npm --prefix playwright run test:reader -- --list
npm --prefix playwright run test:reader -- --case reader-pronunciation
```

The full command runs 14 executions with no skips. Four programs test shared JavaScript assets
inside Chromium. Ten programs publish synthetic artifacts and exercise the real viewer and speech
routes against the Rust server. Extraction, playback and scope assertions stay in their
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
| `reader-controls` | Rust server | Section scope and chapter-control visibility; section-end prefetch boundary; checkpointed sleep; background sleep deadline; paused rewind without new synthesis; preview checkpoint isolation; completed-chunk resume to Finished. |
| `reader-player-scopes` | Rust server | Viewer injection and current-view scope; outside and inside mutations; literal code detail playback; scoped checkpoints and detail resume; keyboard chart navigation; desktop and mobile overflow. |
| `reader-prefetch` | Rust server | Prefetch before playback ends; read-ahead of a later block during the first; at most 2 concurrent worker requests; actual scheduled audio buffers; transition gap below 100 ms; Stop aborts current and prefetched requests. |
| `reader-pronunciation` | Rust server | Enabled real voices; spoken replacements without changing source DOM; source-word highlights; first-audio diagnostics; sentence replay; pronunciation mutation invalidation; partial literal and whole-word overridden selections. |
| `reader-pacing` | Rust server | Scheduled silences after a heading, between paragraphs and at an `<hr>` scene break; Pauses Off; Media Session metadata and artwork while reading; an overlapping speech request accepted under worker concurrency. |
| `reader-wav` | Rust server | First-click WAV playback and Reading state; pause offset; rewind and checkpoint offset; reload resume; paused state; completion and checkpoint deletion. |
| `reader-voice-cast` | Rust server | Cast-declared Voices section; per-role voice on tagged blocks; inline dialogue split into role segments; ereader heading summary and intro read once in the narrator voice; untagged artifacts show no Voices section. |
| `reader-place-sync` | Rust server | Full speed list; a new speed kept in the checkpoint; paused place uploaded to viewer state; a second browser with empty storage shows and plays the synced place at the synced speed; Finished clears local and server copies. |
| `reader-forward` | Rust server | Forward 30 s button label and disabled state; paused seek inside the current block without a state change; remainder carried into the next block. |
| `reader-read-ahead` | Rust server | At least 3 and at most 8 blocks read ahead during the first block; no block synthesized twice; at most 2 concurrent worker requests. |

Every program also asserts that Chromium reports no uncaught page errors. The fixture probes
enabled voices, WAV, PCM and timed PCM through the application routes, and checks reader injection
in the viewer and artifact iframe. A local HTTP worker supplies deterministic audio. There are no
browser response replacements, speech-model downloads, paid API calls or homelab dependencies.

The server uses temporary data, random disposable keys, loopback addresses and dynamically
allocated ports. The runner removes its temporary directory after success or failure. It bounds
each program to 90 seconds and the entire run to 10 minutes, and stops owned server and Chromium
processes on failure or interruption. Startup health checks and browser launch have separate
15-second deadlines. Assertion failures propagate to the npm command and fail the browser CI job.
