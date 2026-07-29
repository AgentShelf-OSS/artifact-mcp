# Conformance notes — stubs, deferrals, and wanted source changes

Working notes for U00 (M0). Everything here is deliberate scope, not oversight.

## TODO — the pending invariant-3 golden (blueprint §B6)

`cases/human-concealment.invariant3.json` exists and is **complete**, but is marked:

```json
"pending": "golden recorded post-patch (M0 §B6)"
```

so it is reported `[PEND]` and never gates.

It is the 404-uniformity matrix: **14 artifact-scoped human routes × 2 personas (unsigned,
cross-org) × {existing-foreign id, nonexistent id} = 57 steps**, each "nonexistent" step
paired to its "existing-foreign" predecessor with `sameAsStep`.

Routes covered: `GET /:id`, `GET /raw/:id`, `GET /thumbnails/:id`, `GET /:id/shares`,
`GET /:id/history`, `GET /:id/feedback`, `POST /:id/feedback`, `POST /:id/react`,
`POST /:id/visibility`, `POST /:id/category`, `POST /:id/move`, `POST /:id/share`,
`POST /:id/restore`, `DELETE /:id`.

**It currently FAILS against Node, which is the point** (§B6: "make the matrix fail against
current Node behavior for cross-org mutations"). Verified failure shape today:

- existing-but-foreign artifact, unsigned viewer → `401 {"error":"Not signed in"}`
- nonexistent artifact, unsigned viewer → `404 {"error":"Not found"}`
- `POST /:id/move` (admin-only) → `403` for existing vs `404` for nonexistent

…i.e. the status itself discloses whether the id exists. Read routes (`/:id`, `/raw/:id`,
`/thumbnails/:id`, `/:id/history`, `/:id/feedback` GET) already conceal correctly; the
mutation routes are the leak.

**Hand-off:** a sibling leg centralizes artifact lookup + concealed access so foreign and
nonexistent ids return the same 404 everywhere. After that merges:

1. `node conformance/runner.mjs --impl node --record --filter human-concealment`
2. delete the `"pending"` key from the case manifest
3. commit the golden as the reviewed contract change

Do **not** record this golden against unpatched Node — that would freeze the leak.

## Source changes wanted but NOT made (file-ownership boundary)

U00 owns `conformance/**` only. No file under `lib/`, `test/`, `server.js` or `package.json`
was touched. Changes I would otherwise have made:

1. **The concealment fix itself** (above) — owned by the sibling leg. Noted here rather than
   applied.
2. **Nothing else.** No source defect was found that blocked building the oracle. The Node
   server was drivable as-is for every family in scope.

## Stubs

- **`rust-driver.mjs`** — interface-complete, spawn intentionally unimplemented. With no
  binary, `start()` throws `RUST_NOT_BUILT` with an actionable message. `--impl both`
  degrades to `[SKIP]` + Node-only gating (exit 0); `--impl rust` exits 1 because nothing
  ran. When the M1 binary lands, wire `start()` to mirror `node-driver.start()` and honour
  the same `DATA_DIR` / `PORT` / env contract; no case or golden should need to change.

## Deferred coverage (out of M0 scope, listed so nothing is silently lost)

Blueprint §B4 families **fully covered** here: MCP + HTTP envelope, raw/public delivery,
publisher tenant-lock. Deferred:

- **Expired public shares.** `invalid` and `revoked` are covered and proven mutually
  indistinguishable via `sameAsStep`. `expired` is not reachable through the API — the
  `create_share` contract only accepts `'24h'`, `'never'`, or a **future** ISO date — so it
  needs either a time-shifted fixture or a pre-seeded `artifact_shares` row. It shares the
  exact `shares.resolve()` predicate (`revoked_at IS NULL AND (expires_at IS NULL OR
  expires_at > now)`) with the revoked case, so the codepath is exercised; only the
  expiry-specific branch is unproven. Add with the first non-empty fixture.
- **Workflow + persistent state** (§B4 fourth family) — update/optimistic revision, no-op
  update creating no revision, history retention, restore-as-new-revision, delete cascades,
  move re-tenanting composite-FK rows, startup recovery from staged/trashed pre-states,
  digest backfill idempotence. The harness already supports all of it (`postState.sql`,
  file hashes, `dirEntries`, multi-step ordering); only the case manifests are unwritten.
  A slice is already proven incidentally: `publisher.ownership-clientid-and-org` asserts an
  admin move re-tenants the row and does not bump `revision`.
- **Non-empty fixtures** — frozen legacy v0/v9/v17 DBs and staged/trashed crash states.
  `materializeFixture()` copies arbitrary directory trees already, so these drop in as new
  `fixtures/<name>/` directories with no runner change.
- **UI snapshots** (`html-dom`) — the mode is implemented (exact snapshot) and used for 404
  bodies, but no gallery/shell/settings full-page snapshot is frozen yet. Those pages are
  identity- and content-dependent and belong with the `CONF-UI` unit.
- **Thumbnails/preview** — `PREVIEW_RENDERER_URL` is pinned empty so no headless browser is
  required. `/thumbnails/:id` therefore serves the SVG placeholder path; the PNG path needs
  a fixture renderer service (`CONF-PREVIEW`).

## Design decisions worth knowing

- **`empty-v21` is an empty directory, not a committed binary.** Booting any implementation
  against an empty `DATA_DIR` migrates it to schema v21 and creates `artifacts/`. That keeps
  the fixture implementation-neutral (both servers must perform the migration) and avoids an
  opaque, drift-prone SQLite blob in git. See `fixtures/README.md`.
- **Request paths are sent verbatim.** `testkit.httpRequest` parses only the origin out of
  `baseUrl` and passes `path` straight to `node:http`. Running paths through the WHATWG URL
  parser would collapse dot-segments and percent-decode `%2e`, silently defusing the bundle
  traversal cases before they reached the server.
- **The Node driver copies worktree source but symlinks `node_modules`.** The worktree may
  carry no `node_modules`; a built one (with native `better-sqlite3`) is resolved from the
  worktree, `CONFORMANCE_NODE_MODULES`, or the sibling main checkout. `server.js`, `lib/`
  and `package.json` are copied **from this worktree on every run**, so a sibling leg's
  patch to `lib/` is picked up immediately — the oracle always judges current worktree code.
  Only the dependency directory is borrowed. Nothing is written inside either checkout.
- **`--record` refuses to write when runtime assertions fail**, so a regression can never be
  laundered into a golden by re-recording.
- **`sameAsStep` is golden-independent.** Uniformity/concealment contracts are checked
  within a single run, so they hold even if someone re-records the goldens.
