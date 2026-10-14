// Listen pacing, Media Session metadata and concurrent speech requests.
// Scheduled gaps are measured from AudioBufferSourceNode.start times, so they are exact.
const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');

const HTML = '<main><h1>The Harbour</h1><p>First paragraph.</p><p>Second paragraph.</p><hr><p>After the break.</p></main>';

async function scheduledParagraphs(page, count) {
  // Group buffer starts by gaps longer than 50 ms: each group is one spoken block.
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const groups = await page.evaluate(() => {
      const starts = (window.__starts || []).slice().sort((a, b) => a.when - b.when);
      const out = [];
      for (const item of starts) {
        const last = out[out.length - 1];
        if (last && item.when - last.end < 0.05) last.end = Math.max(last.end, item.when + item.duration);
        else out.push({ start: item.when, end: item.when + item.duration });
      }
      return out;
    });
    if (groups.length >= count) return groups;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error('not enough scheduled blocks');
}

async function stopIfPlaying(page) { if (await page.locator('#vreader-stop').isEnabled()) await page.locator('#vreader-stop').click(); }

(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 1 } });
  let browser;
  try {
    const { base, viewerHeaders, publish } = fixture;
    const { id } = await publish({ html: HTML });
    browser = await launchChromium();
    const context = await browser.newContext({ extraHTTPHeaders: viewerHeaders });
    await context.addInitScript(() => {
      window.__starts = [];
      const original = AudioBufferSourceNode.prototype.start;
      AudioBufferSourceNode.prototype.start = function (when = 0, ...rest) {
        if (this.buffer && this.buffer.duration > 0.02) window.__starts.push({ when, duration: this.buffer.duration / (this.playbackRate?.value || 1) });
        return original.call(this, when, ...rest);
      };
    });
    const page = await context.newPage();
    await page.goto(`${base}/${id}`);
    await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    await page.locator('#vreader-voice').selectOption('pocket_alba');
    assert.equal(await page.locator('#vreader-pauses').inputValue(), 'natural');

    // Natural pauses: about 0.9 s after a heading, 0.6 s between paragraphs, 1.8 s at a scene break.
    await page.locator('#vreader-play').click();
    const groups = await scheduledParagraphs(page, 4);
    const gaps = groups.slice(1, 4).map((group, index) => group.start - groups[index].end);
    const near = (value, expected) => Math.abs(value - expected) < 0.25;
    assert.ok(near(gaps[0], 0.9), `after heading ${gaps[0].toFixed(3)} s`);
    assert.ok(near(gaps[1], 0.6), `between paragraphs ${gaps[1].toFixed(3)} s`);
    assert.ok(near(gaps[2], 1.8), `scene break ${gaps[2].toFixed(3)} s`);

    // Lock-screen metadata is set while reading.
    const session = await page.evaluate(() => { const metadata = navigator.mediaSession && navigator.mediaSession.metadata; return metadata ? { title: metadata.title, artist: metadata.artist, album: metadata.album, artwork: metadata.artwork.map(item => item.src), state: navigator.mediaSession.playbackState } : null; });
    assert.ok(session, 'media session metadata is set');
    assert.equal(session.state, 'playing');
    assert.ok(session.artist.length > 0 && session.album.length > 0, 'artist and album are set');
    assert.ok(session.artwork.some(src => /\/icons\/app-192\.png$/.test(src)), 'artwork uses the app icon');

    // A second speech request during playback is accepted (worker concurrency > 1).
    const second = await page.evaluate(async artifact => (await fetch(`/${artifact}/speech`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ text: 'A preview line.', voice: 'pocket_alba' }) })).status, id);
    assert.equal(second, 200, 'overlapping speech request is not rejected as busy');
    await stopIfPlaying(page);

    // Pauses off: blocks follow each other without inserted silence.
    await page.locator('#vreader-pauses').selectOption('off');
    await page.evaluate(() => { window.__starts = []; });
    await page.locator('#vreader-play').click();
    const tight = await scheduledParagraphs(page, 2).catch(() => null);
    const starts = await page.evaluate(() => (window.__starts || []).slice().sort((a, b) => a.when - b.when));
    const maxGap = Math.max(...starts.slice(1).map((item, index) => item.when - (starts[index].when + starts[index].duration)));
    assert.ok(tight === null || maxGap < 0.15, `pauses off leaves ${maxGap.toFixed(3)} s`);
    await stopIfPlaying(page);
    console.log(`PASS: pacing ${gaps.map(gap => gap.toFixed(2)).join('/')} s, media session, concurrent request, pauses off`);
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
