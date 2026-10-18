# ADR-0019: Listen and the artifact library on the native host

- Status: Accepted
- Date: 2026-10-18
- Builds on: ADR-0018 (native Android app), Listen voice roles (ADR-0014)

## Context

Listen narrates an artifact in the viewer and reports its state through `navigator.mediaSession`.
Chrome turns that into system media controls, but an Android WebView does not, so Android Auto,
the media notification and Bluetooth buttons never see Listen in the Artifact MCP app. A
backgrounded WebView also pauses page scripts, so narration stops when the screen turns off.

Android Auto media apps can show a browsable library. Users want to pick an artifact on the car
screen and hear it. The server offers the gallery only as HTML.

## Decision

**Mirror Listen into the app.** Inside the native host, Listen also sends
`media.session {state, title, artist, album, actions}` over the ADR-0018 channel whenever its
player state changes (identical states are not resent; `state:"none"` when it stops). The app
shows it through its Media3 session. Android Auto, notification and headset buttons come back as
`media.command {action}` and press the same Listen controls a viewer would (play, pause, next,
previous, stop). Only the app's `media.mirror` capability enables this. Highlighting and the
in-page player are unchanged. While Listen plays, the app keeps the viewer running in the
background.

**A library endpoint.** `GET /api/library` returns the artifacts the signed-in viewer may open,
grouped by organization:
`{"orgs":[{"org","artifacts":[{"id","title","category","updated_at"}]}]}`. Visibility is exactly
the gallery's, without hidden uploads. Artifacts are sorted newest first, with at most 200 per
organization. A missing session returns `403 {"error":"not_signed_in"}`. Both runtimes implement
it. The app shows one folder per organization in Android Auto.

**Start Listen from the car.** When an artifact is picked, the app opens `<origin>/<id>?listen=autoplay`.
The shell starts Listen once, after the reader is ready, and only when the native host advertises
`media.mirror`. In a browser the parameter does nothing, so a link cannot start audio on its own.

## Consequences

- Listen works in the car and with the screen off, with no change to how artifacts are written.
- The library endpoint exposes titles and categories that the gallery already shows. It adds no
  new visibility.
- The app owns one hidden reader view while a car-started narration runs. Its lifecycle and audio
  policy live in the app repository.
