// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// Shared raw-prefix Discord webhook allowlist. Keep this literal in lockstep with the Rust
// `persistence::webhooks::is_discord_webhook_url` compatibility predicate.

const DISCORD_WEBHOOK_RE = /^https:\/\/(discord|discordapp)\.com\/api\/webhooks\//i;

export function isDiscordWebhookUrl(value) {
  return DISCORD_WEBHOOK_RE.test(String(value || ""));
}
