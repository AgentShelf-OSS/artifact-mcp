# artifact-mcp

> A self-hosted MCP server and gallery for HTML made by AI agents.

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
![CI](https://img.shields.io/badge/CI-Rust%20%2B%20Node-2088FF.svg)
![MCP 2026](https://img.shields.io/badge/MCP-2025--06--18%20%2B%202026--07--28-6E56CF.svg)

[![Arty, the Artifact MCP mascot, beside the words Give agent-made pages a home](docs/screenshots/00-arty-hero.png)](https://artifact-mcp.neilblackman.dev)

[Website](https://artifact-mcp.neilblackman.dev) | [Documentation](docs/README.md) | [Latest release](https://github.com/AgentShelf-OSS/artifact-mcp/releases/latest) | [AgentShelf.ai](https://agentshelf.ai)

Agents already generate dashboards, reports, one-pagers, and small websites. Artifact MCP gives
those pages stable URLs on infrastructure you control. It keeps them searchable, versioned, and
ready for human review after the chat that created them has ended.

The production server is one Rust and Axum container backed by SQLite and ordinary files. Agents
publish through MCP. People browse an organization-scoped gallery behind Cloudflare Access, leave
feedback on exact points or regions, inspect older revisions, and create revocable public links.

## What it handles

- Publish a self-contained HTML page or a multi-file bundle through MCP.
- Update an artifact without changing its URL, with retained revision history and restore.
- Persist org-shared notes and other JSON through the [viewer state bridge](GETTING_STARTED.md#persisting-state-from-an-artifact).
- Read HTML aloud from the viewer with optional [local narration](ops/tts/README.md), including
  a mini player, saved positions, and automatic chapter continuation for supported ereaders.
- Search and organize artifacts by organization, category, owner, review state, and visibility.
- Attach threaded feedback to a point or region and copy the exact revision context back to an
  agent.
- Keep organizations isolated with scoped publisher keys and verified viewer identity.
- Run optional Discord notifications, persistent thumbnails, OAuth credentials, and Prometheus
  metrics without making them requirements for the core server.

## Listen to artifacts

The authenticated viewer can read visible text from HTML artifacts without requiring each artifact
to embed a player. **Listen** opens a compact player; **Expand** exposes the selected voice,
playback speed, and reading scope. The compact player shows the current scope, voice, and speed.
The expanded player groups playback, reading position, and voice settings, with playback controls
kept visible while scrolling. Speed changes preserve the voice’s pitch. The reader supports the current page, selected text, a clicked
reading position, and the current semantic section. A clicked paragraph or heading is highlighted
and gets an inline play action, so reading can begin exactly where attention is focused.
The selected passage stays highlighted while audio prepares, then follows the spoken word during
Pocket playback. Pausing keeps the current word marked; stopping clears it. Passage highlighting
remains the fallback when word timings or browser support are unavailable.

Authors can supply short `data-artifact-pronounce` hints for names and abbreviations. Listen speaks
the replacement while highlighting the original displayed term. See the
[pronunciation examples](docs/readable-artifacts.md#pronunciation-hints).

The expanded player also offers Replay sentence when Pocket word timings are available. It reuses
the current paragraph audio without another synthesis request or an additional persistent cache.
The existing streamed-audio limit bounds decoded samples to 8 MiB per chunk; stopping, leaving the
page, or moving to the next chunk releases those samples.

Playback includes pause/resume, 15-second rewind, block navigation, sleep timers, saved-place
resume, and chapter continuation where the artifact exposes a supported ereader structure.
Interrupted streams retry twice from the last played position; Pause and Stop cancel automatic
recovery, and Play can resume after a longer outage. The default server installation does not require a speech worker. Deployments that want local CPU
narration can use the [Pocket TTS worker](ops/pocket-tts/README.md), which provides 21 preset
English voices and streamed audio; the [TTS operations guide](ops/tts/README.md) documents worker
configuration, authorization, limits, and rollback. An optional [RAVEN trial](ops/pocket-tts/trials/raven/README.md) adds two CPU voices to the same player with passage highlighting. Pocket remains the default when both are configured.

The reader preserves document order around embedded prototypes, groups list items and labelled
cards, and reads figure descriptions once. Simple tables use column headers when narrating rows;
large or ambiguous tables use a short description instead. Charts and generated row descriptions
highlight the complete block. Controls, hidden panels, timers, and live logs are excluded.
See [authoring readable artifacts](docs/readable-artifacts.md) for optional reading regions,
block grouping, and visual summaries. The expanded player offers Current view and contextual Read details, with scoped saved positions
and change detection. Further semantic improvements are tracked in [PBI #41](https://github.com/AgentShelf-OSS/artifact-mcp/issues/41).

## Quick start

Docker and Docker Compose are required. Start with the full [getting started guide](GETTING_STARTED.md)
if this is your first installation.

```bash
git clone https://github.com/AgentShelf-OSS/artifact-mcp.git
cd artifact-mcp
cp .env.example .env
```

Set a long bootstrap key in `.env`:

```dotenv
ARTIFACT_API_KEYS=agent1:local:REPLACE_WITH_A_LONG_RANDOM_SECRET
```

For a loopback-only local gallery, set `TRUST_ACCESS_HEADERS=1`. Never use that setting on a
reachable origin. Then start the native server:

```bash
docker compose up -d --build
```

Publish a first artifact:

```bash
export KEY=REPLACE_WITH_A_LONG_RANDOM_SECRET
curl -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"publish_artifact",
       "arguments":{"html":"<h1>hi</h1>","title":"Demo","description":"first artifact"}}}' \
  http://localhost:3480/mcp
```

The response includes the artifact ID and URL. The [getting started guide](GETTING_STARTED.md)
continues through local gallery access, organization setup, and a production Cloudflare deployment.

## Screenshots

| Artifact library | Anchored review |
|---|---|
| [![Administrator artifact library](docs/screenshots/01-gallery-admin-grid.png)](docs/screenshots/01-gallery-admin-grid.png) | [![Artifact feedback inspector](docs/screenshots/05-viewer-feedback.png)](docs/screenshots/05-viewer-feedback.png) |
| Search, filter, sort, and switch layouts without losing the collection context. | Leave threaded feedback on a point or region and copy its revision context for an agent. |

| Version history | Organization settings |
|---|---|
| [![Artifact version history](docs/screenshots/06-viewer-history.png)](docs/screenshots/06-viewer-history.png) | [![Organization administration](docs/screenshots/07-admin-organizations.png)](docs/screenshots/07-admin-organizations.png) |
| Open retained revisions or restore one as a new revision at the same stable URL. | Manage tenant membership, routing, categories, colors, and delivery settings. |

[View all eight product screenshots](docs/screenshots/README.md).

## How it works

```text
Agent  -> POST /mcp ------------------------+
                                                |
Human  -> gallery, viewer, settings --------+--> Artifact MCP --> SQLite + files
                                                |
Public -> /s/:token read-only share --------+
```

Agents authenticate with a scoped API key or OAuth bearer token. Human routes use a verified
Cloudflare Access identity. Only an active `/s/:token` share is public. Uploaded HTML runs in a
sandboxed iframe, separate from the trusted gallery and review controls.

Artifact MCP supports the stateful MCP `2025-06-18` contract and the stateless `2026-07-28`
contract. Modern clients can negotiate typed outputs, resources, MCP Apps, and durable preview
tasks. See the [MCP API reference](docs/mcp-api.md) for the complete tool catalog.

## Is it a fit?

Artifact MCP makes sense when a team wants a durable artifact library, organization boundaries,
review history, and control of the deployment. A hosted one-page publisher is simpler when all you
need is one temporary URL. The [comparison guide](docs/comparison.md) spells out that distinction.

## Documentation

- [Documentation index](docs/README.md)
- [Getting started](GETTING_STARTED.md)
- [MCP API and tools](docs/mcp-api.md)
- [Configuration reference](docs/configuration.md)
- [Architecture and routes](docs/architecture.md)
- [Security model](docs/security.md)
- [Cloudflare deployment](docs/DEPLOY-CLOUDFLARE.md)
- [Operations runbooks](docs/README.md#operations)
- [Release notes](docs/releases/README.md)

## Contributing

Issues, ideas, and focused pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md)
before starting a substantial change. Report vulnerabilities through the private process in
[SECURITY.md](SECURITY.md), not a public issue.

## Roadmap

- Full-text search across artifact bodies
- More precise cooperative anchors and text-range highlights
- Per-key quotas and artifact expiry
- Optional content scanning and a separate artifact-delivery origin

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
