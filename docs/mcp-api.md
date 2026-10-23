# MCP API and tools

Artifact MCP exposes MCP JSON-RPC at `POST /mcp`. Publishing clients authenticate with an API key
or a configured OAuth bearer token.

## Protocol versions

The server supports two contracts side by side:

- Stateful MCP `2025-06-18` for existing clients.
- Stateless MCP `2026-07-28` for clients that negotiate typed outputs, resources, MCP Apps, and
  durable tasks.

Legacy `initialize` always negotiates `2025-06-18`. An unsupported string version, including
`2026-07-28` in a legacy handshake, falls back to that supported legacy version. Omitting
`protocolVersion` keeps the same default; supplying a non-string returns `-32602` (Invalid Params).
Modern clients use stateless discovery and request metadata instead of `initialize`.

Request IDs must be strings or integers. Empty strings, zero, and negative integers are valid.
A present null, boolean, array, object, or fractional ID returns `-32600` (Invalid Request)
before the method runs. Omitting the ID remains a notification with no JSON-RPC response.
Errors with an unreadable ID use `id: null` in the legacy contract and omit `id` in the modern
contract. Malformed requests with a valid ID preserve it in the error response.
Use string IDs for values outside JavaScript's safe integer range (`±9007199254740991`) to
preserve the identifier exactly.

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
| `publish_artifact(html, title, description, category, org, artifact_key?, discard_newer?)` | Publish one self-contained HTML page. With `artifact_key`, update the existing keyed artifact in place. |
| `publish_bundle(files, entry, title, description, category, org, artifact_key?, discard_newer?)` | Publish a multi-file artifact. `files` maps paths to content. With `artifact_key`, replace the existing keyed bundle in place. |
| `list_artifacts()` | List artifacts published by the current key, including their URLs. |
| `read_artifact(id, path?, revision?, offset?, limit?)` | Read an artifact, retained revision, or bundle file with bounded UTF-8 paging. |
| `update_artifact(id, html\|files, entry, title, description, category, expected_revision?, discard_newer?)` | Replace content or metadata at the same URL and create a revision. |
| `patch_artifact(id, expected_revision, edits, path?)` | Apply an atomic, revision-guarded batch of UTF-8-safe partial edits. |
| `set_visibility(id, hidden)` | Unlist or relist an artifact. |
| `list_collections(org?, limit?, cursor?)` | List accessible folder summaries. |
| `get_collection(id, org?, limit?, cursor?)` | Read folder metadata and paginated artifact references. |
| `create_collection(name, org?, description?, color?, cover_artifact_id?, artifact_ids?)` | Create an organization folder with optional initial members. |
| `update_collection(id, org?, name?, description?, color?, cover_artifact_id?)` | Edit folder metadata while preserving omitted fields. |
| `delete_collection(id, org?)` | Delete a folder while preserving its artifacts. |
| `add_artifacts_to_collection(id, artifact_ids, org?)` | Add up to 100 same-organization references atomically. |
| `remove_artifacts_from_collection(id, artifact_ids, org?)` | Remove references while preserving artifacts and other folders. |
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
| `list_data_sources(org?)` | Discover configured sources and public operation/subscription definitions for an organization. |
| `get_data_bindings(id)` | Read an artifact's live-data bindings. |
| `set_data_bindings(id, bindings)` | Atomically replace an artifact's bindings with authorized named sources. |
| `set_artifact_data(id, binding, key, value)` | Update a producer snapshot without changing the HTML revision. |
| `append_artifact_events(id, binding, subscription, events)` | Append an idempotent producer event batch for subscribed viewers. |
| `set_artifact_reminder(id, key, fire_at?, delay_seconds?, title?, body?, clear?)` | Arm, replace, or clear a named `org` reminder. Opted-in viewers of the artifact's organization get a Web Push notification when it fires. |

The legacy catalog contains 34 tools. MCP 2026 adds `regenerate_artifact_preview` for 35. A client
that negotiates MCP Apps also receives the app-only `submit_feedback` action.

Live-data discovery and binding reads require `artifacts:read`; binding and producer writes require
`artifacts:publish` plus the existing artifact write policy.

`set_artifact_reminder` requires `artifacts:publish` and the artifact write policy. Give exactly one
of `fire_at` (epoch milliseconds) or `delay_seconds`; the time must be 60 seconds to 30 days ahead.
`title` is 1 to 80 characters and `body` at most 240. Setting a key again replaces its time and text
and increments `revision`. An artifact can have at most 16 armed reminders. `clear: true` deletes the
key and ignores the other fields. The result is `{id, key, scope: "org", fire_at, revision}` or
`{id, key, cleared: true}`. When the server has no Web Push configuration, the tool returns the error
`Web Push reminders are not configured on this server.` See [scheduled reminders](web-push-reminders.md). See [live data and PR Watch](live-data.md)
for source configuration, client examples, limits, and publication.

## Republishing by artifact key

Agents often publish the same logical artifact again without its `id`. To prevent duplicates, pass
a stable `artifact_key` to `publish_artifact` or `publish_bundle`, for example
`<project>/<topic>`. The key is 1 to 128 characters from `a-z`, `0-9`, `.`, `_`, `/`, and `-`,
and it starts with a letter or digit. Any other value is a JSON-RPC invalid-params error.

A key is unique in one organization. Keys in different organizations never match.

- No artifact in the target organization has the key: the server creates a new artifact and
  stores the key. The result adds `revision: 1`, `artifact_key`, and `created: true`.
- An artifact has the key: the server updates it exactly as `update_artifact` does. The same
  write policy applies. The `id` and URL stay the same, the revision increments when something
  changes, omitted `title`, `description`, and `category` stay, and a bundle `files` map replaces
  the complete snapshot. The result has the normal publish fields plus `revision`, `artifact_key`,
  and `created: false`.
- A key that belongs to a bundle cannot be used with `publish_artifact`, and the reverse. The
  server returns an error and changes nothing.
- A key that belongs to an artifact the caller cannot modify returns the same permission error as
  `update_artifact`.

Deleting an artifact frees its key. An administrator move to another organization removes the
key. `list_artifacts` and `read_artifact` include `artifact_key` only for artifacts that have one.
Results of a publish without `artifact_key` do not change. When two requests create the same key
at the same time, one creates the artifact and the other updates it. A concurrent update that
reads a stale revision fails with the `update_artifact` conflict error. See
[ADR-0015](adr/0015-publish-upsert-by-artifact-key.md).

## Stale-base guard

A full-content update replaces the whole body. Before such an update, the server finds the retained
revision that the new body is most similar to. This is the **base**. If the base is older than the
current revision, and the new body removes enough of the work added after the base, the update is
**stale**: it was probably built from an old local copy.

The guard applies to `update_artifact` with `html` or `files`, and to `publish_artifact` and
`publish_bundle` with an `artifact_key` that updates an existing artifact. It does not apply to
metadata-only or entry-only updates, `patch_artifact`, `restore_artifact`, or a publish that
creates an artifact. It runs after the write policy, so a caller that cannot update the artifact
gets the usual error. When `update_artifact` sends an `expected_revision` that is not current, the
usual revision-conflict error is returned and the guard does not run.

The text for a stale update is:

```text
Update is based on revision {base}, but the artifact is at revision {current}. It would discard changes from {range}. Read the current revision and apply your change to it, use restore_artifact to roll back on purpose, or pass discard_newer: true.
```

`{range}` is `revision {current}` when the base is the previous revision, otherwise
`revisions {base+1}-{current}`.

| `STALE_GUARD` | Behavior for a stale update |
|---|---|
| `warn` (default) | The update is written. The result adds `warnings: ["<text>"]`. |
| `enforce` | The update is refused with a tool error (`isError: true`) that has the text. Nothing changes. |
| `off` | The guard does not run. |

A result has a `warnings` field only for a stale update in `warn` mode. All other results are
unchanged. To write a stale update on purpose, pass `discard_newer: true`; the update continues
with no warning. `discard_newer` must be a boolean; any other value is a JSON-RPC invalid-params
error (`discard_newer must be a boolean`). It is accepted, and has no effect, on a publish that
creates an artifact.

| Variable | Default | Meaning |
|---|---|---|
| `STALE_GUARD` | `warn` | `warn`, `enforce`, or `off`. Other values use `warn`. |
| `STALE_GUARD_MIN_SEGMENTS` | `20` | Minimum count of lost segments for a stale update. Must be a positive integer. |
| `STALE_GUARD_MIN_SHARE` | `0.10` | Minimum share of the newer segments that must be lost. Must be greater than 0 and at most 1. |

Each stale detection writes one log line, `stale_base_detected id=<id> base=<n> current=<n>
newer=<n> lost=<n> mode=<mode> discard_newer=<true|false>`. It does not log content. See
[ADR-0023](adr/0023-stale-base-guard.md) for the segment and comparison rules.

## Organization folders

Collections are the folders shown in Reel Shelf, Contact Sheets, and Gallery Ribbons.
All three views use the same membership links. A folder can reference an artifact that is
also in another folder. Adding or removing a reference does not change artifact content,
revision, category, organization, visibility, or public shares. Deleting a folder removes
only that folder and its links.

Categories label artifacts within an organization. Organizations define access boundaries.
Folders group readable artifacts within one organization. View selection, ribbon order,
and collapse state are personal gallery preferences and are not changed by these tools.

An organization credential is locked to its own organization. Administrator credentials must
supply a concrete registered `org` for every collection tool. The virtual `all` gallery scope
cannot own a folder. Readers can list and inspect folders but cannot mutate them. Authors and
collaborators can create folders and edit the folders they created. Administrators can edit any
folder. Artifact references and covers still require the caller's existing artifact read access;
a folder never grants extra access to its members.

MCP folders belong to the authenticated service principal. An API key uses its stable client ID;
OAuth uses the configured issuer and client ID together. A key label, assigned owner email, or
caller-supplied field cannot impersonate a browser folder creator. Browser-created folders
keep their email creator. A browser administrator can curate a service-owned folder. Replacing
a secret while retaining the client ID preserves ownership; creating a different client ID does
not inherit old folders. See [ADR-0013](adr/0013-mcp-collection-principals.md).

OAuth reads require `artifacts:read`; collection mutations require `artifacts:publish`, including
folder deletion. Existing artifact deletion continues to require `artifacts:delete`. Collection
read results filter hidden artifact discovery according to the publisher policy and include only
authorized references, counts, and covers.
The `editable` flag also reflects the caller's role and granted OAuth publish scope.

`list_collections` and `get_collection` default to 25 results and accept `limit` from 1 to 100.
Pass the returned `next_cursor` unchanged to continue the same query with the same credential
and page size. An empty `next_cursor` means there are no more results. Cursors bind the caller,
organization, operation, folder, and page size; they never authorize access. Permissions are
checked again on every request. Concurrent membership changes can shift page boundaries.

Create and update return `collection` summaries. Colors and cover IDs use an empty string when
unset. Omitted update fields stay unchanged; an empty `description`, `color`, or
`cover_artifact_id` clears that field. A nonempty cover must be a readable current member.
Names are limited to 80 characters, descriptions to 500, collections to 200 per organization,
and memberships to 1,000 per collection. Each membership request accepts at most 100 IDs and
commits all of them together. Duplicate names return a conflict instead of updating a folder.

Add/remove requests are safe to retry. They report `added` and `already_present`, or `removed`
and `already_absent`. Creation is not idempotent: after a lost response, list folders to recover
the ID rather than retrying blindly. Repeating deletion returns Not found. The underlying state
remains deleted and the artifacts remain intact.

For example, publish your artifacts first and then use their returned IDs:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"create_collection","arguments":{"name":"Interface ideas","artifact_ids":["<artifact-a>","<artifact-b>"],"cover_artifact_id":"<artifact-a>"}}}
```

Use the returned `result.structuredContent.collection.id` for subsequent calls:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"add_artifacts_to_collection","arguments":{"id":"<collection-id>","artifact_ids":["<artifact-c>"]}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_collection","arguments":{"id":"<collection-id>","limit":25}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"update_collection","arguments":{"id":"<collection-id>","name":"Interaction research","color":"#c4d8cd"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"remove_artifacts_from_collection","arguments":{"id":"<collection-id>","artifact_ids":["<artifact-b>"]}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"delete_collection","arguments":{"id":"<collection-id>"}}}
```

Administrator calls must also include `org`. Folder mutations use the existing transactional
security audit with the MCP credential as actor and `mcp` as source. No gallery preference is
written by this workflow. Refresh the browser library to see the shared folder changes.

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
