// ADR-0022: the saved Listen place follows the signed-in viewer to a second browser, keeps the new
// speeds, and "Finished" clears the server copy too.
const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const sentence = word => `${word} paragraph begins here. It keeps going with a second sentence. A third sentence closes it.`;
(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 20, latencyMs: 30 } });
  let browser;
  try {
    const html = `<main><p id=a>${sentence('Alpha')}</p><p id=b>${sentence('Bravo')}</p><p id=c>${sentence('Charlie')}</p></main>`;
    const { id } = await fixture.publish({ title: 'place sync QA', html });
    await fixture.assertSpeechRoutes(id);
    const serverPlace = async () => {
      const response = await fetch(`${fixture.base}/${id}/state/listen-place?scope=viewer`, { headers: fixture.viewerHeaders });
      if (response.status === 404) return null;
      assert.equal(response.status, 200, 'state read failed');
      return (await response.json()).value;
    };
    const poll = async (check, label) => {
      for (let attempt = 0; attempt < 12; attempt += 1) { const value = await serverPlace(); if (check(value)) return value; await wait(750); }
      throw new Error(label);
    };
    browser = await launchChromium();
    const open = async rate => {
      const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: fixture.viewerHeaders });
      await context.addInitScript(rate => { if (window.top === window) localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice: 'pocket_alba', rate })); }, rate);
      const page = await context.newPage(), errors = []; page.on('pageerror', e => errors.push(e.message));
      await page.goto(`${fixture.base}/${id}`, { waitUntil: 'domcontentloaded' });
      await page.locator('#vreader-toggle').waitFor({ state: 'visible' });
      await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
      return { context, page, errors };
    };
    const checkpoint = page => page.evaluate(() => { const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); return k ? JSON.parse(localStorage.getItem(k)) : null; });
    const playing = page => page.waitForFunction(() => document.querySelector('.vreader').dataset.streamState === 'playing', null, { timeout: 10000 });

    // First browser: a new speed, the second paragraph, then pause.
    const first = await open('2.5');
    const rates = await first.page.locator('#vreader-rate option').evaluateAll(options => options.map(option => option.value));
    for (const rate of ['0.75', '0.8', '0.9', '1', '1.1', '1.25', '1.5', '1.75', '2', '2.5', '3']) assert.ok(rates.includes(rate), `rate ${rate} missing`);
    assert.equal(await first.page.locator('#vreader-rate').inputValue(), '2.5', 'saved preference rate not applied');
    await first.page.locator('#vreader-play').click(); await playing(first.page);
    await first.page.locator('#vreader-next').click(); await playing(first.page); await wait(1500);
    await first.page.locator('#vreader-play').click();
    await first.page.waitForFunction(() => { const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); return k && JSON.parse(localStorage.getItem(k)).current.ordinal === 1; }, null, { timeout: 5000 });
    const saved = await checkpoint(first.page);
    assert.equal(saved.current.rate, '2.5', 'new speed not kept in the checkpoint');
    assert.equal(saved.current.ordinal, 1); assert.ok(saved.current.offset > 0, 'no offset saved');
    assert.ok(Number.isFinite(saved.savedAt), 'checkpoint has no savedAt');
    const uploaded = await poll(value => value?.pages?.['']?.savedAt === saved.savedAt, 'paused place never reached the server');
    assert.deepEqual(uploaded.pages[''].place.current, saved.current);
    assert.deepEqual(first.errors, []);
    await first.context.close();

    // Second browser: empty localStorage, another default speed. Play continues the first browser's place.
    const second = await open('1');
    await second.page.locator('#vreader-resume').waitFor({ state: 'visible', timeout: 8000 });
    assert.equal((await checkpoint(second.page)).savedAt, saved.savedAt, 'server place not cached locally');
    fixture.worker.reset();
    await second.page.locator('#vreader-play').click(); await playing(second.page);
    assert.match(fixture.worker.requests[0].body.text, /^Bravo/, 'Play did not continue from the synced place');
    assert.equal(await second.page.locator('#vreader-rate').inputValue(), '2.5', 'synced speed not restored');
    const restored = await checkpoint(second.page);
    assert.equal(restored.current.ordinal, 1);
    assert.ok(restored.current.offset >= saved.current.offset - 0.05, `restored offset ${restored.current.offset} < ${saved.current.offset}`);

    // Finishing clears both copies.
    fixture.worker.configure({ durationSeconds: 0.3 });
    await second.page.locator('#vreader-next').click();
    await second.page.waitForFunction(() => document.querySelector('#vreader-status').textContent === 'Finished', null, { timeout: 10000 });
    assert.equal(await checkpoint(second.page), null, 'local place kept after Finished');
    await poll(value => value === null || !value.pages?.[''], 'server place kept after Finished');
    assert.deepEqual(second.errors, []);
    console.log(JSON.stringify({ crossDeviceResume: true, newRatesInCheckpoint: true, finishedClearsServer: true }));
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
