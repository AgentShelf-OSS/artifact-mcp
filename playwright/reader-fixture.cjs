/* SPDX-License-Identifier: Apache-2.0 */
// Shared isolated fixture for the standalone reader regression programs.
// It deliberately mocks only the worker HTTP boundary; application speech routes remain real.
const http = require('node:http');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { randomBytes } = require('node:crypto');
const { spawn } = require('node:child_process');

const { chromium } = require('playwright');

const ROOT = path.resolve(__dirname, '..');
const HEALTH_TIMEOUT_MS = 15_000;
const SHUTDOWN_TIMEOUT_MS = 3_000;
const VIEWER_EMAIL = 'reader-fixture@example.test';
const activeFixtures = new Set();
const activeBrowsers = new Set();
let signalCleanupStarted = false;

async function cleanupOnSignal(signal) {
  if (signalCleanupStarted) return;
  signalCleanupStarted = true;
  await Promise.race([
    Promise.allSettled([
      ...[...activeBrowsers].map(browser => browser.close().catch(() => {})),
      ...[...activeFixtures].map(fixture => fixture.close().catch(() => {})),
    ]),
    new Promise(resolve => setTimeout(resolve, 2500)),
  ]);
  process.exit(128 + (signal === 'SIGINT' ? 2 : 15));
}

process.once('SIGTERM', () => { void cleanupOnSignal('SIGTERM'); });
process.once('SIGINT', () => { void cleanupOnSignal('SIGINT'); });

function fetchBounded(input, init = {}, timeoutMs = 10_000) {
  return fetch(input, { ...init, signal: AbortSignal.timeout(timeoutMs) });
}

function requiredRuntime() {
  const runtime = String(process.env.READER_RUNTIME || '').trim().toLowerCase();
  if (runtime !== 'node' && runtime !== 'rust') {
    throw new Error('READER_RUNTIME must be node or rust');
  }
  return runtime;
}

function requiredRoot() {
  const root = String(process.env.READER_TEST_ROOT || '').trim();
  if (!root) throw new Error('READER_TEST_ROOT is required');
  return path.resolve(root);
}

function randomKey(bytes = 32) {
  return randomBytes(bytes).toString('hex');
}

async function freePort() {
  return new Promise((resolve, reject) => {
    const server = http.createServer();
    server.once('error', reject);
    server.listen({ host: '127.0.0.1', port: 0 }, () => {
      const address = server.address();
      const port = typeof address === 'object' && address ? address.port : 0;
      server.close(error => error ? reject(error) : resolve(port));
    });
  });
}

function wavBytes(text, durationSeconds) {
  const sampleRate = 24_000;
  const seconds = Math.max(0.05, Math.min(30, Number(durationSeconds) || 1));
  const samples = Math.floor(sampleRate * seconds);
  const bytes = Buffer.alloc(44 + samples * 2);
  const write = (offset, value) => bytes.write(value, offset, 'ascii');
  write(0, 'RIFF'); bytes.writeUInt32LE(bytes.length - 8, 4); write(8, 'WAVE');
  write(12, 'fmt '); bytes.writeUInt32LE(16, 16); bytes.writeUInt16LE(1, 20);
  bytes.writeUInt16LE(1, 22); bytes.writeUInt32LE(sampleRate, 24);
  bytes.writeUInt32LE(sampleRate * 2, 28); bytes.writeUInt16LE(2, 32);
  bytes.writeUInt16LE(16, 34); write(36, 'data'); bytes.writeUInt32LE(samples * 2, 40);
  const seed = [...String(text || '')].reduce((sum, char) => sum + char.codePointAt(0), 0);
  for (let index = 0; index < samples; index += 1) {
    const value = Math.round(Math.sin((index + seed) / 12) * 1000);
    bytes.writeInt16LE(value, 44 + index * 2);
  }
  return bytes;
}

function pcmFrame(payload) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const frame = Buffer.alloc(4 + body.length);
  frame.writeUInt32BE(body.length, 0);
  body.copy(frame, 4);
  return frame;
}

function timedFrame(type, payload) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const frame = Buffer.alloc(5 + body.length);
  frame.writeUInt32BE(body.length + 1, 0);
  frame[4] = type;
  body.copy(frame, 5);
  return frame;
}

function pcmBody(durationSeconds) {
  const seconds = Math.max(0.05, Math.min(30, Number(durationSeconds) || 1));
  let remaining = Math.floor(seconds * 24_000) * 2;
  const frames = [];
  // One frame for a two-second prefetch fixture makes each scheduled buffer a paragraph.
  // Longer control fixtures remain below the application's 192 KiB frame limit.
  while (remaining > 0) {
    const bytes = Math.min(remaining, 96_000);
    frames.push(pcmFrame(Buffer.alloc(bytes)));
    remaining -= bytes;
  }
  return Buffer.concat([...frames, Buffer.alloc(4)]);
}

function makeTimedBody(text, durationSeconds) {
  const words = String(text || '').match(/[\p{L}\p{N}]+/gu) || [];
  const duration = Math.max(0.2, Number(durationSeconds) || 1);
  const parts = [];
  words.forEach((word, index) => {
    const start = index * 0.3;
    const end = start + 0.3;
    parts.push(timedFrame(2, JSON.stringify({ word, index, start, end })));
  });
  const audioFrames = Math.max(1, Math.ceil((duration * 48_000) / 24_000));
  for (let index = 0; index < audioFrames; index += 1) parts.push(timedFrame(1, Buffer.alloc(24_000)));
  parts.push(Buffer.alloc(4));
  return Buffer.concat(parts);
}

function createWorker() {
  let options = { durationSeconds: 1, latencyMs: 0 };
  const requests = [];
  const token = randomKey(24);
  const timers = new Set();
  const worker = http.createServer((request, response) => {
    const url = new URL(request.url || '/', 'http://127.0.0.1');
    if (request.headers.authorization !== `Bearer ${token}`) {
      response.writeHead(401).end();
      return;
    }
    if (request.method !== 'POST' || !/^\/speech(?:\/stream(?:-timed)?)?$/.test(url.pathname)) {
      response.writeHead(404).end();
      return;
    }
    const chunks = [];
    request.on('data', chunk => chunks.push(chunk));
    request.on('error', () => response.destroy());
    request.on('end', () => {
      let body;
      try { body = JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch {
        response.writeHead(400).end();
        return;
      }
      const record = { path: url.pathname, body, at: Date.now() };
      requests.push(record);
      const delay = Math.max(0, Number(options.latencyMs) || 0);
      const requestOptions = { ...options };
      const timer = setTimeout(() => {
        timers.delete(timer);
        if (response.destroyed) return;
        if (url.pathname === '/speech') {
          const audio = wavBytes(body.text, requestOptions.durationSeconds);
          response.writeHead(200, { 'content-type': 'audio/wav', 'content-length': audio.length });
          response.end(audio);
          return;
        }
        const payload = url.pathname.endsWith('-timed')
          ? makeTimedBody(body.text, requestOptions.durationSeconds)
          : pcmBody(requestOptions.durationSeconds);
        response.writeHead(200, {
          'content-type': url.pathname.endsWith('-timed')
            ? 'application/vnd.artifact.pcm-timed;v=1'
            : 'application/vnd.artifact.pcm',
          'content-length': payload.length,
        });
        response.end(payload);
      }, delay);
      timers.add(timer);
      timer.unref?.();
      response.once('close', () => { clearTimeout(timer); timers.delete(timer); });
    });
  });
  worker.once('close', () => { for (const timer of timers) clearTimeout(timer); timers.clear(); });
  return {
    server: worker,
    token,
    requests,
    configure(next = {}) {
      if (next.durationSeconds !== undefined) options.durationSeconds = Number(next.durationSeconds);
      if (next.latencyMs !== undefined) options.latencyMs = Number(next.latencyMs);
    },
    reset() { requests.splice(0, requests.length); },
  };
}

function baseEnvironment({ runtime, dataDir, port, base, apiKey, workerUrl, tokenFile }) {
  const env = {
    PATH: process.env.PATH || '/usr/local/bin:/usr/bin:/bin',
    HOME: process.env.HOME || os.tmpdir(),
    LANG: process.env.LANG || 'C.UTF-8',
    NODE_ENV: 'test',
    PORT: String(port), LISTEN_HOST: '127.0.0.1', PUBLIC_BASE_URL: base,
    DATA_DIR: dataDir, TRUST_ACCESS_HEADERS: '1', REQUIRE_ACCESS_JWT: '0',
    CF_ACCESS_AUD: '', CF_ACCESS_TEAM_DOMAIN: '', ADMIN_EMAILS: VIEWER_EMAIL,
    ARTIFACT_API_KEYS: `reader-fixture:homelab:${apiKey}`,
    AUDIT_LEDGER_HMAC_KEY: randomBytes(32).toString('base64'),
    WEBHOOK_ENC_KEY: randomBytes(32).toString('base64'),
    TTS_ENABLED: '1', TTS_ARTIFACT_IDS: '', PREVIEW_RENDERER_URL: '',
    TTS_WORKER_URL: workerUrl, TTS_WORKER_TOKEN_FILE: tokenFile,
    POCKET_TTS_WORKER_URL: workerUrl, POCKET_TTS_WORKER_TOKEN_FILE: tokenFile,
    QWEN_TTS_WORKER_URL: workerUrl, QWEN_TTS_WORKER_TOKEN_FILE: tokenFile,
    QWEN_TTS_REFERENCE_VOICE_ENABLED: '1', QWEN_TTS_CUSTOM_VOICES_ENABLED: '1',
    RAVEN_TTS_WORKER_URL: workerUrl, RAVEN_TTS_WORKER_TOKEN_FILE: tokenFile,
    MOSS_TTS_WORKER_URL: workerUrl, MOSS_TTS_WORKER_TOKEN_FILE: tokenFile,
  };
  if (runtime === 'rust') {
    env.INGRESS_READS_PER_WINDOW = '1000';
    env.INGRESS_MUTATIONS_PER_WINDOW = '1000';
    env.INGRESS_MCP_PER_WINDOW = '1000';
    env.INGRESS_UPLOADS_PER_WINDOW = '1000';
  }
  return env;
}

function waitForExit(child, timeout = SHUTDOWN_TIMEOUT_MS) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise(resolve => {
    const timer = setTimeout(resolve, timeout);
    child.once('exit', () => { clearTimeout(timer); resolve(); });
  });
}

async function stopProcess(child) {
  if (!child || child.exitCode !== null) return;
  const hasPid = Number.isInteger(child.pid) && child.pid > 0;
  try { if (hasPid) process.kill(-child.pid, 'SIGTERM'); else child.kill('SIGTERM'); } catch { try { child.kill('SIGTERM'); } catch {} }
  await waitForExit(child);
  if (child.exitCode === null) {
    try { if (hasPid) process.kill(-child.pid, 'SIGKILL'); else child.kill('SIGKILL'); } catch { try { child.kill('SIGKILL'); } catch {} }
    await waitForExit(child, 1000);
  }
}

async function startWorker(worker) {
  await new Promise((resolve, reject) => {
    worker.server.once('error', reject);
    worker.server.listen({ host: '127.0.0.1', port: 0 }, resolve);
  });
  const address = worker.server.address();
  return `http://127.0.0.1:${address.port}`;
}

async function startApp(env, runtime) {
  const command = runtime === 'rust'
    ? (process.env.RUST_ARTIFACT_MCP_BIN || '')
    : process.execPath;
  if (runtime === 'rust' && !command) throw new Error('RUST_ARTIFACT_MCP_BIN is required for Rust fixtures');
  const args = runtime === 'rust' ? [] : [path.join(ROOT, 'server.js')];
  const child = spawn(command, args, { cwd: ROOT, env, detached: true, stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  let spawnError;
  child.stderr.on('data', chunk => { stderr = `${stderr}${chunk}`.slice(-4000); });
  child.once('error', error => { spawnError = error; });
  const deadline = Date.now() + HEALTH_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (spawnError) {
      await stopProcess(child);
      throw new Error(`${runtime} server failed to spawn: ${spawnError.message}`);
    }
    if (child.exitCode !== null) throw new Error(`${runtime} server exited during startup: ${stderr}`);
    try {
      const response = await fetchBounded(`${env.PUBLIC_BASE_URL}/health`, {}, 1500);
      if (response.ok) return child;
    } catch {}
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  await stopProcess(child);
  throw new Error(`${runtime} server startup timed out: ${stderr}`);
}

async function createReaderFixture({ audio = {} } = {}) {
  const runtime = requiredRuntime();
  const root = requiredRoot();
  await fs.mkdir(root, { recursive: true });
  const fixtureDir = await fs.mkdtemp(path.join(root, 'reader-fixture-'));
  const worker = createWorker();
  worker.configure(audio);
  const tokenFile = path.join(fixtureDir, 'worker-token');
  let workerUrl;
  try {
    workerUrl = await startWorker(worker);
    await fs.writeFile(tokenFile, `${worker.token}\n`, { mode: 0o600 });
  } catch (error) {
    worker.server.closeAllConnections?.();
    await new Promise(resolve => worker.server.close(() => resolve()));
    await fs.rm(fixtureDir, { recursive: true, force: true });
    throw error;
  }
  const apiKey = randomKey();
  let child;
  let port = 0;
  let base = '';
  let env;
  let startupError;
  try {
    for (let attempt = 0; attempt < 3 && !child; attempt += 1) {
      port = await freePort();
      base = `http://127.0.0.1:${port}`;
      env = baseEnvironment({ runtime, dataDir: fixtureDir, port, base, apiKey, workerUrl, tokenFile });
      try {
        child = await startApp(env, runtime);
      } catch (error) {
        startupError = error;
      }
    }
  } catch (error) {
    startupError = error;
  }
  if (!child) {
    await new Promise(resolve => worker.server.close(() => resolve()));
    await fs.rm(fixtureDir, { recursive: true, force: true });
    throw startupError || new Error('reader fixture server failed to start');
  }
  const viewerHeaders = {
    'cf-access-authenticated-user-email': VIEWER_EMAIL,
    'x-artifact-mutation': '1',
    'sec-fetch-site': 'same-origin',
  };
  async function publish({ title = 'Reader fixture', html = '<main><p>Reader fixture.</p></main>' } = {}) {
    const response = await fetchBounded(`${base}/mcp`, {
      method: 'POST',
      headers: { authorization: `Bearer ${apiKey}`, 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: {
        name: 'publish_artifact', arguments: { title, html },
      } }),
    });
    if (!response.ok) throw new Error(`publish failed: ${response.status}`);
    const body = await response.json();
    const result = body.result?.structuredContent || JSON.parse(body.result?.content?.find(item => item.type === 'text')?.text || '{}');
    if (!result.id) throw new Error(`publish returned no artifact id: ${JSON.stringify(body)}`);
    return { id: result.id, url: `${base}/${result.id}` };
  }
  async function assertSpeechRoutes(id) {
    const headers = { ...viewerHeaders };
    const voicesResponse = await fetchBounded(`${base}/${id}/speech/voices`, { headers });
    if (!voicesResponse.ok) throw new Error(`speech voices route failed: ${voicesResponse.status}`);
    const voices = await voicesResponse.json();
    if (!voices.enabled || !Array.isArray(voices.voices) || voices.voices.length === 0) throw new Error('speech voices are not enabled');
    const choose = prefix => {
      const voice = voices.voices.find(candidate => candidate.id.startsWith(prefix));
      if (!voice) throw new Error(`speech provider voice ${prefix} is missing`);
      return voice;
    };
    const probes = [
      [`${base}/${id}/speech`, choose('bm_').id, 'audio/wav'],
      [`${base}/${id}/speech/stream`, choose('qwen_').id, 'application/vnd.artifact.pcm'],
      [`${base}/${id}/speech/stream-timed`, choose('pocket_').id, 'application/vnd.artifact.pcm-timed'],
    ];
    for (const [url, voice, expected] of probes) {
      const response = await fetchBounded(url, { method: 'POST', headers: { ...headers, 'content-type': 'application/json' }, body: JSON.stringify({ text: 'deterministic reader probe', voice }) });
      if (!response.ok) throw new Error(`${url} failed: ${response.status}`);
      const contentType = String(response.headers.get('content-type') || '').toLowerCase();
      if (!contentType.startsWith(expected)) throw new Error(`${url} content type ${contentType}`);
      const bytes = Buffer.from(await response.arrayBuffer());
      if (bytes.length < 8) throw new Error(`${url} returned empty audio`);
      if (expected === 'audio/wav' && bytes.subarray(0, 4).toString() !== 'RIFF') throw new Error('invalid WAV fixture');
    }
    const shell = await fetchBounded(`${base}/${id}`, { headers });
    const html = await shell.text();
    if (!shell.ok || !html.includes('vreader-toggle')) throw new Error('reader shell injection is missing');
    const raw = await fetchBounded(`${base}/raw/${id}?reader=1`, { headers });
    const rawHtml = await raw.text();
    if (!raw.ok || !rawHtml.includes('artifact-reader-bridge')) throw new Error('reader bridge injection is missing');
    worker.reset();
  }
  async function close() {
    activeFixtures.delete(fixtureHandle);
    await stopProcess(child);
    await new Promise(resolve => {
      const timer = setTimeout(() => { worker.server.closeAllConnections?.(); worker.server.close(resolve); }, 1000);
      worker.server.close(() => { clearTimeout(timer); resolve(); });
    });
    await fs.rm(fixtureDir, { recursive: true, force: true });
  }
  const fixtureHandle = { base, viewerHeaders, worker, publish, close, assertSpeechRoutes };
  activeFixtures.add(fixtureHandle);
  return fixtureHandle;
}

async function launchChromium(options = {}) {
  process.env.PW_USE_BUNDLED_CHROMIUM = '1';
  const browser = await chromium.launch({ headless: true, timeout: 15_000, args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'], ...options });
  activeBrowsers.add(browser);
  browser.once('disconnected', () => activeBrowsers.delete(browser));
  return browser;
}

module.exports = { createReaderFixture, launchChromium };
