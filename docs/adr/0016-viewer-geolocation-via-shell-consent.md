# ADR-0016: Viewer geolocation via shell consent

- Status: Accepted
- Date: 2026-10-17

Some artifacts need the viewer's current position, for example a road-trip planner that
searches for fuel or food near the car. ADR-0003 runs artifact code in a sandboxed iframe with
an opaque origin and no `allow` attribute, so the Geolocation API is unavailable to it. The
trusted viewer shell is a top-level document on the artifact origin and can read the position.

The shell brokers one-shot position requests over `postMessage`, in the same way as viewer state
(ADR-0007). The iframe sandbox, raw-response CSP and response headers stay unchanged. Artifact code
still has no network access, and it cannot watch the position continuously.

The browser permission belongs to the whole artifact origin. Once it is granted, any artifact
could read the position without a new browser prompt. The shell therefore asks its own question
for each artifact before it calls the browser API: "Don't allow", "Allow once" or "Always allow
here". "Always" is stored in the viewer's `localStorage` under `artifact-geo:<artifact id>`. A
browser denial removes that entry. Historical views have no geolocation capability. Raw, public
share and direct `/raw` views have no shell, so they receive no answer.

Protocol, frame to shell:

- `geo:hello` → `geo:ready { supported, remembered }`.
- `geo:request { requestId, highAccuracy? }` → `geo:position { requestId, lat, lon, accuracy,
  timestamp }` or `geo:error { requestId, reason }`.
- `geo:forget` → clears the stored consent, then `geo:ready`.

`requestId` matches `^[A-Za-z0-9._:-]{1,64}$`. Reasons are `declined` (the shell question),
`denied` (the browser), `timeout`, `unavailable`, `unsupported`, `busy` and `bad_request`. The
shell runs one request at a time and refuses a new one within two seconds of the last lookup.
Lookups use a 20-second timeout and accept a cached fix up to 60 seconds old.

The position never reaches the server through this channel. An artifact that sends it onward
does so through its own live-data operations (ADR-0009), which the operator configures.
