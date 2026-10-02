# Contributing

Thanks for your interest in artifact-mcp. Issues, ideas, and pull requests are welcome.

## Ground rules

- **Open an issue first** for anything non-trivial, so we can agree on the approach before you build.
  Small fixes (typos, obvious bugs) can go straight to a PR.
- **Keep the security model intact.** This is a multi-tenant server; tenant isolation, the fail-closed
  identity default, the sandboxed rendering, and the SSRF-limited webhooks are load-bearing. If a
  change touches any of them, call it out explicitly in the PR.
- **No new runtime dependencies** without discussion. The production server is an Axum/SQLite Rust
  binary; the ESM Node + better-sqlite3 implementation is a dependency-light reference runtime
  with no build step.

## Development

The production server is Rust and uses the repository-pinned `1.97.1` toolchain. The Node
implementation is retained as a reference runtime and conformance oracle; it is useful for
behavioral comparison and tests, but it is not the production deployment target.

```bash
npm ci
npm test                                      # Node reference suites
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo test --doc --locked
```

- Node 22+ is required for the reference runtime and its tests. It has no build step and runs from
  source. Use `npm ci` for the lockfile-resolved dependencies; do not infer a Node production
  deployment from its presence in this repository.
- Rust uses `rust-toolchain.toml` (`1.97.1`, with `rustfmt`, `clippy`, and the musl release target).
  Native Linux builds need a C compiler, build tools, and CMake. Release-like local builds should
  use `cargo build --release --locked`.
- The shared MCP contract can be checked against both implementations:

  ```bash
  cargo build --release --locked
  node conformance/runner.mjs --impl both
  ```

- Browser-facing changes require the isolated Playwright checks. The Playwright command does not
  start either server. First build Rust, install the pinned browser dependencies, and start separate
  Node and Rust instances on ports `3485` and `3483`, each with a temporary `DATA_DIR`,
  `LISTEN_HOST=127.0.0.1`, `TRUST_ACCESS_HEADERS=1`, `ADMIN_EMAILS=admin@example.test`, matching `PUBLIC_BASE_URL`, an empty
  `PREVIEW_RENDERER_URL`, and disposable test-only `AUDIT_LEDGER_HMAC_KEY` and `WEBHOOK_ENC_KEY`.
  Give the Rust instance the elevated isolated-run `INGRESS_*` budgets used by CI. Then run:

  ```bash
  npm --prefix playwright ci
  npm --prefix playwright exec -- playwright install --with-deps chromium
  export PW_NODE_URL=http://127.0.0.1:3485 PW_RUST_URL=http://127.0.0.1:3483
  export PW_ADMIN_EMAIL=admin@example.test PW_RUN_ID=local-$(date +%s)
  export PW_USE_BUNDLED_CHROMIUM=1
  (cd playwright && npm test)
  ```

  The complete server launch and cleanup sequence is in the `browser` job in
  `.github/workflows/ci.yml`. Do not point these checks at a live data directory.
- Match the surrounding style: small modules, a DI factory (`createApp`, `createArtifactStore`), and
  the module-singleton pattern already in `lib/`; Rust keeps composition in `src/main.rs` and
  router construction in `src/app.rs`.
- Add or update tests for any behavior change. Prefer the existing DI/in-memory harness in `test/`
  and Rust test fixtures over anything that needs real sockets or a real Cloudflare. Use the
  conformance runner when a behavior must remain equivalent between runtimes, and Playwright when
  the browser shell, raw delivery, or navigation behavior changes.
- Schema changes go through a new versioned migration in `src/persistence/migrations.rs`, with the
  corresponding Node reference step in `lib/migrations.js`. Both ledgers are ordered and append-only;
  never edit a shipped migration.

## Pull requests

- Keep PRs focused — one concern each.
- Describe **what** changed, **why**, and **how you verified it** (Rust checks, Node reference
  checks, conformance, browser checks, or manual steps as applicable).
- Confirm the relevant Rust checks are green. For behavior shared with Node, also confirm `npm test`
  and `node conformance/runner.mjs --impl both`; for browser-facing behavior, include the Playwright
  result.
- By contributing, you agree your contributions are licensed under the project's Apache License 2.0.

## Sign your commits (DCO)

This project uses the [Developer Certificate of Origin](DCO) — a lightweight statement that you
wrote, or have the right to submit, the code you're contributing. No paperwork; you certify it by
adding a `Signed-off-by` line to each commit.

Add it automatically with the `-s` flag:

```bash
git commit -s -m "Your message"
```

That appends a trailer matching your Git `user.name` / `user.email`:

```
Signed-off-by: Jane Doe <jane@example.com>
```

Every commit in a pull request must be signed off — CI checks it. Forgot on an existing branch?
Re-sign the last commit with `git commit -s --amend`, or a range with
`git rebase --signoff <base>`, then force-push.

## Reporting security issues

Do **not** open a public issue for a vulnerability. See [`SECURITY.md`](SECURITY.md) for private
disclosure.

## Where to start

Look for issues labeled **good first issue**. `CONTEXT.md` explains the domain language, invariants,
and module seams — read it before a larger change.
