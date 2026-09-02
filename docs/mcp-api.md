# MCP API and tools

Artifact MCP exposes MCP JSON-RPC at `POST /mcp`. Publishing clients authenticate with an API key
or a configured OAuth bearer token.

## Protocol versions

The server supports two contracts side by side:

- Stateful MCP `2025-06-18` for existing clients.
- Stateless MCP `2026-07-28` for clients that negotiate typed outputs, resources, MCP Apps, and
  durable tasks.

Clients without the newer capabilities receive the ordinary text and structured result fallback.
Artifact resources use private-cache-aware responses through `resources/list`, `resources/read`,
and `resources/templates/list`. Configured servers also support `server/discover`.

## Authentication and authorization

API keys use this header:

```http
Authorization: Bearer <API key>
```

An organization key can only act within its organization. An administrator key may target another
organization with an `org` argument. Operations that change an artifact or read another
publisher's private data require the artifact owner or an administrator.

OAuth deployments accept short-lived JWT access tokens. See the [configuration reference](configuration.md#oauth)
for required claims and scopes.

## Tool catalog

| Tool | Purpose |
|---|---|
| `publish_artifact(html, title, description, category, org)` | Publish one self-contained HTML page. |
| `publish_bundle(files, entry, title, description, category, org)` | Publish a multi-file artifact. `files` maps paths to content. |
| `list_artifacts()` | List artifacts published by the current key, including their URLs. |
| `read_artifact(id, path?, revision?, offset?, limit?)` | Read an artifact, retained revision, or bundle file with bounded UTF-8 paging. |
| `update_artifact(id, html\|files, entry, title, description, category)` | Replace content or metadata at the same URL and create a revision. |
| `patch_artifact(id, expected_revision, edits, path?)` | Apply an atomic, revision-guarded batch of UTF-8-safe partial edits. |
| `set_visibility(id, hidden)` | Unlist or relist an artifact. |
| `list_categories(org?)` | List categories for the current organization or an administrator-selected organization. |
| `set_category(id, category)` | Assign a category without creating a content revision. |
| `create_category(name, org?)` | Add an organization category. |
| `delete_category(name, org?)` | Remove an organization category. |
| `delete_artifact(id)` | Delete an artifact and its related state. |
| `list_revisions(id)` | List retained revision history. |
| `restore_artifact(id, revision)` | Restore a retained body as a new revision. |
| `create_share(id, expires)` | Create an unlisted public link with optional expiry. |
| `list_shares(id)` | List active public links for an artifact. |
| `revoke_share(token)` | Revoke a public link immediately. |
| `artifact_stats(id)` | Return views, unique viewers, and the authorized named-viewer list. |
| `list_feedback(id?)` | List threaded viewer feedback and anchor evidence. |
| `resolve_feedback(feedback_id)` | Mark a feedback thread resolved. |
| `reopen_feedback(feedback_id)` | Reopen a resolved feedback thread. |
| `regenerate_artifact_preview(id)` | Regenerate the current single-file thumbnail. Newer clients receive a durable task. |

The legacy catalog contains 21 tools. MCP 2026 adds `regenerate_artifact_preview` for 22. A client
that negotiates MCP Apps also receives the app-only `submit_feedback` action.

## Durable tasks

Clients that advertise `io.modelcontextprotocol/tasks` may receive a durable task from
`regenerate_artifact_preview`. Use `tasks/get` to poll it, `tasks/update` to acknowledge input
updates, and `tasks/cancel` to request cooperative cancellation.

Task state lives under the data volume and resumes after restart. Clients without Tasks support
receive the same preview operation as a bounded synchronous result. No other artifact operation
uses tasks.

## Reconnect after an upgrade

MCP clients often cache `tools/list` for the life of a connection. Reconnect the integration after
upgrading Artifact MCP so the client sees new tools and fields.

## Keeping the contracts synchronized

The release gate derives its report from frozen tool definitions, typed output schemas, Rust
dispatch, OAuth scope mapping, documentation, conformance cases, and native test registration:

```bash
node scripts/check-mcp-surface.mjs
```

For an intentional compatibility change that spans a branch, include the base revision:

```bash
node scripts/check-mcp-surface.mjs --base origin/master
```

Update the existing definition, schema, dispatch, docs, and tests together. Do not add a separate
handwritten registry. Run the conformance and Rust test commands reported by the checker.

[Return to the documentation index](README.md).
