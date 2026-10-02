// End-to-end pronunciation controls with the shared deterministic speech worker.
const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');

(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 12, latencyMs: 0 } });
  let browser;
  try {
    async function waitForSpeech(text, start = 0, timeoutMs = 10000) {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        const record = worker.requests.slice(start).find(request => request.body?.text === text);
        if (record) return record;
        await new Promise(resolve => setTimeout(resolve, 20));
      }
      assert.fail(`timed out waiting for speech request: ${text}`);
    }
    const { base, viewerHeaders, publish, worker, assertSpeechRoutes } = fixture;
    const { id } = await publish({ title: 'Mixed reader QA', html: '<main><p id="passage">Talk to <span id="name" data-artifact-pronounce="Shiv awn">Siobhan</span>. She reads <span data-artifact-pronounce="sequel">SQL</span>.</p></main>' });
    await assertSpeechRoutes(id); worker.configure({ durationSeconds: 12 });
    browser = await launchChromium();
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, extraHTTPHeaders: viewerHeaders });
    await context.addInitScript(() => { window.messages = []; addEventListener('message', e => messages.push(e.data)); });
    const p = await context.newPage(); const errors = []; p.on('pageerror', e => errors.push(e.message)); await p.goto(`${base}/${id}`);
    await p.locator('#vreader-toggle').click(); await p.locator('#vreader-expand').click(); await p.locator('#vreader-voice').selectOption('pocket_alba');
    const frame = p.frames().find(f => f.url().includes('/raw/')); await p.locator('#vreader-play').click();
    await p.waitForFunction(() => document.querySelector('#vreader-status')?.textContent?.includes('Reading'));
    await waitForSpeech('Talk to Shiv awn. She reads sequel.');
    await frame.waitForFunction(() => { const h = CSS.highlights.get('artifact-reader-word'); return h && [...h][0].toString() === 'Siobhan'; });
    assert.equal(await frame.locator('#passage').innerText(), 'Talk to Siobhan. She reads SQL.');
    assert.ok(await p.evaluate(() => artifactReaderDiagnostics()[0].firstScheduledAudioMs >= 0));
    await frame.waitForFunction(() => { const h = CSS.highlights.get('artifact-reader-word'); return h && [...h][0].toString() === 'SQL'; });
    await p.locator('#vreader-play').click(); assert.equal(await p.locator('#vreader-replay').isEnabled(), true); await p.locator('#vreader-replay').click();
    await frame.waitForFunction(() => { const h = CSS.highlights.get('artifact-reader-word'); return h && [...h][0].toString() === 'She'; });
    await frame.locator('#name').evaluate(n => n.setAttribute('data-artifact-pronounce', 'Shiv on')); await p.waitForFunction(() => messages.some(m => m.type === 'reader:changed'));
    await frame.locator('#name').evaluate(n => { const r = document.createRange(); r.setStart(n.firstChild, 1); r.setEnd(n.firstChild, 4); getSelection().removeAllRanges(); getSelection().addRange(r); }); await p.waitForTimeout(60); await p.locator('#vreader-mode').selectOption('selection');
    const partialStart = worker.requests.length; await p.locator('#vreader-play').click(); await waitForSpeech('iob', partialStart); await p.locator('#vreader-stop').click();
    await frame.locator('#name').evaluate(n => { const r = document.createRange(); r.selectNodeContents(n); getSelection().removeAllRanges(); getSelection().addRange(r); }); await p.waitForTimeout(60); await p.locator('#vreader-mode').selectOption('selection');
    const wholeStart = worker.requests.length; await p.locator('#vreader-play').click(); await waitForSpeech('Shiv on', wholeStart);
    await frame.waitForFunction(() => { const h = CSS.highlights.get('artifact-reader-word'); return h && [...h][0].toString() === 'Siobhan'; }); await p.locator('#vreader-stop').click(); assert.deepEqual(errors, []);
    console.log('PASS: spoken replacements, source DOM unchanged, multiword-to-term highlighting, SQL mapping, sentence replay, metadata invalidation, partial and whole-term selection, bounded diagnostics');
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(e => { console.error(e); process.exitCode = 1; });
