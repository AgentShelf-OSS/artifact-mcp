const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 2, latencyMs: 300 } });
  let browser;
  try {
    const html = '<!doctype html><html><body><main><h1>Gap test</h1><p>First paragraph deliberately lasts long enough to reveal network gaps.</p><p>Second paragraph should already be prefetched before the first ends.</p><p>Third paragraph verifies that prefetch stays bounded to one lookahead.</p></main></body></html>';
    const body = await fixture.publish({ title: 'paragraph gap synthetic', html });
    await fixture.assertSpeechRoutes(body.id);
    browser = await launchChromium();
    const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: fixture.viewerHeaders });
    await context.addInitScript(() => {
      window.__gap = { requests: [], starts: [], cancels: 0 };
      const NativeAudioContext = window.AudioContext || window.webkitAudioContext;
      if (NativeAudioContext) window.AudioContext = class extends NativeAudioContext {
        createBufferSource() {
          const source = super.createBufferSource(); const start = source.start.bind(source); const stop = source.stop.bind(source);
          const record = { start: null, wall: null, offset: 0, duration: 0, stopped: false }; window.__gap.starts.push(record);
          source.start = (...args) => { record.start = args[0]; record.wall = performance.now(); record.offset = args[1] || 0; record.rate = source.playbackRate.value; record.duration = source.buffer ? source.buffer.duration : 0; return start(...args); };
          source.stop = (...args) => { record.stopped = true; return stop(...args); }; return source;
        }
      };
      const nativeFetch = window.fetch.bind(window); window.fetch = async (input, init) => {
        const url = String(typeof input === 'string' ? input : input.url);
        let request;
        if (url.endsWith('/speech/stream')) {
          request = { at: performance.now(), index: window.__gap.requests.length };
          window.__gap.requests.push(request);
          init?.signal?.addEventListener('abort', () => { request.aborted = true; window.__gap.cancels++; }, { once: true });
        }
        try { return await nativeFetch(input, init); }
        catch (error) { if (request && error.name === 'AbortError') request.fetchAborted = true; throw error; }
      };
    });
    const page = await context.newPage();
    const errors = []; page.on('pageerror', e => errors.push(e.stack || e.message));
    await page.goto(`${fixture.base}/${body.id}`, { waitUntil: 'domcontentloaded' });
    await page.locator('#vreader-toggle').waitFor({ state: 'visible' });
    await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await page.locator('#vreader-voice').selectOption('qwen_reference');
    await page.locator('#vreader-play').click();
    await page.waitForFunction(() => document.querySelector('#vreader-status')?.textContent?.includes('Reading'), null, { timeout: 5000 });
    const prefetchDeadline = Date.now() + 5000;
    while (fixture.worker.requests.length < 2 && Date.now() < prefetchDeadline) await sleep(20);
    assert.ok(fixture.worker.requests.length >= 2, 'second request did not reach the worker');
    // Keep the third request in flight so Stop must cancel a real pending HTTP request.
    fixture.worker.configure({ latencyMs: 10000 });
    // Let the first 2-second paragraph play through and allow the second transition to be scheduled.
    await sleep(3800);
    const result = await page.evaluate(() => ({ gap: window.__gap, status: document.querySelector('#vreader-status')?.textContent, errors: [], now: performance.now() }));
    result.errors = errors;
    console.log(JSON.stringify(result, null, 2));
    assert.equal(result.gap.requests.length >= 2, true, 'expected second paragraph request');
    const second = result.gap.requests[1];
    const starts = result.gap.starts.filter(item => !item.stopped && item.start != null);
    assert.ok(starts.length >= 2, 'expected two scheduled paragraph buffers');
    assert.equal(starts[0].duration, 2, 'fixture must schedule one complete paragraph per buffer');
    assert.equal(starts[1].duration, 2, 'fixture must schedule one complete paragraph per buffer');
    const firstEndWall = starts[0].wall + (starts[0].duration - starts[0].offset) * 1000 / starts[0].rate;
    assert.ok(second.at < firstEndWall, `second request at ${second.at.toFixed(1)}ms was not prefetched before first playback ended at ${firstEndWall.toFixed(1)}ms`);
    if (result.gap.requests[2]) assert.ok(result.gap.requests[2].at >= starts[1].wall - 20, 'third request began before second paragraph playback started');
    const transitionGap = (starts[1].start - starts[0].start) - (starts[0].duration - starts[0].offset) / starts[0].rate;
    assert.ok(transitionGap < 0.1, `paragraph transition gap ${transitionGap}s exceeds 100ms`);
    await page.locator('#vreader-stop').click();
    await page.waitForFunction(() => window.__gap.requests.at(-1)?.fetchAborted === true, null, { timeout: 3000 });
    assert.ok(await page.evaluate(() => window.__gap.requests.at(-1)?.aborted === true), 'stop did not abort active/prefetched stream');
    assert.deepEqual(errors, [], errors.join('; '));
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
