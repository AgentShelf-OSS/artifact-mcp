# Fixtures

Each fixture is copied into a fresh `DATA_DIR` for every test/runtime. The checked-in source is
never mounted read-write by tests or release checks.

## Historical migration and recovery corpus

`historical/` contains immutable, synthetic SQLite-aware snapshots for every legitimate migration
boundary, `boundary-v00` through `LATEST_SCHEMA_VERSION` (`src/persistence/migrations.rs`), plus
populated snapshots matching the distinct public release terminals: v1.2/v1.3 (16), v1.4 (20),
v1.5 (21), and v1.6 (23), and the current-ledger durability-recovery case (24). Boundary snapshots
are created by running the real migration ledger through that version; they are never
manufactured by deleting rows from a newer database. `v00` is a populated pre-ledger layout.

Every case has a `fixture.json` with its source ref, target schema, database SHA-256 and complete
body manifest. The rich cases cover single-file and bundle bodies, retained history, share
lifecycle states, resolved/unresolved feedback, plaintext and synthetic AES-GCM webhook rows, and
deterministic recovery states. The only values resembling credentials are explicitly public,
synthetic test material (`fixture-public-token-v1` and a zero-byte fixture encryption key); no
production data or secrets are included.

The corpus is frozen. Check its immutable bytes and pre-migration SQLite integrity with:

```sh
cargo test --locked --test native u50
cargo xtask historical-fixture --verify-against-committed
```

The first command boots every fixture and checks its manifest, digests, and that verification
leaves the source untouched. The second regenerates every reproducible fixture and compares it
with the committed one.

### Adding the fixture for a new migration

Every schema version needs one boundary fixture; `u50_historical_fixtures` fails until it exists.
After you append migration `NN` and set `LATEST_SCHEMA_VERSION = NN`, run:

```sh
cargo xtask historical-fixture
```

The command adds every missing `historical/boundary-vNN`, prints what it wrote, and keeps every
existing case. It runs the current ledger through `NN` with `migrations::migrate_through` (the
same steps as startup, stopped at `NN`), inserts the synthetic rows only into the tables and
columns that exist at that schema, writes the body files, and records every written byte in
`fixture.json`. Commit the new directory unchanged.

Options:

- `--schema N` or `--case NAME` selects one case. `release-v24-durability-recovery` is the only
  release case that the current ledger can rebuild. The four tag-built release cases came from
  each tag's Node `lib/migrations.js` and cannot be regenerated.
- `--out DIR` writes below `DIR` instead of `historical/`.
- `--replace-frozen-fixtures` rewrites a selected case that already exists. Existing fixtures are
  evidence, so do this only after a provenance review.
- `--verify-against-committed` regenerates every boundary and the durability-recovery case into a
  scratch directory (or `--out DIR`) and compares each one with the committed fixture. It exits
  non-zero on any unexpected difference.

The verify mode compares:

- the file tree;
- every body byte for byte;
- `fixture.json` byte for byte, except the provenance fields `generation`,
  `origin.sourceRef.migrationLedgerSha256`, `expectedTargetSchema`, and `database.sha256`. Manifests
  older than `boundary-v25` also lack `expectedRecovery.remainingIntentIds`, which the generator
  always writes as `[]`;
- `artifacts.db` logically: the header fields that describe the database, `integrity_check`,
  every `sqlite_master` row in creation order, and every table row in rowid order.

SQL text that differs only in whitespace is accepted, because the Rust ledger re-indents some of
Node's statements. The comparison masks only clock-filled `DEFAULT (datetime('now'))` columns that
the generator never writes, such as `schema_migrations.applied_at` and `artifacts.created_at`.

`boundary-v31` and `boundary-v36` were frozen from pre-release drafts of migrations 31 and 36.
Their differing objects (`discord_inbound_events`, `discord_inbound_message_state` and its two
feedback triggers, and `data_sources` with its index) are listed in `xtask/src/verify.rs` and
reported separately. The final Node ledger at `7211de0` has the same DDL as the Rust ledger, so
the old Node generator could not reproduce these objects either.

Provenance changed with the port. Fixtures written by the retired Node generator,
`scripts/generate-historical-fixtures.mjs` (last present at commit `7211de0`), have
`"generation": "node scripts/generate-historical-fixtures.mjs"`, and their
`origin.sourceRef.migrationLedgerSha256` is the SHA-256 of `lib/migrations.js`. Fixtures written by
`cargo xtask historical-fixture` have `"generation": "cargo xtask historical-fixture"`, and the
same field is the SHA-256 of `src/persistence/migrations.rs`. All other rows, bodies, constants,
and manifest fields are unchanged.

`cargo test --test native u50_historical_fixtures` checks the Rust production boot,
reconciliation, digest backfill, authenticated read, write, update, restore, share lifecycle,
and encrypted-webhook delivery. The release workflow additionally runs the same corpus through
the exact candidate OCI digest and attaches a checksummed report.

## `empty-v21`

The older conformance fixture remains an intentionally empty baseline used by language-neutral
contract cases. Historical compatibility is proven by `historical/`, not by this placeholder.
