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
