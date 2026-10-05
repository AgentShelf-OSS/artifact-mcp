# Dashboard actions pilot

This operator-installed pilot adds a separate action bridge beside read-only
Connections. It supports `check-live-signals`, `analyze-differences`, and nine
literal `investigate-<ticket>` actions. It does not accept arbitrary API requests,
prompts, commands, ticket identifiers, or component names from an artifact.

`ARTIFACT_ACTIONS_FILE` selects a reviewed JSON grant list. Each grant contains
`artifact_id`, `org`, `revision`, `action`, and `worker_url`. The worker URL must
be HTTP loopback with an explicit port and no path, credentials, query, or
fragment. The server permits at most sixteen grants. Grants must match the
artifact's current revision and tenant.

The viewer routes are `GET /{id}/actions` and
`GET|POST /{id}/actions/{action}`. Owner or administrator authorization is
required. POST uses the existing same-origin mutation checks and accepts only
`{"request_id":"operator-selected-id"}`. `assets/actions.js` connects the
sandboxed artifact to these routes. Read-only Connections cannot invoke them.

## Ticket investigations

The nine fixed tickets are `OS-5082`, `SFD-686`, `SFD-703`, `SFD-709`, `SFD-724`,
`SFD-757`, `SFD-842`, `SFD-887`, and `SFD-978`. For example, the literal action
`investigate-SFD-842` maps to `/investigation/SFD-842/latest` or
`/investigation/SFD-842/start`. The dispatch payload includes the grant's
artifact ID, revision, literal action, and request ID.

Responses use `org-intelligence/investigation-run/v1`. The response contains
`ticketId`, `current`, `latest`, `history`, `availableAt`, `workerCheckedAt`, and
`teamBusy`. Run actions must match the selected literal action. The validator
checks provenance, stage counts, proposal fields, ticket-owned sources, and
citations. The latest saved proposal must be a successful run with a result.
Current and historical receipts can omit duplicate model output. Responses are
limited to 65,536 bytes. Invalid upstream replies produce a generic error.

Signed audit entries use `artifact.action.investigate-finding`. They record the
request and dispatch outcome without model output or ticket evidence.

The worker owns admission, cooldowns, fresh evidence collection, CLI selection,
and durable proposal storage. There is no apply action. Deployment and recovery
instructions belong to the owning workspace and homelab operator runbooks.

## Validation

Run `cargo test --lib actions::tests`, `cargo test --test native u17_routes`,
and `cargo test --test native u58_admin_audit`. These check literal routing,
response validation, viewer boundaries, same-origin mutations, and signed audit
records. Confirm that each test invocation reports a nonzero test count.
