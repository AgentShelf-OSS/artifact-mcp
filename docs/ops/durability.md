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
