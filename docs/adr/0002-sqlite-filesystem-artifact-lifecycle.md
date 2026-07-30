# ADR-0002: Coordinate SQLite metadata and filesystem bodies with recoverable moves

- **Status:** Accepted
- **Date:** 2026-07-09

## Context

Artifact metadata benefits from SQLite queries while artifact bodies are naturally served as files and directories. A single database transaction cannot atomically commit both SQLite and filesystem state. Writing final bodies before metadata, or deleting metadata before bodies, creates failure windows that can silently orphan one side.

The service runs as one process with a mounted persistent data directory. That makes synchronous staging moves, compensating actions, and startup reconciliation practical without adding an object store or job system.

## Decision

Retain SQLite for artifact records, keys, reactions, and migration history, and retain the filesystem for artifact bodies.

- Apply ordered schema migrations transactionally and record them in `schema_migrations`.
- Enable SQLite foreign keys; artifact deletion cascades reactions.
- Publish through hidden staging paths, insert metadata, then rename to the final body path. Compensate both sides when an in-process step fails.
- Delete by moving the body to hidden trash, deleting metadata, then removing trash. Restore the body if database deletion fails.
- Reconcile transient paths at startup. If a live artifact record has no final body, recover its staging/trash path. Before replacing an installed body from staging, verify that staging matches the committed digest; preserve and report both paths on mismatch.
- When recovery installs verified staging over an outgoing body, move that outgoing body to its revision-history path before the swap. This keeps the committed outgoing revision readable and restorable across the commit-then-snapshot crash window.
- PBI-051 sets SQLite `synchronous=FULL` on the bootstrap and every pooled writer connection.
  Bodies are written in a staging path on the same filesystem, each file and bundle directory is
  synced, and every staging/final/history/trash rename is followed by parent-directory syncs.
- A prepared durability intent conceals an artifact from normal read and list paths while its
  metadata and body transition. Startup resolves digest-proven completed or aborted intents before
  the ordinary transient sweep. Ambiguous states retain every viable copy and remain concealed for
  operator investigation rather than deleting content to make a row appear ready.

## Durability recommendation (2026-07-22)

Adopt stronger power-loss durability in a separately scoped, coordinated Node/Rust change:

- move SQLite to `synchronous=FULL`;
- sync staged file contents before treating staging as recoverable (every file plus the staging directory for bundles); and
- sync the affected parent directories after staging creation and after history/final renames so the directory entries themselves survive power loss.

The benefit is real: it reduces the chance that a committed database revision survives while the corresponding staged bytes or rename does not. It does not make SQLite plus filesystem updates atomic, so digest verification, history-first recovery, and startup reconciliation remain required.

The cost is extra storage flush latency on publish, body update, and restore, with a larger multiplier for multi-file bundles. At this deployment's scale (62 artifacts and roughly 30 lifetime views), that cost is immaterial compared with avoiding manual recovery after power loss. The recommendation is therefore to enable both `FULL` and explicit file/directory syncing, but to benchmark and roll them out in their own change rather than folding them into PBI-038.

## Consequences

- Normal and caught failure paths keep metadata, bodies, and reactions consistent.
- An acknowledged mutation has a best-effort local power-loss guarantee only when the SQLite DB,
  artifact tree, and their parents are on one local filesystem with working file and directory
  flush semantics. Network filesystems (NFS/SMB) and cross-device artifact paths are unsupported;
  a rename or directory-sync failure fails the mutation and preserves recovery evidence.
- SQLite and filesystems are still not a distributed transaction. Recovery repairs only
  digest-verified states, preserves the outgoing revision before replacement, and reports
  unresolved divergence without destroying either path. The durability cost is extra write latency
  for file, tree, and directory flushes; the operational benchmark records it before release.
- Orphan artifact bodies are not deleted automatically; destructive reconciliation requires an explicit future decision.
- SQLite plus local storage remains intentionally single-writer and tied to a persistent volume. Horizontal multi-writer deployment would require revisiting this decision.
