// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman

import test from "node:test";
import assert from "node:assert/strict";
import { recoverExactNotification } from "../lib/discord-history-recovery.js";

function destination() {
  return {
    organizationId: "acme",
    guildId: "guild-1",
    channelId: "channel-1",
    selectedWebhookId: "webhook-1"
  };
}

function exactMessage(id, overrides = {}) {
  return {
    id,
    guild_id: "guild-1",
    channel_id: "channel-1",
    webhook_id: "webhook-1",
    embeds: [{ url: "https://artifacts.example/artifacts/old-one" }],
    ...overrides
  };
}

function request(overrides = {}) {
  return {
    credential: { resolveForOrganization: async () => ({ opaque: true }) },
    destination: destination(),
    artifact: { id: "old-one", canonicalUrl: "https://artifacts.example/artifacts/old-one" },
    ...overrides
  };
}

test("recovery accepts one exact selected-webhook canonical-URL notification without retaining its body", async () => {
  const calls = [];
  const rest = {
    async listChannelMessages(input) {
      calls.push(input);
      return {
        messages: [{
          id: "message-1",
          guild_id: "guild-1",
          channel_id: "channel-1",
          webhook_id: "webhook-1",
          content: "this must not cross the boundary",
          embeds: [{ url: "https://artifacts.example/artifacts/old-one" }]
        }],
        nextCursor: null
      };
    }
  };

  const result = await recoverExactNotification({
    rest,
    credential: { resolveForOrganization: async () => ({}) },
    destination: destination(),
    artifact: { id: "old-one", canonicalUrl: "https://artifacts.example/artifacts/old-one" }
  });

  assert.deepEqual(result, {
    state: "recovered",
    messageId: "message-1",
    provenance: "exact_selected_webhook_canonical_url"
  });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].channelId, "channel-1");
  assert.deepEqual(Object.keys(result).sort(), ["messageId", "provenance", "state"]);
  assert.doesNotMatch(JSON.stringify(result), /this must not cross the boundary/);
});

test("recovery rejects same-URL messages from another webhook or channel", async () => {
  const result = await recoverExactNotification(request({
    rest: { async listChannelMessages() {
      return {
        messages: [
          exactMessage("wrong-webhook", { webhook_id: "webhook-other" }),
          exactMessage("wrong-channel", { channel_id: "channel-other" })
        ],
        nextCursor: null
      };
    } }
  }));
  assert.deepEqual(result, { state: "not_found" });
});

test("recovery chooses the newest exact match without scanning older pages", async () => {
  const cursors = [];
  const result = await recoverExactNotification(request({
    rest: { async listChannelMessages({ before }) {
      cursors.push(before);
      return before === null
        ? { messages: [exactMessage("message-1")], nextCursor: "older" }
        : { messages: [exactMessage("message-2")], nextCursor: null };
    } }
  }));
  assert.deepEqual(result, {
    state: "recovered",
    messageId: "message-1",
    provenance: "exact_selected_webhook_canonical_url"
  });
  assert.deepEqual(cursors, [null]);
});

test("recovery fails closed when notification embeds are redacted or a scan is rate-limited", async () => {
  const redacted = await recoverExactNotification(request({
    rest: { async listChannelMessages() { return { messages: [exactMessage("redacted", { embeds: [] })], nextCursor: null }; } }
  }));
  assert.deepEqual(redacted, { state: "unavailable" });

  const limited = await recoverExactNotification(request({
    rest: { async listChannelMessages() { const error = new Error("provider detail"); error.status = 429; throw error; } }
  }));
  assert.deepEqual(limited, { state: "rate_limited" });
});

test("recovery returns incomplete rather than calling bounded history absent", async () => {
  const result = await recoverExactNotification(request({
    maxPages: 1,
    rest: { async listChannelMessages() { return { messages: [], nextCursor: "older" }; } }
  }));
  assert.deepEqual(result, { state: "incomplete" });
});
