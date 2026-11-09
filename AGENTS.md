## Agent skills

### Issue tracker

Issues live in GitHub Issues for `AgentShelf-OSS/artifact-mcp`. See `docs/agents/issue-tracker.md`.

### Triage labels

The repository uses the default five-label triage vocabulary. See `docs/agents/triage-labels.md`.

### Domain docs

This is a single-context repository with `CONTEXT.md` and decisions under `docs/adr/`. See `docs/agents/domain.md`.

## Verify a change

Run the main workspace checks from the repository root:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy --workspace --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --doc --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo xtask historical-fixture --verify-against-committed
```

Run `cargo xtask gate` for all three workspaces. See `CONTRIBUTING.md` for browser prerequisites.
Use `cargo xtask gate --dry-run` to list commands, or `--skip-browser` to omit browser-tests explicitly.

- For a contract change, run `CONFORMANCE_RECORD=1 cargo test --test conformance` and review the golden diff.
- For a new migration, append to the ledger in `src/persistence/migrations.rs` and run `cargo xtask historical-fixture`.
- Register each new `tests/native/*.rs` file with a `mod` line in `tests/native/main.rs`; `tests/repo_guards.rs` checks this.
- Follow the dependency rules enforced by `tests/layering.rs`.
- Check MCP tool changes with `tests/mcp_surface.rs`, which detects tool contract drift.
- Follow "Changing an MCP tool contract" in the module docs at `src/mcp/tool_defs.rs` for deliberate legacy contract changes.

Read `CONTRIBUTING.md` for development rules and `CONTEXT.md` for domain facts.
