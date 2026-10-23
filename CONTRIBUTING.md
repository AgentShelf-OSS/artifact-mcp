# Contributing

Thanks for your interest in artifact-mcp. Issues, ideas, and pull requests are welcome.

## Ground rules

- **Open an issue first** for anything non-trivial, so we can agree on the approach before you build.
  Small fixes (typos, obvious bugs) can go straight to a PR.
- **Keep the security model intact.** This is a multi-tenant server; tenant isolation, the fail-closed
  identity default, the sandboxed rendering, and the SSRF-limited webhooks are load-bearing. If a
  change touches any of them, call it out explicitly in the PR.
- **No new runtime dependencies** without discussion. The server is one Rust binary (Axum, Askama,
  rusqlite) and should stay small and easy to audit.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --locked   # includes HTTP conformance and the MCP surface check
cargo test --doc --locked
cargo xtask historical-fixture --verify-against-committed

# Browser-asset tests in chrome-headless-shell (see browser-tests/README.md)
export CHROME_HEADLESS_SHELL=/path/to/chrome-headless-shell-linux64/chrome-headless-shell
cargo fmt --manifest-path browser-tests/Cargo.toml --check
cargo clippy --manifest-path browser-tests/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path browser-tests/Cargo.toml --locked
```

- `rust-toolchain.toml` pins the Rust toolchain. Repository tooling is `cargo xtask` (run
  `cargo xtask --help`). The browser-asset tests in `browser-tests/` run the JavaScript in real
  Chrome for Testing, driven from Rust; use the version pinned in `preview-renderer/Dockerfile`.
  Without `CHROME_HEADLESS_SHELL` they are skipped. The browser journeys (`--test e2e`) and reader
  regressions (`--test reader`) in the same crate also need `ARTIFACT_MCP_BIN`. The repository
  needs no Node (ADR-0024).
- Changed browser JavaScript in `assets/` needs a test in `browser-tests/js/` when its behaviour
  changes; `browser-assets-parse` checks that every asset still parses.
- Match the surrounding style: small modules behind the traits in `src/ports/`, and production or
  test adapters passed to `build_router()` through `AppDeps`.
- Add or update tests for any behavior change. Prefer the deterministic adapters in `tests/native/`
  over anything that needs real sockets or a real Cloudflare.
- Schema changes go through a new versioned migration in `src/persistence/migrations.rs`
  (append-only; never edit a shipped migration).
- A change to HTTP or MCP behavior can change a conformance golden. Record it with
  `CONFORMANCE_RECORD=1 cargo test --test conformance` and review the golden diff as a contract
  change.
- A new migration needs a boundary fixture: `cargo xtask historical-fixture` adds it.

## Pull requests

- Keep PRs focused — one concern each.
- Describe **what** changed, **why**, and **how you verified it** (tests, manual steps).
- Confirm the commands in [Development](#development) are green.
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
