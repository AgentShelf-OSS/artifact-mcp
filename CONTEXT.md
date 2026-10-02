# artifact-mcp context

This document is the domain and architecture source of truth for maintainers and coding agents. The README explains how to run the product; the decision records in `docs/adr/` explain why its major seams have their present shape.

## Purpose

artifact-mcp is a private, multi-tenant publishing gallery. Authorized AI agents publish HTML through MCP, and authenticated people browse only the artifacts belonging to their organization. An administrator can browse all organizations and manage publisher keys.

The application is a dependency-light modular monolith: one server process, one SQLite database, one artifact directory, and server-rendered HTML/CSS/JavaScript. Rust serves production; Node is the reference implementation.

## Domain language

- **Artifact** — one published page plus its metadata, addressed by a stable ID and viewer URL. Its current revision can advance until deletion; retained revision bodies remain immutable.
- **Single-page artifact** — an artifact whose body is one self-contained `.html` file.
- **Bundle** — an artifact whose body is a directory of relative-path files and one HTML entry file.
- **Artifact record** — the SQLite metadata row for an artifact.
- **Artifact body** — the file or directory containing the bytes for the current or a retained revision.
- **Current revision** — the revision selected by the live artifact record and stable viewer URL. An update changes this revision without changing the artifact ID.
- **Retained revision** — an immutable historical body and its metadata, addressed by the artifact ID and revision. Restoring one creates a new current revision rather than mutating history.
- **Organization (org)** — the tenant key shared by publisher credentials, viewers, and artifacts.
- **Publisher** — an AI agent using an API key or configured OAuth bearer token to call MCP tools. `client_id` identifies the publishing machine credential.
- **Publisher role** — the credential's `reader`, `author`, or `collaborator` permission level. Readers read their organization, authors manage their own artifacts, and collaborators can update other artifacts in their organization; non-admin deletion still requires ownership.
- **Viewer** — a person authenticated by Cloudflare Access. Their verified email resolves to one organization.
- **Administrator** — a viewer allowed to browse all organizations and create or revoke publisher keys. An admin publisher key may target an explicit organization.
- **Publisher key** — a revocable, per-organization upload credential. Only its hash is stored; the secret is displayed once.
- **Reaction** — a viewer’s favorite flag and sentiment vote (`-1`, `0`, or `1`) for an artifact.
- **Gallery** — the organization-scoped artifact index.
- **Viewer shell** — trusted application chrome around a sandboxed raw artifact.
- **Viewer state**: JSON values belonging to an artifact in a chosen viewer state scope. State survives artifact revisions. An artifact treats state as disabled when no viewer shell answers its `state:hello` within about one second.
- **Viewer state scope**: The audience for a state key. `org` shares the value with viewers who can access the artifact and is the default. `viewer` restricts the value to the authenticated person who owns it, including when that person is an administrator.
- **Viewer handle**: An opaque, stable viewer ID and a display name that an artifact can use for attribution. A handle does not grant access to another viewer's private state.
- **Raw delivery** — artifact bytes served from `/raw/:id` or `/raw/:id/*`.
- **Storage reconciliation** — inspection and recovery of interrupted staging/trash operations, plus reporting of missing or orphan bodies.

## Invariants

1. A non-admin publisher can only publish into the organization fixed by its key; an `org` argument cannot override it.
2. A non-admin viewer can only list, open, or react to artifacts in their resolved organization. Viewer deletion requires the artifact's verified uploader identity or administrator status; publisher deletion requires an owning author/collaborator key or an administrator key.
3. Cross-organization reads are concealed as `404`, so artifact identifiers do not disclose tenant membership.
4. Raw HTML is untrusted. Every raw response receives a CSP `sandbox` directive without `allow-same-origin`; the viewer iframe also omits `allow-same-origin`.
5. Bundle paths are relative, normalized, and traversal-guarded. Storage limits apply before publication completes.
6. An artifact record and its current body represent one lifecycle. Publication and updates use staging; updates retain the outgoing body as immutable history before swapping the current body; deletion uses trash; reactions cascade on deletion; and startup reconciliation recovers interrupted moves where possible.
7. A publisher mutation is authorized against the publisher key’s fixed organization, except an administrator key may explicitly target one organization. Viewer reads and mutations are authorized against the resolved viewer organization; administrators may browse all organizations, while cross-organization access is concealed as `404`.
8. An unresolved durability intent conceals the affected current or retained body until recovery. Reconciliation verifies the recorded digest before promoting a filesystem body; an unresolved or mismatched intent remains fail-closed with evidence rather than selecting an ambiguous body.
9. SQLite schema changes are ordered, transactional migrations recorded in `schema_migrations`.
10. MCP tool schemas are runtime contracts, not documentation only. Unknown, missing, or wrongly typed arguments produce JSON-RPC invalid-params errors.
11. Persistent data, secrets, repository metadata, and local planning files are excluded from Docker build contexts.
12. Artifact connection APIs are blocked by `connect-src 'none'`; presentation resources follow ADR-0003's allowlist. Viewer state crosses the sandbox only through the shell bridge.
13. Viewer-scope rows are readable and writable only by their owner; the bridge exposes a viewer handle, never an email. Administrators have no cross-viewer access to this scope.

## Trust model

There are three ingress paths:

- `POST /mcp` is bypassed by Cloudflare Access and authenticated by a publisher API key or, when enabled, an OAuth bearer token.
- Human routes are protected by Cloudflare Access. In production, `lib/identity.js` (Node reference) and `src/security/identity.rs` (Rust production) verify the Access identity before deriving viewer email, organization, and administrator status.
- Public-share routes use a revocable share token and serve read-only artifact content without a viewer shell.

The application shell is trusted code. Published artifact code is untrusted. CSP sandboxing gives raw HTML an opaque origin, including when it is opened directly, but the current deployment still delivers trusted and untrusted documents from one hostname. A separate artifact-delivery origin remains a defense-in-depth option; see ADR-0003.

## Module map

Rust is the production runtime. Node remains a reference implementation and conformance oracle; its similarly named modules describe behavior, not a second production deployment target.

### Rust production modules

- `src/main.rs` — production composition: configuration, SQLite and filesystem adapters, startup reconciliation, workers, health, and listener.
- `src/app.rs` — assembles `AppDeps` and the HTTP router; it does not bind a listener.
- `src/http/` — ingress, middleware, and routes for artifacts, raw delivery, state, feedback, admin, thumbnails, public shares, health, speech, gallery, and discussions.
- `src/mcp/` — JSON-RPC protocol handling, dispatch, validation, tool definitions, apps, tasks, and resources.
- `src/artifacts/` — lifecycle, durable storage, paths, reads, validation, digests, and reconciliation of current and retained bodies.
- `src/persistence/` — SQLite opening/migrations and domain adapters for artifacts, state, keys, organizations, shares, reactions, feedback, notifications, discussions, webhooks, and outbox delivery.
- `src/security/` — access policy, bearer authentication, identity/JWKS/OAuth, audit, cryptography, and access retry behavior.
- `src/integrations/` — preview generation, delivery, Discord, access synchronization, telemetry, and workers.
- `src/ports/`, `src/model/`, and `src/render/` — application boundaries, domain types, and server-rendered views.

### Node reference counterparts

- `server.js` — Node reference composition entrypoint: configuration, adapters, startup reconciliation, health check, and listener.
- `lib/app.js` — HTTP application module. `createApp()` accepts production or test adapters and never starts a listener.
- `lib/access.js` — organization and administrator policy, including concealed-read decisions.
- `lib/auth.js` / `lib/identity.js` — publisher-key authentication and Cloudflare viewer identity. These are security-critical modules.
- `lib/mcp.js` / `lib/contracts.js` — MCP method dispatch, declared tool schemas, and request validation.
- `lib/config.js` — shared upload and HTTP-envelope limits.
- `lib/store.js` — artifact lifecycle module: publication, reads, deletion, and storage reconciliation.
- `lib/db.js` / `lib/migrations.js` — SQLite opening, runtime database adapter, and ordered schema evolution.
- `lib/keys.js` / `lib/reactions.js` — publisher-key and reaction persistence.
- `lib/state.js` / `src/persistence/state.rs`: viewer-state persistence, transactional revision checks, and per-artifact limits.
- `lib/app.js` / `src/http/routes/state.rs`: authenticated viewer-state HTTP routes. `assets/shell.js` brokers state messages, caches reads, and debounces writes.
- `src/ports/state.rs` / `src/persistence/viewer_state_service.rs`: the authorization-gated viewer-state interface and its shared-pool SQLite adapter.
- `lib/artifact-http.js` — raw-delivery response policy, including HTML sandbox headers. Viewer state does not alter raw, historical, or public-share response bodies. Those views have no shell; artifacts treat state as disabled if no `state:ready` arrives within about one second of `state:hello`.
- `lib/portal.js` / `lib/settings.js` — server-rendered gallery, viewer shell, not-found, and key-management pages.

The Node reference seam is `createApp()`: its adapters are assembled in `server.js`, while HTTP tests provide in-memory adapters. Rust composes production dependencies in `src/main.rs` and exposes router construction through `build_router` in `src/app.rs`. Both runtimes keep storage lifecycle logic behind their artifact-store boundary.

## Supported workflows

### Publish

1. Authenticate the publisher key.
2. Validate the JSON-RPC call against the advertised tool schema.
3. Resolve the target organization from the key.
4. Write the artifact body to a staging path and validate its size and paths.
5. Insert the new artifact record, or commit the new current revision while retaining the outgoing body as history, then atomically promote the staged body.
6. Return the stable viewer URL; the artifact ID remains stable across updates.

### Read, update, and restore

1. Authorize the caller's organization and role or ownership, then resolve the current revision or an explicitly requested retained revision.
2. Read only a body whose durability intent is complete; unresolved intents fail closed, while recovery verifies the recorded digest before promoting a filesystem body.
3. An update stages replacement content and advances the current revision while preserving the previous revision body.
4. A restore reads an immutable retained revision and applies it as a new current revision, preserving the intervening history.

### View and react

1. Verify the viewer identity and resolve its organization.
2. Apply artifact access policy before reading bytes or rendering metadata.
3. Render the viewer shell or deliver raw content with the appropriate headers.
4. Validate reaction input and upsert the viewer’s reaction.

### Delete

1. Apply publisher ownership or viewer organization/admin policy.
2. Move the artifact body to a transient trash path.
3. Delete the artifact record; SQLite cascades its reactions.
4. Remove trash after success, or restore it if database deletion fails.

### Start and recover

1. Open SQLite, enable foreign keys and WAL, and apply unapplied migrations.
2. Reconcile durability intents and transient storage paths: recover digest-verified bodies for live records and remove unreferenced staging/trash remnants.
3. Report unresolved missing, ambiguous, or orphan bodies without silently deleting evidence or exposing an unverified body.

## Verification and operations

- `npm test` runs unit, persistence integration, MCP, and HTTP integration tests.
- Rust production checks use the pinned `1.97.1` toolchain: `cargo fmt --check`, `cargo clippy --all-targets --all-features --locked -- -D warnings`, `cargo test --all-targets --locked`, and `cargo test --doc --locked`.
- `node conformance/runner.mjs --impl both` exercises the shared MCP contract against both runtimes after building Rust with `cargo build --release --locked`.
- Browser checks run `cd playwright && npm test` against isolated Node and Rust servers with temporary data directories; they validate the user-facing shell and raw delivery without using production data.
- Every changed JavaScript file must pass `node --check`.
- Build with `docker compose build`; `.dockerignore` must prevent `.env` and `data/` from entering the image.
- Deployment is an owner-run operation; repository changes should not deploy automatically.
