# Browser-level parity harness

The UI twin of `conformance/`. The conformance oracle compares Node and Rust **on the wire** — it
proved bytes match but could not prove the *page works*: a truncated `shell.js` was served
byte-faithfully, passed all 23 conformance cases, yet no button worked because the browser could
not parse the script. This harness drives a real browser against **both** implementations and
asserts behaviour a user would see.

## Run

The Playwright suite does not start the application servers. Build the Rust release binary, create
separate throwaway data directories, and start Node on `3485` and Rust on `3483` with
`LISTEN_HOST=127.0.0.1`, `TRUST_ACCESS_HEADERS=1`, `ADMIN_EMAILS=admin@example.test`, matching
`PUBLIC_BASE_URL`, an empty
`PREVIEW_RENDERER_URL`, and disposable test-only `AUDIT_LEDGER_HMAC_KEY` and `WEBHOOK_ENC_KEY`.
Use the elevated isolated-run `INGRESS_*` budgets shown in the CI browser job. The complete launch,
health-check, and cleanup recipe is in [`ci.yml`](../.github/workflows/ci.yml#L101-L156).

Then, from the repository root, install dependencies and run both configured projects against those
instances:

    cargo build --release --locked
    npm --prefix playwright ci
    npm --prefix playwright exec -- playwright install --with-deps chromium
    PW_NODE_URL=http://127.0.0.1:3485 \
      PW_RUST_URL=http://127.0.0.1:3483 \
      PW_ADMIN_EMAIL=admin@example.test \
      PW_RUN_ID=local-$(date +%s) \
      PW_USE_BUNDLED_CHROMIUM=1 \
      bash -c 'cd playwright && npm test'

The `node` and `rust` projects run the same specs against the two already-running servers. The
global setup creates a throwaway organization in each instance; teardown removes it. Never point
the suite at production data.

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
