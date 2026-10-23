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
- `Cargo.toml` is the only version source. `package.json` has no version, and the release workflow
  no longer compares the two files.
- Remove the historical fixture generator, because it ran `lib/migrations.js`. The committed
  fixtures stay frozen.

Node stays only where it is the correct tool:

- unit tests for the browser code in `assets/`;
- tooling scripts: the release manifest and SBOM, the MCP surface check, the Anthropic tunnel, the
  Cloudflare Access setup, data publishing, and the OCI durability benchmark;
- the conformance HTTP runner, which drives language-neutral cases and goldens;
- Playwright browser tests;
- the separate `preview-renderer/` service, which uses Playwright Chromium.

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

## Consequences

- Each server feature is written once, in Rust.
- Parity sections in earlier ADRs (for example ADR-0012 and ADR-0023) no longer apply to Node.
  Their shared test vectors still apply to Rust.
- The project loses an independent second implementation. A Rust change that alters old behaviour
  is now caught only by the conformance goldens, the frozen values, the historical fixtures, and
  the Rust tests. Review a golden diff as a contract change.
- Many comments in `src/` cite `lib/*.js` or "Node". They describe the retired twin. To read those
  files, use `git show 7211de0:lib/<file>.js`.
- The next schema migration is blocked until a Rust-side fixture generator exists.
  `tests/native/u50_historical_fixtures.rs` requires one boundary fixture for every schema
  version, and the removed generator was the only tool that made them.

## Follow-up work

- Add a Rust-side generator for historical fixtures. The next migration needs it.
- Rename `Dockerfile.rust` to `Dockerfile`, and update the release workflow, Compose, and docs.
- Optionally port the release `.mjs` scripts to a `cargo xtask`.
- Optionally rewrite the `src/` comments that cite the retired twin.
