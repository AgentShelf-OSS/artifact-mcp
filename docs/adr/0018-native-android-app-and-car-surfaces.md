# ADR-0018: Native Android app and car surfaces

- Status: Accepted (Phase 1 in progress)
- Date: 2026-10-18
- Issue: [#105](https://github.com/AgentShelf-OSS/artifact-mcp/issues/105)

## Context

Artifacts run in a sandboxed iframe inside the authenticated viewer shell (ADR-0003). The shell
brokers the capabilities an artifact may use: viewer state (ADR-0007), live data (ADR-0009,
ADR-0017), reminders (ADR-0012), geolocation (ADR-0016) and Listen speech. Viewers install the
site as a Progressive Web App.

The road-trip planner, a family artifact used in a car, found what a web app cannot do on Android:

- Android Auto shows only apps built with the Car App Library and media and messaging apps. A web
  app can never appear on the car screen.
- A backgrounded or screen-off web app loses location, so location-triggered narration stops.
- Web audio is not an Android Auto media source: no car controls and no audio-focus handling
  under navigation prompts.
- Web push notifications never reach the car screen.

The data and services already exist. Only a native surface is missing.

Options considered:

1. **One native app per artifact** (for example, a road-trip app). This is the fastest for one
   use, but every new car or background use repeats the work and adds another codebase.
2. **A Trusted Web Activity wrapper.** It packages the PWA, but adds no Android Auto, media
   session or background location.
3. **A general Artifact MCP Android app** with a WebView viewer, native capability bridges
   brokered by the shell, and a declarative car surface. This is chosen.

## Decision

Build one Android app for Artifact MCP.

**Viewer.** A WebView loads the normal viewer from the configured origin and signs in through
the deployment's existing login (Cloudflare Access in the homelab). The artifact iframe keeps
exactly the browser sandbox, CSP and response headers. The app adds nothing to the artifact
document.

**Native bridges through the shell only.** The viewer shell gains optional bridges that it offers
only when it detects the native host (a versioned JavaScript interface injected into the top-level
shell page, never into the artifact frame). Artifact code still talks only to the shell over
`postMessage`, and the shell applies the same per-artifact consent model as ADR-0016:

- `geo:watch` / `geo:unwatch`: background location through a foreground service with a
  persistent notification and a stop control. Fixes are delivered to the shell, then to the
  artifact. Consent is per artifact and revocable.
- `media:*`: a playback queue of Listen speech clips and same-origin audio, exposed as a
  MediaSession / MediaBrowserService so Android Auto shows it as a media source with standard
  controls and audio focus.
- `notify`: native notifications for existing reminders. A messaging-style channel, which
  Android Auto can read aloud, is used only for content the viewer explicitly opted into.

In a browser, none of these exist, and the shell answers `unsupported` as it does today for
historical views.

**Car surfaces.** An artifact may publish a *car view*: a small, versioned, size-capped JSON
document. It describes one of the Car App Library templates (list, place list with map, pane,
message, now playing), with text, places and a fixed set of actions: navigate (handoff to the
default navigation app), play or pause media, and mark an item done (a state write with the same
authorization as the viewer). The app renders car views only with those templates. There is no
HTML, script or remote image on the car screen, so the platform's driver-distraction rules come
from the templates rather than from artifact authors. The schema is documented in
`docs/car-surfaces.md` and validated by the server where it stores or relays car views.

**Distribution.** A signed APK is sideloaded from a release asset. Android Auto runs it in
developer mode ("Unknown sources"). Play Store publication and iOS / CarPlay are out of scope
for this decision.

## Decisions recorded at acceptance

- The app lives in its own repository, `AgentShelf-OSS/artifact-mcp-android` (Kotlin, Gradle,
  Android Car App Library). Shell-side bridges live in this repository.
- Builds run in a container with the Android SDK; CI builds signed release APKs. The release
  signing key is held in the deployment's secrets manager, never in the repository.
- Phase 1 ships the viewer, login and the native bridges below. Phase 2 (car surfaces) follows
  after review.
- Android pauses page scripts in a backgrounded WebView, so anything that must happen with the
  screen off runs natively. Location-triggered narration is therefore a native capability
  (`audio.geotrigger`), not a page loop.

## Bridge protocol (version 1)

**Host channel.** The app registers a WebView message listener named `ArtifactNative`, scoped to
the artifact origin (`WebViewCompat.addWebMessageListener` with an allowed-origin rule). Only the
top-level viewer shell page matches that origin. The artifact iframe has an opaque origin and
never receives the object. `addJavascriptInterface` is not used, because it exposes the object to
every frame. Messages are JSON strings with `v: 1`.

Shell → app: `{"type":"hello"}`; app → shell: `{"type":"ready","app":"<version>",
"capabilities":["geo.watch","audio.queue","audio.geotrigger","notify"]}`. Without a reply, the
shell assumes a browser and offers none of these.

**Artifact ↔ shell messages** (the shell checks consent, then relays to the app):

| Artifact sends | Shell/app behaviour | Artifact receives |
| --- | --- | --- |
| `native:hello` | reports what the host supports | `native:ready {available, capabilities}` |
| `geo:watch {requestId, intervalMs≥15000, highAccuracy?}` | per-artifact background-location consent; foreground service with a persistent notification | `geo:position {requestId, lat, lon, accuracy, timestamp}` while the page runs; `geo:error {requestId, reason}` |
| `geo:unwatch {requestId}` | stops the watch | — |
| `audio:queue {requestId, item}` | app synthesizes `item.text` with `item.voice` through the artifact's speech route (same session cookie) or plays a same-origin `item.url`, in a MediaSession visible to Android Auto | `audio:state {id, state: queued\|playing\|done\|error}` |
| `audio:geotrigger {requestId, items}` | replaces the artifact's trigger set; while active, the app's location service plays an item the first time the device is within `radius_m` (500–10000) of `lat/lon` | `audio:played {id, at}`, replayed when the page next runs |
| `audio:stop {}` | stops playback and clears the queue | — |
| `notify:show {requestId, title, body}` | a local notification attributed to the artifact | — |

An audio item is `{id ≤64 chars, title ≤120, text ≤1500 | url, voice?, lat?, lon?, radius_m?}`.
An artifact may hold at most 50 trigger items. Consent for background location and for
notifications is asked by the shell per artifact, stored like ADR-0016 consent, and revocable
from the app's settings. A browser viewer answers `native:ready {available:false}`.

## Consequences

- One app gives every artifact background location, car audio and a car screen. Examples: road
  trips, a PR Watch "needs review" list, a baby-log "last feed" pane.
- The browser viewer's security model does not change. Native capabilities are reachable only
  through shell-brokered, consent-gated messages, and the artifact frame stays sandboxed.
- The app talks only to the artifact origin. Live-data sources stay behind the broker, so no
  upstream service needs a new public hostname.
- A third codebase (Kotlin) joins the Rust and Node runtimes, with Android SDK upgrades, a
  release signing key to protect, and sideload update friction.
- The car view schema is a new public contract. Changes to it need versioning, like the other
  bridge protocols.

## Open questions

- Should car views (Phase 2) travel in viewer state or as a push data binding (updates without an
  open viewer)?
