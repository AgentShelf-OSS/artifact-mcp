# ADR-0007: Viewer state via the shell broker

- Status: Accepted
- Date: 2026-09-08

Interactive artifacts need to retain notes, highlights, and other JSON data across reloads.
ADR-0003 gives artifact code an opaque origin and forbids connection APIs. The trusted,
authenticated viewer shell brokers persistence over `postMessage` and first-party HTTP routes.
The raw-response CSP and iframe sandbox stay unchanged. Artifact code receives no network
capability and cannot choose an HTTP destination or another artifact's identifier.

State belongs to the artifact's organization. Each `(artifact_id, key)` has one value shared by
its viewers, including administrators acting across organizations. This supports shared reading
notes without implying private storage. Per-viewer scope and live synchronization are deferred.
Public shares and historical representations have no state capability, even if their recipient
also has a viewer session.

The shell accepts state messages only from its artifact iframe. It sends value and revision
data back to that frame, without the last writer's email. The server uses the same viewer
identity, request-authenticity, organization checks, and concealed `404` policy as feedback.
State requests have a separate rate-limit budget. A public share token grants no state access.
`INGRESS_STATE_PER_WINDOW` defaults to 30 requests per source per 60-second window, using
`INGRESS_RATE_WINDOW_SECONDS` for the window duration in both implementations.

Public shares and historical HTML are raw documents, so they have no viewer shell. Artifact code
must send `state:hello` during load and wait about one second for `state:ready`. If no shell
answers by then, the artifact treats state as disabled and uses its temporary fallback. The same
fallback applies to direct `/raw` navigation. Viewer state introduces no changes to public-share,
historical, or raw artifact response bytes. A late `state:ready` does not turn state back on for
that page.

Keys match `^[A-Za-z0-9._-]{1,64}$`. An artifact may have at most 64 keys, and a serialized JSON
value may occupy at most 256 KiB of UTF-8. HTTP request envelopes have a separate bound.
The database checks the key count and optional `if_revision` inside the write transaction.
Every successful PUT increments that key's revision, starting at 1. An absent key has revision
0 for comparisons. Deleting and recreating a key starts its revision sequence again.

A stale `if_revision` returns `409` with the current value and revision. The shell sends
`state:value` with `conflict:true`, then `state:error` with reason `conflict`; the artifact
chooses how to merge. Without a revision condition, the last successful write wins. DELETE
is idempotent and follows the existing audit path. High-frequency PUTs are not audited.

The shell coalesces sets to the last value for each key over 500 ms and sends `state:saved`
only after server acknowledgement. Its localStorage cache uses
`artifact-state:<artifact_id>:<key>`. A get can return a cached value immediately, then a second
value when the server revision differs. The server is canonical; the cache supports fast reads
and transient connection failures, not independent shared state or multi-viewer synchronization.
Consumers must tolerate repeated values and must not treat a sent set as an acknowledged save.
Unacknowledged sets remain local drafts on transient failures. Reading that key after reload
paints the draft and retries its original revision condition. A definitive conflict replaces
the attempted draft with the server value. The HTTP key-cap error `too_many_keys` maps to bridge
reason `too_large`, keeping the issue's fixed error vocabulary without pretending it is a
revision conflict.

The `artifact_state` table references artifact records with `ON DELETE CASCADE`. Content updates
and history restores keep state because the artifact record survives. A successful artifact
deletion removes state. If a filesystem trash move rolls back before the database row is deleted,
state remains attached to that row and survives recovery.
