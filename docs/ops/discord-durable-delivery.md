# Discord durable delivery runbook

Artifact MCP sends subscribed Discord events through a durable SQLite outbox. A committed artifact
or feedback mutation records its event and its subscribed targets in the same transaction; two
workers claim ready rows after storage reconciliation. The request does not wait for Discord.

Delivery is **bounded at-least-once**, not exactly-once. A definitive accepted provider response
finishes a row. Retryable outcomes are bounded to eight attempts and terminal failures are retained
as `dead_letter`. If the process or network fails after Discord may have accepted a request but
before Artifact MCP persists its result, a retry can create a second Discord message. Such rows
carry explicit duplicate-risk state. Never describe this integration as exactly-once.

## Metrics

`GET /metrics` exposes only aggregate, low-cardinality delivery telemetry. It does not label
metrics with organization, webhook, event, payload, credential, or Discord response data.

| Metric | Meaning |
|---|---|
| `artifact_mcp_delivery_queue_active` | Non-terminal rows: blocked, ready, leased, or retrying. |
| `artifact_mcp_delivery_queue_ready` / `artifact_mcp_delivery_queue_retrying` | Ready-to-claim and scheduled-retry rows. |
| `artifact_mcp_delivery_queue_dead_letter` | Retained rows requiring review. |
| `artifact_mcp_delivery_oldest_active_age_seconds` | Age of the oldest non-terminal row. |
| `artifact_mcp_delivery_rate_limit_blocked{scope}` | Active Discord rate-limit records by global, target, or bucket scope. |
| `artifact_mcp_delivery_rate_limit_max_delay_seconds` | Largest currently persisted Discord backoff. |
| `artifact_mcp_delivery_workers_healthy` / `artifact_mcp_delivery_workers_expected` | Running and configured workers per process (expected: two). |
| `artifact_mcp_delivery_worker_errors_total` | Turns that failed before persisting an outcome. |
| `artifact_mcp_delivery_ambiguous` | Retained rows that record duplicate-delivery risk. |
| `artifact_mcp_discussion_mirrors{state}` | Aggregate discussion state only: `connected`; `pending` (enabled or retried and awaiting a durable root-thread outcome); `paused` (a disabled/draining mirror row); `failed` (regardless of mode); or `local_only` (the default no-row state plus any stored Artifact-MCP-only `local` row). |
| `artifact_mcp_discussion_pending_threads` / `artifact_mcp_discussion_oldest_pending_thread_age_seconds` | Active durable root-thread jobs and age of the oldest one; these are outbox jobs, not artifact counts. |
| `artifact_mcp_discussion_terminal_failures` | Retained `dead_letter` outcomes for discussion root, message, or tombstone work only. |
| `artifact_mcp_discord_gateway_organizations{state}` / `artifact_mcp_discord_gateway_reconnects_total` | Aggregate organization Gateway health and reconnect signals. |
| `artifact_mcp_discord_inbound_inbox_depth` / `artifact_mcp_discord_inbound_pending_fetches` | Retained body-free event receipts and partial updates awaiting REST hydration. |
| `artifact_mcp_discord_inbound_last_event_age_seconds` / `artifact_mcp_discord_inbound_oldest_pending_age_seconds` | Event recency and deferred-update age. |
| `artifact_mcp_discord_inbound_duplicates_total` / `artifact_mcp_discord_inbound_application_errors_total` | Process replay and pre-result error counters. |
| `artifact_mcp_discord_inbound_ignored` / `artifact_mcp_discord_inbound_rejected_or_degraded` / `artifact_mcp_discord_inbound_tombstones` | Retention-window safety outcomes and provider-delete tombstones. |

The supplied Prometheus rules page immediately for dead-letter presence, for an active queue with
no healthy worker, and warn for a queue older than 15 minutes or rate-limit blocking sustained for
10 minutes. Discussion mirrors additionally alert on `failed`, root-thread work older than 15
minutes, and discussion terminal failures. Pending state without stuck root work, paused, and
local-only states are not alerts.

## Triage

Start with the aggregate metrics and the structured application logs. Use opaque request IDs when
they are available; never put webhook URLs, tokens, payloads, or decrypted secrets into a ticket
or query.

### Dead letter

1. Treat any `ArtifactMcpDeliveryDeadLetterPresent` page as a delivery failure requiring review.
2. Check `artifact_mcp_delivery_ambiguous`, worker errors, recent webhook configuration changes,
   and Discord's status before changing state.
3. Inspect the row through a controlled local operator session only. Read its state, redacted error,
   event type, and timestamps; do **not** query or export `payload` or `secret_ref`.
4. Correct the underlying condition (for example, restore the intended webhook registration or
   resolve a temporary Discord outage), then decide whether a new supported application mutation is
   appropriate. Preserve the dead-letter row as evidence.

There is intentionally no generic database replay command. Updating a row directly can bypass
leases, durability intent handling, and provider ordering; replaying an ambiguous or previously
accepted event can duplicate a Discord message. Do not edit `provider_delivery_outbox` to clear an
alert. Escalate a missing safe replay workflow as product work instead.

### No healthy workers

1. Confirm `artifact_mcp_delivery_queue_active > 0` and
   `artifact_mcp_delivery_workers_healthy == 0` for the same Prometheus target.
2. Check process health, startup logs, migration/reconciliation completion, storage availability,
   and whether the process is in shutdown.
3. Restore the normal application process. The durable queue is the recovery source; do not run a
   second ad-hoc worker or hand-edit leases.
4. Verify healthy workers return to two and that active/oldest-age values decline.

### Stuck or old queue

An older queue can be legitimate while Discord has applied a bounded retry delay, so compare queue
age with `artifact_mcp_delivery_rate_limit_blocked` and
`artifact_mcp_delivery_rate_limit_max_delay_seconds` first. Then inspect worker errors, database
health, and provider availability. If rate limiting is not active, an increasing oldest age with
healthy workers points to a claim, configuration, or provider problem and should be escalated.

### Sustained Discord rate limiting

Rate-limit records are persisted at global, target, and Discord bucket scope so restarts do not
forget a provider backoff. Do not restart workers to evade the delay and do not manually delete
rate-limit state. Reduce event volume or wait for the provider window, then confirm the blocked
gauges and oldest queue age return to zero/normal.

## Replacing a Discord webhook safely

Do not delete a Discord credential or its Artifact MCP registration before old queued work has
drained. The worker resolves the webhook just in time; removing it early turns outstanding work
into terminal `invalid_webhook` failures.

1. Create the replacement webhook in Discord, then add it in Artifact MCP Settings with no
   subscriptions (or a non-production test event set).
2. Use the administrator **Test** action and wait for its completed result. The test is awaited;
   do not continue on a timeout or failure.
3. Switch subscriptions safely: Artifact MCP atomically replaces the full event set on each
   individual webhook, but it deliberately has no cross-webhook transaction. First enable each
   required event on the tested replacement; only then remove the corresponding events from the
   old registration. This short overlap is intentional: it prevents a loss window and can produce
   duplicate event posts.
4. Monitor the queue, worker health, dead-letter count, and Discord channel. Drain old target work
   before deletion. In a controlled local operator session, identify the old registration ID and
   inspect only aggregate state/timestamps for rows where `provider = 'discord'` and
   `target_key = '<old-registration-id>'`; never select `payload` or `secret_ref`. Wait until no
   non-terminal rows remain for that target.
5. Remove the old Artifact MCP webhook registration, verify no new dead letters appear, then delete
   the old Discord webhook credential.

Keep both registrations and the current encryption key available throughout the drain. If an
emergency revocation requires deleting the old Discord credential sooner, expect its queued rows to
dead-letter; preserve them for audit and do not silently replay them.

## Organization Discord threading

Discussion mirroring is anchored to one existing artifact-notification webhook per organization.
The incoming webhook continues to author rich artifact notifications and mirrored comments; an
organization-scoped bot credential starts and manages the public threads.

Grant the bot View Channel, Create Public Threads, and Send Messages in Threads in the selected
text or announcement channel. Add Read Message History only when historical-anchor recovery is
required. Do not grant Administrator. Outbound-only mirroring does not need the privileged Message
Content intent.

Configure one organization in this order:

1. In Settings, save the bot token with organization threading still disabled. The password field
   is write-only and is cleared after submission.
2. Select an existing webhook subscribed to `published` as the artifact notification destination.
   Saving it verifies the exact provider webhook, guild, channel, and bot access.
3. Test the credential and destination. The connection test posts visible Discord content; run it
   only in an approved disposable channel.
4. Enable Discord threads for the organization. Eligible artifacts now inherit outbound mirroring
   without a per-artifact enable step. An owner or administrator may choose **Keep discussion in
   Artifact MCP**, and may later reset that exception to **Use organization default**.

The first inherited comment waits on the durable publication outbox row. After the notification is
accepted and its Discord message ID is recorded, the bot creates the public thread and the webhook
posts the comment into it. A retry probes for the deterministic thread ID before creating again,
covering a process loss after Discord created the thread. Later comments, markers, and tombstones
reuse the retained mapping. Ordinary `feedback`/`resolved` fan-out is suppressed only for the
selected webhook while its mirror is active; other organization webhooks are unchanged.

Enabling the organization also queues bounded recovery for eligible older artifacts that lack a
retained notification receipt. Recovery scans only the configured channel and accepts the newest
message authored by the selected provider webhook whose visible embed URL equals the canonical
artifact URL. Discord returns channel history newest-to-oldest, so older duplicate publication or
update cards do not make the anchor ambiguous. It stores provider IDs and a fixed outcome
classification, never Discord message content. Missing, permission-denied, rate-limited, redacted,
or unavailable history stays local and never triggers a replacement publication card. Restore
Read Message History or provider availability and use the Settings recovery action to retry
supported outcomes; do not edit recovery rows directly.

Rotating a token validates the replacement before the encrypted row is replaced. A failed
validation leaves the previous credential active. Removing a credential disables new organization
threading work but preserves canonical feedback, recovery evidence, and accepted provider
mappings. Disable the organization policy before planned bot revocation, allow already committed
outbox work to drain, then remove the credential.

`DISCORD_BOT_TOKEN` is a migration fallback only for an organization with an existing PBI-079
discussion connection. Its value must remain in protected deployment configuration and must never
be copied into commands, logs, tickets, screenshots, database fields, or browser diagnostics.
Settings reports fallback status without revealing any token fragment. Save and validate a
per-organization credential to retire fallback use. New or unrelated organizations cannot
implicitly consume the process token.

Before expanding scope, verify in a disposable Discord environment:

1. Two organizations can save independent credentials and one organization's rotation/removal does
   not affect the other.
2. A new artifact's first comment creates one thread on its exact publication notification without
   a per-artifact enable action; an artifact-only exception remains local.
3. The newest historical exact match is recovered, wrong-webhook matches fail closed, and removing
   Read Message History degrades recovery without affecting Artifact MCP feedback.
4. Concurrent first comments produce one generation-scoped root job and correlated comment jobs.
5. `connected`, `pending`, `pending_threads`, queue age, and terminal-failure metrics behave as
   expected. Preserve dead letters and duplicate-risk evidence.
6. The encrypted database backup and `WEBHOOK_ENC_KEY` are retained together. Restore drills use
   the visible validation flow instead of exporting a token/webhook URL or replaying SQL.

Inbound Discord replies are a separately authorized capability. When enabled, they reuse this same
organization credential; outbound organization threading alone must not start a Gateway session.

## Two-way Discord discussion operations

Two-way mode is explicit per artifact. Before enabling it, the artifact must already have the
exact connected notification-anchored thread, the organization credential must validate, and the
bot must be able to read the thread. Set `DISCORD_INBOUND_ENABLED=1` only after the Discord
application has the `GUILDS`, `GUILD_MESSAGES`, and privileged `MESSAGE_CONTENT` intents. The
first enable request may briefly report that the Gateway is connecting; retry after readiness is
reported. An outbound-only organization never remains connected to the Gateway.

Use least-privilege channel permissions: View Channel and Read Message History for inbound reads,
plus the outbound thread permissions above. Do not grant Administrator. Ordinary human text and
reply references are imported; attachments, embeds, polls, reactions, stickers, voice content,
DMs, and messages outside exact mapped threads are ignored.

Artifact MCP stores provider/session/message identifiers, a payload fingerprint, timestamps, and
a safe result classification in the inbound inbox. It does not store raw Gateway frames or message
bodies there. Partial updates are durably marked `needs_fetch`, hydrated with bounded REST outside
SQLite, and retried atomically. A provider `Retry-After` is capped at 60 seconds; after 20 failed
hydrations the receipt becomes terminal and only the optional inbound integration degrades.
Terminal inbox receipts are removed in batches after 30 days.
Canonical Discord-origin feedback remains until normal product retention or a provider delete
replaces its body with the deletion tombstone. An administrator's local deletion keeps only a
body-free provider identity tombstone, preventing later replay from restoring moderated content.

### Incident disablement and recovery

1. Set `DISCORD_INBOUND_ENABLED=0` and restart the application to stop all inbound Gateway tasks.
   HTTP, local feedback, and the outbound durable outbox remain available.
2. Check aggregate Gateway health and inbound inbox metrics/logs. Never query message bodies,
   decrypted credentials, webhook URLs, or provider payloads for triage.
3. Correct the intent, guild membership, channel permission, credential, or provider outage.
   Token rotation must use the write-only organization Settings field; a validated replacement
   changes the credential version so an old Gateway session cannot resume with it.
4. Re-enable the operator gate, then retry the artifact's two-way action. Stored event uniqueness,
   provider versions, and deferred-update receipts make replay safe.
5. If a bot is being removed, first move affected artifacts to outbound-only or Artifact MCP-only,
   confirm Gateway tasks stop, then remove the organization credential. Existing feedback and
   mappings are preserved.

Thread deletion or archive/lock marks both inbound sync degraded and the canonical discussion
failed without deleting canonical feedback. Organization Gateway recovery does not clear this
per-artifact failure. Provider unavailability is not a reason to replay SQL or create a
replacement notification. Restore the same exact destination and explicitly reconnect the
artifact discussion.

## Encryption keys and backups

`WEBHOOK_ENC_KEY` is required to decrypt existing encrypted registrations. Keep the active key
outside the repository and retain it with encrypted backups. A database restore that contains
encrypted webhook rows also needs the key that encrypted those rows; a replacement key cannot open
them.

During a webhook replacement, do not rotate `WEBHOOK_ENC_KEY` or restore a backup until the old
target has drained. Follow the manual key-rotation procedure in the
[README](../../README.md#rotating-the-webhook-encryption-key): inventory and unsubscribe
registrations while the old key is active, drain their queued work, preserve an encrypted backup
and the old key, then install the replacement key and recreate/test the registrations. Deleting an
old key before every backup encrypted with it has expired makes those registrations unrecoverable.
