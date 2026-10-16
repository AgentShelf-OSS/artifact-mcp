# 15. Publish upsert by artifact key

Date: 2026-10-15

## Status

Accepted

## Context

Agents often publish the same logical artifact again in a later session without its id. Each
`publish_artifact` or `publish_bundle` call makes a new artifact, so the gallery fills with
duplicates. `update_artifact` keeps the same id and URL, but the agent must remember the id. Users
repeatedly ask agents to "update it in place, don't make a new one".

## Decision

`publish_artifact` and `publish_bundle` accept an optional `artifact_key`. It is a stable name that
the publisher chooses, for example `project/topic`.

- **Grammar.** `^[a-z0-9][a-z0-9._/-]{0,127}$`. Any other value is a JSON-RPC invalid-params
  error. Both runtimes check the key before the role check and before any lookup.
- **Namespace.** The key is unique per organization. Migration 040 adds a nullable
  `artifacts.artifact_key` column and the partial unique index
  `artifacts_org_artifact_key ON artifacts(org, artifact_key) WHERE artifact_key IS NOT NULL`.
  Keys in different organizations never match, so a key lookup cannot reveal another tenant
  (invariant 3).
- **Create.** If no ready artifact in the target organization has the key, publication continues as
  usual and stores the key. The result adds `revision`, `artifact_key`, and `created: true`.
- **Update.** If an artifact has the key, the server applies the `update_artifact` write policy
  and then the same update. The `id` and URL stay the same. The revision increments only when
  something changes. Omitted `title`, `description`, and `category` stay. A bundle `files` map is
  a complete snapshot. The expected revision is the revision that the lookup read. The result has
  the normal publish shape plus `revision`, `artifact_key`, and `created: false`.
- **Refusals.** A caller that cannot modify the keyed artifact gets the same error as
  `update_artifact`. The error does not include the id. A key that belongs to the other artifact
  type returns an error and changes nothing.
- **Delete and move.** Deletion removes the row, so the key becomes free and the next keyed publish
  creates a new artifact. An administrator move to another organization sets the key to `NULL`.
  The key is part of the source organization's namespace, and keeping it could collide in the
  destination.
- **Concurrency.** The unique index is the guard. If an INSERT violates it, the publish
  compensates as for any failed insert: it removes the staged body and the durability intent.
  It then reports a typed conflict. The MCP layer resolves the key again one time and updates the
  winner. In Rust, the lifecycle write gate serializes publishes, so two concurrent creates
  always end as one create and one update. If the winner is still invisible behind its
  durability intent, the caller gets
  `artifact_key is being published by another request; retry`. This also happens when the
  lookup lands while another keyed update holds its intent, or when the winner runs in a
  second process. More than two racers become
  concurrent updaters with `update_artifact` semantics, so a stale updater gets the normal
  revision-conflict error. It is never a second artifact.
- **Discovery.** `list_artifacts` and `read_artifact` include `artifact_key` only for artifacts
  that have one. Calls without `artifact_key` keep their exact earlier output.

## Consequences

- An agent that sends the same key each time gets one artifact with a revision history, not
  many duplicates.
- The advertised input schemas and typed output schemas of both publish tools change. The frozen
  tool-definition golden is recorded again from Node.
- Keyed publication does not overwrite blindly. Under heavy contention on one key, a caller can
  get a conflict and must retry. This matches an `update_artifact` call without
  `expected_revision`.
- No HTTP publish route exists, so the MCP tools are the only surface.
