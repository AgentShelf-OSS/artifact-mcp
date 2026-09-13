const { spawn } = require('node:child_process');
const fs = require('node:fs/promises');
const { randomBytes } = require('node:crypto');
const assert = require('node:assert/strict');
const { chromium } = require('playwright');

const PORT = Number(process.env.STREAM_TEST_PORT || 3502);
const BASE = `http://127.0.0.1:${PORT}`;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

(async () => {
  const dir = await fs.mkdtemp('/tmp/artifact-paragraph-gap-');
  const key = randomBytes(32).toString('hex');
  const env = { ...process.env, PORT: String(PORT), LISTEN_HOST: '127.0.0.1', PUBLIC_BASE_URL: BASE, DATA_DIR: dir,
    TRUST_ACCESS_HEADERS: '1', REQUIRE_ACCESS_JWT: '0', CF_ACCESS_AUD: '', CF_ACCESS_TEAM_DOMAIN: '',
    AUDIT_LEDGER_HMAC_KEY: randomBytes(32).toString('base64'), ARTIFACT_API_KEYS: `e2e:homelab:${key}`,
    TTS_ENABLED: '1', TTS_ARTIFACT_IDS: '' };
  const binary = process.env.STREAM_TEST_BINARY || require('node:path').resolve(__dirname, '../../target/release/artifact-mcp');
  const server = spawn(binary, { env, stdio: ['ignore', 'ignore', 'pipe'] });
  let browser;
  try {
    await new Promise((resolve, reject) => {
      const timer = setInterval(async () => { try { if ((await fetch(BASE + '/health')).ok) { clearInterval(timer); resolve(); } } catch (_) {} }, 100);
      setTimeout(() => { clearInterval(timer); reject(new Error('server startup timeout')); }, 10000);
    });
    const html = '<!doctype html><html><body><main><h1>Gap test</h1><p>First paragraph deliberately lasts long enough to reveal network gaps.</p><p>Second paragraph should already be prefetched before the first ends.</p><p>Third paragraph verifies that prefetch stays bounded to one lookahead.</p></main></body></html>';
    const published = await fetch(BASE + '/mcp', { method: 'POST', headers: { authorization: 'Bearer ' + key, 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'publish_artifact', arguments: { title: 'paragraph gap synthetic', html } } }) });
    const rpc = await published.json();
    const body = rpc.result.structuredContent || JSON.parse(rpc.result.content.find(x => x.type === 'text').text);
    assert.ok(body.id);
    browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'] });
    const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: { 'cf-access-authenticated-user-email': 'test@homelab' } });
    await context.addInitScript(() => {
      const nativeFetch = window.fetch.bind(window);
      window.__gap = { requests: [], responses: [], starts: [], cancels: 0 };
      const NativeAudioContext = window.AudioContext || window.webkitAudioContext;
      if (NativeAudioContext) window.AudioContext = class extends NativeAudioContext {
        createBufferSource() {
          const source = super.createBufferSource();
          const start = source.start.bind(source); const stop = source.stop.bind(source);
          const record = { start: null, wall: null, offset: 0, duration: 0, stopped: false };
          window.__gap.starts.push(record);
          source.start = (...args) => { record.start = args[0]; record.wall = performance.now(); record.offset = args[1] || 0; record.rate = source.playbackRate.value; record.duration = source.buffer ? source.buffer.duration : 0; return start(...args); };
          source.stop = (...args) => { record.stopped = true; return stop(...args); };
          return source;
        }
      };
      window.fetch = async (input, init) => {
        const url = String(typeof input === 'string' ? input : input.url);
        if (url.endsWith('/speech/voices')) return new Response(JSON.stringify({ enabled: true, maxChars: 1500, voices: [{ id: 'qwen_reference', name: 'Reference voice · Qwen' }] }), { headers: { 'content-type': 'application/json' } });
        if (!url.endsWith('/speech/stream')) return nativeFetch(input, init);
        const signal = init && init.signal;
        const request = { at: performance.now(), index: window.__gap.requests.length };
        window.__gap.requests.push(request);
        const sampleRate = 24000, samples = sampleRate * 2;
        const pcm = new Uint8Array(samples * 2);
        const frame = new Uint8Array(4 + pcm.length); new DataView(frame.buffer).setUint32(0, pcm.length); frame.set(pcm, 4);
        let sent = false, canceled = false, timer;
        const stream = new ReadableStream({
          start(controller) {
            timer = setTimeout(() => {
              if (canceled) return;
              request.headers = performance.now();
              controller.enqueue(frame); controller.enqueue(new Uint8Array([0, 0, 0, 0])); controller.close();
              window.__gap.responses.push({ index: request.index, at: performance.now() }); sent = true;
            }, 300);
          },
          cancel() { canceled = true; window.__gap.cancels++; clearTimeout(timer); }
        });
        if (signal) signal.addEventListener('abort', () => { request.aborted = true; canceled = true; window.__gap.cancels++; clearTimeout(timer); }, { once: true });
        return new Response(stream, { headers: { 'content-type': 'application/vnd.artifact.pcm' } });
      };
    });
    const page = await context.newPage();
    const errors = []; page.on('pageerror', e => errors.push(e.stack || e.message));
    await page.goto(`${BASE}/${body.id}`, { waitUntil: 'domcontentloaded' });
    await page.locator('#vreader-toggle').waitFor({ state: 'visible' });
    await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await page.locator('#vreader-voice').selectOption('qwen_reference');
    await page.locator('#vreader-play').click();
    await page.waitForFunction(() => document.querySelector('#vreader-status')?.textContent?.includes('Reading'), null, { timeout: 5000 });
    // Let the first 2-second paragraph play through and allow the second transition to be scheduled.
    await sleep(3800);
    const result = await page.evaluate(() => ({ gap: window.__gap, status: document.querySelector('#vreader-status')?.textContent, errors: [], now: performance.now() }));
    result.errors = errors;
    console.log(JSON.stringify(result, null, 2));
    assert.equal(result.gap.requests.length >= 2, true, 'expected second paragraph request');
    const first = result.gap.requests[0], second = result.gap.requests[1];
    const firstEndWall = (result.gap.starts[0]?.wall || Infinity) + ((result.gap.starts[0]?.duration || 0) - (result.gap.starts[0]?.offset || 0)) * 1000 / (result.gap.starts[0]?.rate || 1);
    assert.ok(second.at < firstEndWall, `second request at ${second.at.toFixed(1)}ms was not prefetched before first playback ended at ${firstEndWall.toFixed(1)}ms`);
    if (result.gap.requests[2] && result.gap.starts[1]) assert.ok(result.gap.requests[2].at >= result.gap.starts[1].wall - 20, 'third request began before second paragraph playback started');
    const starts = result.gap.starts.filter(item => !item.stopped && item.start != null);
    assert.ok(starts.length >= 2, 'expected two scheduled paragraph buffers');
    const transitionGap = (starts[1].start - starts[0].start) - ((starts[0].duration - starts[0].offset) / starts[0].rate);
    assert.ok(transitionGap < 0.1, `paragraph transition gap ${transitionGap}s exceeds 100ms`);
    await page.locator('#vreader-stop').click();
    await sleep(50);
    assert.ok(await page.evaluate(() => window.__gap.requests.at(-1)?.aborted === true), 'stop did not abort active/prefetched stream');
  } finally {
    if (browser) await browser.close();
    server.kill('SIGTERM');
    await fs.rm(dir, { recursive: true, force: true });
  }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
