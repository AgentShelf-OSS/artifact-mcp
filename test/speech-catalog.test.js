import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync, utimesSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createSpeechService, parsePocketCatalog } from "../lib/speech.js";

// ADR-0020: the Pocket voice catalog adds voices and picker tags without a release.
const dir = mkdtempSync(join(tmpdir(), "pocket-catalog-"));
test.after(() => rmSync(dir, { recursive: true, force: true }));
const token = join(dir, "token");
writeFileSync(token, "worker-token\n");

function catalog(voices) { return JSON.stringify({ version: 1, voices }); }
function service(catalogPath, fetchImpl = async () => ({ ok: true, status: 200, headers: new Headers({ "content-type": "audio/wav" }), arrayBuffer: async () => new ArrayBuffer(8) })) {
  return createSpeechService({ fetchImpl, env: { POCKET_TTS_WORKER_URL: "http://worker.test", POCKET_TTS_WORKER_TOKEN_FILE: token, POCKET_TTS_VOICE_CATALOG: catalogPath } });
}

test("catalog entries override built-ins and add new voices with tags", () => {
  const path = join(dir, "a.json");
  writeFileSync(path, catalog([
    { id: "alba", label: "Alba", gainDb: 0, gender: "female", source: "pocket-preset" },
    { id: "river_reader", label: "River Reader", gainDb: 3.5, gender: "male", accent: "Irish", style: "storyteller", source: "kyutai-donation" }
  ]));
  const voices = service(path).voices;
  const alba = voices.find(v => v.id === "pocket_alba");
  assert.deepEqual(alba.tags, { gender: "female", source: "pocket-preset" });
  const river = voices.find(v => v.id === "pocket_river_reader");
  assert.deepEqual(river, { id: "pocket_river_reader", name: "River Reader · Pocket", streaming: true, gainDb: 3.5, tags: { gender: "male", accent: "Irish", style: "storyteller", source: "kyutai-donation" } });
  assert.equal(voices.filter(v => v.id === "pocket_alba").length, 1, "override, not duplicate");
});

test("catalog changes are picked up without a restart; a broken file is ignored", () => {
  const path = join(dir, "b.json");
  writeFileSync(path, catalog([{ id: "first", label: "First", source: "custom" }]));
  const speech = service(path);
  assert.ok(speech.voices.some(v => v.id === "pocket_first"));
  writeFileSync(path, catalog([{ id: "second", label: "Second", source: "custom" }]));
  utimesSync(path, new Date(), new Date(Date.now() + 5000));
  assert.ok(speech.voices.some(v => v.id === "pocket_second"));
  assert.ok(!speech.voices.some(v => v.id === "pocket_first"));
  writeFileSync(path, "{ not json");
  utimesSync(path, new Date(), new Date(Date.now() + 10000));
  assert.ok(speech.voices.some(v => v.id === "pocket_alba"), "built-ins remain");
  assert.ok(!speech.voices.some(v => v.id === "pocket_second"));
});

test("catalog voices are accepted for synthesis; unknown ones are not", async () => {
  const path = join(dir, "c.json");
  writeFileSync(path, catalog([{ id: "river_reader", label: "River Reader", source: "kyutai-donation" }]));
  const calls = [];
  const speech = service(path, async (url, init) => { calls.push(JSON.parse(init.body)); return { ok: true, status: 200, headers: new Headers({ "content-type": "audio/wav" }), arrayBuffer: async () => new ArrayBuffer(8) }; });
  const ok = await speech.synthesize("Hello there.", "pocket_river_reader");
  assert.notEqual(ok.error, "bad_voice");
  assert.equal(calls.at(-1).voice, "pocket_river_reader");
  const bad = await speech.synthesize("Hello there.", "pocket_not_listed");
  assert.equal(bad.error, "bad_voice");
});

test("catalog validation rejects bad entries", () => {
  const bad = [
    { id: "Bad-Id", label: "x", source: "custom" },
    { id: "ok", label: "a:b", source: "custom" },
    { id: "ok", label: "x", gainDb: 13, source: "custom" },
    { id: "ok", label: "x", gender: "robot", source: "custom" },
    { id: "ok", label: "x", source: "elsewhere" },
    { id: "ok", label: "x", source: "custom", extra: 1 }
  ];
  for (const entry of bad) assert.throws(() => parsePocketCatalog(catalog([entry])), JSON.stringify(entry));
  assert.throws(() => parsePocketCatalog(JSON.stringify({ version: 2, voices: [] })));
  assert.throws(() => parsePocketCatalog(catalog(Array.from({ length: 401 }, (_, i) => ({ id: `v${i}`, label: "V", source: "custom" })))));
});
