# Security Policy

## Reporting a vulnerability

Please report security issues privately — **do not** open a public issue for a
suspected vulnerability. Use GitHub's **[Report a vulnerability](../../security/advisories/new)**
(Security → Advisories) or email the maintainer. You'll get an acknowledgement,
and a fix or mitigation will be coordinated before public disclosure.

## Security model (what the app does and does not guarantee)

- **Tenant isolation** is enforced server-side: API keys are locked to an org, and
  viewers are scoped to their org by verified identity. Cross-org reads/writes 404.
- **Viewer identity** is intended to run behind [Cloudflare Access](https://developers.cloudflare.com/cloudflare-one/policies/access/).
  When `CF_ACCESS_TEAM_DOMAIN` + `CF_ACCESS_AUD` are set the app **verifies the
  Access JWT**, so identity/org cannot be spoofed. With those unset the app **fails
  closed** — no request can obtain a viewer/admin identity from a header. Header trust is
  an explicit loopback-only dev opt-in (`TRUST_ACCESS_HEADERS=1`) that additionally refuses
  to start on a non-loopback bind. **Set the JWT vars in production.**
- **Untrusted artifact content** is served with a CSP sandbox (no `allow-same-origin`)
  on every raw/download/share response, so it runs in a null origin. In addition, every
  cookie-authenticated portal mutation requires a first-party `X-Artifact-Mutation: 1` header and
  same-origin browser metadata (or a canonical `Origin` fallback). This closes the otherwise
  possible artifact-iframe → administrator-action forgery: sandboxed artifact JavaScript can issue
  a credentialed `no-cors` request, but cannot attach the non-simple portal header. `/mcp` uses
  explicit bearer authentication and is not an ambient-cookie surface.
- **Public share links** (`/s/:token`) are opt-in, read-only, `noindex`, `no-store`,
  and gated only by an unguessable token — treat them as "anyone with the link."
  They rely on a Cloudflare Access **Bypass** on `/s/*`.
- **Webhooks** are validated to the Discord host (no SSRF to arbitrary hosts) and
  delivered fire-and-forget without following redirects.
- **Not included:** content scanning, rate limiting, and a physically separate
  raw-content origin. Run the service on a dedicated hostname.

## Supported versions

This is a young project; security fixes target the `main` branch. Pin a commit if
you need stability and watch releases for advisories.
