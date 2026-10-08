/* Artifact viewer service worker (ADR-0012).
 * Handles only push, notificationclick, and pushsubscriptionchange.
 * It has no fetch handler, no caching, and no importScripts by design. */
"use strict";

const ICON = "/icons/app-192.png";
const BADGE = "/icons/badge-72.png";

function sameOriginUrl(value) {
  try {
    const url = new URL(String(value || "/"), self.location.origin);
    return url.origin === self.location.origin ? url.href : null;
  } catch {
    return null;
  }
}

self.addEventListener("push", (event) => {
  let title = "Reminder";
  let options = { body: "", icon: ICON, badge: BADGE, data: { url: "/" } };
  try {
    const payload = event.data ? event.data.json() : null;
    if (payload && typeof payload === "object" && typeof payload.title === "string" && payload.title) {
      title = payload.title;
      options = {
        body: typeof payload.body === "string" ? payload.body : "",
        icon: ICON,
        badge: BADGE,
        data: { url: typeof payload.url === "string" ? payload.url : "/" }
      };
      if (typeof payload.tag === "string" && payload.tag) {
        options.tag = payload.tag;
        options.renotify = true;
      }
    }
  } catch {
    // Browsers require a visible notification for every push; fall back to the generic one.
  }
  event.waitUntil(self.registration.showNotification(title, options));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const target = sameOriginUrl(event.notification.data && event.notification.data.url);
  if (!target) return;
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    for (const client of windows) {
      if (client.url === target && "focus" in client) return client.focus();
    }
    if (self.clients.openWindow) return self.clients.openWindow(target);
    return undefined;
  })());
});

self.addEventListener("pushsubscriptionchange", (event) => {
  event.waitUntil((async () => {
    try {
      const old = event.oldSubscription;
      const options = old && old.options
        ? { userVisibleOnly: true, applicationServerKey: old.options.applicationServerKey }
        : null;
      const subscription = event.newSubscription
        || (options && options.applicationServerKey ? await self.registration.pushManager.subscribe(options) : null);
      if (!subscription) return;
      const json = subscription.toJSON();
      await fetch("/push/subscriptions", {
        method: "PUT",
        credentials: "include",
        headers: { "content-type": "application/json", "x-artifact-mutation": "1" },
        body: JSON.stringify({ endpoint: json.endpoint, keys: json.keys })
      });
    } catch {
      // Ignore failure; the shell re-registers on the next opt-in.
    }
  })());
});
