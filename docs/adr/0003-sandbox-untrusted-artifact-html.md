# ADR-0003: Sandbox raw artifacts and restrict network egress

- **Status:** Accepted
- **Date:** 2026-07-09
- **Amended:** 2026-07-22 (PBI-040)

## Context

Published artifact code is agent-supplied, active, and untrusted. The viewer shell embeds it in a
sandboxed iframe without `allow-same-origin`, but users can also open `/raw/:id` directly. An iframe
attribute does not constrain a raw document opened as a top-level page. The existing response CSP
sandboxed raw documents but had no fetch directives, so scripts could make unrestricted `fetch`,
XHR, WebSocket, EventSource, or beacon requests.

PBI-040 measured all 62 production artifacts on 2026-07-22 before changing that contract. The only
external resource loads were:

- `cdn.jsdelivr.net`: scripts in 6 artifacts;
- `fonts.googleapis.com`: stylesheets in 2 artifacts; and
- `fonts.gstatic.com`: a font in 1 artifact.

There were no live outbound data calls. Text matches for `fetch()` and WebSocket were prose or code
samples inside documentation artifacts (one document contains 223 `<code>` elements), not executing
JavaScript. There were no XMLHttpRequest, `sendBeacon`, EventSource, or dynamic `import()` calls.
Other external hosts such as `docs.lusha.com`, `github.com`, and `fathom.video` appeared only in
`<a href>` navigation; `w3.org` and `schema.org` appeared as namespaces or microdata. CSP fetch
directives do not govern ordinary link navigation.

Artifacts rely on inline `<style>` and `<script>`. Self-contained resources can use `data:` or
`blob:` URLs, while multi-file bundles can load relative resources from the response origin. A bare
`default-src 'none'` would therefore break valid artifacts even though it would close network
egress.

## Decision

Every raw artifact response uses this exact enforcing policy:

```text
sandbox allow-scripts allow-popups allow-forms allow-modals; default-src 'none'; connect-src 'none'; script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' data: blob: https://fonts.gstatic.com; img-src 'self' data: blob:; media-src 'self' data: blob:; worker-src 'self' blob:
```

The policy is identical for HTML main documents, SVG, XML, CSS, images, other bundle files,
downloads, history representations, and public shares. Applying it in the shared raw-response helper
matches the previous sandbox coverage exactly. The sandbox capabilities are unchanged and
`allow-same-origin` remains absent, so an artifact document keeps an opaque origin.

`default-src 'none'` denies every unlisted fetch class. `connect-src 'none'` is stated explicitly so
the no-data-channel decision survives any later change to the default. It is safe for the current
corpus because the complete 62-artifact measurement found zero executing connect requests.

Inline script and style remain enabled because they are required by the artifacts. `'self'` is
allowed only for the resource classes used by relative multi-file bundle assets; the CSP self-origin
is the response URL's origin, so that source remains usable even though the sandbox gives executing
code an opaque origin. Non-network `data:` and `blob:` sources preserve self-contained images,
fonts, media, and workers. External presentation dependencies are allowlisted by directive and
scheme: jsDelivr can serve scripts, Google Fonts can serve stylesheets, and gstatic can serve fonts.
No external host is allowed by a broader directive than its measured use.

This allowlist is preferred to blocking all three hosts. Blocking would make the policy smaller,
but would degrade the 9 artifacts that use those resources. The accepted tradeoff is that those
three third-party hosts remain reachable for their narrow presentation classes; same-origin bundle
assets remain loadable; all other external fetch destinations and all connection APIs are denied.

The policy is enforced immediately with `Content-Security-Policy`, not
`Content-Security-Policy-Report-Only`. There is no CSP report collector, so Report-Only would produce
no actionable telemetry by itself. The completed inventory, the absence of live connect calls, and
the compatibility allowlist make the remaining breakage risk low enough to enforce now.

`Cross-Origin-Embedder-Policy: require-corp` is deliberately not added. Cross-origin isolation would
grant untrusted artifact code access to `SharedArrayBuffer` and higher-resolution timers, which are
Spectre primitives. That is a researched non-goal, not missing hardening.

A separate registrable domain for `/raw` is not a substitute for this policy. The current sandbox
without `allow-same-origin` gives each artifact an opaque origin, and egress restriction remains an
independent control.

## Consequences

- Executing artifact code cannot use fetch/XHR/WebSocket/EventSource/beacon or any unlisted resource
  type or network host.
- Existing inline behavior, relative bundle assets, self-contained data/blob resources, and the
  three measured presentation dependencies continue to work.
- Ordinary link navigation and the existing sandbox capabilities are unchanged; this decision
  restricts resource fetching and connection APIs rather than redefining navigation behavior.
- Every response path carries one byte-identical Node/Rust CSP value, frozen by native, parity, and
  conformance coverage.
- CSP and opaque-origin sandboxing reduce risk but do not make artifact code trustworthy.
