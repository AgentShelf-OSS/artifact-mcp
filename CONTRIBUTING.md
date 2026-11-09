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
cargo clippy --workspace --locked -- -D warnings
cargo test --workspace --all-targets --locked   # server, HTTP conformance, MCP surface, and xtask
cargo test --doc --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo xtask historical-fixture --verify-against-committed

# Preview renderer workspace (see browser-tests/README.md for the pinned browser)
export CHROME_HEADLESS_SHELL=/path/to/chrome-headless-shell-linux64/chrome-headless-shell
cargo fmt --manifest-path preview-renderer/Cargo.toml --check
cargo clippy --manifest-path preview-renderer/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path preview-renderer/Cargo.toml --locked
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path preview-renderer/Cargo.toml --no-deps --locked

# Browser-asset tests in chrome-headless-shell (see browser-tests/README.md)
cargo fmt --manifest-path browser-tests/Cargo.toml --check
cargo clippy --manifest-path browser-tests/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path browser-tests/Cargo.toml --locked
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path browser-tests/Cargo.toml --no-deps --locked
```

Run `cargo xtask gate` for the checks above in order, stopping at the first failure.
It sets `CI=1` for both isolated workspaces so browser tests cannot silently skip.
Set `CHROME_HEADLESS_SHELL` to the pinned browser and `ARTIFACT_MCP_BIN` to a server binary
built with `cargo build --release --locked` before running the full gate.
`cargo xtask gate --dry-run` prints every command without running checks or requiring those variables.
`cargo xtask gate --skip-browser` explicitly skips browser-tests; preview-renderer still needs Chrome
and the unprivileged user namespaces required by CI. See `.github/workflows/ci.yml`.

- `rust-toolchain.toml` pins the Rust toolchain. Repository tooling is `cargo xtask` (run
  `cargo xtask --help`). `preview-renderer/` and `browser-tests/` are separate Cargo workspaces.
  The browser tests use the Chrome for Testing `chrome-headless-shell` version pinned in
  `preview-renderer/Dockerfile`. Set `CHROME_HEADLESS_SHELL` to run them. The browser journeys
  (`--test e2e`) and reader regressions (`--test reader`) also need `ARTIFACT_MCP_BIN`.
- Changed browser JavaScript in `assets/` needs a test in `browser-tests/js/` when its behaviour
  changes; `browser-assets-parse` checks that every asset still parses.
- The layering test guards dependency direction; add a file, module, and reason to its allowlist only for an accepted exception.
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

## Add an artifact metadata field

Keep private metadata private. Review each public projection below and add the field only where
its contract needs it.

- Append the schema change to the ledger in `src/persistence/migrations.rs` and implement it in
  a migration module under `src/persistence/migrations/`. Never edit a shipped migration.
- Add the field and its test default to `ArtifactMeta` and `ArtifactMeta::fixture` in
  `src/model/artifact.rs`. Add it to `ArtifactRevision` there if revisions retain it.
- Update `META_COLUMNS` and `meta_from_row` in `src/persistence/artifact_rows.rs`, including the
  reordered-column and invalid-counter tests when applicable.
- Update artifact writes in `src/artifacts/lifecycle/publish.rs` and
  `src/artifacts/lifecycle/update.rs`. For mutable fields, update the `MetaBefore` snapshot in
  `src/artifacts/lifecycle/mod.rs`, its construction in `src/artifacts/lifecycle/update.rs`, and
  the rollback UPDATE in `src/artifacts/lifecycle/body_swap.rs`. Review dedicated metadata writes
  in `src/artifacts/lifecycle/metadata.rs` and `src/artifacts/lifecycle/org_move.rs`. For retained
  fields, update revision columns, mapping, and writes in `src/artifacts/lifecycle/sql.rs`, and
  restore inputs in `src/artifacts/lifecycle/trash.rs`. Review field-specific backfills in
  `src/artifacts/reconciliation/backfill.rs` and `src/persistence/keys/operations.rs` if affected.
- Review MCP list/read projections in `src/mcp/tools/artifacts.rs`, shared projections in
  `src/mcp/shape.rs`, publish/update responses in `src/mcp/tools/publish.rs` and
  `src/mcp/tools/update.rs`, and collection references in `src/mcp/collections.rs`. Update their
  contracts in `conformance/mcp.tool-output-schemas.json` for any output change.
- Review MCP resource metadata in `src/mcp/resources.rs` and review-card metadata in
  `src/mcp/decorate.rs`.
- Review HTTP library JSON in `src/http/routes/gallery.rs`, collection previews in
  `src/http/routes/collections.rs`, and metadata responses in `src/http/routes/artifact.rs`,
  including `RevisionResponse` for retained fields. Review visible fields in
  `src/render/portal/gallery.rs` and `src/render/portal/shell.rs`.
- Review delivery payload snapshots in `src/artifacts/lifecycle/context.rs` and
  `src/persistence/feedback_delivery.rs` if notifications expose the field.
- Run `cargo xtask historical-fixture` to add the new migration boundary fixture. Record affected
  goldens with `CONFORMANCE_RECORD=1 cargo test --test conformance` and review the diff as a
  contract change.

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
