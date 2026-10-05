# Reviewed scene capture through the viewer shell

Status: accepted for the Baby log casting change requested on 2026-10-04.

## Context

Baby log has sleep scenes and generated sound inside its sandboxed HTML. Its user wants Roku setup, pairing, and casting in the same Sounds tab. Raw HTML cannot capture a screen or upload media under the existing sandbox and Content Security Policy. A separate capture page added an unwanted step.

## Decision

The trusted viewer can broker scene capture for an explicitly reviewed artifact revision. `ARTIFACT_CAST_IDS` contains comma-separated `artifactId@revision` grants. It defaults to empty and rejects malformed grants at startup. Updating artifact HTML removes the capability until the operator reviews and grants the new revision.

Only an authenticated current single-page view with an exact grant loads `assets/cast.js`. Bundles, raw responses, public shares, and historical query views have no capture bridge. The internal raw-body digest used for cache busting does not identify a historical viewer.

The enabled viewer pins its raw iframe request with `cast-pin=revision.fullSha256`. The raw handler checks the revision and full digest against metadata, then hashes the stored body before injecting any bridge. A mismatch returns concealed 404. This prevents a concurrent publication from replacing the reviewed HTML between the viewer response and the iframe request. The existing `v` query remains a cache buster. Unpinned raw delivery keeps its existing behavior.

The artifact requests pairing and capture through `postMessage`. Each message must come from the current iframe and carry the current per-load nonce, artifact ID, revision, and protocol version. The shell owns session credentials and fixed `/cast/` destinations. It never sends a publish token, session ID, upstream address, or arbitrary network grant to artifact code.

Capture requires a click on a trusted confirmation dialog and the browser's sharing picker. Capture Handle identifies this viewer tab. A `CropTarget` from the reviewed sleep-scene element identifies the allowed video region. The shell rejects another tab, a window, a desktop, missing required audio, and browsers without these controls. It awaits `cropTo` before opening the upload or recording media. It never falls back to recording the full tab or iframe.

Video covers only the sleep scene. Audio covers the selected tab. The trusted reader pauses and cannot start playback while capture is active. The reviewed artifact stops capture before leaving TV mode or navigating to its clinical screens. The shell releases media on Stop, frame load, page exit, socket failure, and stale asynchronous completion. Local track cleanup precedes the stop acknowledgment. Server cleanup can complete separately.

The existing Roku broker receives browser WebM and produces LAN HLS. The installed receiver and Cloudflare Access policy remain compatible. The raw-response CSP, iframe sandbox, microphone restrictions, and camera restrictions do not change.

## Consequences

- Baby log keeps setup, pairing, and casting in its Sounds tab.
- The operator must renew the exact revision grant after each reviewed publication.
- Capture depends on desktop Chrome support and an awake source device.
- The target is supplied by reviewed artifact code. It is not an isolation guarantee for arbitrary HTML. The exact revision grant and trusted user confirmation are required controls.
- Tests must cover revision gating, current-frame nonces, trusted activation, crop-before-upload ordering, audio behavior, and asynchronous stop cleanup. Browser QA must verify the opaque child can transfer its scene target and that decoded video omits the surrounding application.

Platform references: [Region Capture specification](https://w3c.github.io/mediacapture-region/), [Chrome Region Capture](https://developer.chrome.com/docs/web-platform/region-capture/), and [Chrome Capture Handle](https://developer.chrome.com/docs/web-platform/capture-handle/).
