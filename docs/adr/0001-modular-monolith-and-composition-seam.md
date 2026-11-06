# ADR-0001: Keep a modular monolith with an explicit application seam

- **Status:** Accepted
- **Date:** 2026-07-09

Amended 2026-11-06 by ADR-0024: in the Rust server, the composition seam is `build_router` with `AppDeps` (`src/app.rs`). Each port is an `Arc<dyn Trait>` injected by `src/main.rs` and by tests.

## Context

artifact-mcp is one small operational unit, but route registration, production configuration, database imports, policy checks, rendering, and listener startup previously lived together in `server.js`. Importing the application started real infrastructure, repeated tenant checks across routes, and made HTTP behavior difficult to test without a production-shaped environment.

Splitting the application into independently deployed processes would add operational interfaces without improving this workload. The useful variability is between production adapters and deterministic test adapters.

## Decision

Keep one deployable Node modular monolith and make `createApp()` in `lib/app.js` its primary seam.

- `createApp()` registers routes and accepts adapters for authentication, identity, artifacts, keys, reactions, rendering, logging, and health checks.
- `server.js` is the production composition entrypoint and is the only module that starts a listener.
- `lib/access.js` owns organization/admin policy and concealed-read semantics.
- HTTP tests assemble the same application module with test adapters.

This seam is real because it has at least two adapter sets: production adapters and test adapters.

## Consequences

- Route behavior can be tested through the same interface callers use, without listening during import or touching production data.
- Access decisions gain locality: a policy correction applies to raw delivery, the viewer shell, deletion, and reactions.
- `createApp()` has a deliberately broad composition interface. Domain behavior should stay behind the artifact, key, reaction, and access modules rather than migrating into adapter setup.
- The application remains one container and one release unit.

