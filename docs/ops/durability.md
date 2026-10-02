# Artifact body durability

Artifact MCP uses SQLite WAL with `synchronous=FULL` and durable filesystem barriers for artifact
bodies. This protects an acknowledged mutation against a host power loss only on a local,
single-filesystem persistent volume whose files and directories honour `fsync`/`sync_all`.

Do not place `artifacts.db`, `artifacts/`, staging, history, or trash paths on NFS/SMB, overlay
paths with unknown flush semantics, or separate devices. A cross-device rename or directory-sync
failure is a failed mutation; preserve its staging/trash/history evidence and run startup recovery
rather than removing files manually.

The recovery states are: `prepared` (concealed while body/metadata transition), verified completed
(intent cleared), verified abort (prior digest retained), and ambiguous (concealed and retained for
an operator). This gives an RPO of zero for an acknowledged, supported-volume mutation, subject to
the storage device honouring flushes; latency increases with each body file and directory.

The marker records phase, not visibility: all states conceal normal reads. Publish moves through
`prepared → metadata_committed → body_durable`; update moves through `prepared → metadata_committed
→ body_durable`; delete moves through `prepared → metadata_committed → body_durable`. The ordering
differs in the filesystem step: publish stages then atomically records metadata/revision before its
final install; update commits metadata before replacing the body; delete moves the body to trash
before its SQL delete. `updated_at` advances at every phase for operator recovery evidence.

Run `node scripts/benchmark-durability-oci.mjs IMAGE_OR_LOCAL_TAG OUT.json` with
`BENCH_DATA_DIR` set to an empty dedicated directory on the target storage and
`BENCH_REQUIRE_PERSISTENT=1` before making a latency claim. It drives the production Rust OCI
image through authenticated `/mcp`, with 20
warmups and 200 measured publish/update operations for 64 KiB single bodies and ten-file 640 KiB
bundles, reporting p50/p95/max plus inspected image ID, OCI labels, host, and mount identity. A
local image tag is valid for before/after comparison; release evidence must name an immutable OCI
digest. `benchmark-durability.mjs` is a Node-reference compatibility twin only and is not
production performance evidence.

## Backups

Run `scripts/backup.sh` against the persistent data directory. The backup takes a consistent SQLite
`VACUUM INTO` cut first, copies artifact bodies, retained history, and optional previews afterward,
then verifies every current and retained body against the copied database snapshot and recorded
digest. A concurrent lifecycle mutation can make verification fail; the script removes its
incomplete staging directory and returns nonzero so it can be retried after the mutation settles.
Pending durability intents are treated the same way because they represent concealed, recoverable
transitions rather than a ready recovery point. Previews are optional caches: their absence does not
fail the backup. Any regular, non-symlink preview copied into a backup is checked against a SHA-256
manifest captured before copying; a changed included preview fails that attempt. This verifies file
bytes only and does not decode or validate image formats. A preview cache disappearing after capture
is allowed, because preview availability is optional.

The final backup directory is published only after SQLite integrity and body/history coherence checks
pass. Restore into a fresh data directory and run normal startup reconciliation before relying on the
backup.

The verifier is a Python 3 standard-library helper (`scripts/backup-coherence.py`), so restore and
backup operators do not need the application’s Node native modules. It requires the staged database
when the source has one, rejects pending durability intents and missing current/retained bodies,
and verifies canonical bundle manifests and SHA-256 digests. It fsyncs copied files and directories
before the staging directory is atomically renamed; the destination directory is then synced. Each
run uses a unique staging and final name, and `KEEP` must be a positive integer. A failed check
leaves no completed backup. Pending intents are retained in the live data directory for the normal
startup reconciliation path and must be resolved before retrying the backup.

Run the focused backup regressions from the repository root:

```bash
node --test test/backup.test.js
```

These tests use temporary databases and directories. They cover writes and missing required
content around the SQLite snapshot, pending lifecycle intents, optional previews, publication
cleanup, and retention. Before deploying a change to this workflow, also restore a completed
backup into fresh data directories and verify current bodies, retained revisions, and bundles
through both application runtimes.
