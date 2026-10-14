// End-to-end cast roles: role controls, per-segment speech voices, inline dialogue splits,
// ereader spoken headings and intros, and the untagged fallback.
const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');

const CAST = '<script type="application/json" id="artifact-reader-cast">' + JSON.stringify({
  version: 1, default: 'narrator', roles: [
    { id: 'narrator', label: 'Narrator' },
    { id: 'victor', label: 'Victor', description: 'Writes letters.', suggest: ['pocket_alba'] },
    { id: 'creature', label: 'The creature', description: 'Speaks in dialogue.' },
  ],
}) + '</script>';

// A minimal ereader in the shape the ereader adapter recognises.
const BOOK = { chapters: [{ part: 'Part One', title: 'Chapter 1', blocks: [
  { t: 'p', s: 'I walked out into the snow.' },
  { t: 'p', s: 'He said, "Wait for me," and vanished.' },
] }] };
const EREADER = `${CAST}<script id="book" type="application/json">${JSON.stringify(BOOK)}</script>
<article id="article"><header class="ch-head" data-artifact-reader-voice="narrator" data-artifact-reader-summary="Chapter One. Part One.">
<div class="part">Part One</div><h1>Chapter 1</h1><div class="meta">12 words</div>
<p class="recap" data-artifact-reader-intro>Previously, nothing happened.</p></header>
<p data-p="0" data-artifact-reader-voice="victor">I walked out into the snow.</p>
<p data-p="1" data-artifact-reader-voice="victor">He said, <span data-artifact-reader-voice="creature">"Wait for me,"</span> and vanished.</p>
</article>`;

async function requestsFor(worker, before, texts) {
  for (let attempt = 0; attempt < 160; attempt += 1) {
    const seen = worker.requests.slice(before);
    if (texts.every(text => seen.some(item => item.body.text === text))) return seen;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  return worker.requests.slice(before);
}

(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 1 } });
  let browser;
  try {
    const { base, viewerHeaders, publish, worker } = fixture;
    const { id } = await publish({ html: `${CAST}<main><p>Before the letter.</p><p data-artifact-reader-voice="victor">Dear Mara.</p></main>` });
    const { id: bookId } = await publish({ html: EREADER });
    const { id: plainId } = await publish({ html: '<main><p>One ordinary paragraph.</p></main>' });
    browser = await launchChromium();
    const context = await browser.newContext({ extraHTTPHeaders: viewerHeaders });
    const page = await context.newPage();

    // Generic artifact: roles render, a role voice applies to its tagged block.
    await page.goto(`${base}/${id}`);
    await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await page.locator('#vreader-cast').waitFor({ state: 'visible' });
    assert.equal(await page.locator('#vreader-cast-roles .vreader-cast-role').count(), 3);
    await page.locator('#vreader-voice').selectOption('bm_george');
    await page.locator('#vreader-cast-roles select').nth(1).selectOption('pocket_alba');
    let before = worker.requests.length; await page.locator('#vreader-play').click();
    let seen = await requestsFor(worker, before, ['Before the letter.', 'Dear Mara.']);
    assert.equal(seen.find(item => item.body.text === 'Dear Mara.')?.body.voice, 'pocket_alba', 'tagged block uses its role voice');
    assert.equal(seen.find(item => item.body.text === 'Before the letter.')?.body.voice, 'bm_george', 'untagged text uses the main voice');
    await page.locator('#vreader-stop').click();

    // Ereader: spoken heading summary, intro, and an inline dialogue split into three segments.
    await page.goto(`${base}/${bookId}`);
    await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await page.locator('#vreader-cast').waitFor({ state: 'visible' });
    await page.locator('#vreader-voice').selectOption('bm_george');
    await page.locator('#vreader-cast-roles select').nth(1).selectOption('pocket_alba');
    await page.locator('#vreader-cast-roles select').nth(2).selectOption('pocket_javert');
    before = worker.requests.length; await page.locator('#vreader-play').click();
    const texts = ['Chapter One. Part One.', 'Previously, nothing happened.', 'I walked out into the snow.', 'He said,', '"Wait for me,"', 'and vanished.'];
    seen = await requestsFor(worker, before, texts);
    const voiceOf = text => seen.find(item => item.body.text === text)?.body.voice;
    assert.equal(voiceOf('Chapter One. Part One.'), 'bm_george', 'heading summary is spoken in the narrator (main) voice');
    assert.ok(!seen.some(item => /12 words/.test(item.body.text)), 'heading meta is not read');
    assert.equal(seen.filter(item => item.body.text === 'Previously, nothing happened.').length, 1, 'intro is read once');
    assert.equal(voiceOf('I walked out into the snow.'), 'pocket_alba');
    assert.equal(voiceOf('He said,'), 'pocket_alba');
    assert.equal(voiceOf('"Wait for me,"'), 'pocket_javert', 'inline dialogue uses the creature voice');
    assert.equal(voiceOf('and vanished.'), 'pocket_alba');
    await page.locator('#vreader-stop').click();

    // Word and passage highlights stay inside the dialogue segment, not the whole paragraph.
    const frame = page.frames().find(item => item !== page.mainFrame());
    await page.evaluate(() => { window.__castMessages = []; addEventListener('message', event => window.__castMessages.push(event.data)); document.querySelector('iframe').contentWindow.postMessage({ type: 'reader:extract', requestId: 'cast-segments', mode: 'page' }, '*'); });
    await page.waitForFunction(() => window.__castMessages.some(message => message && message.requestId === 'cast-segments'));
    const blocks = await page.evaluate(() => window.__castMessages.find(message => message.requestId === 'cast-segments').blocks);
    const dialogue = blocks.find(block => block.text === '"Wait for me,"');
    assert.equal(dialogue?.role, 'creature', 'segment carries its role');
    const send = message => page.evaluate(value => document.querySelector('iframe').contentWindow.postMessage(value, '*'), message);
    const marked = name => frame.evaluate(value => { const found = CSS.highlights.get(value); return found ? [...found].map(range => range.toString()).join('') : null; }, name);
    await send({ type: 'reader:highlight', id: dialogue.id });
    await frame.waitForFunction(() => CSS.highlights.has('artifact-reader-passage'));
    assert.equal(await marked('artifact-reader-passage'), '"Wait for me,"', 'passage highlight covers only the segment');
    await send({ type: 'reader:word', id: dialogue.id, text: dialogue.text, offset: 0, start: 1, end: 5 });
    await frame.waitForFunction(() => CSS.highlights.has('artifact-reader-word'));
    assert.equal(await marked('artifact-reader-word'), 'Wait', 'word offsets map into the segment');
    await send({ type: 'reader:highlight', id: null });

    // Untagged artifact: no Voices section.
    await page.goto(`${base}/${plainId}`); await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    assert.equal(await page.locator('#vreader-cast').isHidden(), true);
    console.log('PASS: cast controls, role voices, inline dialogue split, ereader heading and intro, untagged fallback');
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
