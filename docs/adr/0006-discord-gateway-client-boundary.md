# ADR 0006: Use Twilight behind the Discord inbound boundary

- Status: Accepted
- Date: 2026-07-30

## Context

Two-way discussion synchronization needs Discord Gateway identify, heartbeat, reconnect, resume,
compression, and privileged-intent behavior. Owning that protocol directly would duplicate
security-sensitive lifecycle machinery. Discord dispatch events can also replay or contain
partial message updates, while bot credentials and thread authorization must remain
organization-scoped.

Discord documents the Gateway lifecycle and resume contract in its
[Gateway guide](https://docs.discord.com/developers/events/gateway) and the message dispatch
shapes in [Gateway events](https://docs.discord.com/developers/events/gateway-events).

## Decision

Artifact MCP uses `twilight-gateway` 0.17 as the maintained production Gateway client. Twilight
owns websocket framing, heartbeat acknowledgement, reconnect/resume, compression, and intent
protocol behavior. Artifact MCP owns:

- one supervised shard task per eligible organization;
- credential resolution through PBI-081's encrypted, write-only organization credential service;
- exact server-side guild/thread authorization;
- normalization into the provider-neutral PBI-080 event model;
- bounded Discord REST reads for current message state;
- durable, body-free inbox receipts and idempotent canonical mutation;
- optional-integration health and shutdown.

The core inbound processor contains no Twilight or HTTP types. It has no outbound enqueue
capability. The production adapter requests only `GUILDS`, `GUILD_MESSAGES`, and
`MESSAGE_CONTENT`. Invalid/disallowed intents, lost permissions, and provider outages degrade the
integration without changing application liveness.

`DISCORD_INBOUND_ENABLED=1` is an operator kill-switch gate and defaults off. Even when it is on,
an outbound organization policy or stored credential alone does not start a Gateway connection.
A task is eligible only during a bounded readiness request or while at least one mapped artifact
is explicitly in two-way mode.

Gateway session ID, resume URL, and sequence are stored only with the matching organization
credential version. Credential rotation invalidates the old resume boundary. Message updates are
first recorded as body-free `needs_fetch` rows, hydrated outside SQLite through a no-redirect,
four-second REST client, and atomically retried. Discord `Retry-After` is honored with a bounded
one-to-sixty-second delay; twenty failed hydration attempts terminally degrade only the optional
inbound integration. Terminal inbox metadata is retained for 30 days and removed in batches; raw
Gateway payloads and message bodies are never retained in the inbox.

## Consequences

The Twilight dependency is part of the production security/update surface and must be covered by
normal dependency review. The narrow adapter keeps a future library replacement local.

Discord-origin authors remain provider identities, never verified Artifact MCP email identities.
Bot- and webhook-authored messages are discarded. Unmapped and cross-tenant routing attempts get
body-free ignored/rejected receipts so operators have a safe security signal without revealing
another tenant's mapping. Provider-origin mutations bypass outbound planning. A local admin
deletion leaves a body-free provider tombstone so a replay cannot resurrect moderated content.
Artifact MCP feedback remains available when the Gateway is stopped or unhealthy.

The same bot token explicitly configured for multiple organizations can create multiple Gateway
sessions. Operators must account for Discord session limits during a broader rollout; Artifact MCP
does not silently share credentials or authorization between organizations.
