# Conformance oracle

A language-neutral, black-box conformance suite for artifact-mcp. It drives a **real server
process** over HTTP, then inspects the persistent state it left behind. It never imports
application code, so the same cases judge the Node reference today and the Rust rewrite at
M1+ — this is the executable definition of "the rewrite behaves identically".

Implements blueprint §B2 (harness design), §B3 (comparison modes) and §B4 (required
conformance families).

## Running

```bash
node conformance/runner.mjs --impl node            # gate: replay every recorded golden
node conformance/runner.mjs --impl both            # Node fully; Rust degrades if unbuilt
node conformance/runner.mjs --impl rust            # M1+ only
node conformance/runner.mjs --impl node --record   # regenerate goldens (see discipline)
node conformance/runner.mjs --impl node --filter raw   # subset by case id or tag
```

Exit codes: `0` all good, `1` a diff / failed assertion / runner error, `2` bad usage.

`--impl rust` fails while no binary exists (nothing ran is not success). `--impl both`
deliberately does **not** fail for a missing Rust binary — it reports `[SKIP]` and gates on
Node, so `both` is safe to wire into CI now and gains Rust coverage for free later. Point
`RUST_ARTIFACT_MCP_BIN` at the binary (or `RUST_ARTIFACT_MCP_DIR` at the crate) to enable it.

### Per case, the runner

1. copies the named fixture into a **fresh temp `DATA_DIR`**,
2. starts the target server on a **random loopback port**,
3. waits for `/health`,
4. executes the case's HTTP steps, capturing generated ids,
5. normalizes **only declared-volatile fields**,
6. compares each declared aspect against the golden,
7. stops the server **cleanly**, then
8. runs post-state SQL / file-hash / directory assertions.

Node and Rust never share a data directory: each implementation gets its own `mkdtemp` copy
of the fixture, and the temp dir is removed afterwards.

## Case schema

One JSON file per case in `cases/`. The file name is the case id.

```jsonc
{
  "id": "raw.single-html.delivery",
  "tags": ["raw", "csp", "invariant-4"],   // --filter matches id or any tag
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
for cross-runtime-neutral fixture shaping that public APIs cannot express, such as changing an
existing author key to reader after it published the reader-own fixture. It is not a production
workflow mechanism.

`${name}` is substituted into paths, header values, JSON bodies and SQL queries.

- **Constants** (`testkit.mjs`): publisher secrets `${key_acme}`, `${key_acme2}`,
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
  `ARTIFACT_API_KEYS` at boot, exactly as `npm run dev` does.
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

- `--record` is only accepted with `--impl node`. Goldens are recorded from the Node
  reference; recording from the implementation under test would be circular.
- **`--record` is forbidden in CI.** CI runs `--impl node` (or `--impl both`) only.
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
| `runner.mjs` | CLI, orchestration, substitution/capture, golden compare, post-state |
| `node-driver.mjs` | Boots the real `node server.js` |
| `rust-driver.mjs` | Same interface; explicit "not built yet" stub until M1 |
| `comparators.mjs` | The B3 modes as pure normalizers |
| `testkit.mjs` | Credentials, base env, dependency resolution, HTTP client |
| `cases/` | Case manifests |
| `goldens/` | Recorded expectations |
| `fixtures/` | Starting `DATA_DIR` states (see `fixtures/README.md`) |
| `NOTES.md` | Deferrals, stubs, and source changes wanted but out of scope |

Node stdlib only — no new dependencies. `better-sqlite3` is resolved at runtime from an
existing built `node_modules` purely to read post-state (override with
`CONFORMANCE_NODE_MODULES`).
