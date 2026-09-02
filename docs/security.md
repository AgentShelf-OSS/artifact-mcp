# Security model

Artifact MCP stores and renders untrusted HTML for multiple organizations. Its security model
depends on verified identity, explicit tenant checks, a null-origin artifact sandbox, and a strict
split between agent, human, and public routes.

Report vulnerabilities through the private process in [SECURITY.md](../SECURITY.md).

## Viewer identity

Cloudflare removes client-supplied `Cf-Access-*` headers at the edge. Artifact MCP still verifies
the Access JWT at the origin before trusting a viewer's email or organization.

Viewer identity fails closed when `CF_ACCESS_TEAM_DOMAIN` and `CF_ACCESS_AUD` are absent.
`TRUST_ACCESS_HEADERS=1` restores unverified header trust only for loopback development. A reachable
deployment must not use it. Production can set `REQUIRE_ACCESS_JWT=1` so incomplete identity
configuration prevents startup.

Identity may come from the verified `Cf-Access-Jwt-Assertion` header or the equally verified
`CF_Authorization` session cookie immediately after login. Both paths use the same issuer, audience,
JWKS, expiry, and organization checks. A bounded one-time reload handles the short propagation
window after login without accepting an unverified identity.

## Tenant authorization

Every artifact records its publishing key and that key's optional verified human owner. Revoking a
key stops future publishing. The owner snapshot controls member Hide, Show, and Delete actions
without trusting a display label or caller-supplied identity.

Organization keys cannot cross their tenant boundary. Cross-organization reads conceal known
artifacts with 404 responses. Unauthorized mutations return 403 where doing so does not create a
cross-tenant existence oracle. Administrators may act across organizations. Moving an artifact to
another organization updates the artifact and all child rows atomically.

## Rendering untrusted HTML

Every raw artifact and public share response carries a CSP sandbox without `allow-same-origin`.
This rule also covers SVG, XML, and downloaded content, so uploaded code runs in a null origin.

The trusted viewer shell does not read the artifact iframe DOM. Point and region feedback uses a
fixed server-owned bridge injected only into the `?anchor=1` representation. The parent validates
the sending frame and a message type allowlist, then treats all anchor data as untrusted. Ordinary
raw bodies and downloads remain byte-for-byte unchanged.

## Browser mutations

Cloudflare Access authenticates the browser, but the application still checks mutation origin.
Every cookie-authenticated portal `POST`, `PUT`, `PATCH`, and `DELETE` requires the first-party
`X-Artifact-Mutation: 1` header plus a same-origin `Sec-Fetch-Site` value or an `Origin` matching
`PUBLIC_BASE_URL`.

The portal adds the header. Uploaded artifacts and other sites cannot add it to a credentialed
request without CORS preflight, and Artifact MCP does not authorize cross-origin portal mutations.
`/mcp` is the deliberate exception because it uses an explicit API key or OAuth bearer token.

Set the Cloudflare Access authorization cookie to `Lax` or `Strict`, never `None`. See the
[Cloudflare deployment guide](DEPLOY-CLOUDFLARE.md#browser-mutation-protection).

## Public shares and privacy

An artifact share is unlisted public content. Anyone with its URL can read the current artifact.
The controls are a random URL-safe token, optional server-side expiry, and immediate revoke.
Unknown, expired, and revoked tokens return the same 404 response. Shared artifacts do not expose
the viewer shell, feedback bridge, analytics, or mutation routes and include
`X-Robots-Tag: noindex`.

Named viewer lists are available only to administrators and the artifact's owning publisher. They
never cross organizations. Administrative and owner views do not count toward ordinary audience
analytics.

## Webhooks and Discord

The server accepts Discord webhook URLs only for the expected Discord host, which blocks arbitrary
server-side requests. It masks webhook URLs in every API and UI response. When `WEBHOOK_ENC_KEY` is
set, AES-256-GCM encrypts stored integration secrets. Without the key, webhook URLs use the
documented plaintext fallback and startup emits a warning.

The durable queue delivers provider events at least once. Bounded retries, dead-letter state, and
privacy-safe metrics expose failures. A process stop during an ambiguous provider attempt can
create a duplicate notification.

Settings accepts a write-only Discord bot token per organization. The server validates the bot and
selected destination, encrypts the token, and never returns or places it in logs, audits, queue
rows, or webhook requests. Outbound thread management does not start inbound Gateway consumption.
Two-way replies require explicit organization and artifact authorization plus the documented
Discord permissions. See the [Discord runbook](ops/discord-durable-delivery.md).

## Preview renderer

The optional preview renderer receives HTML bodies instead of authenticated artifact URLs. It runs
without host, data, or secret mounts on an internal network with no egress. It blocks browser
requests and navigation, uses an ephemeral Chromium context, and has time, response-size, memory,
and CPU limits.

The renderer has no public or tunnel route. If it fails or is absent, the core server continues to
publish artifacts and uses a first-party placeholder. The [configuration reference](configuration.md#preview-isolation)
lists the required container controls.

## Storage and ingress

Bundle paths reject `..`, absolute paths, and configured size or file-count overages. Artifact
writes use staging and atomic rename patterns, while startup recovery reconciles database and file
state after interruption.

Origin admission limits cover headers, URIs, bodies, JSON complexity, connection and request
concurrency, render queues, and source or principal token buckets. Source identifiers and metrics
remain opaque. Trusted proxy configuration only accepts `CF-Connecting-IP` from declared proxy
CIDRs and ignores `X-Forwarded-For`. See [ingress controls](ops/ingress-controls.md).

The Docker build context excludes deployment secrets, persistent data, and local planning files.
The production process runs as a non-root user in a distroless container.

## Current exclusions

Artifact MCP does not scan artifact content and does not place raw artifact delivery on a separate
physical origin. Sandboxing, origin admission limits, and Cloudflare controls remain the active
boundaries. These exclusions are recorded so an operator can decide whether the current model fits
the sensitivity of a deployment.

[Return to the documentation index](README.md).
