# Security audit ledger protocol

PBI-058 records privileged security events independently from Discord and other outbound
delivery. It stores no request payloads, bearer credentials, share tokens, webhook URLs, JWTs,
artifact bodies, or source. A webhook test records only the owning organization and the persisted
webhook row identifier that was resolved from the route path. Callers must use an `AuditContext`
constructed from a verified API key or verified viewer identity; it is never created from
caller-supplied actor fields.

## Chain format

`security_audit_events` is a global, HMAC-SHA-256 hash chain. Every row has a fixed `key_id`, a
canonical format version, the previous event hash, and its event hash. Version 1 canonical bytes
are: a one-byte version followed by the following UTF-8 fields in this exact order, each prefixed
with an unsigned big-endian 32-bit byte length: event id, key id, tenant, actor type, actor id,
actor role, operation, target type, target id, result, classification, source, request id,
revision, and occurred-at. The event HMAC input is the fixed
`artifact-mcp/security-audit/v1\0` domain separator, an unsigned big-endian 64-bit sequence, the
length-prefixed previous hash, an unsigned big-endian 32-bit canonical-byte length, and the
canonical bytes. The head MAC uses the same domain followed by `head\0`, length-prefixed key id,
version byte, sequence, and length-prefixed head hash.

An append runs inside the same SQLite transaction as the audited mutation. It reads the singleton
`security_audit_chain_head`, explicitly inserts `head.sequence + 1`, and conditionally advances
the sequence, hash, and authenticated head MAC using the old values. Any write or conditional-head
failure aborts the mutation; there is no success response without a durable event.

## Outcomes, concealment, and retries

A verified, authorized SQLite mutation that changes durable state appends exactly one terminal
success event in the business transaction. A missing target, an already-applied retry, or another
no-op appends nothing. Validation and mutation failures roll back, including failures caused by
the audit key, chain head, or event append. Consequently a retry cannot create a duplicate success
event unless it performs a new state change.

Authentication failures, authorization denials, and concealed missing or foreign targets also
append nothing. A durable row for those paths could turn the ledger into an existence oracle.
Operational counters aggregate high-rate authentication failures separately.

Terminal `denied` or `failure` events are permitted only after authorization and target disclosure
are already established, so the event cannot create an oracle. The webhook test endpoint is the
current example: after resolving the stored `(organization, webhook id)`, it commits a redacted
`webhook.test.requested` marker before external I/O and a correlated
`webhook.test.completed` success or failure afterward. External I/O cannot be part of the SQLite
transaction. A crash may therefore leave a requested-only record, and a manual retry may redeliver,
but the ledger never silently loses the attempted delivery or claims exactly-once delivery.

The optional Discord discussion-mirror lifecycle follows the same redaction rule. Connection setup,
mode changes, safe retry requests, and visible test request/completion outcomes are recorded as
transactional audit events where they change durable state. They never record a Discord URL,
webhook token, external thread/message identifier, or Discord error body. The internal opaque
`discussion_connection` identifier is retained as the exact immutable binding target. The delivery
runbook covers the separate connection, disposable-pilot test side effect, encryption, backup, and
restore procedure: [`discord-durable-delivery.md`](ops/discord-durable-delivery.md#discussion-mirror-pilot).

## Lifecycle receipts and recovery

Cross-store artifact operations reserve a `security_audit_receipts` row in their metadata
transaction, keyed by the PBI-051 durability intent/correlation id. Once filesystem durability is
known, finalization atomically appends one terminal event and changes that receipt from `pending`
to `finalized`. A retry returns the existing event rather than appending a second one. Startup
reconciliation owns pending receipts: it finalizes verified durable success, compensation, or an
explicit ambiguous/recovered outcome; it never guesses silently. This makes notification failure
irrelevant to the audit result.

## Access, query, retention, and rotation

Audit readers are default-deny. `audit:read` is required for the caller's tenant,
`audit:export` is required for NDJSON export, and `audit:global` is additionally required for a
different tenant. Queries have a fixed sequence order, a default limit of 100 and maximum 500;
their opaque cursor encodes the tenant and last sequence and cannot be swapped to another tenant.
Exports cap at 10,000 rows and 5 MiB and return a tenant-bound signed continuation when they stop
between rows; a single first row over the byte cap returns no continuation to avoid retry loops.

Retention is 180 days. Pruning runs in a transaction and only removes a contiguous expired
sequence prefix; it first writes a checkpoint containing the pruned sequence interval, key
identity, canonical version, bridge hash, prior checkpoint hash, and a checkpoint HMAC.
Verification requires contiguous checkpoints and retained sequences, starts the retained suffix
from this bridge, and verifies every checkpoint/head MAC before the remaining events. Mixed key
ids currently fail closed: key rotation is an explicit future operational migration, not an
implicit overwrite of an existing key id.

A valid whole-database snapshot rollback can restore an earlier valid head. Retain an external,
trusted checkpoint/head anchor and alert if the observed head moves backward; this anchor must not
be restored from the same SQLite snapshot.

`AUDIT_LEDGER_HMAC_KEY` is a required canonical base64 value decoding to exactly 32 bytes. Node
startup fails closed without it; fixtures supply a deterministic key. The same fail-closed
configuration and byte protocol are required for the Rust runtime before it serves mutations.

## Signals

The ledger's metrics use only fixed low-cardinality signal names: authentication failure,
unexpected administrative action, integrity/reconciliation failure, sustained rate limit, and
the staged PBI-056 dead-letter growth signal. High-rate unauthenticated failures are aggregated
in memory/window counters rather than one durable event per attacker attempt.
