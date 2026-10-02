const assert = require('node:assert/strict');
const { createReaderFixture, launchChromium } = require('../reader-fixture.cjs');
const wait = ms => new Promise(r => setTimeout(r, ms));
(async () => {
  const fixture = await createReaderFixture({ audio: { durationSeconds: 0.5, latencyMs: 30 } });
  let browser;
  try {
    const html = '<main><section><h1>First section</h1><p id=first>First paragraph.</p></section><section><h2>Second section</h2><p id=second>Second paragraph.</p></section></main>';
    const { id } = await fixture.publish({ title: 'pocket QA', html });
    await fixture.assertSpeechRoutes(id);
    browser = await launchChromium();
    const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: fixture.viewerHeaders });
    await context.addInitScript(() => {
      if (window.top !== window) return;
      localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice: 'pocket_alba', rate: '1.25' }));
      localStorage.setItem('artifact-reader-style', JSON.stringify({ style: 'custom', custom: 'stale Qwen instruction' }));
      window.__pocket = { requests: [], cancels: 0 }; window.__clockOffset = 0;
      const realNow = Date.now; Date.now = () => realNow() + window.__clockOffset;
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const url = String(typeof input === 'string' ? input : input.url);
        if (/\/speech\/stream(?:-timed)?$/.test(url)) window.__pocket.requests.push({ body: JSON.parse(init.body), at: performance.now() });
        try { return await nativeFetch(input, init); } catch (error) { if (error.name === 'AbortError') window.__pocket.cancels++; throw error; }
      };
    });
    const page = await context.newPage(), errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.goto(`${fixture.base}/${id}`, { waitUntil: 'domcontentloaded' }); await page.locator('#vreader-toggle').waitFor({ state: 'visible' }); await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
    const readStatus = () => page.locator('#vreader-status').textContent();
    const finished = () => page.waitForFunction(() => /Finished|Sleep timer ended/.test(document.querySelector('#vreader-status').textContent),null,{timeout:10000});
    const checkpoint = () => page.evaluate(()=>{const k=Object.keys(localStorage).find(k=>k.startsWith('artifact-reader-place:'));return k?localStorage.getItem(k):null;});
    const iframe = page.frameLocator('#vframe');
    await iframe.locator('#first').dispatchEvent('pointerdown');
    await page.locator('#vreader-mode').selectOption('section');
    await page.locator('#vreader-play').click(); await finished();
    assert.deepEqual(await page.evaluate(()=>__pocket.requests.map(r=>r.body.text)),['First section','First paragraph.'],'section mode crossed sibling');
    assert.equal(await page.locator('#vreader-sleep option[value=chapter]').evaluate(el=>el.disabled && el.hidden),true,'generic chapter timer enabled');
    await page.evaluate(()=>__pocket.requests=[]);
    await page.locator('#vreader-mode').selectOption('page');await page.locator('#vreader-sleep').selectOption('section');
    await page.locator('#vreader-play').click();await finished();
    assert.match(await readStatus(),/Sleep timer ended/);
    assert.deepEqual(await page.evaluate(()=>__pocket.requests.map(r=>r.body.text)),['First section','First paragraph.'],'sleep endsection prefetched sibling');
    assert.ok(await checkpoint(),'boundary sleep did not save place');
    // A timed sleep must stop ongoing audio, including when its tab was backgrounded.
    fixture.worker.configure({ durationSeconds: 25 }); await page.evaluate(()=>{__pocket.requests=[];});
    await page.locator('#vreader-sleep').selectOption('15');await page.locator('#vreader-play').click();
    await page.waitForFunction(()=>document.querySelector('.vreader').dataset.streamState==='playing');await wait(14000);
    await page.locator('#vreader-play').click();
    const beforeRewind=JSON.parse(await checkpoint()), requestCount=await page.evaluate(()=>__pocket.requests.length);
    assert.ok(beforeRewind.current.offset > 15, `stream did not reach 15 seconds: ${JSON.stringify({ checkpoint: beforeRewind, requests: requestCount, state: await page.locator('.vreader').getAttribute('data-stream-state'), status: await readStatus(), worker: fixture.worker.requests })}`);
    await page.locator('#vreader-rewind').click();
    const afterRewind=JSON.parse(await checkpoint());
    assert.ok(Math.abs(beforeRewind.current.offset - afterRewind.current.offset - 15) < .05, `stream rewind did not move 15 media seconds: ${beforeRewind.current.offset} -> ${afterRewind.current.offset}`);
    assert.equal(await page.evaluate(()=>__pocket.requests.length),requestCount,'current stream rewind refetched audio');
    assert.equal(await page.locator('#vreader-play').textContent(),'Resume','rewind changed paused state');

    await page.evaluate(()=>{window.__clockOffset=16*60000;document.dispatchEvent(new Event('visibilitychange'));});
    assert.match(await readStatus(),/Sleep timer ended/);assert.equal(await page.locator('#vreader-play').textContent(),'Play');
    assert.equal(await page.locator('#vreader-sleep').inputValue(),'off');assert.ok(JSON.parse(await checkpoint()).current.offset > 0);
    const beforePreview=await checkpoint();
    fixture.worker.configure({ durationSeconds: .5 }); await page.evaluate(()=>{__clockOffset=0;});
    await page.locator('#vreader-preview').click();await wait(100);
    assert.equal(await checkpoint(),beforePreview,'preview changed existing checkpoint during playback');
    await page.waitForFunction(()=>document.querySelector('#vreader-status').textContent.includes('Preview finished'),null,{timeout:5000});
    assert.equal(await checkpoint(),beforePreview,'preview changed existing checkpoint after completion');
    // Resume skips a fully completed chunk and must progress, not leave loading stuck.
    fixture.worker.configure({ durationSeconds: .5 }); await page.evaluate(()=>{window.__clockOffset=0;const k=Object.keys(localStorage).find(k=>k.startsWith('artifact-reader-place:'));const v=JSON.parse(localStorage.getItem(k));v.current.offset=.5;localStorage.setItem(k,JSON.stringify(v));});
    await page.locator('#vreader-resume').click();await finished();
    assert.match(await readStatus(),/Finished/);
    assert.deepEqual(errors,[]);
    console.log(JSON.stringify({sectionScope:true,endSectionNoPrefetch:true,wallClockSleep:true,fullChunkResume:true,streamRewind15:true,previewCheckpointIsolation:true,errors}));
  } finally { if (browser) await browser.close(); await fixture.close(); }
})().catch(e => { console.error(e.stack); process.exitCode = 1; });
