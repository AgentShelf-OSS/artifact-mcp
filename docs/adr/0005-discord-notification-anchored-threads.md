# ADR 0005: Anchor Discord discussions to artifact notifications

- Status: Accepted
- Date: 2026-07-30

## Context

Artifact MCP already posts rich artifact notifications through organization-scoped Discord
incoming webhooks. The first discussion-mirror design used a separate Forum/Media webhook, which
split the artifact card and its conversation into different Discord posts. Product feedback
preferred a public thread attached to the existing artifact notification.

An incoming webhook can post into a thread but cannot start a public thread from an existing
message. Discord requires a bot-authenticated REST call for that operation. Human Discord replies
also require a Gateway-capable bot to ingest, which remains outside the one-way pilot.

## Decision

Each organization may select one existing webhook subscribed to `published` as its discussion
destination. An administrator stores that organization's bot token through the write-only Settings
contract and explicitly enables the organization threading policy. The token is encrypted with the
deployment integration-secret key and is resolved only inside tenant-bound provider adapters.

Enabled organizations make outbound mirroring the inherited default for eligible existing and
future artifacts. An artifact owner or administrator may persist only an `artifact_only`
exception, or remove it to return to the organization default. Policy disablement stops planning
new Discord work without deleting canonical feedback or already committed provider mappings.

The published-notification outbox row is the preferred durable thread anchor. The first comment's
discussion job depends on that row reaching `accepted` with a validated Discord message ID. For an
older artifact whose exact receipt predates retained message IDs, a bounded asynchronous worker may
recover the newest message that matches all of:

- the selected Artifact MCP webhook registration;
- Discord's provider webhook snowflake for that exact registration;
- the validated organization guild and channel; and
- the exact canonical artifact URL in a visible embed.

Missing, redacted, or cross-destination history fails closed. When the selected webhook posted
multiple cards for the same canonical artifact URL, Discord's newest-to-oldest channel ordering
selects the newest exact match. Recovery never guesses by title, never crosses the configured
destination, and never posts a replacement publication card. With either retained or exact
recovered provenance, the delivery worker:

1. resolves the encrypted organization credential and starts a public thread from the notification
   message;
2. uses the selected incoming webhook to post the first comment into that thread; and
3. persists the thread/message correlation atomically with accepting the discussion job.

The thread ID is Discord's source-message ID. On retry, a failed create attempts a bounded channel
probe and reuses an existing matching thread before posting the comment. Later comments and state
markers use the existing webhook/thread mapping.

Bot tokens are never returned, masked with token fragments, logged, audited, placed in queues, or
sent to the webhook endpoint. Configuration validates bot identity and the exact webhook
channel/guild before enabling the policy. `DISCORD_BOT_TOKEN` remains only as a migration fallback
for an organization that already has a deployed PBI-079 discussion connection; it is not offered
to unrelated organizations and Settings prompts migration to a stored organization credential.
The selected webhook's standalone `feedback` and `resolved` notifications are suppressed while
mirroring is active so the parent channel does not receive duplicate discussion events; other
webhooks are unchanged.

PBI-080 inbound synchronization must consume the same organization credential resolver. It has a
separate explicit two-way authorization state; enabling outbound organization threading does not
silently authorize the Discord Gateway or inbound message content.

## Consequences

- The artifact card and its discussion stay together in a normal Discord text or announcement
  channel.
- Outbound operation requires a bot with View Channel, Create Public Threads, and Send Messages in
  Threads. Historical recovery additionally needs Read Message History.
- Older artifacts remain local until retained delivery evidence or a newest exact recovered
  notification is available.
- Delivery remains bounded at-least-once. Thread creation is retry-safe, while an ambiguous webhook
  comment response may still duplicate the comment as documented by the outbox contract.
- Discord replies remain Discord-only unless the separately authorized two-way synchronization
  policy is enabled.
- Legacy Forum/Media connection rows remain readable and drainable, but new settings configure the
  notification-thread strategy.
