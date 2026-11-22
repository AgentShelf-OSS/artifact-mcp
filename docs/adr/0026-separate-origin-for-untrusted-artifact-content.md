# 26. Separate origin for untrusted artifact content

Date: 2026-11-15

## Status

Proposed

- Issue: [#71](https://github.com/AgentShelf-OSS/artifact-mcp/issues/71)
- Builds on: ADR-0003, ADR-0007, ADR-0009, ADR-0011, ADR-0016, ADR-0017 and ADR-0018
- Changes on acceptance: ADR-0003 delivery location and ADR-0018's single-origin deployment assumption

## Context

The trusted application and untrusted artifact bytes currently use one hostname. The browser
sandbox gives executing artifact code an opaque origin. This prevents ordinary access to the
application's DOM and browser storage. It does not separate the delivery hostname, cookies,
infrastructure configuration or consequences of a sandbox failure.

Keep the sandbox. Add a separate registrable domain for executable artifact content. Keep the
trusted viewer shell and its authenticated broker on the application origin. This is a proposed
design and implementation plan. It changes no running service.

This record maps checkout `547c73163e9bef5f3ea8209225cd93074ca98ea1`. All existing ADRs through
0024 were reviewed, including both records numbered 0012. Number 0025 is reserved by another
task. Historical Node parity requirements no longer apply after ADR-0024. The native host and
per-organization MCP endpoints are deployment constraints supplied with this task. Deployment
documentation was reviewed; Cloudflare account settings and the Android repository were not
inspected live.

This record was checked against the body of issue #71 on 2026-11-15. It answers the issue's design
gate: the origin/access matrix (Delivery today), the hostname versus registrable-domain and
shared versus per-artifact matrix (Options), content authorization (Read-grant contract), and
migration and rollback (Phase 6). The issue's Node/Rust parity validation step no longer applies
after ADR-0024. Its file references to `lib/artifact-http.js` point at the retired Node tree.

### Terms

- An **origin** is a scheme, hostname and port. `https://app.example.com` and
  `https://content.example.com` are different origins.
- A **registrable domain** is the domain an operator registers, such as `example.com`. The
  Public Suffix List defines its boundary. For example, the boundary below `co.uk` differs.
- A **site** uses the scheme and registrable domain. Those two HTTPS subdomains are same-site.
  `https://artifact-content.example.net` is cross-site to both. Ports do not separate sites.
- An **opaque origin** is a browser-assigned identity without a reusable host identity. Its
  serialized `postMessage` origin is `"null"`. It is not the content hostname.
- The **application origin** serves trusted pages and authenticated APIs.
- A **content origin** serves untrusted artifact bytes. A **delivery grant** is a short-lived
  bearer token authorizing reads of one artifact revision. Possession of its URL grants that
  limited read access. It is not a viewer session or a publisher credential.
- **CSRF** means cross-site request forgery. A hostile page causes a browser to send a request
  using credentials that the browser attaches automatically. CORS controls script access and
  preflight permission; it does not stop every form, image or navigation request.

### Delivery today

`src/http/mod.rs` merges these routes into one router. `src/app.rs::build_router` wraps it in
the mutation guard and ingress admission. `src/main.rs::runtime_router` adds the Access retry,
audit request IDs, request tracing and `no-transform` handling. The retry is outside the normal
ETag middleware. The route map below distinguishes HTML bodies from trusted chrome and images.

| Request or component | Current behavior and concrete source |
| --- | --- |
| `GET /:id` | `src/http/routes/gallery.rs::shell_result` resolves viewer identity and tenant access, records the view and builds `ShellView`. `src/render/portal/shell.rs` and `templates/artifact-shell.html` render trusted HTML with `no-store`. No document sandbox applies to the shell. Its iframe uses `sandbox="allow-scripts allow-popups allow-forms allow-modals"`. |
| Current single-page `/raw/:id` | `src/http/routes/raw.rs::current_root_result` authorizes through `resolve_page_artifact`, reads the body and calls `artifact_response`. The shell requests `?anchor=1&reader=1&v=<digest-prefix>`. `v` is a cache buster, not a revision pin. |
| Bundle `/raw/:id/` and `/raw/:id/*` | The root without a trailing slash redirects. `current_file_result` authorizes every request and calls traversal-guarded `read_bundle_file`. Relative CSS, script, image, media and HTML paths use the same hostname. HTML page paths are normalized before bridge injection. |
| Retained `/raw/:id/rev/:n` and descendants | The same viewer authorization applies before `read_revision_body`. Bundle roots redirect to a trailing slash. These handlers use default response options and inject no bridges. The History panel builds these links in `assets/shell/18-history-audience.js`. |
| Raw transformations and downloads | `src/http/artifact_response.rs::artifact_response` injects the anchor bridge and data client for HTML `anchor=1`, then the reader bridge for `reader=1`, then removes closed script blocks for `preview`. A single-page `?download` sets an attachment filename and suppresses injection. Bundle `download` suppresses bridge flags but does not create a bundle attachment. Ordinary raw bytes have no broker capability by themselves. |
| Preview HTML | `/raw` current handlers accept `?preview`. Script stripping is explicitly noise suppression, not sanitization. The response still has the sandbox CSP. It is distinct from gallery thumbnails. |
| Gallery previews | `templates/gallery.html` uses `<img>`, not artifact iframes. `src/http/routes/thumbnails.rs` authorizes the current digest before returning a PNG with `private, max-age=31536000, immutable`, or a server-generated SVG placeholder with `no-store`. Both use `nosniff`. |
| Preview generation | `src/integrations/preview.rs` sends an HTML body to the private renderer. `preview-renderer/src/render.rs` uses an ephemeral browser context, blocks network and service workers and returns a PNG. It does not load an Access-authenticated artifact URL. Digest validation and queueing live in `src/integrations/thumbnails/`. |
| Public `/s/:token` and descendants | `src/http/routes/public_share.rs` resolves an active share and applies `AccessPolicy::authorize_share`, without viewer identity. Bundle roots redirect. Raw response options disable all bridge injection. Responses use `no-store` and `X-Robots-Tag: noindex`. The edge must bypass Access for this path. |
| Shell broker | `assets/viewer-boot.js` buffers the first `state:hello`. `assets/shell/16-frame-wiring.js` checks `event.source === frame.contentWindow` and dispatches allowed message families. State, data, reminders, geolocation and native brokers check the frame again. Reader controls and `assets/actions.js` also have message listeners. The brokers call fixed application routes using the viewer's session. |
| Child bridges | `assets/artifact-anchor-bridge.html`, `assets/artifact-data-client.js` and `assets/reader-bridge.js` run inside the untrusted document. They send to the parent with `targetOrigin: '*'`. Parent replies also use `'*'`, because the child is opaque. The parent checks the sending window, not a content-host origin string. |
| Reviewed capture | ADR-0011 grants only an exact current single-page revision. The shell adds `cast-pin=revision.fullSha256`; the raw handler checks metadata and hashes the delivered body. `assets/cast.js` adds per-load nonces and checks artifact, revision, protocol and sending frame. |
| Trusted static code | `src/http/routes/push.rs` serves `/sw.js` with root scope. `assets/push-sw.js` handles push and notification events. It has no fetch handler, cache or `importScripts`. Manifests, library, speech, settings, MCP and all broker APIs use the application hostname. The MCP review app in `src/mcp/apps.rs` is trusted, self-contained metadata UI, not raw artifact HTML. |

`DOCUMENT_SANDBOX` in `src/http/artifact_response.rs` is applied to every artifact response,
including SVG, XML, other bundle assets and attachments:

```text
sandbox allow-scripts allow-popups allow-forms allow-modals; default-src 'none'; connect-src 'none'; script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' data: blob: https://fonts.gstatic.com; img-src 'self' data: blob:; media-src 'self' data: blob:; worker-src 'self' blob:
```

Raw and history responses use `private, max-age=60`. Shares use `no-store`. The listener appends
`no-transform`. `express_etag` in `src/http/middleware.rs` adds body-derived weak ETags and handles
conditional responses. The helper also sets `nosniff` and `Referrer-Policy: no-referrer`.

### Threat model and impact ranking

Assume an authorized publisher, a compromised publisher key or an upstream script supplies hostile
artifact code. A victim opens it while signed in. The attacker controls artifact HTML and bundle
files, but not trusted templates, runtime configuration or signing keys. Tenant authorization,
the browser sandbox and CSP currently work unless a row below states a failure condition.

Rank reflects realistic impact and required preconditions. These are design risks, not claims of
a demonstrated cookie theft or CSRF bypass.

| Rank | Threat and current limit | Effect of a separate site |
| --- | --- | --- |
| 1: high, available today | Hostile code can abuse its granted broker capabilities and deceive the viewer. It can read state that the broker intentionally exposes, call bound data operations including POST, request allowed actions, and ask for location or notification consent. HTML can imitate login UI. `allow-forms` remains enabled, and there is no `form-action` directive. Forms and ordinary navigation can send artifact-visible data off-site despite `connect-src 'none'`. The allowed presentation hosts also remain outbound request destinations. | Reduces credible application-host impersonation in direct views and contains delivery credentials. It does not make broker-authorized data safe from the artifact receiving it. Keep per-artifact consent and narrow server grants. Review form and external-resource policy independently. |
| 2: critical impact, conditional | A missed sandbox header, browser opaque-origin bug or future unsafe representation could give uploaded code application-origin authority. That would expose shell DOM, cached viewer data and authenticated requests. An HttpOnly Access cookie still cannot be read by script, but the script could use it for requests. Current HTML has both response and iframe sandbox controls; direct raw views depend on the response control. | Provides an independent hostname and site boundary. A failed artifact sandbox must still cross the browser's normal origin boundary to reach application data. Browser sandbox escapes remain outside this guarantee. |
| 3: high impact, conditional | Shared cookie and Access JWT exposure. Initial authenticated raw requests reach the application host with applicable cookies, and the edge can attach `Cf-Access-Jwt-Assertion`. `src/security/identity.rs` verifies that header or the `CF_Authorization` cookie. Artifact script cannot normally read either credential through the opaque origin. A header reflection bug, bad cache or loss of sandbox protection would have a wider impact because content delivery shares this credential-bearing path. | Content delivery needs neither the Access cookie nor its assertion. A separate registrable domain cannot receive an application-domain cookie, even one scoped to a parent domain. Do not create a content Access session. |
| 4: high impact, currently guarded | Ambient-cookie CSRF. Same-host resource loads, forms and navigations still create requests subject to browser cookie rules. Opaque-origin and SameSite rules can suppress cookies in some contexts; do not assume either always sends or always strips them. `same_origin_gate` requires `X-Artifact-Mutation: 1` and `Sec-Fetch-Site: same-origin`, or the configured exact Origin when Fetch Metadata is absent. It rejects `same-site`, `cross-site`, `none` and `Origin: null`. No portal CORS permission bypasses this gate. `/mcp` instead requires explicit bearer credentials. | Cross-site delivery adds SameSite cookie protection to the existing guard. It cannot replace the guard, and top-level safe navigations can still carry Lax cookies. GET routes must stay free of privileged mutations. |
| 5: medium likelihood, potentially high impact | Bridge confusion after iframe navigation. A `WindowProxy`, the object used in `event.source`, survives document navigation. A sibling frame cannot pass the current source check, but source alone does not identify the loaded revision or protect delayed replies across navigation. General brokers do not have the capture bridge's per-load nonce contract. | Host separation alone does not fix it. Keep source and schema checks, add document-lifetime cancellation, and test stale replies. Never accept every sender with origin `"null"`. |
| 6: medium impact, available with navigation or user action | CSP does not stop all ordinary navigation. Embedded content cannot navigate the top ancestor because neither sandbox grants `allow-top-navigation`. It can navigate itself. Direct raw documents can navigate their own top-level context. `allow-popups` permits popup creation subject to browser activation rules. Popups inherit sandbox flags because `allow-popups-to-escape-sandbox` is absent. The anchor bridge asks the trusted shell to confirm external links, but hostile code need not use that bridge. | Separates the address-bar identity and credentials. It does not remove phishing, resource exhaustion, self-navigation or popup abuse. Preserve the existing restrictions and opener protection. |
| 7: high impact, configuration dependent | Cache confusion or poisoning. An edge cache that ignores private/no-store, mixes hosts, strips query parameters, or drops policy headers can expose one tenant's content to another viewer or serve the wrong representation. Correct existing private caching and authorization prevent this; a shared hostname alone is not proof of a poisoning exploit. | Reduces trusted/untrusted cache overlap. Signed delivery URLs introduce a new secret and cache key. Require no shared caching initially, distinct host keys and full policy headers on 304 and error responses. |
| 8: high impact, low likelihood today | Service-worker persistence. An opaque document cannot register a service worker. `worker-src` does not grant registration authority. An uploaded script's default worker scope is also path-limited; raw delivery does not grant `Service-Worker-Allowed: /`. The trusted root worker currently has no fetch interception. A future origin or scope mistake could change those facts. | Even a content-origin worker cannot control application-origin pages. Reject service-worker script requests on the content router and never serve the trusted root worker there. |

Added 2026-11-22: rank 1 omits WebRTC, which the artifact CSP does not block; see the amendment to
ADR-0003. Rank 5's navigation case is closed for the viewer shell by `frame-src 'self'` on the
shell response; the stale-reply case within one origin remains.

CPU, memory, deceptive content and browser engine vulnerabilities remain risks on any origin.
Downloaded HTML can execute outside server policy when opened elsewhere; response headers do not
travel with the downloaded file. Server-side renderer isolation remains a separate requirement.

### Why a separate site matters

The same-origin policy blocks script reads across different subdomains or ports. Cookie scope is
different. Cookies do not include a port. A `Domain=example.com` cookie can reach sibling hosts,
and SameSite cookies do not provide the intended protection between those HTTPS siblings.
Even a host-only cookie for the application can be attached to a same-site request targeting that
application. A sibling does not need to receive that cookie to cause such a request.

A separate registrable domain breaks that shared cookie namespace and makes requests back to the
application cross-site. Lax and Strict then give an additional restriction. Lax still permits
some top-level safe navigations. CORS and the mutation header remain necessary. The sandbox's
opaque origin is another restriction, not a reason to depend on same-site deployment.

## Options

Effort is relative implementation and operations cost, not a delivery estimate. Every option
retains sandboxing, tenant checks and the mutation guard. Separate origins must not host trusted
APIs through an alias of the full application router.

Option A can add `base-uri 'none'`, review `form-action 'none'`, remove unnecessary popup/form
permissions, and test `Cross-Origin-Opener-Policy: same-origin` on trusted pages. The last header
separates unrelated opener relationships, but can affect popup login flows. Restrictive Permissions
Policy can deny camera and microphone access to artifact documents. Each change needs browser
compatibility evidence. Do not add cross-origin embedder isolation to artifacts; ADR-0003 rejects
granting them the extra shared-memory and timer capabilities that such isolation can enable.

| Option | Security gain | Cloudflare, DNS and Access work | Authentication for artifact bytes | Broker and postMessage checks | Android app impact | Local and dev setup | Migration risk | Effort |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A. Status quo plus hardening | Better CSP, navigation and cache controls; delivery still shares application credentials and hostname. | Audit current Access cookie flags, bypasses, cache rules and transforms. No DNS change. | Existing verified viewer session; public share token. No delivery token. | Current opaque source checks; add navigation cleanup and bounded messages. | Small; existing single-host WebView continues. | Existing one-origin setup. | Low, except tighter forms or popup policy can break content. | Low to medium. Does not meet the independent delivery boundary goal. |
| B. Separate registrable domain with per-artifact or per-org hosts | Strongest independent cookie/site boundary from the app. Per-artifact hosts also separate artifacts if opaque sandboxing fails; per-org hosts contain that failure only at org level. | New domain/zone, wildcard DNS and TLS, Tunnel route, content Access bypass or no Access app. Audit wildcard certificate coverage and cache rules. | Trusted shell mints short-lived read URLs. Content ignores Access and publisher credentials. Public shares keep their own revocable token. | Child stays opaque, so source plus document lifecycle checks remain essential. Child-to-parent messages can target the exact app origin. | Cross-site iframe must work without third-party cookies. Native channel remains app-only. Signed native media needs coordinated support. | Two sites; local HTTPS names and isolated browser fixtures. | Medium to high: bundle paths, redirects, raw consumers, grants and long-lived views. | Highest, but meets the goal. Recommended with per-artifact hosts. |
| C. Separate subdomain of the same site | Independent normal origin, storage and service-worker scope. Parent-domain cookies and same-site CSRF exposure remain. | Sibling DNS/Tunnel route; separate Access app or bypass. Less domain administration than B. | Prefer the same read-grant design as B. Reusing Access introduces content sessions and iframe login dependencies. | Same opaque checks as B. Changing the hostname does not change event origin from `"null"`. | Similar iframe change; fewer SameSite problems, still test cookie-free delivery. | Two hostnames, often one certificate. | Similar URL and routing changes to B; less edge work. | Medium to high. Useful interim containment, not the final cross-site boundary. |
| D. Separate port on the same hostname | Different normal origin and worker scope. Cookies ignore port; site and hostname remain shared. | Additional listener and proxy mapping. Cloudflare proxy port support constrains public choices. An origin-only port change hidden behind one public URL gives no browser separation. | Grants still preferable. Existing cookies may reach both public ports. | Same opaque checks as B. Ports require exact origin matching. | Origin allowlists and URL parsing need changes; cookies still shared. | Easy two-port demo, but it does not prove cross-site behavior. | Medium client compatibility risk for little cookie benefit. | Medium. Reject for production isolation. |

Within option B, prefer `a-<artifact-id>.artifact-content.example.net` over
`<org>.artifact-content.example.net`. Artifact IDs already have a validated DNS-safe alphabet.
An org move then preserves the delivery hostname. The grant still binds the current org.
Per-artifact hosts avoid placing every artifact in an org in one normal origin after a sandbox
failure. They cost wildcard DNS and certificate management. They are still same-site with each
other unless a public suffix service is introduced. Do not claim process isolation between
artifacts or introduce a public suffix service in this change.

## Decision

Recommend option B with per-artifact hosts. Put every executable artifact representation on a
dedicated registrable domain. Use the existing Rust process and storage with separate, explicit
application and content routers. A second process or listener is not required for browser
isolation. This is a browser trust boundary, not a process or storage security boundary.

Keep `PUBLIC_BASE_URL`, the gallery, stable `/:id` viewer links, broker APIs, Access login, MCP,
OAuth discovery, `/api/library`, speech, manifests and push on the application origin. Preserve
each organization's existing MCP endpoint and publisher authorization. There is no MCP endpoint,
settings page, Access retry page, native bridge or root worker on a content host.

Move current HTML, bundle files of every MIME type, retained revisions, raw HTML previews,
attachments and public shares. Retain `DOCUMENT_SANDBOX` and the iframe sandbox without
`allow-same-origin`. Retain `connect-src 'none'`. A different origin does not authorize
artifact networking. Keep authenticated raster thumbnails and escaped server-generated SVG
placeholders on the app origin. Never substitute uploaded SVG for those placeholders.

The proposed flow is:

```text
Viewer browser / Android WebView
  |
  +--> app.example.com -- Cloudflare Access --> application router
  |       trusted shell -- fixed authenticated routes --> state / data / speech / consent
  |          |                                            |
  |          +-- source-checked postMessage <--------------+
  |          |
  |          +-- iframe URL containing a limited delivery grant
  |                    |
  +--> a-ID.artifact-content.example.net -- Tunnel --> content router
             no Access session                       verify grant or public share
             sandboxed opaque document               read one artifact revision

Agents --> existing org MCP endpoint --> publisher authentication --> artifact storage
```

### Read-grant contract

1. After verified viewer and artifact authorization, the application issues a signed URL such as
   `https://a-ID.artifact-content.example.net/d/<grant>/raw/ID/` for a bundle. A single page
   omits the final slash. The trusted shell receives its initial URL in escaped `ShellView`
   configuration. It can request a replacement with `POST /:id/content-grants`, using the
   normal mutation header, viewer authentication and read authorization. Artifacts cannot
   choose that endpoint's destination host, org or artifact.
2. Use versioned, bounded, canonical claims authenticated with HMAC-SHA-256 and a dedicated
   32-byte key. Existing `hmac`, `sha2`, `base64` and `url` dependencies suffice for signing.
   Bind the deployment's application origin as issuer, the exact content origin as audience,
   artifact ID, org, revision, full body digest, normalized file scope, representation profile,
   issue time and expiry. Include a random grant ID. Reject unknown versions, oversized tokens,
   duplicate fields, non-canonical encodings and excess lifetime. Use constant-time MAC checking.
   Claims contain no email, Access JWT, publisher key, upstream secret or viewer-state value.
3. A grant authorizes GET and HEAD bytes only. A bundle grant permits its revision's normalized
   file tree, not another artifact or revision. Profile names distinguish `viewer`, `raw`,
   `preview`, `history` and `download`. Bind bridge and attachment behavior to the signed
   profile. Query flags cannot turn a raw, historical or preview grant into a viewer grant.
   Viewer profiles can inject the existing helpers; those helpers confer no API access alone.
4. Resolve fresh ready metadata and check the host, org, revision and digest before reading.
   Use the storage authorization and traversal guards, not a direct filesystem server.
   A current grant fails after a content revision change. A history grant reads only its
   retained revision while the artifact still belongs to the signed org. Deletion, org moves,
   unavailable history and non-ready lifecycle states conceal as 404. Preserve ADR-0011's
   metadata and delivered-body hash checks for capture, beyond checking signed claims.
5. Start with a five-minute grant lifetime, with a configurable hard maximum of ten minutes.
   Removing viewer membership or signing out prevents new grants. An already issued grant
   remains readable until expiry unless deletion, org move or key rotation invalidates it.
   This bounded revocation delay is an explicit cost of stateless bearer delivery. Public
   shares must still be checked against live share state on every request for immediate revoke.
6. Put the grant in a path prefix. A query-only token would disappear from relative CSS, images
   and next-page links. Keep the prefix in all relative bundle requests and trailing-slash
   redirects. Recheck normalized paths after decoding; dot segments, encoded separators,
   backslashes and redirects must not escape the grant. Absolute root paths and hard-coded
   old `/raw` URLs need a compatibility inventory. Do not rewrite arbitrary artifact source
   or add a cookie fallback to make them work. Publishers may need to change these to relative
   paths. Queries and fragment links keep their existing semantics within the allowed profile.
7. Expiry does not stop an already loaded document or its authorized broker session. New lazy
   assets and bundle page navigation can fail after expiry. On a trusted navigation or reload
   action, the shell renews and loads the requested page under a new grant. Offer an explicit
   "Reload artifact" control for an expired view. Do not reload a form or discard a draft on
   a timer. Direct raw URLs expire; their stable application links issue fresh URLs after
   authentication. Document this limitation and test a bundle open beyond expiry.
8. The artifact can read its own URL. Treat its grant as disclosed to that artifact and its
   allowed scripts. It must grant nothing beyond those bytes. Keep the signer key and all
   broader credentials outside the iframe. Do not return grants in MCP publication metadata,
   notifications, persistent library records, audits or shared browser caches.

A cookie on the content domain is rejected as the primary design. Cross-site iframe cookie
blocking would break delivery. A single cookie could also authorize other artifacts on that
domain. Passing the Access JWT through the frame, granting credentialed CORS to `"null"`, and
placing publisher keys in URLs are rejected.

### Routing, headers and edge policy

Route by an exact configured application authority or a validated per-artifact content authority
before dispatching any route, Access retry or error template. Unknown hosts fail closed. The
content router permits only the signed byte paths, public-share byte paths and a minimal
non-sensitive health response if needed. Application APIs return no content-host capability.
Content errors are static and credential-free; they cannot render trusted shell configuration.

The Tunnel must preserve the intended Host, or use a fixed, explicitly trusted authority mapping.
Never infer canonical URLs from arbitrary `Host`, `Forwarded` or `X-Forwarded-Host` headers.
Reject attempts to use a content alias to reach `/mcp`, `/sw.js`, `/settings` or the viewer.
Do not trust Access identity on the content router, even if a proxy mistakenly forwards it.
Ignore incoming cookies there and emit no `Set-Cookie`. Strip cookie and Access identity headers
at the content ingress where supported, before application middleware can observe them.

All grant responses, redirects and errors start with `Cache-Control: private, no-store,
no-transform`. Shares retain `no-store`, `no-transform` and `noindex`. Retain `nosniff` and
`no-referrer`. Disable Cloudflare caching for the entire content domain, including assets.
Do not serve cached bytes or 304 before checking the grant. Do not add shared caching until a
separate review defines tenant, token, profile and expiry behavior.

For viewer and preview profiles, add `frame-ancestors` restricted to the configured application
origin. The sandbox and lack of CORS still apply. Raw and public-share framing compatibility
needs an explicit decision before tightening their current policy. Deny service-worker script
requests on content hosts. Do not add `Service-Worker-Allowed` or a content-domain `/sw.js`.

Prevent delivery grants and share tokens from entering request logs, error reports, analytics,
audit rows and telemetry labels. `src/main.rs` currently traces method and request ID rather than
the URI; preserve that property. Review ingress logs, proxies and Cloudflare Logpush separately,
since path tokens are secrets too. Browser history and a malicious artifact can still disclose
its own token. `no-referrer` limits incidental leakage, not deliberate disclosure.

Grant issuance is a read authorization step with no durable business mutation. It does not append
a success row to the security audit chain. Denied or concealed requests use aggregate counters,
consistent with `docs/security-audit-ledger.md`. Any later durable grant store must define its
transaction and audit policy separately, without recording the bearer token.

### Broker and native-host compatibility

Do not change parent checks to `event.origin === contentOrigin`. The child remains opaque and
sends `"null"`. Require the current frame source, a permitted message type, bounded values and
the shell-selected artifact ID. A null origin by itself proves nothing. Continue deriving HTTP
destinations from trusted shell configuration, never from child URLs.

For injected child helpers, supply the exact application origin as escaped configuration and
use it as the target for parent-bound messages. Validate both parent source and that origin on
replies. Parent-to-child `'*'` remains necessary for an opaque child. It must carry only data
already authorized for this artifact, never cookies or delivery credentials for another view.
Artifacts with their own existing postMessage clients remain compatible through source and
schema checks; do not require a new field in every published artifact's messages.

Before navigation, revoke the current broker generation. On frame load, abort pending data and
speech operations, close streams, stop capture and background capability sessions, clear queued
replies and require fresh handshakes. Apply generation checks to asynchronous completions in all
listeners, including actions, geolocation, reminders and the boot handshake. Capture keeps its
stronger nonce contract. For shell-initiated page changes, replace the iframe window where
needed to avoid delivering a late reply to its successor. A generation or nonce is a lifecycle
check, not proof that hostile artifact code is trusted. Test self-navigation as well as explicit
shell navigation; document the remaining race limits of wildcard replies to opaque documents.

`assets/artifact-anchor-bridge.html` and `assets/reader-bridge.js` currently recognize `/raw/`
paths. The reader assumes `raw` is the first segment. Replace those assumptions with a validated
delivery-root descriptor supplied with the injected helper. It identifies the allowed bundle
prefix and parent origin, not an arbitrary broker URL. Update pin navigation, history links and
outbound-link handling in the shell. A same-content-host link is not a trusted application link.
Never let a child-supplied URL renew a different artifact's grant.

The Android app keeps its configured application origin, Access login and `ArtifactNative`
allowed-origin rule. Never add content hosts or `*` to that native listener. Verify that only
the main application frame can invoke it. The content iframe must load with third-party cookies
disabled. No Access login redirect, cookie copy or native JavaScript interface belongs there.
Library, text speech, reminders and media-control messages keep their existing application URLs.

`assets/shell/03-native-broker.js` currently accepts audio URLs only on the application origin.
Do not widen this to all HTTPS URLs. Coordinate a capability such as `audio.content-origin`
with `AgentShelf-OSS/artifact-mcp-android` for artifact audio files. The shell must authorize an
artifact-relative media path and mint its restricted delivery URL. The app must restrict playback
to the expected content host/path, handle expiry and never forward application cookies or follow
redirects to arbitrary hosts. Older apps keep text speech and fail closed for the new URL form.
The Android code has not been reviewed here, so device verification is a rollout gate.

## Implementation plan

Each phase is a reviewable change. Phases 1 and 2 precede shell integration. Phase 4 and the
operator work precede any production enablement. No phase in this record authorizes deployment.

### Phase 1: Define configuration and the delivery boundary

- Add `src/config/content.rs`, wired through `src/config/mod.rs`, defaults, parsing and test
  fixtures. Add the configuration below to `.env.example` and `docs/configuration.md`.
- Add typed `DeliveryGrant`, `DeliveryProfile` and `ContentLocation` contracts. Proposed home is
  `src/security/content_grants.rs` for signing/verification and a small URL module under
  `src/http/`. Pass signer, clock and random adapters at `src/main.rs` and `AppDeps`.
  Use the existing deterministic clock/random test adapters. Keep raw filesystem paths out of
  these contracts. Check `tests/layering.rs` before choosing public module dependencies.
- Split route assembly in `src/http/mod.rs` and `src/app.rs` into explicit application and
  content routers. Put authority classification before application routing and before
  `access_session_retry` in `src/main.rs`. Both paths keep ingress limits and `no-transform`.
- Add native tests for host classification and configuration first. Prove that a valid content
  host cannot reach any trusted route even when given a valid Access assertion.

| Proposed variable | Default and validation |
| --- | --- |
| `ARTIFACT_CONTENT_MODE` | `legacy` initially; accepted values `legacy` and `isolated`. This is the rollout flag. No automatic fallback from isolated delivery to application-host bytes. |
| `ARTIFACT_CONTENT_BASE_URL` | Required in isolated mode. HTTPS origin of the dedicated registrable domain, with no credentials, path, query or fragment. Derive `a-<id>.<base-host>`; do not accept a publisher host template. Compare its registrable domain to `PUBLIC_BASE_URL` using a maintained Public Suffix List implementation. Reject same-site production configuration. |
| `ARTIFACT_CONTENT_SIGNING_KEY` | Required in isolated mode. Canonical base64 of 32 random bytes, wrapped in `Secret`. Dedicated to content delivery; do not reuse audit, OAuth or integration keys. Never include its value in validation errors. |
| `ARTIFACT_CONTENT_SIGNING_KEY_ID` | Required non-secret bounded identifier. Included in the signed format. An unknown ID fails closed. Initial rotation replaces the key and invalidates outstanding grants; no implicit mixed-key fallback. |
| `ARTIFACT_CONTENT_GRANT_TTL_SECONDS` | `300`, accepted range `60..600`. Enforce maximum lifetime at verification as well as issuance. No expiry leeway that extends the configured maximum. |
| `ARTIFACT_CONTENT_DEV_INSECURE` | `0`. Test/dev-build-only escape for HTTP with explicitly loopback-resolved fixture hosts and a loopback listener. It cannot make a reachable production same-site setup valid. Production builds do not compile this escape and reject the variable when enabled. |

Public-suffix validation is a dependency decision before implementation. The current dependency
list has no PSL library. Approve a maintained implementation or a maintained checked-in list;
do not compare only the final two hostname labels. This proposal does not add a dependency.

### Phase 2: Implement limited byte authentication and compatibility redirects

- Add `src/http/routes/content.rs` and the authenticated grant endpoint. Factor reusable
  authorized reads from `src/http/routes/raw.rs` and `public_share.rs`. Use
  `src/security/access.rs`, `src/ports/artifacts.rs`, `src/artifacts/read.rs` and existing path
  guards. Keep authorization before body reads and concealed cross-org failures.
- In isolated mode, old application `/raw` current/history/download/preview URLs authenticate
  normally and return non-cacheable redirects to limited content URLs. They never return an
  executable body on the application origin. Do not redirect an unauthenticated private read
  into public access. Preserve semantic flags through signed profiles, not arbitrary query copy.
- Application `/s/:token` URLs remain stable public entrypoints. Resolve the share and redirect
  to that artifact's content host. Content-host `/s/:token` requests re-resolve the live share
  on every byte read. A token for artifact A cannot work under artifact B's host. Retain the
  existing share lifetime, current-content behavior and immediate revoke.
- Extend `src/http/artifact_response.rs` with explicit delivery/cache profiles. Preserve MIME,
  transformation order, attachment names and sandboxing. Cover response policy on failures,
  redirects and conditional requests in `src/http/middleware.rs`. Keep bridge-free raw bodies
  byte-identical after following the redirect.
- Treat existing raw HTTP contracts as deliberately changed in isolated mode. Record and review
  the affected conformance goldens. Legacy-mode contracts remain unchanged during rollout.
  No database migration or artifact rewrite is planned.

### Phase 3: Integrate the shell, bundle navigation and broker lifecycle

- Add delivery locations to `src/render/view_models.rs::ShellView`, with the types passed to
  `src/render/portal/shell.rs`, `src/render/portal/templates.rs` and
  `templates/artifact-shell.html`. Prepare grants in the authorized HTTP/service layer, not
  inside template rendering. Update gallery download links in `templates/gallery.html` only
  where needed; stable application redirect links can remain.
- Update `assets/shell/00-prelude.js`, `07-comment-bridge-wiring.js`,
  `15-anchors-comments.js`, `16-frame-wiring.js`, `18-history-audience.js` and
  `assets/viewer-boot.js` for explicit content locations, expiry/reload UX and frame generations.
  Update `assets/artifact-anchor-bridge.html`, `assets/artifact-data-client.js` and
  `assets/reader-bridge.js` for parent origin and delivery-root configuration.
- Apply lifecycle cancellation to the state/data/geo/native/reminder/speech brokers, reader
  controls, `assets/actions.js` and `assets/cast.js`. Preserve scoped state, one-second fallback,
  named operations, stream cancellation, per-artifact consent and exact capture revision pins.
- Keep raster gallery previews and the renderer body-based contract unchanged. Test that raw
  `?preview` delivery has moved and that thumbnails still require viewer authorization.

### Phase 4: Prove the boundaries in native, browser and Android tests

Extend `tests/native/u02_config.rs`, `u14_response.rs`, `u17_routes.rs`, `u28_csrf.rs` and relevant
viewer-state/data tests. A new `tests/native/content_origin.rs` must be registered in
`tests/native/main.rs`. Add focused signer tests with fixed clock/random inputs. Cover:

- Wrong host/audience/issuer/org/artifact/revision/digest/profile, invalid MAC, duplicate claims,
  expiry boundaries, excessive TTL, key rotation, path decoding and traversal. No body read on
  refusal. Test current publication, org move, deletion and history retention races.
- Every current, bundle, history, raw preview, download and public-share representation on both
  host classes. Content APIs stay absent even with forged or valid viewer headers. Invalid
  grants never fall through to viewer or publisher authentication.
- Bridge-injection profile confusion, capture body hashing, no cookies set, no credentialed
  CORS, no service-worker registration, cache and security headers on redirects/404/304/HEAD,
  and no token-bearing diagnostics. No authorization based on Referer or Origin for byte reads.
- Legacy versus isolated conformance, immediate share revoke, and stable MCP/viewer/library URLs.

Add a registered `browser-tests/tests/e2e/` journey for content isolation. Extend the fixture
server in `browser-tests/src/server.rs` and the scoped network support if needed. Use two
different test registrable domains with TLS and loopback resolution, such as
`app.artifact.test` and `a-ID.artifact-content.test`. Different localhost ports are insufficient
to prove SameSite behavior. Permit only fixture hosts, not general network access. Verify:

- The app's test session cookie and a parent-domain cookie never arrive at the content host.
  The content iframe still loads with third-party cookies blocked. No test identity assertion
  reaches it through the fixture proxy. Script cannot read parent DOM, cookies or storage.
- The child sends origin `"null"`. An unrelated null-origin sibling, popup, old frame or forged
  message cannot use the broker. Self-navigation and bundle navigation cancel pending results,
  streams, speech, capture and native sessions. No stale sensitive reply reaches a successor.
- State in both scopes, live-data GET/POST/streaming, reminders, geolocation consent, reader
  extraction/highlighting, bundle-relative HTML/CSS/fonts/media, feedback pins and capture
  retain their contracts across the new origin. Include expired lazy assets and unsaved drafts.
- Top-level `/raw`, public shares, history, SVG/XML, downloads and preview HTML retain sandbox
  headers. Forms cannot mutate the trusted app. Popups retain sandbox restrictions; top ancestor
  navigation stays blocked. Test service-worker rejection and opener behavior.

Extend the matching `browser-tests/js/` suites, including `shell-state`, `shell-data`,
`shell-data-stream`, `shell-geo`, `shell-native`, `shell-reminders`, `state-bridge`,
`cast-bridge` and `artifact-http`. Keep asset parsing and reader regression coverage.

Run the root, preview-renderer and browser workspaces through `cargo xtask gate`, with pinned
Chrome and `ARTIFACT_MCP_BIN`, as required by `CONTRIBUTING.md`. On pve1 invoke build/check/test/
clippy/doc/xtask through `cargo-sandboxed`, use a fresh target under
`/mnt/ssd-bulk/codex-scratch/build/` and remove it afterward. Record deliberate contract changes
with `CONFORMANCE_RECORD=1 cargo-sandboxed test --test conformance`; review the golden diff and
run `tests/mcp_surface.rs` and `tests/layering.rs`. Do not silently skip browser tests.

In the Android repository, verify the supported WebView version, application-only native
listener, login/logout, third-party-cookie-disabled rendering, expired grants, app restarts,
background location, Listen/car playback and any new signed-media capability. Browser tests
cannot certify the WebView native listener. An owner-reviewed Android result is required before
that deployment enables isolated mode.

### Phase 5: Prepare the native host and Cloudflare outside this repository

1. The owner chooses a dedicated content domain with no trusted apps, mail or login cookies.
   Create its Cloudflare zone, wildcard DNS route and edge certificate covering
   `a-ID.<content-domain>`. Verify the certificate's wildcard depth. Do not reuse the
   application zone's parent-domain cookie namespace. Reserve the base host; do not expose
   gallery or administrative pages there.
2. Add a Tunnel route for the content wildcard to the native service's existing private
   loopback listener. Preserve the public authority as required by Phase 1. Keep unknown
   authorities denied and keep the host port private. A native process deployment is sufficient;
   the Docker descriptions in some general docs are not a requirement to change production.
3. Keep the existing application Access catch-all, org MCP policies and `/s/*` bypasses.
   Content delivery has either no matching Access app or a dedicated **Bypass → Everyone**
   app. Verify precedence against account-wide wildcards. The origin grant/share validator is
   authoritative there. Never broaden the application's catch-all bypass. Confirm that content
   responses and subresource requests contain no Access login redirects or Access session cookies.
4. Add content-domain cache-bypass and no-transform safeguards. Disable HTML injection and
   confirm Email Obfuscation settings for the new zone. Do not copy application response rules
   that set cookies, widen CORS or rewrite content URLs. Check WAF/challenge behavior in iframes.
5. Store the dedicated signing key through the deployment secrets workflow, without printing it.
   Configure the proposed variables, confirm Access cookies remain Lax or Strict, and fully
   restart the native service. Configuration is startup state, not a hot reload contract.
6. Inspect the edge and origin with a disposable artifact and viewer. Verify host routing,
   cookie absence, no shared caching, grant expiry, immediate share revoke and concealed foreign
   reads. Audit Cloudflare, tunnel and host logs for token disclosure. Record aggregate outcomes,
   not full URLs. Exercise grant-key rotation and the rollback procedure before pilot approval.

Update `docs/DEPLOY-CLOUDFLARE.md` and `docs/security.md` with the reviewed operator procedure.
If `xtask` setup automation is extended later, it must retain the existing dry-run behavior and
must not create or change Access policies without a separate explicit operator action.

### Phase 6: Roll out without breaking stable links

Ship support disabled with `ARTIFACT_CONTENT_MODE=legacy`. Existing URLs and output contracts
then behave as before. Deploy Phase 5 resources before switching any deployment. Pilot one
organization's deployment, including its Android client, then enable the remaining deployments
one at a time. A shared multi-org instance switches as a whole; a per-org experiment requires
separate deployment configuration, not a caller-controlled query parameter.

Before enablement, inventory root-absolute bundle paths, scripts that assume `/raw` is the first
path segment, non-browser raw consumers and artifacts open for long periods. Redirect-following
HTTP clients keep receiving bytes. Clients that require application-host byte URLs must adapt.
MCP publication URLs, artifact IDs, keys, revision numbers, stored HTML, state, consent and library
links remain stable. `/:id?v=...` currently affects capability gating rather than selecting a
historical body; do not silently give that query a new meaning in this change.

Enable isolated mode only after the native, cross-site browser, edge and Android gates pass.
Observe aggregate grant failures, host refusals, expired reads and content load failures. Do not
add artifact IDs, grants or viewer identities as metric labels. There is no silent fallback to
same-origin content when DNS, Access bypass or grant delivery fails.

Plan the routing cutover and its rollback under issue #54, and check the topology against the
homelab routing playbooks before any route changes. Deployment stays an owner-run operation.

Rollback is an explicit restart in `legacy` mode. It restores the old delivery security posture,
not equivalent isolation. Keep content routes available only for already issued grants until
their maximum expiry, then disable the wildcard route if required. Do not reuse a long-lived
redirect cache. Revoke shares or rotate the delivery key for an exposure incident. No database
rollback is needed. After all deployments are stable, propose a separate change to make isolated
mode the default and later retire legacy mode; retain stable application redirect entrypoints.

## Consequences

The application no longer delivers hostile executable bytes under its credential-bearing
hostname in isolated mode. Browser origin and site boundaries still protect the application if
one artifact sandbox control fails. The existing shell capability model survives without giving
artifacts network access or Access credentials.

The cost is a new domain, edge policy, grant signer and URL lifecycle. Grant URLs are secrets,
and short lifetimes affect late bundle loads. Some absolute-path bundles need publisher changes.
Private access revocation has a bounded token lifetime. The content router still shares the Rust
process and files with the application, so server compromise is not contained by this decision.

CSP form/popup hardening and general broker lifecycle work remain valuable. Neither a hostname
split nor a signed URL prevents a hostile artifact from misusing the data and operations the
viewer shell intentionally grants to it.

## Owner decisions

Recorded 2026-11-16. The status stays Proposed until decision 1 names the content domain.

Viewers keep the same URLs. `https://<app>/<id>` viewer links, the gallery, MCP endpoints and the
library do not change. Application `/raw/...` and `/s/<token>` links stay valid and redirect to
the content host (Phase 2). Only the bytes inside the artifact frame load from the content domain.

1. **Content domain: open.** Use per-artifact hosts (`a-<id>.<content-domain>`), one level under
   the apex, so the free Cloudflare Universal SSL wildcard covers them. The domain must not be the
   application's domain or a business domain. The preferred choice is a new domain used only for
   artifact content. A domain that already hosts other apps is acceptable only if the owner accepts
   that those apps become same-site with artifact content.
2. **Grants: accepted.** Stateless five-minute bearer read grants, with a configurable hard
   maximum of ten minutes. The sign-out and membership-revocation delay is bounded by that
   lifetime. Public shares are checked against live share state on every request. An expired
   bundle view offers an explicit "Reload artifact" control.
3. **Site check and Android: accepted.** Ship a pinned Public Suffix List snapshot, used only by a
   startup check that refuses a content domain that is same-site with the application origin.
   The minimum Android app version is the first release that supports content hosts. Rollout
   waits for validation on the owner's phone and Android Auto.
4. **Framing and forms: accepted.**
   - (a) Third-party sites may not embed public shares. Content responses send `frame-ancestors`
     limited to the application origin. Share links still open directly in a browser tab. Allowing
     third-party embedding is a later, separate decision.
   - (b) Keep `allow-forms`, so forms handled in script keep working, and add `form-action 'none'`
     to `DOCUMENT_SANDBOX`, so a form cannot submit data to another site.
   - Popup and presentation-host allowances stay as they are until a separate review.
5. **Pilot: accepted.** Pilot isolated mode on the homelab deployment, including the Android app.
   Retire legacy mode after four weeks of clean operation in isolated mode.
