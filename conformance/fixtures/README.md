# Fixtures

Each subdirectory is a starting `DATA_DIR` state. The runner copies it (minus `.gitkeep`)
into a fresh temp dir per case, per implementation — Node and Rust never share a data dir.

## `empty-v21`

An **empty** directory. There is no committed SQLite binary. When a server boots against an
empty `DATA_DIR`, `openDatabase()` creates `artifacts.db`, enables WAL + foreign keys, and
runs the ordered migrations up to the latest schema version (v21) — and creates the empty
`artifacts/` body directory. That freshly-migrated v21 database + empty body dir *is* the
`empty-v21` starting state, materialized identically by whichever implementation is under
test. Keeping the fixture empty makes it implementation-neutral (both servers must migrate
an empty dir to v21) and avoids committing an opaque, drift-prone binary.

Future non-empty fixtures (frozen legacy v0/v9/v17 DBs, staged/trashed crash states) will be
committed here as real directory trees when the milestones that need them arrive.
