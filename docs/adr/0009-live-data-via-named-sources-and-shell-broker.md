# Live data through named sources and the viewer shell

- Status: Accepted
- Issue: [#58](https://github.com/AgentShelf-OSS/artifact-mcp/issues/58)

Artifacts need current API data without gaining network access or replacing their HTML on each
update. Operator-configured, organization-scoped data sources expose named operations and
subscriptions. An artifact binding grants access to a subset of those names. The trusted viewer
shell queries the server and passes JSON messages into the artifact through `postMessage`, extending
the approach in ADR-0007 while preserving ADR-0003's CSP and opaque-origin sandbox.

One viewer event stream combines updates from multiple bindings. Each subscription retains its own
cursor and connection status, so a failed upstream does not disconnect other sources. HTTP JSON,
polling, SSE, and producer writes through MCP use the same artifact client. Source configuration
and credentials stay on the server; artifact code cannot choose URLs, methods, or headers.

Bindings and producer data persist separately from HTML revisions. Replacing bindings removes data
for removed or changed bindings. Remote API data has its own lifetime and does not become a snapshot
when an HTML revision is retained. Raw, historical, and public-share representations receive no
live-data capability. This decision implements the live-data capability deferred by ADR-0007; its
existing viewer-state behavior remains unchanged.

We chose a shell broker over relaxing `connect-src`: it preserves viewer authentication and tenant
checks and keeps upstream credentials out of published code. Reusing existing streams avoids
copying an entire remote service into Artifact MCP. Producer-enabled sources allow independent
services to push snapshots and events when a direct connection is unavailable.
