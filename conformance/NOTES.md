# Conformance notes — history, deferrals, and design decisions

Working notes that started with U00 (M0). Everything here is deliberate scope, not oversight.

## The Node reference server is retired

The oracle was built to prove that the Rust rewrite behaves like the Node reference server
(`server.js` + `lib/`). Both ran every case until the Node twin was removed from the build. The
goldens in `goldens/` were recorded from Node and replayed byte-for-byte against Rust; they are now
the contract for the Rust server alone. `--record` records from Rust, which freezes current
behavior, so a re-recorded golden must be reviewed as a deliberate behavior change.

## Resolved — the invariant-3 golden (blueprint §B6)

`cases/human-concealment.invariant3.json` is the 404-uniformity matrix: **14 artifact-scoped
human routes × 2 personas (unsigned, cross-org) × {existing-foreign id, nonexistent id} = 57
steps**, each "nonexistent" step paired to its "existing-foreign" predecessor with `sameAsStep`.

Routes covered: `GET /:id`, `GET /raw/:id`, `GET /thumbnails/:id`, `GET /:id/shares`,
`GET /:id/history`, `GET /:id/feedback`, `POST /:id/feedback`, `POST /:id/react`,
`POST /:id/visibility`, `POST /:id/category`, `POST /:id/move`, `POST /:id/share`,
`POST /:id/restore`, `DELETE /:id`.

The case was first committed as `pending` because it failed against the original Node server:
the status of a cross-org mutation disclosed whether the id existed (`401` or `403` for an
existing foreign artifact, `404` for a nonexistent one). After artifact lookup and concealed
access were centralized, the golden was recorded and the case gates like every other case.

## Deferred coverage (out of M0 scope, listed so nothing is silently lost)

Blueprint §B4 families **fully covered** here: MCP + HTTP envelope, raw/public delivery,
publisher tenant-lock. Deferred:

- **Expired public shares.** `invalid` and `revoked` are covered and proven mutually
  indistinguishable via `sameAsStep`. `expired` is not reachable through the API — the
  `create_share` contract only accepts `'24h'`, `'never'`, or a **future** ISO date — so it
  needs either a time-shifted fixture or a pre-seeded `artifact_shares` row. It shares the
  share-resolution predicate (`revoked_at IS NULL AND (expires_at IS NULL OR
  expires_at > now)`) with the revoked case, so the codepath is exercised; only the
  expiry-specific branch is unproven. Add with the first non-empty fixture.
- **Workflow + persistent state** (§B4 fourth family) — update/optimistic revision, no-op
  update creating no revision, history retention, restore-as-new-revision, delete cascades,
  move re-tenanting composite-FK rows, startup recovery from staged/trashed pre-states,
  digest backfill idempotence. The harness already supports all of it (`postState.sql`,
  file hashes, `dirEntries`, multi-step ordering); only the case manifests are unwritten.
  A slice is already proven incidentally: `publisher.ownership-clientid-and-org` asserts an
  admin move re-tenants the row and does not bump `revision`. Historical migration and
  recovery are covered separately by `fixtures/historical/` (see `fixtures/README.md`).
- **UI snapshots** (`html-dom`) — the mode is implemented (exact snapshot) and used for 404
  bodies, but no gallery/shell/settings full-page snapshot is frozen yet. Those pages are
  identity- and content-dependent and belong with the `CONF-UI` unit. Browser behavior is
  covered by `playwright/`.
- **Thumbnails/preview** — `PREVIEW_RENDERER_URL` is pinned empty so no headless browser is
  required. `/thumbnails/:id` therefore serves the placeholder path; the PNG path needs a
  fixture renderer service (`CONF-PREVIEW`).

## Design decisions worth knowing

- **`empty-v21` is an empty directory, not a committed binary.** Booting the server against an
  empty `DATA_DIR` migrates it to the current schema and creates `artifacts/`. That keeps the
  fixture implementation-neutral and avoids an opaque, drift-prone SQLite blob in git. See
  `fixtures/README.md`.
- **Request paths are sent verbatim.** `testkit.httpRequest` parses only the origin out of
  `baseUrl` and passes `path` straight to `node:http`. Running paths through the WHATWG URL
  parser would collapse dot-segments and percent-decode `%2e`, silently defusing the bundle
  traversal cases before they reached the server.
- **No npm dependencies.** Post-state SQL uses the built-in `node:sqlite` module. Post-state
  reads open the database read-only after the server stops; `afterSql` fixture shaping is the
  only writer, and it uses a busy timeout because the server is still running.
- **`--record` refuses to write when runtime assertions fail**, so a regression can never be
  laundered into a golden by re-recording.
- **`sameAsStep` is golden-independent.** Uniformity/concealment contracts are checked
  within a single run, so they hold even if someone re-records the goldens.
- **A run that ends without reporting results fails.** The runner exits through one function;
  a natural exit (for example a server handle that never settled) is reported as a failure, not
  a silent success.
