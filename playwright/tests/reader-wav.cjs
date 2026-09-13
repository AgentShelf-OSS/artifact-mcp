const { spawn } = require('node:child_process');
const fs = require('node:fs/promises');
const { randomBytes } = require('node:crypto');
const assert = require('node:assert/strict');
const { chromium } = require('playwright');

const PORT = Number(process.env.WAV_TEST_PORT || 3505);
const BASE = `http://127.0.0.1:${PORT}`;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const rpcBody = r => r.result?.structuredContent || JSON.parse(r.result.content.find(x => x.type === 'text').text);

function wav25s() {
  const sampleRate = 24000, samples = sampleRate * 25, bytes = new Uint8Array(44 + samples * 2);
  const view = new DataView(bytes.buffer), put = (at, text) => [...text].forEach((c, i) => view.setUint8(at + i, c.charCodeAt(0)));
  put(0, 'RIFF'); view.setUint32(4, bytes.length - 8, true); put(8, 'WAVE'); put(12, 'fmt ');
  view.setUint32(16, 16, true); view.setUint16(20, 1, true); view.setUint16(22, 1, true);
  view.setUint32(24, sampleRate, true); view.setUint32(28, sampleRate * 2, true);
  view.setUint16(32, 2, true); view.setUint16(34, 16, true); put(36, 'data'); view.setUint32(40, samples * 2, true);
  for (let i = 0; i < samples; i++) view.setInt16(44 + i * 2, Math.round(Math.sin(i / 12) * 1000), true);
  return bytes.buffer;
}

(async () => {
  const dir = await fs.mkdtemp('/tmp/artifact-wav-qa-');
  const key = randomBytes(32).toString('hex');
  const env = { ...process.env, PORT: String(PORT), LISTEN_HOST: '127.0.0.1', PUBLIC_BASE_URL: BASE,
    DATA_DIR: dir, TRUST_ACCESS_HEADERS: '1', REQUIRE_ACCESS_JWT: '0', CF_ACCESS_AUD: '', CF_ACCESS_TEAM_DOMAIN: '',
    AUDIT_LEDGER_HMAC_KEY: randomBytes(32).toString('base64'), ARTIFACT_API_KEYS: `e2e:homelab:${key}`, TTS_ENABLED: '1', TTS_ARTIFACT_IDS: '' };
  const server = spawn(process.env.WAV_TEST_BINARY || require('node:path').resolve(__dirname, '../../target/release/artifact-mcp'), { env, stdio: ['ignore', 'ignore', 'pipe'] });
  let serverError = ''; let serverExit = null; server.stderr.on('data', chunk => { serverError += String(chunk); }); server.on('exit', (code, signal) => { serverExit = { code, signal }; });
  let browser;
  try {
    await new Promise((resolve, reject) => {
      const timer = setInterval(async () => { try { if ((await fetch(BASE + '/health')).ok) { clearInterval(timer); resolve(); } } catch (_) {} }, 100);
      setTimeout(() => { clearInterval(timer); reject(new Error('server startup timeout exit=' + JSON.stringify(serverExit) + ': ' + serverError.slice(-1200))); }, 10000);
    });
    const response = await fetch(BASE + '/mcp', { method: 'POST', headers: { authorization: 'Bearer ' + key, 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'publish_artifact', arguments: { title: 'WAV resume QA', html: '<!doctype html><html><body><main><p>Twenty five seconds of synthetic narration.</p></main></body></html>' } } }) });
    const id = rpcBody(await response.json()).id; assert.ok(id);
    const wav = wav25s();
    browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'] });
    const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: { 'cf-access-authenticated-user-email': 'test@homelab' } });
    await context.addInitScript(({ wavBase64 }) => {
      if (window.top !== window) return;
      const wavBytes = Uint8Array.from(atob(wavBase64), c => c.charCodeAt(0)).buffer;
      localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice: 'bm_george', rate: '1' }));
      window.__wav = { speechCalls: 0, wavLength: wavBytes.byteLength };
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const url = String(typeof input === 'string' ? input : input.url);
        if (url.endsWith('/speech/voices')) return new Response(JSON.stringify({ enabled: true, maxChars: 1500, voices: [{ id: 'bm_george', name: 'George · Kokoro', provider: 'Kokoro' }] }), { headers: { 'content-type': 'application/json' } });
        if (url.endsWith('/speech')) { window.__wav.speechCalls++; return new Response(wavBytes, { headers: { 'content-type': 'audio/wav' } }); }
        return nativeFetch(input, init);
      };
    }, { wavBase64: Buffer.from(wav).toString('base64') });
    const page = await context.newPage();
    const errors = []; page.on('pageerror', error => errors.push(error.message));
    await page.goto(`${BASE}/${id}`, { waitUntil: 'domcontentloaded' });
    await page.locator('#vreader-toggle').waitFor({ state: 'visible' }); await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await wait(1000); await page.locator('#vreader-play').click();
    await wait(2000);
    const initialState = await page.evaluate(() => ({ readyState: document.querySelector('#vreader-audio')?.readyState, status: document.querySelector('#vreader-status')?.textContent, speechCalls: window.__wav?.speechCalls, wavLength: window.__wav?.wavLength, src: document.querySelector('#vreader-audio')?.getAttribute('src') }));
    console.error('initial state', initialState);
    if (initialState.readyState < 1) { console.error('play button', await page.locator('#vreader-play').isDisabled()); await page.locator('#vreader-play').click(); }
    await wait(1500);
    const loadedState = await page.evaluate(() => { const a = document.querySelector('#vreader-audio'); return { readyState: a?.readyState, status: document.querySelector('#vreader-status')?.textContent, src: a?.src, error: a?.error && { code: a.error.code, message: a.error.message }, disabled: document.querySelector('#vreader-play')?.disabled }; });
    console.error('loaded state', loadedState);
    assert.ok(loadedState.readyState >= 1, 'synthetic WAV was not loaded');
    assert.ok(initialState.readyState >= 1, 'synthetic WAV was not loaded');
    await page.waitForFunction(() => document.querySelector('#vreader-status')?.textContent?.includes('Reading'), null, { timeout: 10000 });
    await page.evaluate(() => { const audio = document.querySelector('#vreader-audio'); audio.currentTime = 18; });
    await page.locator('#vreader-play').click();
    await wait(100);
    const keyName = await page.evaluate(() => Object.keys(localStorage).find(key => key.startsWith('artifact-reader-place:')));
    const saved18 = await page.evaluate(key => JSON.parse(localStorage.getItem(key)), keyName);
    assert.ok(saved18 && Math.abs(saved18.offset - 18) < 0.4, `pause checkpoint offset was ${saved18?.offset}`);
    await page.locator('#vreader-rewind').click(); await wait(100);
    const rewound = await page.evaluate(() => document.querySelector('#vreader-audio').currentTime);
    const saved3 = await page.evaluate(key => JSON.parse(localStorage.getItem(key)), keyName);
    assert.ok(Math.abs(rewound - 3) < 0.4, `rewind currentTime was ${rewound}`);
    assert.ok(saved3 && Math.abs(saved3.offset - 3) < 0.4, `rewind checkpoint offset was ${saved3?.offset}`);
    await page.reload({ waitUntil: 'domcontentloaded' }); await page.locator('#vreader-toggle').waitFor({ state: 'visible' }); await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await page.locator('#vreader-resume').waitFor({ state: 'visible' }); await page.locator('#vreader-resume').click();
    await page.waitForFunction(() => document.querySelector('#vreader-audio')?.readyState >= 1 && document.querySelector('#vreader-status')?.textContent?.includes('Reading'), null, { timeout: 10000 });
    const resumed = await page.evaluate(() => document.querySelector('#vreader-audio').currentTime);
    assert.ok(Math.abs(resumed - 3) < 0.6, `reload resume currentTime was ${resumed}`);
    await page.locator('#vreader-play').click(); await page.waitForFunction(() => document.querySelector('#vreader-audio').paused && document.querySelector('#vreader-status')?.textContent.startsWith('Paused'), null, { timeout: 3000 });
    await page.evaluate(() => { document.querySelector('#vreader-audio').currentTime = 24.95; });
    await page.locator('#vreader-play').click();
    await page.waitForFunction(() => document.querySelector('#vreader-status')?.textContent === 'Finished', null, { timeout: 10000 });
    assert.equal(await page.evaluate(key => localStorage.getItem(key), keyName), null, 'finished playback left a checkpoint');
    assert.deepEqual(errors, [], errors.join('; '));
    console.log(JSON.stringify({ speechCalls: await page.evaluate(() => window.__wav.speechCalls), saved18: saved18.offset, rewound, saved3: saved3.offset, resumed, finished: true, errors }, null, 2));
  } finally { if (browser) await browser.close(); server.kill('SIGTERM'); await fs.rm(dir, { recursive: true, force: true }); }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
