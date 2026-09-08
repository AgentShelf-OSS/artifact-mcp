# Getting Started

A step-by-step setup for **artifact-mcp** — from a local test run to a production deployment behind
Cloudflare. Follow it top to bottom; each phase ends with a check so you know it worked before
moving on.

> **For AI agents helping a user set this up:** this file is written to be executed. Work one phase
> at a time, run the verification at the end of each, and stop and report if a check fails rather
> than continuing. Never invent secrets — ask the user for their domain, Cloudflare team name, and
> admin email. The only place identity can be trusted without a verified JWT is loopback
> development (`TRUST_ACCESS_HEADERS=1`); never set that on a reachable host.

---

## What you'll end up with

- An MCP endpoint (`POST /mcp`) where authorized agents publish HTML artifacts and get back a URL.
- A private, org-scoped gallery for humans, gated by Cloudflare Access (SSO).
- Optional public, unguessable share links under `/s/:token`.
- One native Rust core container by default, SQLite + files on disk, no database server. Preview
  thumbnails add an optional browser sidecar.
- Dual MCP compatibility: existing `2025-06-18` clients keep working while `2026-07-28` clients can
  negotiate typed outputs, resources, MCP Apps, and durable preview tasks.

## Prerequisites

- Docker + Docker Compose.
- A domain you control, on Cloudflare (for production). Local testing needs neither.
- For production SSO: a Cloudflare Zero Trust (Access) account — the free tier is enough.

---

## Phase 1 — Get the code

```bash
git clone <this-repo-url> artifact-mcp
cd artifact-mcp
cp .env.example .env
```

**Check:** `.env` exists in the repo root.

---

## Phase 2 — Configure `.env`

Open `.env`. The only value you must set to boot is a bootstrap publishing key.

| Var | Needed | Notes |
|---|---|---|
| `ARTIFACT_API_KEYS` | **yes** | `clientId:org:secret` (comma-separated for several). The DB is authoritative after first boot; this just seeds the first key. Use a long random secret. |
| `MCP_OAUTH_ISSUER` + `MCP_OAUTH_AUDIENCE` + `MCP_OAUTH_JWKS_URL` | optional | Enables short-lived OAuth machine credentials for `/mcp`. Configure the complete triple; keep API keys enabled during rollout. |
| `MCP_API_KEYS_ENABLED` | optional | Defaults to `1`. Set to `0` only after OAuth clients are verified; startup refuses to disable the only authentication path. |
| `WEBHOOK_ENC_KEY` | recommended | A 32-byte base64 key that encrypts Discord webhook URLs in SQLite with AES-256-GCM. If omitted, webhooks remain zero-config but are stored in plaintext and startup warns loudly. |
| `PREVIEW_RENDERER_URL` | optional | Enables persistent gallery/Discord PNGs for single-file publish/update/restore events. Leave unset for gallery placeholders and text-only Discord. |
| `PUBLIC_BASE_URL` | prod | Your real `https://artifact.your-domain`. Defaults to `http://localhost:3480`. Used to build share URLs. |
| `APP_NAME` / `APP_BRAND` | optional | Portal display name and compact mark. Defaults to `Artifact Index` / `A`. |
| `ADMIN_EMAILS` | prod | Comma-separated emails that see every org (the admin gallery). |
| `CF_ACCESS_TEAM_DOMAIN` + `CF_ACCESS_AUD` | prod | Turns on Access JWT verification. Set both in Phase 4. |
| `TRUST_ACCESS_HEADERS` | dev only | `1` trusts an unverified identity header — **loopback development only**, never on a reachable origin. |
| `REQUIRE_ACCESS_JWT` | optional | `1` makes the server refuse to start unless JWT verification is configured. Good for prod images/CI. |
| `HOST_BIND` | optional | Host publish address; defaults to loopback `127.0.0.1`. See Phase 4d. |

Everything else (size caps, `MAX_HISTORY`, `FEEDBACK_MAX_BODY`) has sane defaults — leave it.

Example minimal bootstrap key:
```
ARTIFACT_API_KEYS=agent1:acme:REPLACE_WITH_LONG_RANDOM_SECRET
```

If you will use Discord notifications, generate the deployment encryption key once and add the
printed value to `.env`:

```bash
openssl rand -base64 32
# WEBHOOK_ENC_KEY=<paste the generated value>
```

Keep this key in the same protected secret store as the deployment and backup credentials. On the
first boot with a key, any existing plaintext webhook rows are encrypted in place. Losing the key
means existing encrypted webhook URLs cannot be delivered or recovered by artifact-mcp.

**Check:** `ARTIFACT_API_KEYS` is set to a value you control.

---

## Phase 3 — Run locally (development)

For a first local run with no Cloudflare, enable loopback header-trust so you can act as a viewer:

```bash
echo 'TRUST_ACCESS_HEADERS=1' >> .env      # loopback dev only
docker compose up -d --build
docker logs artifact-mcp 2>&1 | grep "viewer identity mode ready"
```

> Need the compatibility twin for development? `npm install && npm run dev` starts the Node
> reference runtime directly with loopback header-trust already enabled. The shipped Compose
> service and production image use Rust.

You should see a structured log with `"identity_mode":"header-trust"`. Publish a test artifact:

```bash
KEY=REPLACE_WITH_LONG_RANDOM_SECRET   # the secret from ARTIFACT_API_KEYS
curl -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"publish_artifact",
       "arguments":{"html":"<h1>hi</h1>","title":"Demo","description":"first artifact"}}}' \
  http://localhost:3480/mcp
```

Browse the gallery as an admin (header trusted locally):
```bash
curl -H "Cf-Access-Authenticated-User-Email: you@example.com" http://localhost:3480/ | head
```

**Check:** the publish call returns a URL, and the gallery HTML renders. When done testing, remove
`TRUST_ACCESS_HEADERS=1` before exposing the app anywhere — with it gone, identity fails closed.

---

## Phase 4 — Production behind Cloudflare

Two surfaces are deliberately split:
- **Upload** (`/mcp`) — API-key auth; Access-bypassed (agents can't do interactive SSO).
- **View** (`/`, `/:id`, `/settings`) — behind Access; the app verifies the JWT and scopes to org.
- **Share** (`/s/:token`) — public, but only with a valid token.

### 4a. Tunnel

Create a Cloudflare Tunnel and route a public hostname (e.g. `artifact.your-domain`) to the
artifact-mcp origin. See Phase 4d for the exact origin URL — prefer the container name over a host
IP.

### 4b. Bootstrap the catch-all Access application

Do this before enabling strict runtime startup. Cloudflare assigns the Application Audience (AUD)
only after an app exists, while artifact-mcp reads the AUD when its identity module is imported.

Create a least-privilege API token, then run the setup command from the repo root:

```bash
export CF_API_TOKEN=REPLACE_WITH_A_LEAST_PRIVILEGE_TOKEN
export CF_ACCOUNT_ID=REPLACE_WITH_ACCOUNT_ID
export PUBLIC_BASE_URL=https://artifact.your-domain
export CF_ACCESS_IDP_ID=REPLACE_WITH_YOUR_ONE_IDP_ID

node scripts/cf-access-setup.mjs          # dry-run
node scripts/cf-access-setup.mjs --apply  # explicit mutation
```

The command finds or creates the catch-all app, configures one allowed IdP with automatic redirect,
and prints `CF_ACCESS_AUD=...` plus `CF_ACCESS_TEAM_DOMAIN=...`. It never writes `.env` and never
creates or edits Access policies. Optional account-wide login branding and a defense-in-depth
Email Obfuscation Configuration Rule are documented in
[`docs/DEPLOY-CLOUDFLARE.md`](docs/DEPLOY-CLOUDFLARE.md).

### 4c. Create policies, then turn on JWT verification

In Zero Trust, create or verify these applications and policies in precedence order:

1. **`/mcp`** → policy **Bypass → Everyone**. Agents authenticate with the API key, not SSO.
2. **`/s/*`** → policy **Bypass → Everyone**. The app validates the opaque share token. Application
   code cannot make an Access-gated route public.
3. The setup-created **catch-all** app → policy **Allow** your viewer domains and admin email(s).

Copy the emitted values into `.env` and enable strict mode:

```dotenv
CF_ACCESS_TEAM_DOMAIN=yourteam.cloudflareaccess.com
CF_ACCESS_AUD=REPLACE_WITH_THE_EMITTED_AUD
PUBLIC_BASE_URL=https://artifact.your-domain
ADMIN_EMAILS=you@your-domain
REQUIRE_ACCESS_JWT=1
```

Fully restart the process: `docker compose up -d --build`. A hot env-file edit is not enough. The
boot log must now read `Access identity: JWT-verified`.

### 4d. Don't publish the origin on the LAN

Cloudflare Access only guards the **tunnel hostname**. A directly-reachable origin port bypasses it
entirely. Two ways to close that, best first:

**Option A — tunnel-only (no host port at all):** use an operator-owned public Cloudflare Tunnel
Compose overlay on the app's default network. Set its origin service to
`http://artifact-mcp:3480` and use a Compose override to reset the app's `ports` list only after the
public hostname has been verified. Nothing is then published on the host.

**Option B — loopback bind:** keep the default `HOST_BIND=127.0.0.1`, so the port is reachable only
from the host, and point the tunnel at `http://localhost:3480` from a `cloudflared` running on that
host.

Do not confuse either public-gallery option with the optional `anthropic-tunnel` profile. That
profile is Anthropic's research-preview, outbound-only private MCP transport and does not serve the
human gallery. Its staged enablement and rollback are documented in
[`docs/ops/anthropic-mcp-tunnel.md`](docs/ops/anthropic-mcp-tunnel.md).

**Check:**
```bash
ss -ltn | grep 3480          # want 127.0.0.1:3480 (or nothing published, Option A) — NOT 0.0.0.0
curl https://artifact.your-domain/mcp -X POST -d '{}'   # reaches the app (401/JSON), site is up
```

---

## Phase 5 — Create keys and onboard orgs (in the app)

Once you can sign in as admin at `https://artifact.your-domain/settings`:

- **Onboard a viewer org:** Settings → create the org (name + email domain), then add that domain to
  the catch-all Access allow-policy so its people can sign in. A signed-in viewer is auto-tenanted
  by their email domain.
- **Let an org publish:** Settings → generate an upload key for that org. The secret is shown once —
  hand it to the agent/integration. Revoke anytime without a redeploy.
- **Notifications (optional):** Settings → add a per-org Discord webhook and pick which events it
  receives. The UI and HTTP responses always show a masked URL. With `WEBHOOK_ENC_KEY` configured,
  the full URL is encrypted at rest; without it, the documented plaintext fallback applies.

### Optional: persistent gallery and Discord thumbnails

To add inline PNG previews for single-file publish/update/restore notifications, set this in
`.env`:

```dotenv
PREVIEW_RENDERER_URL=http://artifact-preview:3000
```

Then enable the renderer profile:

```bash
docker compose --profile preview up -d --build
```

The renderer processes untrusted HTML. It must remain on the shipped internal-only network with no
published port, tunnel route, host/app-data mounts, or secrets. One validated PNG per current
single-file content digest is stored in `DATA_DIR/previews` and reused by the authenticated gallery
and Discord; existing artifacts backfill serially after startup. Bundles always use a distinct
first-party placeholder. Removing `PREVIEW_RENDERER_URL` stops new rendering without breaking the
gallery; renderer failures use placeholders and text embeds. Set `PREVIEW_MAX_PNG_BYTES` only if you
need to override the safe 7,500,000-byte default.

Gallery cards are 16:10. The renderer defaults to `PREVIEW_VIEWPORT=1200x630` (a Discord social-card
ratio); set `PREVIEW_VIEWPORT=1200x750` if you want thumbnails that fill the card without a crop.

**Check:** a freshly generated key can publish; the artifact appears in that org's gallery section.

---

## Persisting state from an artifact

Viewer state stores JSON either shared with the artifact's organization in `org` scope or
private to the signed-in person in `viewer` scope. Omitting `scope` selects `org`, preserving
existing artifacts. State survives reloads and artifact updates. An artifact sends `state:hello`
as it loads, then waits about one second
for `state:ready`. If no shell answers, state stays disabled. That is the expected path for
public shares, historical views, and direct `/raw` navigation. Artifact code uses
`window.parent.postMessage(message, "*")`; the trusted shell handles authenticated requests.
No fetch or browser storage access is needed inside the artifact.

| Direction | Message | Fields |
|---|---|---|
| Artifact to shell | `state:hello` | None |
| Shell to artifact | `state:ready` | `enabled`, `scope: "org"`, `keys: [{key, revision}]`; when enabled also `scopes: ["org", "viewer"]`, `viewer: {id, name}`, `viewerKeys: [{key, revision}]` |
| Artifact to shell | `state:get` | `key`, optional `scope` |
| Shell to artifact | `state:value` | `key`, `scope`, `value`, `revision`, optional `conflict: true` |
| Artifact to shell | `state:set` | `key`, `value`, optional `scope`, optional `ifRevision` |
| Shell to artifact | `state:saved` | `key`, `scope`, `revision` |
| Shell to artifact | `state:error` | `key`, `scope`, `reason` |
| Artifact to shell | `state:delete` | `key`, optional `scope` |

Error reasons are `disabled`, `too_large`, `bad_key`, `bad_scope`, `conflict`, `network`, and `forbidden`.
`too_large` covers both value size and the key capacity of the selected scope and owner.
Missing values arrive as `null` with revision `0`. Keys use 1 to 64 ASCII letters, digits,
periods, underscores, or hyphens. Each artifact supports 64 org keys and 64 private keys per
viewer, each holding up to 256 KiB of serialized JSON. A viewer can only read or write their
own private keys, including when they are an administrator. The bridge never forwards the
last writer's email. Viewer-scope HTTP responses also omit `updated_by`.

`viewer.id` is a stable, opaque identifier of 16 lowercase hex characters. `viewer.name` is the
organization email member's display name, or a name derived from the email local part. Names
have at most 40 characters. Neither field contains the full email. An administrator can set a
name with `POST /settings/orgs/:name/emails` and JSON
`{"email":"reader@example.org","display_name":"Alex"}`. The name is trimmed and rejects control
characters or more than 40 characters; sending an empty name restores the derived name.
Writing a name again updates that membership rather than creating a duplicate.

This example keeps a note in memory when persistence is disabled or the page is opened without
a shell. The text is org-shared when persistence is enabled.

```html
<textarea id="note" aria-label="Shared note"></textarea>
<p id="status">Notes are temporary on this page.</p>
<script>
const note = document.querySelector('#note'), status = document.querySelector('#status');
let enabled = false, ready = false, timedOut = false, revision = 0;
const send = message => parent.postMessage(message, '*');
addEventListener('message', event => {
  if (event.source !== parent || !event.data) return;
  const m = event.data;
  if (m.type === 'state:ready' && !timedOut) {
    ready = true; enabled = m.enabled;
    if (enabled) send({type: 'state:get', key: 'note'});
  }
  if (m.type === 'state:value' && m.key === 'note') { note.value = m.value ?? ''; revision = m.revision; }
  if (m.type === 'state:saved' && m.key === 'note') { revision = m.revision; status.textContent = 'Saved for the organization.'; }
  if (m.type === 'state:error') status.textContent = 'Save unavailable: ' + m.reason;
});
note.oninput = () => { if (enabled) send({type: 'state:set', key: 'note', value: note.value, ifRevision: revision}); };
send({type: 'state:hello'});
setTimeout(() => { if (!ready) { timedOut = true; enabled = false; status.textContent = 'Notes are temporary on this page.'; } }, 1000);</script>
```

Sets coalesce per scope and key for 500 ms. Only `state:saved` confirms a server save. Reads can return a
cached value followed by a fresh value with a different revision. On a stale `ifRevision`, the
shell sends the current value with `conflict:true`, followed by `state:error` with reason
`conflict`. The example accepts the server's version; an editor can instead show a merge prompt.
See [ADR-0007](docs/adr/0007-viewer-state-via-shell-broker.md) for the storage and trust boundaries.
If `state:ready` arrives after the one-second timeout, the example keeps state disabled for that
page. The artifact bytes remain unchanged in public shares and historical views.

For a collaborative reader, save a diary privately and publish selected notes with the viewer's
display name. After an enabled `state:ready`, keep its `viewer` object and send messages such as:

```js
// These run inside an artifact, after its state:ready handler enables persistence.
send({type: 'state:set', scope: 'viewer', key: 'diary', value: diaryText});
send({type: 'state:get', scope: 'viewer', key: 'diary'});

// Run only when the reader chooses to publish this note to the organization.
send({
  type: 'state:set', scope: 'org', key: 'note.' + viewer.id,
  value: {author: viewer.name, text: selectedNote}
});
```

Check both `scope` and `key` in response handlers, since the same key can exist in both scopes.
Attribution stored in an org value is supplied by the artifact; it is not a verified author
signature. Other readers can update org keys. Private values only become shared when the
artifact explicitly writes them to an org key. Viewer caches are separated by viewer ID on a
shared browser. The server remains the source of truth.

The shell uses the existing HTTP state routes with `?scope=viewer` for private operations and
`?scope=org` or no parameter for shared operations. Other scope values return
`400 {"error":"bad_scope"}`. Raw, historical, and public-share documents do not gain a shell or
viewer identity. See [ADR-0008](docs/adr/0008-per-viewer-state-scope-and-opaque-viewer-identity.md).

## Identity modes (quick reference)

| Mode | When | Behavior |
|---|---|---|
| `jwt` | `CF_ACCESS_TEAM_DOMAIN` + `CF_ACCESS_AUD` set | Identity from a verified Access JWT. **Use in production.** |
| `header-trust` | JWT unset + `TRUST_ACCESS_HEADERS=1` | Trusts the (spoofable) email header. **Loopback dev only.** |
| `disabled` | JWT unset, no opt-in | Fails closed — no request can get a viewer/admin identity. Safe default. |

`/mcp` (API key) and `/s/:token` (share token) work in all three modes — they don't depend on viewer
identity.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Gallery shows "Not signed in" after login | JWT vars unset/incorrect, or the one-shot post-auth retry also failed | Run setup first, set the emitted `CF_ACCESS_*`, fully restart, then retry sign-in. |
| Boot log says `HEADER-TRUST` in production | `TRUST_ACCESS_HEADERS=1` left in `.env` | Remove it; set the JWT vars. |
| `/mcp` returns 401 | Missing/wrong `Authorization: Bearer <key>` | Use a valid, non-revoked key for that org. |
| Share link 404s | Expired, revoked, unknown token, or `/s/*` Access app missing/not Bypass | Recreate the link; confirm the `/s/*` Bypass app exists. |
| Server won't start, logs `REQUIRE_ACCESS_JWT` | Strict mode on without JWT vars | Set both JWT vars, or drop `REQUIRE_ACCESS_JWT`. |
| Hundreds of blocked `email-decode.min.js` scripts | An old response lacks the origin `no-transform` directive | Confirm `Cache-Control` contains `no-transform`; see the Cloudflare deployment runbook. |
| Access shows a redundant method picker | Catch-all app has several allowed IdPs or auto-redirect is off | Rerun `cf-access-setup.mjs` and apply the proposed app update. |
| Site down right after loopback bind | Tunnel still targets a host IP | Point the tunnel origin at `http://artifact-mcp:3480` on the shared network (Phase 4d). |
| MCP client doesn't see new tools | Clients cache `tools/list` at connect | Reconnect the integration after a server update. |

---

## Where to go next

- `README.md` — full feature list, MCP tool reference, architecture, security model.
- `CONTEXT.md` — domain language, invariants, module seams (for contributors and code-editing agents).
- `.env.example` — every configuration variable with inline notes.
