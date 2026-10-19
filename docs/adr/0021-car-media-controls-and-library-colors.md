# ADR-0021: Car media controls and library colors

- Status: Accepted
- Date: 2026-10-19
- Builds on: ADR-0019

## Context

The Android app shows Listen in Android Auto with generated cover art and a now-playing screen.
Drivers asked for "back 15 seconds" and a speed button there. The covers use each organization's
gallery color. Apps released before this change reject unknown message fields.

## Decision

- `media.command` accepts two more actions, `rewind` (Listen's 15-second rewind) and `speed`, which
  cycles 1×, 1.25× and 1.5×.
- `media.session` carries an optional `rate` string (for example `"1.25"`) **only** when the app
  advertises the `media.rate` capability, so older apps keep receiving the fields they accept.
- `GET /api/library` adds `orgs[].color`: the organization's gallery color as `#rrggbb`, or `null`
  when unset or not a plain hex color. The app's library parser ignores fields it does not know.

## Consequences

Older apps behave as before. Newer apps can draw covers in the gallery's colors and show the
current speed on the car's now-playing screen.
