# Conformance oracle

A language-neutral, black-box conformance suite for artifact-mcp. It drives a **real server
process** over HTTP, then inspects the persistent state it left behind. It never imports
application code. The goldens were first recorded from the retired Node reference server and
are now the executable contract for the Rust server: a golden diff is a behavior change.

Implements blueprint §B2 (harness design), §B3 (comparison modes) and §B4 (required
conformance families).

## Running

The runner is a Rust integration test, `tests/conformance/`. It starts the `artifact-mcp`
binary that Cargo builds for the same test run (`CARGO_BIN_EXE_artifact-mcp`), so no separate
build step or binary path is needed.

```bash
cargo test --test conformance                                 # gate: replay every recorded golden
CONFORMANCE_FILTER=raw cargo test --test conformance          # subset by case id or tag
CONFORMANCE_RECORD=1 cargo test --test conformance            # regenerate goldens (see discipline)
cargo test --test conformance -- --nocapture                  # also print every case result
```

| Variable | Effect |
|---|---|
| `CONFORMANCE_FILTER=<text>` | Run only cases whose id or any tag contains `<text>`. No match is a failure. |
| `CONFORMANCE_RECORD=1` | Write goldens instead of comparing. Any other non-empty value except `0` is a usage error. |
| `CONFORMANCE_JOBS=<n>` | Number of cases that run at the same time (default: CPU count, at most 4). Each case still has its own server and data dir. |

The test fails on any diff, failed assertion, missing golden, or runner error. All case
failures are reported together in one panic message. A server that does not start, exits
before `/health` answers, or misses the 15 s health deadline fails its case, and the failure
includes the server output.

### Per case, the runner

1. copies the named fixture into a **fresh temp `DATA_DIR`**,
2. starts the target server on a **random loopback port**,
3. waits for `/health`,
4. executes the case's HTTP steps, capturing generated ids,
5. normalizes **only declared-volatile fields**,
6. compares each declared aspect against the golden,
7. stops the server **cleanly**, then
8. runs post-state SQL / file-hash / directory assertions.

Cases never share a data directory: each run gets its own temporary copy of the fixture, and
the temp dir is removed afterwards. The server binds its port only after it migrates the data
dir, so a start that fails with `Address already in use` is retried (at most 3 times) with a
new port and a fresh data dir. No other failure is retried.

## Case schema

One JSON file per case in `cases/`. The file name is the case id.

```jsonc
{
  "id": "raw.single-html.delivery",
  "tags": ["raw", "csp", "invariant-4"],   // CONFORMANCE_FILTER matches id or any tag
  "fixture": "empty-v21",                   // directory under fixtures/
  "env": { "MCP_JSON_LIMIT": "1mb" },       // merged over the base env, per case
  "pending": "reason",                      // present => reported PEND, never gates
  "note": "why this case exists",

  "steps": [
    {
      "name": "human-readable step label",
      "request": {
        "method": "POST",
        "path": "/mcp",                     // sent VERBATIM (no URL normalization)
        "headers": { "authorization": "Bearer ${key_acme}" },
        "json": { },                        // OR "rawBody": "...", OR "genBody": {...}
      },
      "capture": { "artifact_id_1": "result.structuredContent.id" },
      "assertSymbols": [
        { "symbol": "artifact_id_1", "length": 12, "alphabet": "0123456789abcdefghijkmnpqrstuvwxyz" }
      ],
      "expect": {
        "status": 200,
        "headers": {
          "mode": "exact-header",
          "require": { "content-security-policy": "sandbox allow-scripts ..." },
          "forbid": ["allow-same-origin"]
        },
        "body": { "mode": "canonical-json", "volatileFields": ["created_at"] },
        "sameAsStep": 3                     // must be indistinguishable from step 3
      }
    }
  ],

  "postState": {
    "sql":        [{ "name": "artifact_row", "query": "SELECT ...", "volatileFields": [] }],
    "files":      [{ "path": "artifacts/${artifact_id_1}.html" }],   // presence + size + sha256
    "dirEntries": [{ "path": "artifacts" }]                          // sorted entry names
  }
}
```

### Symbols

Steps may declare `mcpSuccess: true` or an exact `mcpError` under `expect` for assertions that
remain active while recording. An `afterSql` array runs after that step's captures and exists only
for implementation-neutral fixture shaping that public APIs cannot express, such as changing an
existing author key to reader after it published the reader-own fixture. It is not a production
workflow mechanism.

`${name}` is substituted into paths, header values, JSON bodies and SQL queries.

- **Constants** (`tests/conformance/main.rs`): publisher secrets `${key_acme}`, `${key_acme2}`,
  `${key_beta}`, `${key_admin}`; client ids; viewer emails `${viewer_acme}`,
  `${viewer_beta}`, `${admin_email}`; `${public_base}`. Forward-substituted only.
- **Captures**: values pulled out of a response via `capture` (a dot path into the parsed
  JSON body). These are high-entropy generated ids/tokens, so they are additionally
  **back-substituted** — every occurrence of the literal value is rewritten to `${name}`
  in bodies, header values (e.g. `location`), SQL rows, file paths and directory entries
  before anything is compared or recorded. That is what makes goldens run-independent.

Generated ids are never trusted blindly: `assertSymbols` separately pins their **alphabet
and length** (artifact ids 12 chars of `0123456789abcdefghijkmnpqrstuvwxyz`; share tokens
24 chars of `[0-9A-Za-z_-]`), so a rewrite cannot quietly change the id space.

### Authentication in cases

- **MCP** (`POST /mcp`) — `authorization: Bearer ${key_*}`. Keys are seeded from
  `ARTIFACT_API_KEYS` at boot.
- **Human routes** — `cf-access-authenticated-user-email: ${viewer_*}` with
  `TRUST_ACCESS_HEADERS=1`. `acme.test` → org `acme`, `beta.test` → org `beta`,
  `${admin_email}` is an admin. Sending no such header models an unsigned visitor.
- **Public shares** (`/s/:token`) — no identity at all; the token is the boundary.

`PUBLIC_BASE_URL` is pinned to `http://conformance.test` so response `url` fields do not
depend on the random port.

## Comparison modes (blueprint B3)

| Mode | Used for | Rule |
|---|---|---|
| `exact-bytes` | Raw artifacts, downloads, anchor injection, 404 pages | Every byte must match. Text bodies stored as UTF-8, binary as base64 + sha256. |
| `exact-header` | CSP, content type, cache control, disposition, `nosniff`, referrer policy | Names lowercased; `date`, `server`, `content-length`, `connection`, `keep-alive`, `transfer-encoding` stripped; remaining values exact. |
| `canonical-json` | HTTP JSON and most JSON-RPC envelopes | Parse, recursively key-sort, compare. **Array order stays significant.** |
| `exact-json-text` | MCP `content[0].text` and the `tools/list` golden | As `canonical-json`, plus the embedded JSON string is asserted to be valid JSON. Because `text` is a string leaf, it is compared **exactly**. |
| `state` | SQLite / filesystem effects | Named SQL queries, directory entries, and sha256 file hashes. |
| `html-dom` | Large trusted pages | Exact snapshot is the primary gate; DOM/browser assertions are a later, secondary gate. |
| `viewer-handle` | Viewer shell identity | Decode the JSON literals in `data-viewer-id` and `data-viewer-name` on `#shell-config` and compare both exactly. Reject missing or malformed handles. Other shell markup is outside this comparison. |

Two extra assertions layer on top of any mode:

- `expect.headers.require` / `forbid` — a named header must equal a value, or no header may
  contain a substring. `forbid: ["allow-same-origin"]` is how invariant 4 is enforced
  independently of whatever the golden happens to hold.
- `expect.sameAsStep: N` — this response must be **indistinguishable** (status + headers +
  body) from step `N` of the same run. This encodes the concealment/uniformity contracts
  (invalid vs revoked share, foreign vs nonexistent artifact) without depending on a golden.

## Volatile-field policy

**Nothing is normalized unless the case declares it.** Timestamps from a fixed fixture stay
exact; only fields a case lists in `volatileFields` are replaced with the sentinel
`"<volatile>"`, matched by property name anywhere in the tree (and by column name for SQL).

The only other normalizations are structural and unavoidable, and they are deliberately
narrow:

- transport-only headers (`date`, `server`, `content-length`, …) are dropped;
- captured high-entropy ids/tokens are back-substituted to their symbol name.

`etag` is **not** stripped: it is content-derived, therefore deterministic, and a change in
it means a change in bytes.

## Record discipline

- Goldens are recorded from the Rust server, which is now the only implementation. Recording
  is therefore circular by construction: it freezes whatever the server does today. Record
  only to adopt an intended behavior change, and review the golden diff as that change.
- **`CONFORMANCE_RECORD=1` is forbidden in CI.** CI only replays the goldens.
- Recording is refused for any case whose runtime assertions fail, so a broken state can
  never be frozen as "expected".
- A golden diff is a **contract change**. Regenerating a golden to make a build pass is only
  legitimate when the behavior change is intended, and the golden diff must be reviewed as
  the specification change that it is.
- Cases marked `pending` are never recorded and never gate.

Goldens live in `goldens/<case id>.json` and are managed entirely by the runner — a case
manifest does not name its golden file.

## Files

| File | Role |
|---|---|
| `../tests/conformance/main.rs` | Test entry point, case loading, substitution/capture, assertions, golden compare, record mode, post-state |
| `../tests/conformance/server.rs` | Boots the server binary against the case data dir and port; SIGTERM, then SIGKILL after 3 s |
| `../tests/conformance/http.rs` | HTTP/1.1 client that sends the request-target verbatim and folds response headers as `node:http` does |
| `../tests/conformance/comparators.rs` | The B3 modes as pure normalizers |
| `../tests/conformance/js.rs` | JSON with JavaScript semantics (key order, number format, `JSON.stringify` output) |
| `cases/` | Case manifests |
| `goldens/` | Recorded expectations |
| `fixtures/` | Starting `DATA_DIR` states (see `fixtures/README.md`) |
| `NOTES.md` | Deferrals, stubs, and source changes wanted but out of scope |

The runner uses only crates the server already depends on. Post-state SQL uses `rusqlite`, opened
the way `node:sqlite` opened it for the original recordings (foreign keys on, double-quoted string
literals rejected, 5 s busy timeout). Post-state reads open the database read-only after the
server stops.

### Golden format

The goldens were first written by a Node runner as `JSON.stringify(normal, null, 2)`. The Rust
runner keeps that format byte for byte: JavaScript property order (array-index keys first, then
insertion order), JavaScript number formatting, and `JSON.stringify` string escaping. A record run
with no behavior change therefore rewrites only files that were edited by hand.
`goldens/publisher.write-concealment.json` is such a file: it has hand-wrapped arrays, so record
mode reformats it without changing its content.
