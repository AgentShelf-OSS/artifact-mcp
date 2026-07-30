# Fixtures

Each fixture is copied into a fresh `DATA_DIR` for every test/runtime. The checked-in source is
never mounted read-write by tests or release checks.

## Historical migration and recovery corpus

`historical/` contains immutable, synthetic SQLite-aware snapshots for every legitimate migration
boundary `v00` through `v23`, plus populated snapshots matching the distinct public release
terminals: v1.2/v1.3 (16), v1.4 (20), v1.5 (21), and v1.6 (23). Boundary snapshots are created by
running the real migration ledger through that version; they are never manufactured by deleting
rows from a newer database. `v00` is a populated pre-ledger layout.

Every case has a `fixture.json` with its source ref, target schema, database SHA-256 and complete
body manifest. The rich cases cover single-file and bundle bodies, retained history, share
lifecycle states, resolved/unresolved feedback, plaintext and synthetic AES-GCM webhook rows, and
deterministic recovery states. The only values resembling credentials are explicitly public,
synthetic test material (`fixture-public-token-v1` and a zero-byte fixture encryption key); no
production data or secrets are included.

To reproduce the corpus from the frozen source refs in a clone with release tags available:

```sh
node scripts/generate-historical-fixtures.mjs --write
node scripts/verify-historical-fixtures.mjs
```

`npm test` checks the Node runtime over copies; `cargo test --test native u50_historical_fixtures`
checks the Rust production boot, reconciliation, digest backfill, authenticated read, write,
update, restore, share lifecycle, and encrypted-webhook delivery. The release workflow additionally
runs the same corpus through the exact candidate OCI digest and attaches a checksummed report.

## `empty-v21`

The older conformance fixture remains an intentionally empty baseline used by language-neutral
contract cases. Historical compatibility is proven by `historical/`, not by this placeholder.
