// Read-ahead fetches several blocks (at least 3, about 60 s, at most 8) while one plays, and one
// viewer never has more than 2 speech requests at the worker at once.
const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 4, latencyMs: 300 } });
  let browser;
  try {
    const paragraphs = Array.from({ length: 14 }, (_, i) => `<p>Paragraph ${i + 1} reads one short sentence aloud for the read-ahead bound.</p>`).join('');
    const { id } = await fixture.publish({ title: 'read-ahead QA', html: `<main>${paragraphs}</main>` });
    await fixture.assertSpeechRoutes(id);
    browser = await launchChromium();
    const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: fixture.viewerHeaders });
    const page = await context.newPage(), errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.goto(`${fixture.base}/${id}`, { waitUntil: 'domcontentloaded' });
    await page.locator('#vreader-toggle').waitFor({ state: 'visible' });
    await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await page.locator('#vreader-voice').selectOption('qwen_reference');
    await page.locator('#vreader-pauses').selectOption('off');
    await page.locator('#vreader-play').click();
    await page.waitForFunction(() => document.querySelector('#vreader-status')?.textContent?.includes('Reading'), null, { timeout: 5000 });
    const requests = fixture.worker.requests;
    const firstAt = requests[0].at;
    // The first block lasts 4 s; read-ahead must fill its window well before that.
    const deadline = firstAt + 3500;
    while (requests.length < 9 && Date.now() < deadline) await wait(25);
    const early = requests.filter(item => item.at < firstAt + 3500).length;
    assert.ok(early >= 4, `only ${early - 1} blocks were read ahead during the first block`);
    assert.ok(early <= 9, `${early - 1} blocks read ahead; the bound is 8`);
    await wait(1500);
    const texts = requests.map(item => item.body.text);
    assert.equal(new Set(texts).size, texts.length, 'a block was synthesized twice');
    assert.ok(fixture.worker.load.max <= 2, `viewer ran ${fixture.worker.load.max} concurrent speech requests`);
    await page.locator('#vreader-stop').click();
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ readAhead: early - 1, maxConcurrent: fixture.worker.load.max, requests: texts.length }));
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(error => { console.error(error.stack); process.exitCode = 1; });
