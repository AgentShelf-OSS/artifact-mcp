// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Bounded, body-free historical notification recovery. Persistence owns job claiming,
// checkpoints, and result storage; this module owns the provider-facing exact-match boundary.

const MAX_PAGES = 10;
const MAX_MESSAGES = 500;
const MAX_EMBEDS_PER_MESSAGE = 10;
const MAX_RESPONSE_BYTES = 1_048_576;
const EXACT_MATCH_PROVENANCE = "exact_selected_webhook_canonical_url";

function safeId(value) {
  return typeof value === "string" && value.length > 0 && value.length <= 128 && /^[A-Za-z0-9_-]+$/.test(value);
}

function safeCanonicalUrl(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > 2_048) return false;
  try {
    const url = new URL(value);
    return url.protocol === "https:" && !url.username && !url.password && !url.hash;
  } catch {
    return false;
  }
}

function invalidInput() {
  return { state: "invalid_request" };
}

function providerState(error) {
  // Provider adapters may supply one of these fixed classifications. Never pass provider text
  // across this boundary: it can contain a Discord response body or a credential-bearing URL.
  if (["rate_limited", "permission_denied", "not_found", "unavailable"].includes(error?.classification)) {
    return { state: error.classification };
  }
  if (error?.status === 429) return { state: "rate_limited" };
  if (error?.status === 401 || error?.status === 403) return { state: "permission_denied" };
  if (error?.status === 404) return { state: "not_found" };
  return { state: "unavailable" };
}

function selectedWebhookExactMessage(message, destination, canonicalUrl) {
  if (!message || !safeId(message.id)) return null;
  if (message.guild_id !== destination.guildId || message.channel_id !== destination.channelId) return null;
  if (message.webhook_id !== destination.selectedWebhookId) return null;
  if (!Array.isArray(message.embeds) || message.embeds.length === 0) return { redacted: true };
  const embeds = message.embeds.slice(0, MAX_EMBEDS_PER_MESSAGE);
  return { id: embeds.some((embed) => embed && embed.url === canonicalUrl) ? message.id : null, redacted: false };
}

/**
 * Scan a configured channel through a server-side REST port. The credential is opaque to this
 * function's callers and is never returned or serialized. It deliberately accepts only exact
 * embed URL equality and the configured webhook/guild/channel identity; content and titles are
 * neither examined nor retained.
 */
export async function recoverExactNotification({
  rest,
  credential,
  destination,
  artifact,
  maxPages = MAX_PAGES,
  maxMessages = MAX_MESSAGES,
  maxResponseBytes = MAX_RESPONSE_BYTES,
  cursor = null
} = {}) {
  if (!rest || typeof rest.listChannelMessages !== "function" || !credential || typeof credential.resolveForOrganization !== "function") return invalidInput();
  if (!destination || !safeId(destination.organizationId) || !safeId(destination.guildId) || !safeId(destination.channelId) || !safeId(destination.selectedWebhookId)) return invalidInput();
  if (!artifact || !safeId(artifact.id) || !safeCanonicalUrl(artifact.canonicalUrl)) return invalidInput();
  if (!Number.isInteger(maxPages) || maxPages < 1 || maxPages > MAX_PAGES || !Number.isInteger(maxMessages) || maxMessages < 1 || maxMessages > MAX_MESSAGES || !Number.isInteger(maxResponseBytes) || maxResponseBytes < 1 || maxResponseBytes > MAX_RESPONSE_BYTES) return invalidInput();

  let resolvedCredential;
  try {
    resolvedCredential = await credential.resolveForOrganization(destination.organizationId);
  } catch (error) {
    return providerState(error);
  }
  if (!resolvedCredential) return { state: "credential_unavailable" };

  let currentCursor = cursor === null ? null : String(cursor);
  const seenCursors = new Set();
  const matches = new Set();
  let scanned = 0;

  for (let page = 0; page < maxPages && scanned < maxMessages; page += 1) {
    if (currentCursor !== null) {
      if (seenCursors.has(currentCursor)) return { state: "unavailable" };
      seenCursors.add(currentCursor);
    }
    let result;
    try {
      result = await rest.listChannelMessages({
        credential: resolvedCredential,
        organizationId: destination.organizationId,
        guildId: destination.guildId,
        channelId: destination.channelId,
        before: currentCursor,
        limit: Math.min(100, maxMessages - scanned),
        maxResponseBytes
      });
    } catch (error) {
      return providerState(error);
    }
    if (!result || !Array.isArray(result.messages) || (Number.isFinite(result.responseBytes) && result.responseBytes > maxResponseBytes)) return { state: "unavailable" };
    let redactedEmbeds = false;
    for (const message of result.messages.slice(0, maxMessages - scanned)) {
      scanned += 1;
      const evaluation = selectedWebhookExactMessage(message, destination, artifact.canonicalUrl);
      if (evaluation?.redacted) redactedEmbeds = true;
      if (evaluation?.id) matches.add(evaluation.id);
    }
    if (matches.size > 1) return { state: "ambiguous" };
    // Discord may redact embeds when the bot lacks the capability required to inspect the
    // notification. An empty selected-webhook message is not evidence that no exact anchor
    // exists, so do not call it not_found or create a replacement notification.
    if (redactedEmbeds) return { state: "unavailable" };
    if (result.nextCursor === null || result.nextCursor === undefined || result.nextCursor === "") {
      return matches.size === 1
        ? { state: "recovered", messageId: [...matches][0], provenance: EXACT_MATCH_PROVENANCE }
        : { state: "not_found" };
    }
    currentCursor = String(result.nextCursor);
  }

  // A bounded scan that has not exhausted history is not evidence of absence. Persist a safe
  // retryable state; do not guess, and never publish a replacement notification card.
  return { state: "incomplete" };
}

export const HISTORICAL_RECOVERY_PROVENANCE = EXACT_MATCH_PROVENANCE;
