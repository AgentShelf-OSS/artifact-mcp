import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createSpeechService, speechEnabledFor } from "../lib/speech.js";

test("speech enablement supports global, allowlist, legacy, and disabled modes", () => {
  assert.equal(speechEnabledFor("book", { enabled: true, allowlist: "" }), true);
  assert.equal(speechEnabledFor("book", { enabled: true, allowlist: "book,report" }), true);
  assert.equal(speechEnabledFor("other", { enabled: true, allowlist: "book,report" }), false);
  assert.equal(speechEnabledFor("book", { enabled: false, allowlist: "book" }), true);
  assert.equal(speechEnabledFor("other", { enabled: false, allowlist: "book" }), false);
  assert.equal(speechEnabledFor("book", { enabled: false, allowlist: "" }), false);
});

test("speech service validates input before calling the worker and returns WAV bytes", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-test-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "test-token\n");
  const calls = [];
  const service = createSpeechService({ env: { TTS_WORKER_URL: "http://worker:8788", TTS_WORKER_TOKEN_FILE: tokenPath }, fetchImpl: async (...args) => { calls.push(args); return new Response(new Uint8Array([82, 73, 70, 70]), { headers: { "content-type": "audio/wav" } }); } });
  try {
    assert.ok(service);
    assert.equal((await service.synthesize("   ", "af_heart")).error, "bad_text");
    assert.equal((await service.synthesize("😀".repeat(1501), "af_heart")).error, "bad_text");
    assert.equal((await service.synthesize("hello", "unknown")).error, "bad_voice");
    const result = await service.synthesize("hello", "af_heart");
    assert.equal(result.status, 200);
    assert.deepEqual([...result.audio], [82, 73, 70, 70]);
    assert.equal(calls.length, 1);
    assert.equal(calls[0][1].headers.authorization, "Bearer test-token");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("speech service bounds worker streams and releases its busy lane after failure", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-stream-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "test-token\n");
  const oversized = new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array(4 * 1024 * 1024)); controller.enqueue(new Uint8Array([1])); controller.close(); } });
  let calls = 0;
  const service = createSpeechService({ env: { TTS_WORKER_URL: "http://worker:8788", TTS_WORKER_TOKEN_FILE: tokenPath }, fetchImpl: async () => { calls += 1; return new Response(oversized, { headers: { "content-type": "audio/wav" } }); } });
  try {
    assert.ok(service);
    assert.equal((await service.synthesize("hello", "af_heart")).error, "speech unavailable");
    assert.equal((await service.synthesize("hello", "af_heart")).error, "speech unavailable");
    assert.equal(calls, 2);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("speech service rejects concurrent synthesis and resets after worker errors", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-busy-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "test-token\n");
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const service = createSpeechService({ env: { TTS_WORKER_URL: "http://worker:8788", TTS_WORKER_TOKEN_FILE: tokenPath }, fetchImpl: async () => pending });
  try {
    assert.ok(service);
    const first = service.synthesize("one", "af_heart");
    assert.equal((await service.synthesize("two", "af_heart")).error, "busy");
    release(new Response("failure", { status: 500 }));
    assert.equal((await first).error, "speech unavailable");
    assert.equal((await service.synthesize("three", "af_heart")).error, "speech unavailable");
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("speech service exposes Pocket presets only when its worker is configured and routes the name", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-pocket-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "shared-token\n");
  const calls = [];
  const service = createSpeechService({
    env: { TTS_WORKER_URL: "http://kokoro:8788", POCKET_TTS_WORKER_URL: "http://pocket:8788", TTS_WORKER_TOKEN_FILE: tokenPath },
    fetchImpl: async (...args) => { calls.push(args); return new Response(new Uint8Array([82, 73, 70, 70]), { headers: { "content-type": "audio/wav" } }); }
  });
  try {
    const pocketIds = service.voices.filter(({ id }) => id.startsWith("pocket_")).map(({ id }) => id);
    assert.deepEqual(pocketIds, [
      "pocket_alba", "pocket_marius", "pocket_javert", "pocket_jean", "pocket_cosette", "pocket_eponine", "pocket_fantine", "pocket_azelma",
      "pocket_anna", "pocket_bill_boerst", "pocket_caro_davy", "pocket_charles", "pocket_eve", "pocket_george", "pocket_jane", "pocket_mary",
      "pocket_michael", "pocket_paul", "pocket_peter_yearsley", "pocket_stuart_bell", "pocket_vera"
    ]);
    assert.equal(service.voices.find(({ id }) => id === "pocket_bill_boerst").name, "Bill Boerst · Pocket");
    assert.equal(service.voices.find(({ id }) => id === "pocket_peter_yearsley").name, "Peter Yearsley · Pocket");
    assert.equal((await service.synthesize("hello", "pocket_peter_yearsley")).status, 200);
    assert.equal(JSON.parse(calls[0][1].body).voice, "pocket_peter_yearsley");
    assert.equal((await service.synthesize("hello", "pocket_unknown")).error, "bad_voice");
    assert.match(calls[0][0], /^http:\/\/pocket:8788\/speech$/);
    const kokoroOnly = createSpeechService({ env: { TTS_WORKER_URL: "http://kokoro:8788", TTS_WORKER_TOKEN_FILE: tokenPath }, fetchImpl: async () => new Response(new Uint8Array([82, 73, 70, 70]), { headers: { "content-type": "audio/wav" } }) });
    assert.ok(kokoroOnly);
    assert.equal(kokoroOnly.voices.some(({ id }) => id.startsWith("pocket_")), false);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("speech service exposes MOSS voices only when its worker is configured and routes WAV synthesis", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-moss-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "shared-token\n");
  const calls = [];
  const service = createSpeechService({
    env: { TTS_WORKER_URL: "http://kokoro:8788", MOSS_TTS_WORKER_URL: "http://moss:8792", TTS_WORKER_TOKEN_FILE: tokenPath },
    fetchImpl: async (...args) => { calls.push(args); return new Response(new Uint8Array([82, 73, 70, 70]), { headers: { "content-type": "audio/wav" } }); }
  });
  try {
    assert.deepEqual(service.voices.filter(({ id }) => id.startsWith("moss_")).map(({ id }) => id), ["moss_trump", "moss_ava", "moss_bella", "moss_adam", "moss_nathan"]);
    assert.equal((await service.synthesize("hello", "moss_trump")).status, 200);
    assert.equal(JSON.parse(calls[0][1].body).voice, "moss_trump");
    assert.match(calls[0][0], /^http:\/\/moss:8792\/speech$/);
    assert.equal((await service.synthesize("hello", "moss_unknown")).error, "bad_voice");
    assert.equal((await service.synthesize("hello", "moss_trump", "expressive")).error, "bad_instructions");
    const kokoroOnly = createSpeechService({ env: { TTS_WORKER_URL: "http://kokoro:8788", TTS_WORKER_TOKEN_FILE: tokenPath }, fetchImpl: async () => new Response(new Uint8Array([1]), { headers: { "content-type": "audio/wav" } }) });
    assert.equal(kokoroOnly.voices.some(({ id }) => id.startsWith("moss_")), false);
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("speech service exposes Qwen reference voice only when configured and preserves its name", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-qwen-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "shared-token\n");
  const calls = [];
  const service = createSpeechService({
    env: { QWEN_TTS_WORKER_URL: "http://qwen:8125", TTS_WORKER_TOKEN_FILE: tokenPath },
    fetchImpl: async (...args) => {
      calls.push(args);
      return new Response(new Uint8Array([82, 73, 70, 70]), { headers: { "content-type": "audio/wav" } });
    }
  });
  try {
    assert.ok(service);
    assert.deepEqual(service.voices, [{ id: "qwen_reference", name: "Reference voice · Qwen", streaming: true }]);
    assert.equal((await service.synthesize("hello", "qwen_reference")).status, 200);
    assert.equal(JSON.parse(calls[0][1].body).voice, "qwen_reference");
    assert.match(calls[0][0], /^http:\/\/qwen:8125\/speech$/);
    assert.equal((await service.synthesize("hello", "qwen_unknown")).error, "bad_voice");
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("Qwen stream exposes gated preset voices and returns upstream bytes before completion", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-qwen-stream-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "shared-token\n");
  let release;
  const finished = new Promise((resolve) => { release = resolve; });
  const upstream = new ReadableStream({
    async start(controller) {
      controller.enqueue(new Uint8Array([1, 2, 3]));
      await finished;
      controller.enqueue(new Uint8Array([4]));
      controller.close();
    }
  });
  const calls = [];
  const service = createSpeechService({
    env: { QWEN_TTS_WORKER_URL: "http://qwen:8125", QWEN_TTS_CUSTOM_VOICES_ENABLED: "1", TTS_WORKER_TOKEN_FILE: tokenPath },
    fetchImpl: async (...args) => { calls.push(args); return new Response(upstream, { headers: { "content-type": "application/vnd.artifact.pcm" } }); }
  });
  try {
    assert.ok(service.voices.some(({ id }) => id === "qwen_ryan"));
    const result = await service.streamSynthesize("hello", "qwen_ryan", undefined, "  calm audiobook narrator  ");
    assert.equal(result.status, 200);
    const reader = result.stream.getReader();
    assert.deepEqual([...((await reader.read()).value)], [1, 2, 3]);
    assert.equal((await service.streamSynthesize("second", "qwen_ryan")).error, "busy");
    release();
    assert.deepEqual([...((await reader.read()).value)], [4]);
    assert.equal((await reader.read()).done, true);
    result.release();
    assert.match(calls[0][0], /\/speech\/stream$/);
    assert.equal(JSON.parse(calls[0][1].body).instructions, "calm audiobook narrator");
  } finally { release?.(); await rm(dir, { recursive: true, force: true }); }
});

test("Pocket stream routes preset voices through its own worker lane", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-pocket-stream-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "shared-token\n");
  const calls = [];
  const service = createSpeechService({
    env: { POCKET_TTS_WORKER_URL: "http://pocket:8790", TTS_WORKER_TOKEN_FILE: tokenPath },
    fetchImpl: async (...args) => {
      calls.push(args);
      return new Response(new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array([1, 2, 3])); controller.close(); } }), { headers: { "content-type": "application/vnd.artifact.pcm" } });
    }
  });
  try {
    assert.equal(service.voices.find(({ id }) => id === "pocket_alba").streaming, true);
    const result = await service.streamSynthesize("hello", "pocket_alba");
    assert.equal(result.status, 200);
    assert.deepEqual([...((await result.stream.getReader().read()).value)], [1, 2, 3]);
    result.release();
    assert.match(calls[0][0], /^http:\/\/pocket:8790\/speech\/stream$/);
    assert.equal(JSON.parse(calls[0][1].body).voice, "pocket_alba");
    assert.equal((await service.streamSynthesize("hello", "pocket_alba", undefined, "calm")).error, "bad_instructions");
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("speech style instructions are bounded and limited to Qwen custom voices", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-instructions-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "shared-token\n");
  const calls = [];
  const service = createSpeechService({
    env: { TTS_WORKER_URL: "http://kokoro:8788", QWEN_TTS_WORKER_URL: "http://qwen:8125", QWEN_TTS_CUSTOM_VOICES_ENABLED: "1", TTS_WORKER_TOKEN_FILE: tokenPath },
    fetchImpl: async (...args) => { calls.push(args); return new Response(new Uint8Array([82, 73, 70, 70]), { headers: { "content-type": "audio/wav" } }); }
  });
  try {
    assert.equal((await service.synthesize("hello", "af_heart", "warm")).error, "bad_instructions");
    assert.equal((await service.synthesize("hello", "qwen_ryan", null)).error, "bad_instructions");
    assert.equal((await service.synthesize("hello", "qwen_ryan", "x".repeat(501))).error, "bad_instructions");
    assert.equal((await service.synthesize("hello", "qwen_ryan", "line\rbreak")).status, 200);
    assert.equal((await service.synthesize("hello", "qwen_ryan", "line\u0000break")).error, "bad_instructions");
    assert.equal((await service.synthesize("hello", "qwen_ryan", "😀".repeat(500))).status, 200);
    assert.equal((await service.synthesize("hello", "qwen_ryan", "😀".repeat(501))).error, "bad_instructions");
    assert.equal((await service.synthesize("hello", "qwen_ryan", "   ")).status, 200);
    assert.equal(JSON.parse(calls[2][1].body).instructions, undefined);
    assert.equal((await service.synthesize("hello", "qwen_ryan", "  clear, calm  ")).status, 200);
    assert.equal(JSON.parse(calls[3][1].body).instructions, "clear, calm");
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test("Qwen reference voice can be disabled independently from preset voices", async () => {
  const dir = await mkdtemp(join(tmpdir(), "artifact-speech-qwen-voices-"));
  const tokenPath = join(dir, "token");
  await writeFile(tokenPath, "shared-token\n");
  const service = createSpeechService({
    env: { QWEN_TTS_WORKER_URL: "http://qwen:8125", QWEN_TTS_CUSTOM_VOICES_ENABLED: "1", QWEN_TTS_REFERENCE_VOICE_ENABLED: "0", TTS_WORKER_TOKEN_FILE: tokenPath },
    fetchImpl: async () => new Response(new Uint8Array([1]), { headers: { "content-type": "audio/wav" } })
  });
  try {
    assert.deepEqual(service.voices.map(({ id }) => id), ["qwen_ryan", "qwen_aiden"]);
    assert.equal((await service.synthesize("hello", "qwen_reference")).error, "bad_voice");
  } finally { await rm(dir, { recursive: true, force: true }); }
});
