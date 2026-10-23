# ADR-0024: Retire the Node compatibility twin

- Status: Accepted
- Date: 2026-10-22
- Changes: ADR-0001 (the Node seam), and every parity rule that requires Node and Rust to agree

## Context

Production runs only the Rust server. The release workflow builds only `Dockerfile.rust`, a static
musl binary in a distroless image, and production installs that attested binary. Node is not
installed on the production host.

The repository still kept a second server in Node: `server.js` and `lib/`, about 14,200 lines. The
documentation called it the compatibility twin and test oracle. Every server feature was written
twice. In the 60 days before this decision, 48 commits changed `lib/` and 34 changed `src/`.

The twin no longer matched production. It stored reminders but did not send them, it had no audit
log, and it did not synchronize Cloudflare Access members.

Every CI job needed Node:

- `npm test` ran 654 tests, most of them against `lib/`.
- Conformance ran each case against both servers, and the goldens were recorded from Node.
- 27 Rust files (25 test files and 2 test modules in `src/`) started `node`. Most compared Rust
  with `lib/*.js` through `node -e`. With `REQUIRE_NODE_REFERENCE=1`, a missing Node failed the
  run.
- Playwright ran every browser journey against both servers.

## Decision

Rust is the only server implementation. Remove the Node twin.

- Delete `server.js`, `lib/`, the root `Dockerfile` (the Node image), the `start` and `dev` npm
  scripts, and the npm runtime dependencies (`better-sqlite3`, `express`, `jose`, `nanoid`).
- Delete the Node tests whose subject is `lib/` or `server.js`. Keep the Node tests for browser code
  in `assets/` and for the tooling scripts.
- The conformance runner starts only the Rust server, and `--record` records goldens from Rust.
  Rust passed all 47 cases against the goldens that Node recorded, so the committed goldens do not
  change.
- Delete the Rust tests that compared Rust with Node. A parity test can be the only check of a
  stored data format or a security decision. In that case, keep the last Node result as a literal
  value in a Rust-only test. These frozen values come from commit `7211de0`.
- Replace the Node HTTPS server that the JWKS transport test used with an in-process Rust TLS
  server (`tokio-rustls`, a dev-dependency at the version `reqwest` already locks). The test
  certificates move from `test/fixtures/` to `tests/fixtures/`.
- Move the browser-asset syntax check (`node --check` on every `assets/*.js`) from the Rust suite
  to `npm test`.
- Playwright runs against the Rust server only.
- Rename `Dockerfile.rust` to `Dockerfile`. Compose builds it as `artifact-mcp:local` by default,
  so the default tag no longer goes stale between releases.
- `Cargo.toml` is the only version source. `package.json` has no version, and the release workflow
  no longer compares the two files.
- Remove the historical fixture generator, because it ran `lib/migrations.js`. The committed
  fixtures stay frozen.

Then remove Node from the repository entirely. JavaScript remains only as browser code in
`assets/`, and as test bodies that exercise that code inside a real browser.

| Former Node user | Replacement |
| --- | --- |
| Conformance runner (`conformance/*.mjs`) | `tests/conformance`, a Cargo test target; `CONFORMANCE_RECORD=1` records |
| MCP surface check | `tests/mcp_surface`; `MCP_SURFACE_BASE=<ref>` prints the changed-path report |
| Historical fixture verifier | checks inside `tests/native/u50_historical_fixtures.rs` |
| Historical fixture generator | `cargo xtask historical-fixture`, which also verifies the committed corpus |
| Release scripts (`scripts/release/*.mjs`) | `cargo xtask release source-sbom`, `manifest`, `verify-manifest`, `verify-fixtures-in-image`; outputs are byte-identical |
| Operator scripts | `cargo xtask cf-access-setup`, `tunnel`, `publish-data-artifact`, `benchmark-durability` |
| `backup.sh`, `backup-coherence.py`, `web-push-keys.mjs` | `artifact-mcp backup`, `verify-backup`, `web-push-keys` |
| `preview-renderer/` (Node + Playwright) | the Rust `artifact-preview` binary driving a pinned Chrome for Testing `chrome-headless-shell` over a DevTools pipe |
| Browser-asset tests (`test/*.js`) | `browser-tests/`, which runs the same test bodies in headless Chrome from Rust |
| Playwright journeys and reader suites | Rust tests on the same harness |

The preview renderer keeps the same HTTP contract. It adds network defences that close
speculation-rules prefetch and WebRTC paths, which the Node renderer let through.

## Evidence

Before the removal, at commit `7211de0` on 2026-10-22:

| Check | Result |
| --- | --- |
| `REQUIRE_NODE_REFERENCE=1 cargo test --all-targets --locked` | 971 passed, 0 failed |
| `node conformance/runner.mjs --impl both` | Node 47 passed, Rust 47 passed |
| `npm test` | 654 passed |

After the removal, on the same day, with a `node` stub on `PATH` that fails and logs any call made by
the Rust tests:

| Check | Result |
| --- | --- |
| `cargo fmt --check` and `cargo clippy --all-targets --all-features --locked -- -D warnings` | clean |
| `cargo test --all-targets --locked` | 962 passed, 0 failed; the stub logged no call |
| `cargo test --doc --locked` | 5 passed |
| `node conformance/runner.mjs` | Rust 47 passed against the committed goldens |
| `npm test` | 188 passed, with no npm dependencies installed |
| `node scripts/check-mcp-surface.mjs --base 7211de0` | passed |
| `node scripts/verify-historical-fixtures.mjs` | all fixtures passed |
| Playwright main suite and reader regressions | 110 passed; 14 reader runs passed |

The Rust suite lost 52 test functions and gained 43. Most of the new ones hold the frozen values.

With no Node in the repository, on 2026-10-23, with the same `node` stub on `PATH`:

| Check | Result |
| --- | --- |
| `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | clean |
| `cargo test --workspace --all-targets --locked` (includes conformance, MCP surface, backup, xtask) | 1117 passed, 0 failed |
| `cargo test --doc --locked` | 5 passed |
| `cargo xtask historical-fixture --verify-against-committed` | 42 of 42 cases match |
| `preview-renderer`: fmt, clippy, `cargo test` with `CI=1` | 48 passed |
| `browser-tests`: fmt, clippy, `cargo test` with `CI=1` | 324 passed: 158 asset cases, 110 journeys, 14 reader cases, and harness tests |

Every former `npm test` case, Playwright title, and reader case maps to exactly one passing Rust
case. The stub logged no call.

## Consequences

- Each server feature is written once, in Rust.
- Parity sections in earlier ADRs (for example ADR-0012 and ADR-0023) no longer apply to Node.
  Their shared test vectors still apply to Rust.
- The project loses an independent second implementation. A Rust change that alters old behaviour
  is now caught only by the conformance goldens, the frozen values, the historical fixtures, and
  the Rust tests. Review a golden diff as a contract change.
- Many comments in `src/` cite `lib/*.js` or "Node". They describe the retired twin. To read those
  files, use `git show 7211de0:lib/<file>.js`.
- A new migration needs a boundary fixture: `cargo xtask historical-fixture` adds it.
  `boundary-v31` and `boundary-v36` were frozen from pre-release drafts and cannot be regenerated;
  the verifier lists their known draft objects.
- CI needs no `setup-node`. Repository tooling needs a Rust toolchain instead of Node.
- Callers outside this repository move with it. The homelab publish wrapper runs the built `xtask`
  binary, and deploy scripts call `cargo xtask release verify-manifest`.

## Follow-up work

- Cut CT222 over to the Rust preview renderer with `preview-renderer/DEPLOY-CT222.md`.
- Optionally rewrite the `src/` comments that cite the retired twin.
