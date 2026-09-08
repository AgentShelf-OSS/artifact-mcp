# ADR-0008: Per-viewer state scope and opaque viewer identity

- Status: Accepted
- Date: 2026-09-08

A collaborative reader needs personal notes and bookmarks alongside notes that readers choose
to share. Viewer state therefore has two scopes. `org` remains the default for existing
artifacts. `viewer` belongs to the authenticated viewer, who is the only person allowed to read
or write those rows. Administrators use their own viewer scope even when opening an artifact
in another organization. An administration role does not imply permission to read a diary.
Cross-viewer listing, aggregation, and deletion are outside this contract.

Migration 34 rebuilds `artifact_state` with a primary key of
`(artifact_id, scope, viewer, key)`. Existing rows move to `org` with an empty owner and retain
their values, revisions, timestamps, and writer attribution. Viewer owners are lowercase
emails matched case-insensitively. The 64-key limit applies independently to each artifact,
scope, and owner. The 256 KiB value limit, revision checks, artifact-deletion cascade, and
state retention across artifact revisions continue to apply in both scopes.

The HTTP routes accept `scope=org` or `scope=viewer`, defaulting to `org`. The server selects
the owner from the authenticated identity; the caller cannot select someone else's email.
Access checks still conceal inaccessible artifacts. Invalid scopes return `bad_scope`.
Viewer-scope responses omit `updated_by`; the owner already knows who wrote the value.
DELETE auditing continues to identify the artifact, without putting an email in the target ID.

The bridge gives artifacts a viewer handle containing a stable ID and a display name.
The server derives a dedicated key as HMAC-SHA256 of `artifact-viewer-id` using the decoded
audit ledger key. The ID is the first eight bytes of HMAC-SHA256 of
`viewer-id:` followed by the lowercase email, using that dedicated key, encoded as 16
lowercase hexadecimal characters. Domain separation keeps viewer handles separate from
audit identities. Rotating the audit key changes the handle and its browser-cache namespace;
persisted viewer rows remain attached to the email.

Migration 34 also adds an optional display name to organization email membership. Names are
trimmed, limited to 40 characters, and reject control characters. Without a name, the server
uses the email local part, replaces periods, underscores, and plus signs with spaces,
capitalizes each word, and limits the result to 40 characters. The server renders the handle
into escaped shell configuration attributes. The broker exposes this handle on enabled
`state:ready` messages and never forwards email or HTTP writer attribution to the artifact.
A handle is suitable for attribution, not proof that an artifact's shared note has that author.

The handshake includes both supported scopes, org `keys`, and `viewerKeys`. Requests may
choose a scope; value, saved, and error messages echo it. The broker isolates queues and
revision caches by scope. Org cache keys retain `artifact-state:<artifact_id>:<key>` for
compatibility. Viewer cache keys use
`artifact-state:<artifact_id>:viewer:<viewer_id>:<key>` so another person using the same browser
does not receive the prior viewer's cached private notes. Browser storage remains a cache;
server authentication controls persisted access.

This extends the scope and handshake decisions in [ADR-0007](0007-viewer-state-via-shell-broker.md).
Its broker trust boundary, retry and conflict behavior, raw CSP, iframe sandbox, and one-second
fallback still apply. Raw, historical, and public-share HTML receive no identity or state
injection. Live synchronization remains outside this decision.
