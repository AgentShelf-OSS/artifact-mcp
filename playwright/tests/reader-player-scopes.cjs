// Reader shell/scopes regression against the shared Node/Rust fixture.
const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');

(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 15, latencyMs: 0 } });
  let browser;
  try {
    const { base, viewerHeaders, publish, worker, assertSpeechRoutes } = fixture;
    const { id } = await publish({ title: 'Mixed reader QA', html: `<main>
      <h1>Report</h1><p id="outside">Independent report text.</p>
      <section id="tool" data-artifact-reader-region="view"><h2>Current result</h2><p id="result">Forty tasks completed.</p><input value="private input"><button>Refresh</button></section>
      <pre id="code" style="padding:12px;background:#eee">echo ready</pre>
      <figure id="chart" data-artifact-reader-detail="Revenue rose every quarter, ending at forty units."><figcaption>Revenue trend</figcaption></figure>
    </main>` });
    await assertSpeechRoutes(id);
    worker.configure({ durationSeconds: 15 });
    browser = await launchChromium();
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, extraHTTPHeaders: viewerHeaders });
    await context.addInitScript(() => { window.messages = []; addEventListener('message', e => messages.push(e.data)); });
    const p = await context.newPage(); const errors = []; p.on('pageerror', e => errors.push(e.message));
    await p.goto(`${base}/${id}`); await p.locator('#vreader-toggle').click(); await p.locator('#vreader-expand').click(); await p.locator('#vreader-voice').selectOption('bm_george');
    const frame = p.frames().find(f => f.url().includes('/raw/'));
    await frame.locator('#result').click(); await p.locator('#vreader-mode').selectOption('view'); await p.locator('#vreader-play').click();
    await p.waitForFunction(() => messages.some(m => m.type === 'reader:content' && m.mode === 'view'));
    const view = await p.evaluate(() => messages.find(m => m.type === 'reader:content' && m.mode === 'view'));
    assert.equal(view.scopeKey, 'id:tool'); assert.deepEqual(view.blocks.map(b => b.text), ['Current result', 'Forty tasks completed.']);
    await frame.locator('#outside').evaluate(n => { n.textContent = 'Changed outside'; }); await p.waitForTimeout(300);
    assert.equal(await p.locator('#vreader-play').textContent(), 'Pause');
    await frame.locator('#result').evaluate(n => { n.textContent = 'Forty-one tasks completed.'; }); await p.waitForFunction(() => messages.some(m => m.type === 'reader:changed'));
    assert.equal(await p.locator('#vreader-play').textContent(), 'Play');
    await frame.locator('#code').click(); await p.locator('#vreader-target-details').click(); await p.waitForFunction(() => messages.some(m => m.type === 'reader:content' && m.mode === 'detail'));
    assert.equal(await p.evaluate(() => messages.find(m => m.type === 'reader:content' && m.mode === 'detail').blocks[0].text), 'echo ready');
    await p.waitForTimeout(500); await p.locator('#vreader-play').click(); await p.locator('#vreader-stop').click();
    const detailRequest = await p.evaluate(() => messages.find(m => m.type === 'reader:content' && m.mode === 'detail')?.requestId);
    const checkpoints = await p.evaluate(() => Object.entries(localStorage).filter(([k]) => k.startsWith('artifact-reader-place:')).map(([, v]) => JSON.parse(v)));
    assert.ok(checkpoints.some(c => c.version === 2 && c.current.scopeKey === 'id:code'));
    await p.locator('#vreader-resume').click(); await p.waitForFunction(previous => messages.some(m => m.type === 'reader:content' && m.mode === 'detail' && m.requestId !== previous), detailRequest); await p.locator('#vreader-stop').click();
    await p.locator('#vreader-outline').selectOption({ label: 'Revenue trend' }); await p.locator('#vreader-jump').focus(); await p.keyboard.press('Enter');
    await p.waitForFunction(() => messages.some(m => m.type === 'reader:content' && m.scopeKey === 'id:chart'));
    assert.equal(await p.evaluate(() => messages.find(m => m.type === 'reader:content' && m.scopeKey === 'id:chart').blocks[0].text), 'Revenue rose every quarter, ending at forty units.');
    await p.locator('#vreader-stop').click();
    assert.ok(await p.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'desktop reader overflow');
    await p.setViewportSize({ width: 390, height: 844 });
    assert.ok(await p.evaluate(() => document.documentElement.scrollWidth <= innerWidth)); assert.deepEqual(errors, []);
    console.log('PASS: real shell view reading, outside mutation ignored, inside mutation pauses, detail click, scoped save/resume, desktop/mobile, no page errors');
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(e => { console.error(e.stack || e); process.exitCode = 1; });
