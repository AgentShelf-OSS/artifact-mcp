# Scheduled reminders and Web Push — implementation contract

This is the contract for ADR-0012. Node (`lib/`) and Rust (`src/`) must expose identical HTTP, MCP, and
schema behavior. Only Rust runs the sweeper and the push sender.

## Configuration

| Variable | Meaning |
|---|---|
| `WEB_PUSH_VAPID_PRIVATE_KEY` | base64url (no padding) of the raw 32-byte P-256 private scalar. Unset = feature off. |
| `WEB_PUSH_SUBJECT` | `mailto:` or `https:` contact URL placed in the VAPID JWT `sub`. Required when the key is set. |
| `WEB_PUSH_ENDPOINT_HOSTS` | Comma-separated host allowlist (trimmed, lowercased, empty entries ignored). An entry `*.example.com` matches subdomains only. Every entry needs at least two labels (after `*.` too, so `*.com` and `localhost` are rejected). IP literals (IPv4 or IPv6) and all-numeric labels are rejected. An endpoint whose host is an IP literal never matches. Default: `fcm.googleapis.com,updates.push.services.mozilla.com,push.services.mozilla.com,web.push.apple.com,*.push.apple.com,*.notify.windows.com` |

The feature is **enabled** only when the VAPID key parses as a valid P-256 scalar, the subject is valid, and
`WEBHOOK_ENC_KEY` (the existing encryption key) is configured. An invalid VAPID key or subject is a startup
configuration error, matching other config validation. The public key is derived from the private key
(uncompressed point, 65 bytes, base64url). `node scripts/web-push-keys.mjs` prints a new key pair.

## Schema (migration 39 `web-push-reminders`, shared SQL file)

Migrations 37 and 38 were already used by collections, so this is **migration 39**, named
`web-push-reminders`. Both runtimes execute the shared file `migrations/039-web-push-reminders.sql`
(Node reads it with `readFileSync`; Rust uses `include_str!`), so `sqlite_master` stays byte-identical.

```sql
CREATE TABLE IF NOT EXISTS push_subscriptions (
  id TEXT PRIMARY KEY,
  org TEXT NOT NULL,
  viewer_email TEXT NOT NULL,
  endpoint_ciphertext TEXT NOT NULL,
  endpoint_sha256 TEXT NOT NULL UNIQUE,
  p256dh TEXT NOT NULL,
  auth TEXT NOT NULL,
  label TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  last_success_at TEXT,
  failure_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS push_subscriptions_viewer ON push_subscriptions(org, viewer_email);

CREATE TABLE IF NOT EXISTS artifact_push_optins (
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  viewer_email TEXT NOT NULL,
  org TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (artifact_id, viewer_email)
);

CREATE TABLE IF NOT EXISTS artifact_reminders (
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  scope TEXT NOT NULL CHECK (scope IN ('org','viewer')),
  owner TEXT NOT NULL DEFAULT '',
  key TEXT NOT NULL,
  org TEXT NOT NULL,
  fire_at INTEGER NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'armed' CHECK (state IN ('armed','fired')),
  revision INTEGER NOT NULL DEFAULT 1,
  created_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  fired_at TEXT,
  PRIMARY KEY (artifact_id, scope, owner, key)
);
CREATE INDEX IF NOT EXISTS artifact_reminders_due ON artifact_reminders(state, fire_at);

CREATE TABLE IF NOT EXISTS push_deliveries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  subscription_id TEXT NOT NULL REFERENCES push_subscriptions(id) ON DELETE CASCADE,
  artifact_id TEXT NOT NULL,
  reminder_key TEXT NOT NULL,
  payload TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','accepted','dead')),
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  last_status INTEGER,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS push_deliveries_due ON push_deliveries(state, next_attempt_at);
```

Notes:
- `owner` is `''` for `org` scope, and the lowercased viewer email for `viewer` scope.
- `fire_at`, `next_attempt_at`, `expires_at` are epoch milliseconds. Text timestamps use the same format as
  the rest of the schema.
- `created_by` is `viewer` or `publisher:<client_id>`. Do not store an email here. Each write sets it to the
  latest writer.
- `endpoint_ciphertext` holds the existing `WEBHOOK_ENC_KEY` AES-256-GCM record in one column:
  `v1:<nonce>:<ciphertext>:<tag>`, each part standard base64 (12-byte nonce, 16-byte tag).
- `p256dh` and `auth` are stored exactly as received.
- Follow each runtime's existing migration conventions (checksums, schema fixtures, historical fixture tests).
  If a runtime has a schema snapshot or parity test, update it so both runtimes still agree.

## Validation shared by routes and MCP

- `key`: `^[A-Za-z0-9._-]{1,64}$` (same as viewer state).
- `scope`: `org` (default) or `viewer`. MCP accepts only `org`.
- Time: exactly one of `fire_at` (epoch ms integer) or `delay_seconds` (integer). The resolved time must be
  60 seconds to 30 days after the current server time. Otherwise `400 {"error":"bad_time"}`.
- `title`: string, trimmed, 1..80 characters (Unicode scalar values). `body`: string, 0..240. Control
  characters other than `\n` are rejected. Otherwise `400 {"error":"bad_text"}`.
- At most 16 rows in `armed` state per artifact (all scopes together). Setting a key that is already armed does
  not count as a new row. A new key, or a fired key that is re-armed, past the limit returns
  `409 {"error":"reminder_limit"}`.
- Control characters are Unicode category Cc (`U+0000`–`U+001F`, `U+007F`–`U+009F`); only `U+000A` is allowed.
  `body` is not trimmed. A non-string `body` (including `null`) is `bad_text`.
- A time field that is present counts as given, even when it is `null`. So `null` alone, or `null` next to the
  other field, is `bad_time`, as is any present value that is not a safe integer (for example a string). Over MCP the
  input schema types both fields as `integer`, so a `null` there fails argument validation (JSON-RPC `-32602`)
  before the tool runs, in both runtimes.
- Setting a key upserts the row: new `fire_at`, title, body, `state='armed'`, `fired_at=NULL`,
  `revision = revision + 1` (1 on insert).

## Viewer HTTP API

Every route requires an authenticated viewer who can open the artifact, with the same identity resolution,
concealed `404`, and admin handling as viewer state. Mutating routes (PUT/DELETE) require the same
request-authenticity guard as viewer state (`x-artifact-mutation` header plus same-origin checks) and use the
state rate-limit budget (`INGRESS_STATE_PER_WINDOW`). When the feature is disabled, every route below except
`GET /push/config` returns `404 {"error":"push_disabled"}`. Responses are JSON with `cache-control: no-store`.

| Method and path | Body | Success |
|---|---|---|
| `GET /push/config` | — | `200 {"enabled":bool,"vapid_public_key":string|null}` (signed-in viewer required, else `404 {"error":"Not found"}`) |
| `PUT /push/subscriptions` | `{"endpoint":string,"keys":{"p256dh":string,"auth":string},"label":string?}` | `200 {"id":string}` |
| `DELETE /push/subscriptions` | `{"endpoint":string}` | `204` |
| `GET /{id}/push` | — | `200 {"enabled":true,"opted_in":bool,"devices":int}` |
| `PUT /{id}/push/optin` | `{}` | `200 {"opted_in":true}` |
| `DELETE /{id}/push/optin` | — | `200 {"opted_in":false}` |
| `GET /{id}/reminders?scope=org|viewer` | — | `200 {"reminders":[{"key","scope","fire_at","title","body","revision"}]}` for armed rows the viewer may see |
| `PUT /{id}/reminders/{key}?scope=org|viewer` | `{"fire_at"?:int,"delay_seconds"?:int,"title":string,"body":string?}` | `200 {"key","scope","fire_at","revision"}` |
| `DELETE /{id}/reminders/{key}?scope=org|viewer` | — | `204` (also when absent) |

Route details shared by both runtimes:
- Both runtimes use the same order. For `/{id}/…`: concealed artifact access (`404 {"error":"Not found"}`), then `push_disabled`, then
  (mutations only) the state rate limit `429 {"error":"rate_limited"}`, then `bad_scope`, then `bad_key`, then the
  JSON body (`400 {"error":"bad_body"}` for malformed JSON, a non-object, or unknown fields; `413 {"error":"too_large"}`
  over 8 KiB). `/push/subscriptions` requires a signed-in viewer (else `404 {"error":"Not found"}`), then
  `push_disabled`, then the rate limit, then the body. GET routes do not spend the rate-limit budget.
- `PUT /push/subscriptions` accepts the fields `endpoint`, `keys`, `label`, and `expirationTime` (ignored, because
  `PushSubscription.toJSON()` includes it). Other fields are `bad_body`. A `label` that is not a string, longer than
  60 characters, or contains a control character is `bad_body`. Extra fields inside `keys` are `bad_keys`.
  `p256dh`/`auth` may carry trailing `=` padding; the decoded bytes must be canonical base64url.
- `DELETE /push/subscriptions` requires exactly `{"endpoint":string}`, else `bad_body`.
- `PUT /{id}/push/optin` ignores its body. The opt-in row stores the artifact's org.
- `GET /{id}/push` `devices` counts the viewer's subscriptions whose `org` equals the artifact's current org or is
  `admin` (the rows the sweeper would use).
- Subscription `org` is the viewer's org from identity resolution (`admin` for administrators). Every authenticated
  push request (`/push/*`, `/{id}/push*`, `/{id}/reminders*`, when the feature is enabled) first refreshes the org of
  all the viewer's subscriptions to the current resolved org, so an org change takes effect at the next request.
- When an upsert moves an existing subscription to another viewer or org, its `pending` `push_deliveries` rows are
  deleted in the same transaction.
- `GET /{id}/reminders` lists armed rows ordered by `key`.

Subscription rules:
- `endpoint` must parse as an absolute `https:` URL of at most 1024 bytes, with no userinfo, on port 443, and
  its host must match `WEB_PUSH_ENDPOINT_HOSTS`. Otherwise `400 {"error":"bad_endpoint"}`.
- `p256dh` must be base64url of a 65-byte uncompressed point that is valid on P-256. `auth` must be base64url of
  16 bytes. Otherwise `400 {"error":"bad_keys"}`. `label` is at most 60 characters (for example "iPhone").
- Upsert by `endpoint_sha256` (hex SHA-256 of the endpoint string as received). An existing row keeps its `id` and
  `created_at`, moves to the current viewer and org, gets the new keys, label, and ciphertext, and resets
  `failure_count` to 0. New rows get a random UUID `id`. At most 10 subscriptions per viewer email (across orgs);
  registering an 11th deletes that viewer's oldest rows (`ORDER BY created_at, rowid`).
- `DELETE` removes only a row that belongs to the current viewer. Otherwise it still returns `204`.
- `viewer` scope reminders use the current viewer's email as `owner`. `GET` with `scope=viewer` only lists that
  viewer's rows.

## MCP tool

`set_artifact_reminder` (both runtimes, writer permission through `writeArtifactOrRefuse` or its Rust equivalent):

```json
{"id":"<artifact id>","key":"diaper","fire_at":1791500000000,"delay_seconds":7200,"title":"Diaper change due","body":"...","clear":false}
```

- `clear: true` deletes the org-scope key and ignores the other fields. Result: `{"id","key","cleared":true}`.
- Otherwise the same validation as the route, `scope` fixed to `org`, `created_by` = `publisher:<client_id>`.
  Result: `{"id","key","scope":"org","fire_at","revision"}`.
- When the feature is disabled the tool returns an error result: `Web Push reminders are not configured on this server.`
  This check runs before the artifact permission check.
- The key is validated for both set and clear. Error results use these exact texts:
  - `bad_key`: `Invalid reminder key: use 1-64 characters from A-Z, a-z, 0-9, '.', '_', or '-'.`
  - `bad_time`: `Invalid reminder time: give exactly one of fire_at or delay_seconds, resolving to 60 seconds to 30 days from now.`
  - `bad_text`: `Invalid reminder text: title must be 1-80 characters after trimming, body at most 240, with no control characters other than newline.`
  - `reminder_limit`: `Reminder limit reached: an artifact can have at most 16 armed reminders.`
- Definition (the `tools/list` golden is authoritative; it sits directly after `append_artifact_events`):
  `{"name":"set_artifact_reminder","description":"Arm, replace, or clear a named reminder for an artifact. When it fires, opted-in viewers of the artifact's organization get a Web Push notification.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"key":{"type":"string"},"fire_at":{"type":"integer","description":"Epoch milliseconds."},"delay_seconds":{"type":"integer"},"title":{"type":"string"},"body":{"type":"string"},"clear":{"type":"boolean"}},"required":["id","key"],"additionalProperties":false}}`
- Output schema: `conformance/mcp.tool-output-schemas.json` → `set_artifact_reminder` (required `id`, `key`).
- Add the tool to the OAuth scope list, the observability tool list, and the tool docs (`docs/mcp-api.md`), in
  the same places as `append_artifact_events`.

## Static files

These files contain no private data. Browsers and iOS can fetch the manifest and icons without cookies, so the
application serves all of them **without a viewer check** (Cloudflare Access still applies at the edge unless an
operator adds a bypass). They are served whether or not the feature is enabled and are registered before the
`/{id}` routes. `sw.js`, `manifest.webmanifest`, and `icons` are reserved artifact IDs.

- `GET /sw.js` returns the file `assets/push-sw.js` with `content-type: text/javascript; charset=utf-8`,
  `cache-control: no-cache`, `service-worker-allowed: /`, and `x-content-type-options: nosniff`.
- `GET /manifest.webmanifest` returns `assets/manifest.webmanifest` with
  `content-type: application/manifest+json` (no charset), `cache-control: no-cache`, and
  `x-content-type-options: nosniff`.
- `GET /icons/{name}` for `app-192.png`, `app-512.png`, `maskable-512.png`, `apple-touch-icon.png`, and
  `badge-72.png` returns `assets/icons/{name}` with `content-type: image/png`, `x-content-type-options: nosniff`,
  and `cache-control: public, max-age=3600`. Any other `/icons/…` path returns `404 {"error":"Not found"}`.

## Shell broker (artifact ↔ viewer shell)

Artifact to shell (only accepted from the artifact iframe, same checks as `state:*`):

| Message | Fields |
|---|---|
| `reminder:hello` | — |
| `reminder:set` | `key`, `scope?` (`org`), `delaySeconds?` or `fireAt?`, `title`, `body?`, `requestId?` |
| `reminder:clear` | `key`, `scope?`, `requestId?` |
| `reminder:prompt` | `reason?` (string at most 120 characters; shown as plain text) |

Shell to artifact:

| Message | Fields |
|---|---|
| `reminder:ready` | `enabled` (server feature on and the browser supports push), `optedIn`, `permission` (`default`/`granted`/`denied`/`unsupported`), `needsInstall` (iOS Safari outside Home Screen mode) |
| `reminder:saved` | `key`, `scope`, `fireAt`, `requestId?` |
| `reminder:cleared` | `key`, `scope`, `requestId?` |
| `reminder:error` | `key?`, `reason` (`disabled`, `bad_time`, `bad_text`, `reminder_limit`, `forbidden`, `network`, …), `requestId?` |
| `reminder:status` | same fields as `reminder:ready`, sent when the opt-in or permission changes |

When there is no shell (public share, historical revision, raw navigation), the artifact gets no reply. It must
treat reminders as unavailable after about one second, as with `state:hello`.

Reminders can be armed even when no one has opted in; they then reach nobody. `reminder:set` does not require
the current viewer to be opted in.

## Shell UI

- When `GET /push/config` reports enabled and the browser has `serviceWorker`, `PushManager`, and
  `Notification`, the viewer toolbar shows a bell button: "Notify me" / "Notifications on" / "Blocked in
  browser settings" / "Add to Home Screen to get notifications" (iOS outside standalone mode).
- Turning on: request permission (in the click handler), `navigator.serviceWorker.register('/sw.js')`,
  `pushManager.subscribe({userVisibleOnly:true, applicationServerKey})`, `PUT /push/subscriptions`,
  `PUT /{id}/push/optin`. Turning off removes the opt-in only (the device subscription stays for other artifacts).
- `reminder:prompt` shows a small dismissible banner in the shell with the reason text and a "Turn on
  notifications" button that runs the same flow. The artifact's text is set with `textContent`.
- The shell HTML links the manifest and an `apple-touch-icon`.

## Service worker (`assets/push-sw.js`)

- `push`: parse `event.data.json()` as `{title, body, url, tag}`. Call
  `registration.showNotification(title, {body, tag, renotify: true, data: {url}, icon, badge})` inside
  `event.waitUntil`. If parsing fails, show a generic "Reminder" notification (browsers require a visible
  notification for each push).
- `notificationclick`: close the notification, focus an open window whose URL matches `data.url`, otherwise
  `clients.openWindow(data.url)`. Only open same-origin URLs.
- `pushsubscriptionchange`: resubscribe with the old options and `fetch('/push/subscriptions', {method:'PUT',
  credentials:'include', headers:{'content-type':'application/json','x-artifact-mutation':'1'}})`. Ignore failure.
- No `fetch`, `install`-time caching, or `importScripts`.

## Sweeper and sender (Rust only)

Sweeper: a tokio task started next to the delivery runtime when the feature is enabled. Every 5 seconds, plus a
`wake()` after a reminder write with `fire_at` within 10 seconds:

1. In one `BEGIN IMMEDIATE` transaction, select up to 100 rows `WHERE state='armed' AND fire_at <= now`.
2. For each row: when `now - fire_at > 6h`, set `state='fired'` with no deliveries. Otherwise find the recipients:
   Recipients are computed from the artifact's **current** org at fire time (`artifacts.org`), not the org stored on
   the reminder or opt-in row. A subscription qualifies when its `org` equals that org or is `admin`; administrators
   can read every org, so they receive org reminders for artifacts they opted in to.
   - `org` scope: `artifact_push_optins` rows for the artifact, joined to each opted-in viewer's qualifying
     `push_subscriptions`.
   - `viewer` scope: the owner's opt-in row (required) and the owner's qualifying subscriptions.
   Insert one `push_deliveries` row per subscription: payload JSON
   `{"title","body","url":"<PUBLIC_BASE_URL>/<artifact id>","tag":"<artifact id>:<key>"}`,
   `next_attempt_at=now`, `expires_at=now+30min`. Set the reminder to `state='fired'`, `fired_at=now`.
3. Commit, then wake the sender.

Sender: one task, polling every 2 seconds plus wake. It leases due `pending` rows (`next_attempt_at <= now`) so an
overlapping poll or a restart does not send the same row twice while a send is in flight, sends the leased rows with
bounded concurrency, re-checks the endpoint allowlist for each row, and sends:

- `POST <endpoint>`. Headers: `TTL: 1800`, `Urgency: high`, `Topic: <first 32 base64url chars of SHA-256(tag)>`,
  `Content-Encoding: aes128gcm`, `Content-Type: application/octet-stream`,
  `Authorization: vapid t=<JWT>, k=<public key>`.
- JWT (ES256): header `{"typ":"JWT","alg":"ES256"}`, claims `aud` = endpoint origin (`https://host`),
  `exp` = now + 12 hours, `sub` = `WEB_PUSH_SUBJECT`. Cache it for each audience for up to 1 hour.
- Body: RFC 8291 `aes128gcm` with a fresh ephemeral P-256 key and a 16-byte salt for each message, record size
  4096, a single record, padding delimiter `0x02`. Unit-test it against the RFC 8291 section 5 example vector.
- No redirects, 10-second timeout, `reqwest` client with rustls.
- Outcome: `200`/`201`/`202`: `accepted`, set subscription `last_success_at`, `failure_count=0`. `404`/`410`:
  delete the subscription (the delivery cascades away). `429` and `5xx`, network errors: retry with backoff of
  5s, 30s, 2m, 10m (honor `Retry-After` up to 10m), `failure_count+1`; `dead` once past `expires_at`. Other
  statuses: `dead`.
- Retention: `accepted` and `dead` deliveries are pruned 7 days after their last update.
- Metrics: counters for reminders fired, deliveries accepted, retried, dead, and subscriptions removed, in the
  existing `/metrics` style. No endpoints or emails in logs.
