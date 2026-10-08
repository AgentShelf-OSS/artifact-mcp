# Architecture and routes

Artifact MCP is a modular monolith. The production image runs a static Rust binary with Axum and
Askama in a non-root distroless container. SQLite stores metadata, and ordinary files store
artifact bodies, bundles, retained revisions, and optional previews.

The Node implementation remains an independent compatibility twin and test oracle. Docker Compose
uses the Rust server by default.

## Request paths

```text
Agent  -> MCP with API key or OAuth -> /mcp --------+
                                                       |
Human  -> Cloudflare Access -> gallery and viewer ---+--> Rust server --> SQLite + files
                                                       |
Public -> active share token -> /s/:token -----------+
```

The server separates three access paths:

- Upload requests reach `/mcp` with an API key or OAuth bearer token. Agents do not perform an
  interactive SSO login.
- Human requests reach the gallery, artifact viewer, raw content, and settings through Cloudflare
  Access. The application verifies the Access JWT and resolves the viewer's organization.
- Public requests only reach an active `/s/:token` share. Shared artifacts are read-only,
  sandboxed, and carry `X-Robots-Tag: noindex`.

## HTTP routes

| Route | Role |
|---|---|
| `POST /mcp` | MCP JSON-RPC with API-key or configured OAuth authentication. |
| `GET /metrics` | Prometheus request, outcome, latency, cancellation, and bounded result-size metrics. |
| `GET /` | Organization-scoped gallery. Administrators can see all organizations. |
| `GET /:id` | Trusted viewer shell around a sandboxed artifact iframe. |
| `GET /:id/data`, `POST /:id/data/query` | Authorized live-data bindings and named JSON queries. |
| `GET /:id/data/events` | One authorized SSE stream combining selected subscriptions from several bindings. |
| `GET /thumbnails/:id?v=<body_sha256>` | Authenticated current-revision thumbnail or a no-store placeholder. |
| `GET /raw/:id` and `GET /raw/:id/*` | Raw single-file or bundle delivery. `?anchor=1` injects the comment bridge, and `?download` forces an attachment. |
| `GET /raw/:id/rev/:n/*` | Deliver a retained revision body or bundle path. |
| `GET /s/:token` and `GET /s/:token/*` | Public read-only delivery for an active share token. |
| `GET /:id/history` and `POST /:id/restore` | Revision history and restore. |
| `POST /:id/react` | Per-viewer favorite and sentiment state. |
| `POST /:id/feedback` | Create threaded viewer feedback. |
| `DELETE /:id/feedback/:fid` | Delete feedback when authorized. |
| `POST /:id/feedback/:fid/resolve` | Resolve or reopen a feedback thread. |
| `POST /:id/category` | Assign a category for an authorized artifact. |
| `POST /:id/share`, `GET /:id/shares`, `DELETE /:id/shares/:token` | Create, list, or revoke public shares. |
| `POST /:id/visibility` | Hide or show an artifact as its verified uploader or an administrator. |
| `POST /:id/move` | Change category or organization. Organization moves require an administrator. |
| `DELETE /:id` | Delete an artifact as its verified uploader or an administrator. |
| `GET /push/config`, `PUT`/`DELETE /push/subscriptions` | Web Push status and the viewer's device subscriptions ([reminders](web-push-reminders.md)). |
| `GET /:id/push`, `PUT`/`DELETE /:id/push/optin` | Per-artifact notification opt-in. |
| `GET /:id/reminders`, `PUT`/`DELETE /:id/reminders/:key` | Named scheduled reminders in `org` or `viewer` scope. |
| `GET /sw.js`, `GET /manifest.webmanifest`, `GET /icons/:name` | Static service worker, Home Screen manifest, and app icons. They contain no private data. |
| `GET /settings` and `/settings/*` | Administrator management for organizations, members, categories, webhooks, publisher keys, and live-data connections. |

Bundle path handling rejects traversal, absolute paths, and configured size or file-count overages.

### Scheduled reminders

Both runtimes store reminders, opt-ins, and push subscriptions with identical routes, MCP tool,
validation, and schema (ADR-0012). Only the Rust runtime runs the reminder sweeper and the Web Push
sender, next to the Discord delivery worker. The Node reference stores reminders but never sends them.

## Storage and lifecycle

Content writes use staging and rename steps. Updates commit metadata before swapping bodies, and a
startup audit repairs interrupted work by reconciling SQLite with the file tree. Every successful
content update snapshots the outgoing revision. Restore appends another revision instead of
rewriting history.

The optional preview sidecar writes digest-addressed PNGs under the data volume. Its absence or
failure does not block publishing. See [configuration](configuration.md#persistent-previews) and
the [durability runbook](ops/durability.md).

## Source layout

| Path | Responsibility |
|---|---|
| `src/main.rs` and `src/app.rs` | Process startup and application composition. |
| `src/http/routes/` | HTTP request handling. |
| `src/security/` | Access, API-key, OAuth, and tenant policy. |
| `src/mcp/` | Protocol negotiation, tools, resources, MCP Apps, and tasks. |
| `src/artifacts/` | Artifact lifecycle, history, reads, and recovery. |
| `src/persistence/` | SQLite repositories and migrations. |
| `src/render/`, `templates/`, and `assets/` | Gallery, viewer, and administration UI. |
| `src/integrations/` | Notifications and preview rendering. |
| `src/observability.rs` | Privacy-safe metrics. |
| `server.js` and `lib/` | Node compatibility twin. |

Read [CONTEXT.md](../CONTEXT.md) for domain language and module responsibilities. The decision
records under [docs/adr](adr/) explain why the server uses a modular monolith, SQLite plus files,
and a sandboxed null-origin artifact boundary.

Live API connections run on the server through operator or administrator-managed data sources. The trusted shell
uses first-party data routes and passes selected JSON values and events into the iframe. Bindings
and producer data persist separately from HTML revisions. The Connections workspace stores managed
source definitions in SQLite, applies changes to the affected adapters, and commits audit records
with each mutation. File-defined sources stay read-only. See [live data](live-data.md) and
[ADR-0009](adr/0009-live-data-via-named-sources-and-shell-broker.md).

[Return to the documentation index](README.md).
