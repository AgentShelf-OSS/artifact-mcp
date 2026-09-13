const { spawn } = require('node:child_process');
const fs = require('node:fs/promises');
const { randomBytes } = require('node:crypto');
const assert = require('node:assert/strict');
const { chromium } = require('playwright');
const PORT = Number(process.env.POCKET_TEST_PORT || 3506), BASE = `http://127.0.0.1:${PORT}`;
const wait = ms => new Promise(r => setTimeout(r, ms));
const rpcBody = r => r.result?.structuredContent || JSON.parse(r.result.content.find(x => x.type === 'text').text);
(async () => {
  const dir = await fs.mkdtemp('/tmp/artifact-pocket-qa-'), key = randomBytes(32).toString('hex');
  const env = { ...process.env, PORT: String(PORT), LISTEN_HOST: '127.0.0.1', PUBLIC_BASE_URL: BASE, DATA_DIR: dir, TRUST_ACCESS_HEADERS: '1', REQUIRE_ACCESS_JWT: '0', CF_ACCESS_AUD: '', CF_ACCESS_TEAM_DOMAIN: '', AUDIT_LEDGER_HMAC_KEY: randomBytes(32).toString('base64'), ARTIFACT_API_KEYS: `e2e:homelab:${key}`, TTS_ENABLED: '1', TTS_ARTIFACT_IDS: '' };
  const server = spawn(process.env.POCKET_TEST_BINARY || require('node:path').resolve(__dirname, '../../target/release/artifact-mcp'), { env, stdio: ['ignore', 'ignore', 'pipe'] }); let browser;
  try {
    await new Promise((resolve, reject) => { const t = setInterval(async () => { try { if ((await fetch(BASE + '/health')).ok) { clearInterval(t); resolve(); } } catch (_) {} }, 100); setTimeout(() => { clearInterval(t); reject(new Error('server startup timeout')); }, 10000); });
    const s1 = 'This first sentence has enough words to exercise sentence preservation without punctuation splitting too early. ';
    const s2 = 'The second sentence is deliberately long and includes unicode café naïve résumé words so Array.from boundaries remain correct. ';
    const noPunctuation = 'word '.repeat(260).trim();
    const html = '<main><section><h1>First section</h1><p id=first>First paragraph.</p></section><section><h2>Second section</h2><p id=second>Second paragraph.</p></section></main>';
    const response = await fetch(BASE + '/mcp', { method: 'POST', headers: { authorization: 'Bearer ' + key, 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'publish_artifact', arguments: { title: 'pocket QA', html } } }) });
    const id = rpcBody(await response.json()).id; assert.ok(id);
    browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--autoplay-policy=no-user-gesture-required'] });
    const context = await browser.newContext({ viewport: { width: 1200, height: 800 }, extraHTTPHeaders: { 'cf-access-authenticated-user-email': 'test@homelab' } });
    await context.addInitScript(() => {
      if (window.top !== window) return;
      localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice: 'pocket_alba', rate: '1.25' }));
      localStorage.setItem('artifact-reader-style', JSON.stringify({ style: 'custom', custom: 'stale Qwen instruction' }));
      window.__pocket = { requests: [], cancels: 0, duration: 0.5 }; window.__clockOffset = 0; const realNow=Date.now; Date.now=()=>realNow()+window.__clockOffset;
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const url = String(typeof input === 'string' ? input : input.url);
        if (url.endsWith('/speech/voices')) return new Response(JSON.stringify({ enabled: true, maxChars: 1500, voices: [
          { id: 'pocket_alba', name: 'Alba · Pocket', provider: 'Pocket TTS' }, { id: 'pocket_marius', name: 'Marius · Pocket', provider: 'Pocket TTS' }, { id: 'qwen_ryan', name: 'Ryan · Qwen', provider: 'Qwen3-TTS' }
        ] }), { headers: { 'content-type': 'application/json' } });
        if (!url.endsWith('/speech/stream')) return nativeFetch(input, init);
        const req = { body: JSON.parse(init.body), at: performance.now() }; window.__pocket.requests.push(req); const signal = init.signal; let timer;
        const bytes = new Uint8Array(24000); const frame = new Uint8Array(4 + bytes.length); new DataView(frame.buffer).setUint32(0, bytes.length); frame.set(bytes, 4);
        const stream = new ReadableStream({ start(c) { timer = setTimeout(() => { for(let i=0;i<window.__pocket.duration*2;i++)c.enqueue(frame); c.enqueue(new Uint8Array([0, 0, 0, 0])); c.close(); }, 30); }, cancel() { clearTimeout(timer); window.__pocket.cancels++; } });
        signal?.addEventListener('abort', () => { clearTimeout(timer); window.__pocket.cancels++; }, { once: true }); return new Response(stream, { headers: { 'content-type': 'application/vnd.artifact.pcm' } });
      };
    });
    const page = await context.newPage(), errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.goto(`${BASE}/${id}`, { waitUntil: 'domcontentloaded' }); await page.locator('#vreader-toggle').waitFor({ state: 'visible' }); await page.locator('#vreader-toggle').click(); await page.locator('#vreader-expand').click();
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
    await page.evaluate(()=>{__pocket.requests=[];__pocket.duration=25;});
    await page.locator('#vreader-sleep').selectOption('15');await page.locator('#vreader-play').click();
    await page.waitForFunction(()=>document.querySelector('.vreader').dataset.streamState==='playing');await wait(14000);
    await page.locator('#vreader-play').click();
    const beforeRewind=JSON.parse(await checkpoint()), requestCount=await page.evaluate(()=>__pocket.requests.length);
    assert.ok(beforeRewind.offset>15,'stream did not reach 15 seconds');
    await page.locator('#vreader-rewind').click();
    const afterRewind=JSON.parse(await checkpoint());
    assert.ok(Math.abs(beforeRewind.offset-afterRewind.offset-15)<.05,'stream rewind did not move 15 media seconds');
    assert.equal(await page.evaluate(()=>__pocket.requests.length),requestCount,'current stream rewind refetched audio');
    assert.equal(await page.locator('#vreader-play').textContent(),'Resume','rewind changed paused state');

    await page.evaluate(()=>{window.__clockOffset=16*60000;document.dispatchEvent(new Event('visibilitychange'));});
    assert.match(await readStatus(),/Sleep timer ended/);assert.equal(await page.locator('#vreader-play').textContent(),'Play');
    assert.equal(await page.locator('#vreader-sleep').inputValue(),'off');assert.ok(JSON.parse(await checkpoint()).offset>0);
    const beforePreview=await checkpoint();
    await page.evaluate(()=>{__clockOffset=0;__pocket.duration=.5;});
    await page.locator('#vreader-preview').click();await wait(100);
    assert.equal(await checkpoint(),beforePreview,'preview changed existing checkpoint during playback');
    await page.waitForFunction(()=>document.querySelector('#vreader-status').textContent.includes('Preview finished'),null,{timeout:5000});
    assert.equal(await checkpoint(),beforePreview,'preview changed existing checkpoint after completion');
    // Resume skips a fully completed chunk and must progress, not leave loading stuck.
    await page.evaluate(()=>{window.__clockOffset=0;__pocket.duration=.5;const k=Object.keys(localStorage).find(k=>k.startsWith('artifact-reader-place:'));const v=JSON.parse(localStorage.getItem(k));v.offset=.5;localStorage.setItem(k,JSON.stringify(v));});
    await page.locator('#vreader-resume').click();await finished();
    assert.match(await readStatus(),/Finished/);
    assert.deepEqual(errors,[]);
    console.log(JSON.stringify({sectionScope:true,endSectionNoPrefetch:true,wallClockSleep:true,fullChunkResume:true,streamRewind15:true,previewCheckpointIsolation:true,errors}));
  } finally { if (browser) await browser.close(); server.kill('SIGTERM'); await fs.rm(dir, { recursive: true, force: true }); }
})().catch(e => { console.error(e.stack); process.exitCode = 1; });
