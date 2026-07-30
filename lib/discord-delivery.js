// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Discord durable-delivery provider contract. This intentionally does not replace the detached
// `notify.js` path yet: the PBI-056 worker will consume this API after its outbox exists. Every
// returned classification is safe to persist/log; it never copies a webhook URL, token, or payload
// into an error value.

import { isDiscordWebhookUrl as sharedDiscordWebhookUrl } from "./discord-webhook-url.js";

export const DISCORD_DELIVERY_TIMEOUT_MS = 4_000;
export const MAX_MESSAGE_ID_BYTES = 32;
export const MAX_RESPONSE_BODY_BYTES = 65_536;
export const MAX_RATE_LIMIT_VALUE_BYTES = 128;
export const MAX_RETRY_DELAY_MS = 24 * 60 * 60 * 1_000;
export const DELIVERY_SEMANTICS = "bounded_at_least_once";

const SAFE_QUERY_PARAMS = new Set(["thread_id", "with_components"]);
const MESSAGE_ID = /^[1-9][0-9]{0,31}$/;

function terminal(reason) { return { state: "terminal", reason }; }
function retry(reason, duplicateRisk, rateLimit = null) {
  return { state: "retry", reason, duplicateRisk, rateLimit };
}

function validSnowflake(value) {
  return typeof value === "string" && MESSAGE_ID.test(value);
}
function validWebhookRef(value) { return typeof value === "string" && /^webhook:[A-Za-z0-9_.-]{1,120}$/.test(value); }

function validQueryPair(key, value) {
  return (key === "thread_id" && validSnowflake(value))
    || (key === "with_components" && (value === "true" || value === "false"));
}

function isDiscordWebhookUrl(url) {
  return sharedDiscordWebhookUrl(url.toString())
    && url.protocol === "https:"
    && (url.hostname === "discord.com" || url.hostname === "discordapp.com")
    && !url.username && !url.password
    && url.pathname.startsWith("/api/webhooks/");
}

/**
 * Add/replace `wait=true` for a Discord execute-webhook call without retaining arbitrary query
 * parameters. `thread_id` and `with_components` are preserved only in their safe
 * forms; an explicit `threadId` replaces a safe existing thread. Throws only a fixed error code.
 */
export function executionUrl(webhookUrl, { threadId } = {}) {
  let url;
  try { url = new URL(webhookUrl); } catch { throw new Error("allowlist_rejected"); }
  if (!sharedDiscordWebhookUrl(webhookUrl) || !isDiscordWebhookUrl(url)) throw new Error("allowlist_rejected");
  if (url.hash) throw new Error("allowlist_rejected");
  threadId = threadId ?? undefined;
  if (threadId !== undefined && !validSnowflake(threadId)) throw new Error("bad_request");

  let thread;
  let components;
  for (const [key, value] of url.searchParams) {
    if (!SAFE_QUERY_PARAMS.has(key) || !validQueryPair(key, value)) continue;
    if (key === "thread_id" && thread === undefined) thread = value;
    if (key === "with_components" && components === undefined) components = value;
  }
  if (threadId !== undefined) thread = threadId;
  url.search = "";
  if (thread !== undefined) url.searchParams.append("thread_id", thread);
  if (components !== undefined) url.searchParams.append("with_components", components);
  url.searchParams.append("wait", "true");
  if (!isDiscordWebhookUrl(url) || url.hash) throw new Error("allowlist_rejected");
  return url.toString();
}

function header(headers, name) {
  if (!headers) return undefined;
  if (typeof headers.get === "function") return headers.get(name) ?? undefined;
  const wanted = name.toLowerCase();
  for (const [key, value] of Object.entries(headers)) {
    if (key.toLowerCase() === wanted) return String(value);
  }
  return undefined;
}

function boundedHeader(value) {
  return typeof value === "string" && value.length > 0 && value.length <= MAX_RATE_LIMIT_VALUE_BYTES
    && !value.includes("\r") && !value.includes("\n");
}

function delayMillisecondsFromNumber(seconds) {
  if (typeof seconds !== "number" || !Number.isFinite(seconds) || seconds <= 0) return undefined;
  const milliseconds = Math.ceil(seconds * 1_000);
  return Number.isSafeInteger(milliseconds) && milliseconds <= MAX_RETRY_DELAY_MS ? milliseconds : undefined;
}

function delayMillisecondsFromText(value) {
  if (typeof value !== "string" || !/^[0-9]+(?:\.[0-9]+)?$/.test(value)) return undefined;
  return delayMillisecondsFromNumber(Number(value));
}

function parseJson(body) {
  if (typeof body !== "string" || Buffer.byteLength(body) > MAX_RESPONSE_BODY_BYTES) return undefined;
  try { return JSON.parse(body); } catch { return undefined; }
}

function scope(value) {
  return ["global", "shared", "user"].includes(value) ? value : undefined;
}

function rateLimit(headers, body, webhookRef) {
  const parsed = parseJson(body);
  const bodyRetryAfter = delayMillisecondsFromNumber(parsed?.retry_after);
  const retryAfterHeader = delayMillisecondsFromText(header(headers, "retry-after"));
  // Reset-After is metadata, never a retry delay fallback.
  const retryAfterMs = bodyRetryAfter !== undefined && retryAfterHeader !== undefined
    ? Math.max(bodyRetryAfter, retryAfterHeader)
    : bodyRetryAfter ?? retryAfterHeader;
  const bucket = header(headers, "x-ratelimit-bucket");
  return {
    webhookRef: webhookRef ?? null,
    retryAfterMs: retryAfterMs ?? null,
    bucket: boundedHeader(bucket) ? bucket : null,
    remaining: (() => {
      const value = header(headers, "x-ratelimit-remaining");
      return boundedHeader(value) && /^[0-9]+$/.test(value) ? Number(value) : null;
    })(),
    resetAfterMs: delayMillisecondsFromText(header(headers, "x-ratelimit-reset-after")) ?? null,
    scope: (parsed?.global === true || header(headers, "x-ratelimit-global") === "true"
      ? "global" : scope(header(headers, "x-ratelimit-scope"))) ?? null
  };
}

/** Classify a known pre/post-transport fault without inspecting or returning its message. */
export function classifyDiscordFault(fault) {
  switch (fault) {
    case "invalid_secret": return terminal("invalid_secret");
    case "decrypt_failed": return terminal("decrypt_failed");
    case "allowlist_rejected": return terminal("allowlist_rejected");
    case "network": return retry("network", "possible");
    case "timeout": return retry("timeout", "possible");
    case "ambiguous": return retry("ambiguous", "possible");
    default: return terminal("contract_error");
  }
}

/**
 * Classify a completed HTTP response. A 2xx is accepted only with a bounded JSON `id` snowflake;
 * malformed 2xx is ambiguous and may duplicate on the bounded at-least-once retry. For 429,
 * body `retry_after` and `retry-after` are maxed conservatively; reset-after is metadata only.
 */
export function classifyDiscordResponse({ status, headers, body, webhookRef }) {
  if (status >= 200 && status < 300) {
    if (!validWebhookRef(webhookRef)) return terminal("invalid_secret");
    const id = parseJson(body)?.id;
    return validSnowflake(id)
      ? { state: "accepted", messageId: id, rateLimit: rateLimit(headers, body, webhookRef) }
      : classifyDiscordFault("ambiguous");
  }
  if (status >= 300 && status < 400) return terminal("redirect");
  if (status === 400) return terminal("bad_request");
  if (status === 401) return terminal("unauthorized");
  if (status === 403) return terminal("forbidden");
  if (status === 404) return terminal("not_found");
  if (status === 429) {
    if (!validWebhookRef(webhookRef)) return terminal("invalid_secret");
    const metadata = rateLimit(headers, body, webhookRef);
    // A 429 without a positive supplied delay is terminal/dead-letter, never generic backoff.
    return metadata.retryAfterMs > 0 ? retry("rate_limited", "none", metadata) : terminal("invalid_rate_limit_delay");
  }
  if ([500, 502, 503, 504].includes(status)) return retry("server_error", "possible");
  return terminal(status >= 500 && status < 600 ? "server_error" : "client_error");
}

async function boundedResponseText(response) {
  const contentLength = Number(header(response.headers, "content-length"));
  if (Number.isFinite(contentLength) && contentLength > MAX_RESPONSE_BODY_BYTES) throw new Error("oversized");
  if (response.body?.getReader) {
    const reader = response.body.getReader();
    const chunks = [];
    let bytes = 0;
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        bytes += value.byteLength;
        if (bytes > MAX_RESPONSE_BODY_BYTES) {
          await reader.cancel();
          throw new Error("oversized");
        }
        chunks.push(value);
      }
      return new TextDecoder().decode(Buffer.concat(chunks.map((chunk) => Buffer.from(chunk))));
    } finally {
      reader.releaseLock();
    }
  }
  // Fetch Responses have a stream. This bounded fallback only supports deterministic test doubles.
  const text = await response.text();
  if (Buffer.byteLength(text) > MAX_RESPONSE_BODY_BYTES) throw new Error("oversized");
  return text;
}

/**
 * Execute one worker attempt with the provider's non-negotiable transport controls. This is not
 * used by the legacy notifier. The returned value deliberately excludes URL/payload and raw error
 * text. A worker must only schedule a 429 retry when `rateLimit.retryAfterMs` is defined.
 */
export async function deliverDiscordWebhook({ webhookUrl, threadId, webhookRef, body, headers, fetchImpl = globalThis.fetch }) {
  if (!validWebhookRef(webhookRef)) return terminal("invalid_secret");
  let url;
  try { url = executionUrl(webhookUrl, { threadId }); } catch (error) {
    return error?.message === "bad_request" ? terminal("bad_request") : classifyDiscordFault("allowlist_rejected");
  }
  const controller = new AbortController();
  let phase = "send";
  let expire;
  const timeout = new Promise((_, reject) => {
    expire = () => { controller.abort(); reject(new Error("timeout")); };
  });
  const timer = setTimeout(expire, DISCORD_DELIVERY_TIMEOUT_MS);
  try {
    const response = await Promise.race([fetchImpl(url, {
      method: "POST", body, headers, signal: controller.signal, redirect: "manual"
    }), timeout]);
    if (!(response.status >= 200 && response.status < 300) && response.status !== 429) {
      return classifyDiscordResponse({ status: response.status, headers: response.headers, body: "", webhookRef });
    }
    phase = "body";
    let responseBody = "";
    try { responseBody = await Promise.race([boundedResponseText(response), timeout]); } catch {
      return response.status === 429
        ? classifyDiscordResponse({ status: 429, headers: response.headers, body: "", webhookRef })
        : classifyDiscordFault("ambiguous");
    }
    return classifyDiscordResponse({ status: response.status, headers: response.headers, body: responseBody, webhookRef });
  } catch (error) {
    return classifyDiscordFault(phase === "body" ? "ambiguous" : error?.message === "timeout" || error?.name === "AbortError" ? "timeout" : "network");
  } finally {
    clearTimeout(timer);
  }
}
