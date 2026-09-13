// SPDX-License-Identifier: Apache-2.0
// Optional authenticated speech sidecar client. The bearer token is never returned to callers.

import fs from "node:fs";

export const SPEECH_MAX_CHARS = 1500;
export const SPEECH_MAX_INSTRUCTIONS_CHARS = 500;
export const SPEECH_MAX_AUDIO_BYTES = 4 * 1024 * 1024;
export const SPEECH_MAX_STREAM_BYTES = 4_100_000;
export const SPEECH_VOICES = Object.freeze([
  Object.freeze({ id: "bm_george", name: "George · Kokoro (British)" }),
  Object.freeze({ id: "bf_emma", name: "Emma · Kokoro (British)" }),
  Object.freeze({ id: "af_heart", name: "Heart · Kokoro (American)" })
]);
export const POCKET_SPEECH_VOICES = Object.freeze([
  ["alba", "Alba"],
  ["marius", "Marius"],
  ["javert", "Javert"],
  ["jean", "Jean"],
  ["cosette", "Cosette"],
  ["eponine", "Eponine"],
  ["fantine", "Fantine"],
  ["azelma", "Azelma"],
  ["anna", "Anna"],
  ["bill_boerst", "Bill Boerst"],
  ["caro_davy", "Caro Davy"],
  ["charles", "Charles"],
  ["eve", "Eve"],
  ["george", "George"],
  ["jane", "Jane"],
  ["mary", "Mary"],
  ["michael", "Michael"],
  ["paul", "Paul"],
  ["peter_yearsley", "Peter Yearsley"],
  ["stuart_bell", "Stuart Bell"],
  ["vera", "Vera"]
].map(([id, name]) => Object.freeze({ id: `pocket_${id}`, name: `${name} · Pocket`, streaming: true })));
export const RAVEN_SPEECH_VOICES = Object.freeze([
  Object.freeze({ id: "raven_alba", name: "Alba · RAVEN (trial)", streaming: false }),
  Object.freeze({ id: "raven_marius", name: "Marius · RAVEN (trial)", streaming: false })
]);
export const QWEN_SPEECH_VOICES = Object.freeze([
  Object.freeze({ id: "qwen_reference", name: "Reference voice · Qwen", streaming: true }),
  Object.freeze({ id: "qwen_ryan", name: "Ryan · Qwen", streaming: true }),
  Object.freeze({ id: "qwen_aiden", name: "Aiden · Qwen", streaming: true })
]);
export const MOSS_SPEECH_VOICES = Object.freeze([
  Object.freeze({ id: "moss_trump", name: "Trump · MOSS-TTS Nano" }),
  Object.freeze({ id: "moss_ava", name: "Ava · MOSS-TTS Nano" }),
  Object.freeze({ id: "moss_bella", name: "Bella · MOSS-TTS Nano" }),
  Object.freeze({ id: "moss_adam", name: "Adam · MOSS-TTS Nano" }),
  Object.freeze({ id: "moss_nathan", name: "Nathan · MOSS-TTS Nano" })
]);

export function speechEnabledFor(id, { enabled = process.env.TTS_ENABLED === "1", allowlist = process.env.TTS_ARTIFACT_IDS || "" } = {}) {
  const ids = new Set(String(allowlist).split(",").map((value) => value.trim()).filter(Boolean));
  return (enabled || ids.size > 0) && (ids.size === 0 || ids.has(id));
}

function validText(text) {
  return typeof text === "string" && text.trim().length > 0 && Array.from(text).length <= SPEECH_MAX_CHARS && !/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/u.test(text);
}

function normalizeInstructions(value, voice) {
  if (value === undefined) return { ok: true, value: undefined };
  if (typeof value !== "string") return { ok: false, error: "bad_instructions" };
  const normalized = value.trim();
  if (!normalized) return { ok: true, value: undefined };
  if (!voice.startsWith("qwen_") || !["qwen_ryan", "qwen_aiden"].includes(voice)) return { ok: false, error: "bad_instructions" };
  if (Array.from(normalized).length > SPEECH_MAX_INSTRUCTIONS_CHARS || /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/u.test(normalized)) return { ok: false, error: "bad_instructions" };
  return { ok: true, value: normalized };
}

export function createSpeechService({ fetchImpl = globalThis.fetch, env = process.env } = {}) {
  const clients = new Map();
  const addClient = (prefix, rawValue, tokenValue) => {
    const raw = String(rawValue || "").replace(/\/+$/, "");
    if (!raw || typeof fetchImpl !== "function") return;
    try {
      const url = new URL(`${raw}/`);
      if (!/^https?:$/.test(url.protocol) || !url.hostname) return;
      const tokenPath = String(tokenValue || "");
      const token = fs.readFileSync(tokenPath, "utf8").trim();
    if (token) clients.set(prefix, { prefix, endpoint: new URL("speech", url).toString(), streamEndpoint: new URL("speech/stream", url).toString(), timedStreamEndpoint: new URL("speech/stream-timed", url).toString(), token, busy: false });
    } catch {}
  };
  const fallbackToken = env.TTS_WORKER_TOKEN_FILE;
  addClient("kokoro", env.TTS_WORKER_URL, fallbackToken);
  addClient("pocket", env.POCKET_TTS_WORKER_URL, env.POCKET_TTS_WORKER_TOKEN_FILE || fallbackToken);
  addClient("raven", env.RAVEN_TTS_WORKER_URL, env.RAVEN_TTS_WORKER_TOKEN_FILE || fallbackToken);
  addClient("qwen", env.QWEN_TTS_WORKER_URL, env.QWEN_TTS_WORKER_TOKEN_FILE || fallbackToken);
  addClient("moss", env.MOSS_TTS_WORKER_URL, env.MOSS_TTS_WORKER_TOKEN_FILE || fallbackToken);
  if (!clients.size) return null;
  const voices = Object.freeze([
    ...(clients.has("kokoro") ? SPEECH_VOICES : []),
    ...(clients.has("pocket") ? POCKET_SPEECH_VOICES : []),
    ...(clients.has("raven") ? RAVEN_SPEECH_VOICES : []),
    ...(clients.has("qwen") ? QWEN_SPEECH_VOICES.filter(({ id }) => (id === "qwen_reference" && env.QWEN_TTS_REFERENCE_VOICE_ENABLED !== "0") || (id !== "qwen_reference" && env.QWEN_TTS_CUSTOM_VOICES_ENABLED === "1")) : []),
    ...(clients.has("moss") ? MOSS_SPEECH_VOICES : [])
  ]);
  return {
    voices,
    async synthesize(text, voice, instructions) {
      const voiceId = String(voice || "");
      const prefix = voiceId.startsWith("pocket_") ? "pocket" : voiceId.startsWith("raven_") ? "raven" : voiceId.startsWith("qwen_") ? "qwen" : voiceId.startsWith("moss_") ? "moss" : "kokoro";
      const client = clients.get(prefix);
      const validVoices = prefix === "pocket" ? POCKET_SPEECH_VOICES : prefix === "raven" ? RAVEN_SPEECH_VOICES : prefix === "qwen" ? QWEN_SPEECH_VOICES : prefix === "moss" ? MOSS_SPEECH_VOICES : SPEECH_VOICES;
      if (prefix === "qwen" && ((voiceId === "qwen_reference" && env.QWEN_TTS_REFERENCE_VOICE_ENABLED === "0") || (voiceId !== "qwen_reference" && env.QWEN_TTS_CUSTOM_VOICES_ENABLED !== "1"))) return { status: 400, error: "bad_voice" };
      if (!client || !validVoices.some((item) => item.id === voice)) return { status: 400, error: "bad_voice" };
      if (client.busy) return { status: 429, error: "busy" };
      if (!validText(text)) return { status: 400, error: "bad_text" };
      const style = normalizeInstructions(instructions, voice);
      if (!style.ok) return { status: 400, error: style.error };
      client.busy = true;
      try {
        const payload = { text, voice };
        if (style.value) payload.instructions = style.value;
        const response = await fetchImpl(client.endpoint, { method: "POST", redirect: "error", headers: { authorization: `Bearer ${client.token}`, "content-type": "application/json" }, body: JSON.stringify(payload), signal: AbortSignal.timeout(60_000) });
        if (response.status === 429) return { status: 429, error: "busy" };
        const contentType = String(response.headers.get("content-type") || "").toLowerCase();
        if (!response.ok || !contentType.startsWith("audio/wav")) return { status: 503, error: "speech unavailable" };
        if (response.headers.get("content-length") && Number(response.headers.get("content-length")) > SPEECH_MAX_AUDIO_BYTES) return { status: 503, error: "speech unavailable" };
        const reader = response.body?.getReader();
        if (!reader) return { status: 503, error: "speech unavailable" };
        const chunks = [];
        let total = 0;
        while (true) {
          const chunk = await reader.read();
          if (chunk.done) break;
          total += chunk.value.byteLength;
          if (total > SPEECH_MAX_AUDIO_BYTES) { await reader.cancel(); return { status: 503, error: "speech unavailable" }; }
          chunks.push(Buffer.from(chunk.value));
        }
        const audio = Buffer.concat(chunks, total);
        return { status: 200, audio };
      } catch {
        return { status: 503, error: "speech unavailable" };
      } finally {
        client.busy = false;
      }
    },
    async streamSynthesize(text, voice, signal, instructions) {
      const voiceId = String(voice || "");
      const prefix = voiceId.startsWith("qwen_") ? "qwen" : voiceId.startsWith("pocket_") ? "pocket" : voiceId.startsWith("raven_") ? "raven" : "";
      const voiceList = prefix === "qwen" ? QWEN_SPEECH_VOICES : prefix === "pocket" ? POCKET_SPEECH_VOICES : prefix === "raven" ? RAVEN_SPEECH_VOICES : [];
      const client = clients.get(prefix);
      if (!client || !voiceList.some((item) => item.id === voiceId)) return { status: 400, error: "bad_voice" };
      if (prefix === "qwen" && ((voiceId === "qwen_reference" && env.QWEN_TTS_REFERENCE_VOICE_ENABLED === "0") || (voiceId !== "qwen_reference" && env.QWEN_TTS_CUSTOM_VOICES_ENABLED !== "1"))) return { status: 400, error: "bad_voice" };
      if (!validText(text)) return { status: 400, error: "bad_text" };
      const style = normalizeInstructions(instructions, voice);
      if (!style.ok) return { status: 400, error: style.error };
      if (client.busy) return { status: 429, error: "busy" };
      client.busy = true;
      let released = false;
      const release = () => { if (!released) { released = true; client.busy = false; } };
      try {
        const timeout = AbortSignal.timeout(60_000);
        const requestSignal = signal ? AbortSignal.any([signal, timeout]) : timeout;
        const payload = { text, voice };
        if (style.value) payload.instructions = style.value;
        const response = await fetchImpl(client.streamEndpoint, { method: "POST", redirect: "error", headers: { authorization: `Bearer ${client.token}`, "content-type": "application/json" }, body: JSON.stringify(payload), signal: requestSignal });
        if (response.status === 429) { release(); return { status: 429, error: "busy" }; }
        const contentType = String(response.headers.get("content-type") || "").toLowerCase();
        if (!response.ok || !contentType.startsWith("application/vnd.artifact.pcm")) { release(); return { status: 503, error: "speech unavailable" }; }
        if (response.headers.get("content-length") && Number(response.headers.get("content-length")) > SPEECH_MAX_AUDIO_BYTES) { release(); return { status: 503, error: "speech unavailable" }; }
        if (!response.body) { release(); return { status: 503, error: "speech unavailable" }; }
        let total = 0;
        const bounded = response.body.pipeThrough(new TransformStream({
          transform(chunk, controller) {
            total += chunk.byteLength;
            if (total > SPEECH_MAX_STREAM_BYTES) controller.error(new Error("stream too large"));
            else controller.enqueue(chunk);
          }
        }));
        return { status: 200, stream: bounded, release };
      } catch {
        release();
        return { status: 503, error: "speech unavailable" };
      }
      // The stream consumer owns client.busy until the body has ended or is cancelled.
    },
    async streamTimedSynthesize(text, voice, signal, instructions) {
      const voiceId = String(voice || "");
      const client = clients.get("pocket");
      if (!client || !POCKET_SPEECH_VOICES.some((item) => item.id === voiceId)) return { status: 400, error: "bad_voice" };
      if (!validText(text)) return { status: 400, error: "bad_text" };
      const style = normalizeInstructions(instructions, voiceId);
      if (!style.ok) return { status: 400, error: style.error };
      if (client.busy) return { status: 429, error: "busy" };
      client.busy = true;
      let released = false;
      const release = () => { if (!released) { released = true; client.busy = false; } };
      try {
        const timeout = AbortSignal.timeout(60_000);
        const requestSignal = signal ? AbortSignal.any([signal, timeout]) : timeout;
        const payload = { text, voice: voiceId };
        if (style.value) payload.instructions = style.value;
        const response = await fetchImpl(client.timedStreamEndpoint, { method: "POST", redirect: "error", headers: { authorization: `Bearer ${client.token}`, "content-type": "application/json" }, body: JSON.stringify(payload), signal: requestSignal });
        if (response.status === 429) { release(); return { status: 429, error: "busy" }; }
        if (response.status === 404 || response.status === 415) { release(); return { status: 415, error: "speech timed unavailable" }; }
        const contentType = String(response.headers.get("content-type") || "").toLowerCase();
        if (!response.ok || contentType.split(";", 1)[0].trim() !== "application/vnd.artifact.pcm-timed") { release(); return { status: 503, error: "speech unavailable" }; }
        if (response.headers.get("content-length") && Number(response.headers.get("content-length")) > SPEECH_MAX_STREAM_BYTES) { release(); return { status: 503, error: "speech unavailable" }; }
        if (!response.body) { release(); return { status: 503, error: "speech unavailable" }; }
        let total = 0;
        const bounded = response.body.pipeThrough(new TransformStream({
          transform(chunk, controller) {
            total += chunk.byteLength;
            if (total > SPEECH_MAX_STREAM_BYTES) controller.error(new Error("stream too large"));
            else controller.enqueue(chunk);
          }
        }));
        return { status: 200, stream: bounded, release };
      } catch {
        release();
        return { status: 503, error: "speech unavailable" };
      }
    }
  };
}
