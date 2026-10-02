const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 25, latencyMs: 30 } });
  let browser;
  try {
    const { id } = await fixture.publish({ title: 'WAV resume QA', html: '<!doctype html><html><body><main><p>Twenty five seconds of synthetic narration.</p></main></body></html>' });
    await fixture.assertSpeechRoutes(id);
    browser = await launchChromium();
    const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: fixture.viewerHeaders });
    await context.addInitScript(() => {
      if (window.top !== window) return;
      localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice: 'bm_george', rate: '1' }));
      window.__wav = { speechCalls: 0 };
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const url = String(typeof input === 'string' ? input : input.url);
        if (url.endsWith('/speech')) window.__wav.speechCalls++;
        return nativeFetch(input, init);
      };
    });
    const page = await context.newPage();
    const errors = []; page.on('pageerror', error => errors.push(error.message));
    await page.goto(`${fixture.base}/${id}`, { waitUntil: 'domcontentloaded' });
    await page.locator('#vreader-toggle').waitFor({ state: 'visible' }); await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await page.locator('#vreader-play').click();
    await page.waitForFunction(() => document.querySelector('#vreader-audio')?.readyState >= 1 && document.querySelector('#vreader-status')?.textContent?.includes('Reading'), null, { timeout: 10000 });
    assert.ok(await page.evaluate(() => window.__wav.speechCalls >= 1), 'first click did not request WAV speech');
    await page.evaluate(() => { document.querySelector('#vreader-audio').currentTime = 18; });
    await page.waitForFunction(() => { const audio = document.querySelector('#vreader-audio'); return !audio.seeking && audio.currentTime >= 17.5; }, null, { timeout: 3000 });
    await page.locator('#vreader-play').click();
    await page.waitForFunction(() => Object.keys(localStorage).some(key => {
      if (!key.startsWith('artifact-reader-place:')) return false;
      try { return Math.abs(JSON.parse(localStorage.getItem(key)).current.offset - 18) < 0.4; } catch { return false; }
    }), null, { timeout: 3000 });
    const keyName = await page.evaluate(() => Object.keys(localStorage).find(key => key.startsWith('artifact-reader-place:')));
    const saved18 = await page.evaluate(key => JSON.parse(localStorage.getItem(key)), keyName);
    assert.ok(saved18 && Math.abs(saved18.current.offset - 18) < 0.4, `pause checkpoint offset was ${saved18?.current?.offset}`);
    await page.locator('#vreader-rewind').click(); await wait(100);
    const rewound = await page.evaluate(() => document.querySelector('#vreader-audio').currentTime);
    const saved3 = await page.evaluate(key => JSON.parse(localStorage.getItem(key)), keyName);
    assert.ok(Math.abs(rewound - 3) < 0.4, `rewind currentTime was ${rewound}`);
    assert.ok(saved3 && Math.abs(saved3.current.offset - 3) < 0.4, `rewind checkpoint offset was ${saved3?.current?.offset}`);
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
    console.log(JSON.stringify({ speechCalls: await page.evaluate(() => window.__wav.speechCalls), saved18: saved18.current.offset, rewound, saved3: saved3.current.offset, resumed, finished: true, errors }, null, 2));
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
