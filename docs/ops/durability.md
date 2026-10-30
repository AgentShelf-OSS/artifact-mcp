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

Run `cargo xtask benchmark-durability IMAGE_OR_LOCAL_TAG OUT.json` with
`BENCH_DATA_DIR` set to an empty dedicated directory on the target storage and
`BENCH_REQUIRE_PERSISTENT=1` before making a latency claim. It drives the production Rust OCI
image through authenticated `/mcp`, with 20
warmups and 200 measured publish/update operations for 64 KiB single bodies and ten-file 640 KiB
bundles, reporting p50/p95/max plus inspected image ID, OCI labels, host, and mount identity. A
local image tag is valid for before/after comparison; release evidence must name an immutable OCI
digest. The server needs an audit key and higher admission limits for this load, so the benchmark
supplies a throwaway key and raises the limits; the report lists them under `serverOverrides`.
`cargo xtask benchmark-durability --binary PATH OUT.json` runs the same workload against a local
build for development.

## Backups

The server binary makes and checks backups itself, so the data host needs no shell, Python, or
`sqlite3`. This also works in the distroless container image.

```text
artifact-mcp backup [DATA_DIR] [DEST_ROOT] [KEEP]
artifact-mcp verify-backup [--database-required] [--preview-manifest-required] DIR
```

`DATA_DIR` defaults to `$ARTIFACT_MCP_DATA_DIR`, else `./data`. `DEST_ROOT` defaults to
`$ARTIFACT_MCP_BACKUP_DIR`, else `./backups`. `KEEP` defaults to 14 and must be a positive integer.
An empty argument counts as unset. Pass the server's `DATA_DIR` explicitly: the backup command does
not read the server's `DATA_DIR` variable.

In the container, mount a backup directory and run the binary inside the running container. With
the bundled `docker-compose.yml`, `DATA_DIR` is `/data`. Add a volume such as `./backups:/backups`
that the image user (`nonroot`, UID 65532) can write, then run:

```bash
docker exec artifact-mcp /artifact-mcp backup /data /backups 7
docker exec artifact-mcp /artifact-mcp verify-backup --database-required /backups/backup-<stamp>
```

On a native host, run the installed binary as the service user, so that any SQLite `-wal` or `-shm`
file it creates belongs to the server's user:

```bash
sudo -u artifact-mcp /usr/local/bin/artifact-mcp backup /var/lib/artifact-mcp /var/backups/artifact-mcp 14
```

Adjust the user, binary path, and both directories to the deployment. A cron job or systemd timer
can run the same command; it prints one line per completed or pruned backup and exits nonzero on
any failure.

A run does these steps in this order:

1. It takes a consistent SQLite `VACUUM INTO` snapshot first. This is the recovery point. The live
   database is not stopped.
2. After that cut, it copies `artifacts/` (bodies, bundle files, retained history, and the hidden
   staging and trash evidence) and the optional `previews/` cache. Regular files, directories, and
   symlinks are copied with their mode, times, and, where the process may set it, owner.
3. It verifies the snapshot against the copied files: no pending durability intent, no missing
   current or retained body, no required body that depends on a symlink, and canonical bundle
   manifests and SHA-256 digests equal to the recorded `body_sha256`. Then `PRAGMA quick_check`
   must report `ok`.
4. It fsyncs the copied files and directories, renames the staging directory
   (`.incomplete-<stamp>-<suffix>`) atomically to `backup-<stamp>-<suffix>`, and syncs the
   destination directory.
5. It removes the oldest `backup-*` directories beyond `KEEP`.

The stamp is UTC (`YYYYMMDD-HHMMSS-nanoseconds`), so names sort in creation order. The random
suffix makes concurrent runs unique.

A concurrent lifecycle mutation can make verification fail, but it can never publish a database row
whose required body is absent from the completed backup. On any failure, and on `SIGINT`, `SIGTERM`,
or `SIGHUP`, the command removes its staging directory and exits nonzero (1 for a failure, 128 plus
the signal number for a signal), so a later run can retry after the mutation settles. Pending
durability intents fail the run in the same way: they are concealed, recoverable transitions, not a
ready recovery point. The intents stay in the live data directory for the normal startup
reconciliation path.

Previews are optional caches. A missing preview cache, or a preview that disappears or cannot be
copied, does not fail the backup. The command records the SHA-256 of the bytes it read for each
regular, non-symlink preview it copies in `preview-sha256.json`; verification fails if an included
preview differs from that digest. This checks file bytes only and does not decode image formats. A
`previews` path that is not a directory, or that holds a device, FIFO, or socket, fails the run.

A data directory without `artifacts.db` is backed up without a snapshot, and the summary line says
`artifacts=0 (no database)`.

Failures print the same messages as the former scripts, for example
`backup: coherence check failed: required body missing: artifacts/<id>.html`,
`backup: coherence check failed: 1 pending durability intent(s)`, and
`backup: KEEP must be a positive integer`.

### Verify and restore

`verify-backup DIR` repeats the coherence and integrity checks on a completed backup and changes
nothing. Add `--database-required` unless the source data directory had no database. Every backup
this command writes has a preview manifest, so `--preview-manifest-required` is also safe for them.

To restore:

1. Run `verify-backup --database-required` on the backup.
2. Stop the server.
3. Copy the backup directory into a fresh, empty data directory on the target volume, for example
   `cp -a /var/backups/artifact-mcp/backup-<stamp> /var/lib/artifact-mcp-restored`. The extra
   `preview-sha256.json` file is ignored by the server.
4. Point `DATA_DIR` at the new directory and start the server. Normal startup reconciliation runs
   before the server accepts requests. Check `/health` and the artifact list before relying on it.

### Tests

Run the focused backup regressions from the repository root:

```bash
cargo test --locked --test backup --test web_push_keys
```

These tests use temporary databases and directories. They cover writes and missing required
content around the SQLite snapshot, pending lifecycle intents, optional previews, symlinks, SQL and
URI punctuation in paths, publication failure, signal cleanup, concurrency, and retention. The
signal test uses a pause hook that only debug builds compile. Before deploying a change to this
workflow, also restore a completed backup into a fresh data directory and verify current bodies,
retained revisions, and bundles through the server.
