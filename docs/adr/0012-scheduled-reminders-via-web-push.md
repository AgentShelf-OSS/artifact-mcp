# ADR-0012: Scheduled artifact reminders delivered by Web Push

- Status: Proposed
- Date: 2026-10-08

## Context

Some artifacts must alert people when nothing is on screen. The first case is a family baby log.
After a circumcision, the diaper must be changed every two hours, also at night with the phone
locked. ADR-0003 gives artifact code an opaque origin and `connect-src 'none'`. The artifact
therefore cannot use the Notification, Push, or Wake Lock APIs. Page timers stop when a phone
locks. Today the server has no scheduler. The Discord outbox sends rows as soon as they are
added, and it orders delivery per webhook. A future-dated row would block every later
notification to the same webhook.

## Decision

1. **The artifact asks; the shell and the server enforce.** The viewer shell gets a new
   `reminder:*` postMessage family, next to `state:*` (ADR-0007). The artifact can set or clear
   a named reminder with a time, a title, and a body. It cannot choose recipients, URLs, or icons.
   The shell calls first-party routes with the same viewer identity, request-authenticity checks,
   concealed `404`, and state rate-limit budget as viewer state. Public shares, historical
   revisions, and direct `/raw` navigation have no reminder capability.
2. **Reminders are server records.** `artifact_reminders` stores one row for each
   `(artifact, scope, owner, key)`. Setting a key again replaces its time and text, so "two
   hours after the last change" is one call per change. Scopes follow ADR-0008. An `org`
   reminder goes to every opted-in viewer of the artifact's organization. A `viewer` reminder
   goes only to the viewer who set it. Publishers can set `org` reminders through the MCP tool
   `set_artifact_reminder`.
3. **People opt in for each artifact.** A reminder only reaches a viewer who has (a) a push
   subscription on at least one device and (b) a per-artifact opt-in. The opt-in starts from a
   button in the trusted shell. That button provides the user gesture that
   `Notification.requestPermission()` needs. An artifact can ask the shell to show its prompt.
   It can never subscribe a person on its own.
4. **A sweeper, not future-dated outbox rows.** A Rust runtime task wakes every few seconds. It
   claims due reminders, expands each one into one `push_deliveries` row per recipient
   subscription, and marks the reminder `fired`, all in one transaction. A separate push worker
   sends those rows. Web Push has its own delivery table. It does not use the Discord
   `provider_delivery_outbox`, whose CHECK constraints and per-webhook ordering are specific to
   Discord.
5. **Web Push is implemented in-house.** VAPID (RFC 8292) uses an ES256 JWT. Payload encryption
   is `aes128gcm` (RFC 8291 and RFC 8188). The implementation uses the P-256, HKDF, AES-GCM, and
   JWT crates the build already has. The operator sets the VAPID private key through the
   environment. Subscription endpoints are stored encrypted with the existing `WEBHOOK_ENC_KEY`
   scheme. The feature is disabled unless the VAPID key, the VAPID subject, and the encryption
   key are all configured. Rotating the VAPID key invalidates every subscription, and viewers
   must opt in again.
6. **The server only sends to push services.** Endpoints must use `https` and match an allowlist
   of push-service hosts (`WEB_PUSH_ENDPOINT_HOSTS`, with a default list for Google, Mozilla,
   Apple, and Microsoft). The check runs when a subscription is saved and again before each send.
   Redirects are not followed. This stops a viewer from turning the server into a request
   forwarder.
7. **A minimal service worker on the trusted origin.** `/sw.js` (scope `/`) only handles `push`,
   `notificationclick`, and `pushsubscriptionchange`. It has no `fetch` handler, so it can never
   cache or proxy artifact bytes. `/manifest.webmanifest` lets iOS 16.4+ users add the app to
   the Home Screen. iOS only allows Web Push for apps added there.
8. **Privacy.** A payload holds only the reminder title, the body, the artifact URL, and a tag.
   It never holds viewer emails. Push services can see the endpoint, the timing, and the size.
   They cannot see the content. Logs and metrics keep aggregate counts only.
9. **Delivery semantics.** Delivery is at least once and can repeat. A reminder that is more than
   six hours late is marked fired without delivery. A delivery expires 30 minutes after it is
   queued. Responses `404` and `410` delete the subscription. `429` and `5xx` responses retry
   with backoff until expiry. Other `4xx` responses mark the delivery dead.
10. **Runtime split.** Both runtimes implement migration 37, the routes, the MCP tool, the
    validation, and the static service-worker and manifest responses with identical contracts.
    Only Rust runs the sweeper and the push sender, as with the Discord delivery worker. The Node
    reference stores reminders but does not send them.

## Consequences

- Locked phones get reminders on Android, desktop Chrome, Firefox, and Edge. On iPhone,
  reminders need iOS 16.4 or later and the app added to the Home Screen.
- A notification tap can reach a Cloudflare Access login page after the session expires. The
  notification text is complete without opening the app.
- Any artifact a viewer opts in to can notify that viewer. A viewer can turn reminders off for
  an artifact, or for a device, from the shell. Limits: 16 armed reminders per artifact, fire
  times from 60 seconds to 30 days ahead, a title of at most 80 characters, and a body of at most
  240 characters.
- A Discord channel for reminders can be added later as a second consumer of the sweeper,
  without changing the artifact API.
