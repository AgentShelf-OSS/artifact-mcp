# Configuration reference

Artifact MCP reads configuration from environment variables. Copy `.env.example` to `.env` for a
local Docker installation. Keep production secrets in the secret store used by your deployment,
not in the repository.

The [getting started guide](../GETTING_STARTED.md) identifies the smallest local configuration and
the additional values required behind Cloudflare Access.

## Publishing credentials

| Variable | Purpose |
|---|---|
| `ARTIFACT_API_KEYS` | Bootstrap keys in comma-separated `clientId:org:secret` form. SQLite becomes authoritative after the first boot. |
| `MCP_API_KEYS_ENABLED` | Enables API-key compatibility. Defaults to `1`. Set it to `0` only with complete, verified OAuth configuration. |
| `MCP_JSON_LIMIT` | Optional JSON-envelope size limit. The default remains above the configured bundle cap. |

## OAuth

| Variable | Purpose |
|---|---|
| `MCP_OAUTH_ISSUER` | Expected token issuer. Required with the audience and JWKS URL. |
| `MCP_OAUTH_AUDIENCE` | Expected resource audience. |
| `MCP_OAUTH_JWKS_URL` | JWKS endpoint for asymmetric token verification. |
| `MCP_OAUTH_ALLOWED_ALGS` | Allowed asymmetric JWT algorithms. Defaults to `RS256`. |
| `MCP_OAUTH_MAX_TOKEN_LIFETIME_S` | Maximum accepted access-token lifetime. Defaults to `3600`. |
| `MCP_OAUTH_CLOCK_TOLERANCE_S` | Clock tolerance for token checks. Defaults to `30`. |

OAuth tokens must contain `sub` or `client_id`, `org`, integer `iat` and `exp` values, and an
explicit `scope` string or `scp` string or array. Artifact MCP recognizes these scopes:

- `artifacts:read`
- `artifacts:publish`
- `artifacts:review`
- `artifacts:visibility`
- `artifacts:delete`

Configured deployments publish RFC 9728 metadata at `/.well-known/oauth-protected-resource`.
Insufficient scope returns HTTP 403 with a `WWW-Authenticate` challenge that names the required
scope.

## Viewer identity and public URLs

| Variable | Purpose |
|---|---|
| `PUBLIC_BASE_URL` | Canonical public URL used to generate links. Defaults to `http://localhost:3480`. |
| `CF_ACCESS_TEAM_DOMAIN` | Cloudflare Access team domain used for JWT verification. Configure it with `CF_ACCESS_AUD`. |
| `CF_ACCESS_AUD` | Expected Access application audience. |
| `REQUIRE_ACCESS_JWT` | Set to `1` to refuse startup unless both Access JWT values are present. |
| `ACCESS_CLOCK_TOLERANCE_S` | Access JWT clock tolerance in seconds. Defaults to `60`. Keep the host synchronized with NTP. |
| `TRUST_ACCESS_HEADERS` | Set to `1` only for loopback local development. It trusts a spoofable identity header. |
| `ADMIN_EMAILS` | Comma-separated administrator email addresses. |
| `ADMIN_EMAIL_DOMAINS` | Email domains whose verified members are administrators. |
| `ORG_EMAIL_DOMAINS` | Optional `domain:org` bootstrap mappings. The registry managed in Settings becomes authoritative. |

Viewer organization resolution checks configured administrators, explicit email mappings,
registered domains, `ORG_EMAIL_DOMAINS`, and finally the verified email domain. An explicit member
mapping assigns an already authenticated viewer to an organization. By default it does not change
Cloudflare Access or send an invitation. The Rust runtime can optionally synchronize explicit
members as described below. The Node reference runtime still requires manual Cloudflare updates.

### Automatic explicit-email login access (Rust)

When enabled, adding or removing a specific email, or deleting an organization, wakes a background
task that synchronizes the union of all organizations' explicit member emails to one dedicated
Cloudflare Access policy. It also reconciles on startup and every 60 seconds, repairing missed
updates and retrying failures. SQLite membership is the durable source of truth; local edits commit
even when Cloudflare is unavailable. Settings displays pending, synced, error, or disabled status.
The status covers the complete explicit-email list, not just the selected organization.

Cloudflare grants entry to the application. The application's organization checks continue to
control private artifact access. Domain and administrator policies are independent and untouched.
Removing an explicit member does not revoke access granted through another policy or through the
application's existing domain mapping. Cloudflare sessions are not explicitly revoked by this task.
Membership changes and edge-policy changes are not atomic; allow time for reconciliation and
Cloudflare propagation. A successful sync confirms the policy contents, not email delivery.

Configure this on the Rust server:

| Variable | Meaning |
| --- | --- |
| `ACCESS_SYNC_ENABLED` | `1` enables synchronization; default `0`. |
| `ACCESS_SYNC_API_TOKEN` | Scoped Cloudflare API token with Access application/policy read and write permission for the selected account. Kept server-side and redacted in diagnostics. |
| `ACCESS_SYNC_ACCOUNT_ID` | Account containing the application. |
| `ACCESS_SYNC_APPLICATION_ID` | Access application protecting `PUBLIC_BASE_URL`'s hostname. |
| `ACCESS_SYNC_POLICY_ID` | Dedicated, non-reusable Allow policy named `Artifact member emails`. Its email list is fully managed by the app. |

Before enabling:

1. Create a dedicated application policy named `Artifact member emails`, action Allow, with exact
   email Include rules. Do not add domain rules, Require conditions, or unrelated exclusions to it.
   Do not reuse this policy on another application.
2. Configure the IDs and scoped token in the server's protected environment file. Do not copy a
   global Cloudflare API key into the application. Keep one active runtime managing this policy.
3. Enable sync and restart the Rust service. Verify Settings reports synced before testing a fresh
   email-code request. No invitation or code is sent automatically.
4. Remove redundant manual exact-email grants only after verifying synchronization. For example,
   the earlier `Book Club members` policy would otherwise continue granting those addresses login
   even after their removal from the managed list. Preserve independent domain and admin grants.

An empty membership list is represented by Include Everyone plus Exclude Everyone in the dedicated
policy, so it grants nobody access. Unexpected policy structure, target mismatches, or API failures
are reported as errors and retried without changing other policies. Disabling sync stops updates
and leaves the last Cloudflare policy in place; it does not revoke its grants. For rollback, disable
sync and manage that policy manually using a saved policy snapshot.

## Application and network

| Variable | Purpose |
|---|---|
| `APP_NAME` | Portal name. Defaults to `Artifact Index`. |
| `APP_BRAND` | Compact portal mark. Defaults to `A`. |
| `HOST_BIND` | Host publish address. Defaults to loopback-only `127.0.0.1`. |
| `TRUSTED_PROXY_CIDRS` | Optional Cloudflare or proxy CIDRs allowed to supply `CF-Connecting-IP`. The server ignores `X-Forwarded-For`. |
| `INGRESS_*` | Header, URI, body, JSON complexity, concurrency, render-queue, and token-bucket limits. `.env.example` contains the conservative defaults. |

Cloudflare Access protects the tunnel hostname, not an origin port reached directly. Keep the
origin on a private network or bind it to loopback. See [Cloudflare deployment](DEPLOY-CLOUDFLARE.md)
and [ingress controls](ops/ingress-controls.md).

## Content and retention limits

| Variable | Default | Purpose |
|---|---:|---|
| `MAX_ARTIFACT_BYTES` | 2 MB | Maximum self-contained artifact body. |
| `MAX_BUNDLE_BYTES` | 8 MB | Maximum total multi-file bundle size. |
| `MAX_BUNDLE_FILES` | 100 | Maximum number of files in one bundle. |
| `MAX_HISTORY` | 20 | Retained revisions per artifact. |
| `FEEDBACK_MAX_BODY` | 4000 | Maximum feedback body length. |

## Discord

| Variable | Purpose |
|---|---|
| `WEBHOOK_ENC_KEY` | A 32-byte base64 AES-256-GCM key for stored integration secrets. Existing Discord webhook URLs retain a plaintext fallback when it is unset. Organization bot credentials require it. |
| `DISCORD_BOT_TOKEN` | Process-only migration fallback for an organization with an existing pilot connection. Save an organization credential in Settings to retire it. |
| `DISCORD_INBOUND_ENABLED` | Operator switch for explicitly authorized two-way Discord Gateway consumption. Defaults to `0`. |

Generate `WEBHOOK_ENC_KEY` once and keep it with the deployment's protected credentials:

```bash
openssl rand -base64 32
```

When the key first appears, the server encrypts compatible plaintext webhook rows in place.
Encrypted and plaintext rows can coexist during rollout.

### Rotate the webhook encryption key

Do not overwrite the old key while encrypted rows still need it.

1. While the old key is active, record each webhook's event subscriptions and copy its complete URL
   from Discord's integration settings. Artifact MCP only displays a mask.
2. Remove the event subscriptions from those registrations. Keep the registrations and old key in
   place while the durable-delivery queue drains. Follow the [replacement runbook](ops/discord-durable-delivery.md#replacing-a-discord-webhook-safely).
3. Delete the drained registrations in Settings. Stop the application and back up the data volume
   with the old key.
4. Generate and install the new key. Restart the application, recreate the webhooks, run the Test
   action, and restore their subscriptions.

This procedure creates a notification outage, but it does not strand queued work or write decrypted
URLs back to SQLite. A no-outage rotation would require a future dual-key migration.

## Persistent previews

| Variable | Purpose |
|---|---|
| `PREVIEW_RENDERER_URL` | Internal renderer base URL. Leave it unset for gallery placeholders and Discord text-only notifications. |
| `PREVIEW_RENDER_TIMEOUT_MS` | Renderer timeout. Defaults to `8000`. |
| `PREVIEW_VIEWPORT` | Social-card crop. Defaults to `1200x630`. |
| `PREVIEW_MAX_PNG_BYTES` | Largest accepted and persisted renderer response. Defaults to `7500000`. |

Enable the Compose profile after setting `PREVIEW_RENDERER_URL=http://artifact-preview:3000`:

```bash
docker compose --profile preview up -d --build
docker compose exec artifact-preview npm run smoke
```

### Preview isolation

The sidecar renders untrusted HTML. Keep its container boundary intact:

- Run it as a non-root user with a read-only root filesystem.
- Drop Linux capabilities and retain `no-new-privileges` and the supplied seccomp profile.
- Keep it on the internal-only network with resource limits and browser request blocking.
- Do not publish its port, attach it to the tunnel, mount application data, or provide secrets.

Chromium's in-process sandbox is disabled because it requires `CAP_SYS_CHROOT` or user namespaces
that the hardened container removes. The container restrictions above provide the compensating
boundary.

Valid PNGs live under `DATA_DIR/previews/<artifact-id>/<body_sha256>.png`. Include that directory
in capacity planning and backups. Startup removes partial or orphaned previews and backfills
eligible single-file artifacts at low priority. Bundles use the placeholder in the current release.
If the sidecar is unavailable, publishing continues and notifications fall back to text.

[Return to the documentation index](README.md).
