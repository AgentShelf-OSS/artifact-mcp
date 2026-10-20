// Forward 30 s seeks inside the current block when its audio is there, otherwise it carries the
// remainder into the next block (the mirror of rewind 15 s).
const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
// About 480 characters: the 15 characters-a-second estimate gives each block about 32 s.
const paragraph = word => Array.from({ length: 6 }, (_, i) => `${word} sentence ${i + 1} carries enough words to fill a long paragraph for skipping.`).join(' ');
(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 40, latencyMs: 30 } });
  let browser;
  try {
    const html = `<main><p>${paragraph('Alpha')}</p><p>${paragraph('Bravo')}</p><p>${paragraph('Charlie')}</p></main>`;
    const { id } = await fixture.publish({ title: 'forward QA', html });
    await fixture.assertSpeechRoutes(id);
    browser = await launchChromium();
    const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: fixture.viewerHeaders });
    await context.addInitScript(() => { if (window.top === window) localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice: 'pocket_alba', rate: '1' })); });
    const page = await context.newPage(), errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.goto(`${fixture.base}/${id}`, { waitUntil: 'domcontentloaded' });
    await page.locator('#vreader-toggle').waitFor({ state: 'visible' });
    await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    const forward = page.locator('#vreader-forward');
    assert.equal(await forward.getAttribute('aria-label'), 'Forward 30 seconds');
    assert.equal(await forward.isDisabled(), true, 'forward enabled before audio loaded');
    const checkpoint = () => page.evaluate(() => { const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); return k ? JSON.parse(localStorage.getItem(k)).current : null; });
    await page.locator('#vreader-play').click();
    await page.waitForFunction(() => document.querySelector('.vreader').dataset.streamState === 'playing', null, { timeout: 10000 });
    await wait(1500);
    await page.locator('#vreader-play').click();
    const start = await checkpoint();
    assert.equal(start.ordinal, 0);

    await forward.click();
    const within = await checkpoint();
    assert.equal(within.ordinal, 0, 'forward left a block that still had 30 s of audio');
    assert.ok(Math.abs(within.offset - start.offset - 30) < 0.05, `forward moved ${within.offset - start.offset} s`);
    assert.equal(await page.locator('#vreader-play').textContent(), 'Resume', 'forward changed the paused state');

    await forward.click();
    await page.waitForFunction(() => { const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); return k && JSON.parse(localStorage.getItem(k)).current.ordinal === 1; }, null, { timeout: 8000 });
    const next = await checkpoint();
    assert.ok(Math.abs(next.offset - (start.offset + 60 - 40)) < 0.1, `remainder offset ${next.offset}, expected ${start.offset + 20}`);
    assert.equal(await page.locator('#vreader-play').textContent(), 'Resume', 'forward across blocks started playback');
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ forwardWithinBlock: true, forwardRemainderIntoNextBlock: true, errors }));
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
