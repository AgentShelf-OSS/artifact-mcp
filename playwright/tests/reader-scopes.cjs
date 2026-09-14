// Synthetic browser contract tests for mixed-content reading scopes.
// Run with: node playwright/tests/reader-scopes.cjs
const { chromium } = require('playwright');
const fs = require('node:fs');
const assert = require('node:assert/strict');
const path = require('node:path');

const bridge = fs.readFileSync(path.resolve(__dirname, '../../assets/reader-bridge.js'), 'utf8');

(async () => {
  const browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] });
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    let serial = 0;

    async function setup(html) {
      await page.setContent('<iframe id="reader" style="width:95vw;height:90vh" sandbox="allow-scripts"></iframe>');
      await page.evaluate(source => {
        window.readerMessages = [];
        window.addEventListener('message', event => window.readerMessages.push(event.data));
        document.querySelector('#reader').srcdoc = source;
      }, html + '<script>' + bridge + '</script>');
      await page.waitForFunction(() => window.readerMessages.some(message => message.type === 'reader:ready'));
    }

    const frame = () => page.frames()[1];
    async function send(data) {
      await page.evaluate(data => document.querySelector('#reader').contentWindow.postMessage(data, '*'), data);
    }
    async function messageFor(requestId) {
      await page.waitForFunction(id => window.readerMessages.some(message => message.requestId === id), requestId);
      return page.evaluate(id => window.readerMessages.find(message => message.requestId === id), requestId);
    }
    async function extract(mode, extra = {}) {
      const requestId = `scope-${serial++}`;
      await send({ type: 'reader:extract', requestId, mode, ...extra });
      return messageFor(requestId);
    }
    async function clearMessages() { await page.evaluate(() => { window.readerMessages.length = 0; }); }
    async function changedCount() { return page.evaluate(() => window.readerMessages.filter(message => message.type === 'reader:changed').length); }

    await setup(`
      <main>
        <h1>Outside heading</h1><p id="outside">Outside text</p>
        <section id="view-a" role="tabpanel" aria-label="First view">
          <h2>First view</h2><p id="a-text">First view text.</p>
          <table id="small-table"><tr><th>Metric</th><th>Value</th></tr><tr id="row-a"><td>Users</td><td>42</td></tr></table>
        </section>
        <section id="view-b" role="tabpanel" hidden><h2>Second view</h2><p>Hidden view text.</p></section>
      </main>`);

    const view = await extract('view', { scopeKey: 'id:view-a' });
    assert.equal(view.type, 'reader:content');
    assert.equal(view.mode, 'view');
    assert.equal(view.scopeKey, 'id:view-a');
    assert.deepEqual(view.blocks.map(block => block.text), ['First view', 'First view text.', 'Metric: Users. Value: 42']);
    assert.ok(view.fingerprint, 'view response should include a fingerprint');

    await frame().locator('#a-text').dispatchEvent('pointerdown');
    const section = await extract('section');
    assert.ok(section.scopeKey, 'section response should include a resumable scope key');
    assert.deepEqual(section.blocks.map(block => block.text), ['First view', 'First view text.', 'Metric: Users. Value: 42']);

    const resumed = await (async () => {
      const requestId = `scope-${serial++}`;
      await send({ type: 'reader:resume', requestId, mode: 'view', scopeKey: view.scopeKey, fingerprint: view.fingerprint });
      return messageFor(requestId);
    })();
    assert.equal(resumed.type, 'reader:content');
    assert.equal(resumed.scopeKey, view.scopeKey);
    assert.equal(resumed.fingerprint, view.fingerprint);

    await clearMessages();
    await frame().locator('#outside').evaluate(node => { node.textContent = 'Outside changed but excluded.'; });
    await page.waitForTimeout(300);
    assert.equal(await changedCount(), 0, 'mutation outside the active view must not invalidate playback');

    await frame().locator('#a-text').evaluate(node => { node.textContent = 'First view changed.'; });
    await page.waitForFunction(() => window.readerMessages.some(message => message.type === 'reader:changed'));

    const hiddenRequest = `scope-${serial++}`;
    await frame().locator('#view-a').evaluate(node => { node.hidden = true; });
    await send({ type: 'reader:resume', requestId: hiddenRequest, mode: 'view', scopeKey: view.scopeKey, fingerprint: view.fingerprint });
    const hidden = await messageFor(hiddenRequest);
    assert.equal(hidden.type, 'reader:error', 'hidden views must refuse resume');

    await setup(`
      <main>
        <section id="tabs" role="tablist"><button role="tab" aria-selected="true">Tab A</button><button role="tab">Tab B</button></section>
        <section id="active" role="tabpanel"><h2>Active tab panel</h2><p>Panel content.</p></section>
        <section id="inactive" role="tabpanel" hidden><p>Inactive panel.</p></section>
      </main>`);
    const activeView = await extract('view');
    assert.equal(activeView.scopeKey, 'id:active');
    assert.deepEqual(activeView.blocks.map(block => block.text), ['Active tab panel', 'Panel content.']);
    assert.equal(await changedCount(), 0, 'view extraction should not require a click to identify the active tab panel');

    await setup(`
      <main>
        <table id="large"><caption>Sales by region</caption>
          <tr><th>Region</th><th>Sales</th></tr>
          ${Array.from({ length: 20 }, (_, index) => `<tr id="sales-${index}"><td>Region ${index + 1}</td><td>${(index + 1) * 10}</td></tr>`).join('')}
        </table>
        <pre id="code">const answer = 42;\nconsole.log(answer);</pre>
        <div id="visual" data-artifact-reader-detail="A scatter plot showing sales rising over time."><svg aria-label="plot"><text>axis labels</text></svg></div>
      </main>`);
    const large = await extract('page');
    assert.deepEqual(large.blocks.map(block => block.text), [
      'Sales by region',
      'Code block. Use Read details for literal reading.',
      'plot'
    ]);

    const tableDetail = await extract('detail', { scopeKey: 'id:large' });
    assert.equal(tableDetail.scopeKey, 'id:large');
    assert.equal(tableDetail.blocks[0].text, 'Sales by region');
    assert.equal(tableDetail.blocks.length, 21, 'large-table details should include caption and all visible data rows');
    assert.equal(tableDetail.blocks.at(-1).text, 'Region: Region 20. Sales: 200');

    const codeDetail = await extract('detail', { scopeKey: 'id:code' });
    assert.equal(codeDetail.blocks[0].text, 'const answer = 42; console.log(answer);');

    const visualDetail = await extract('detail', { scopeKey: 'id:visual' });
    assert.equal(visualDetail.blocks[0].text, 'A scatter plot showing sales rising over time.');

    await send({ type: 'reader:pick-mode', enabled: true });
    await frame().locator('#sales-7 td').first().click();
    await page.waitForFunction(() => window.readerMessages.some(message => message.type === 'reader:target'));
    const target = await page.evaluate(() => window.readerMessages.find(message => message.type === 'reader:target'));
    assert.equal(target.detailAvailable, true);
    assert.equal(target.scopeKey, 'id:sales-7');
    assert.equal(target.ordinal, 0, 'selected detail row should map to the collapsed table reading block');

    const selectedRow = await extract('detail', { scopeKey: 'path:' + await frame().locator('#sales-7').evaluate(node => {
      const indexes = []; for (let current = node; current && current !== document.documentElement; current = current.parentElement) indexes.unshift([...current.parentElement.children].indexOf(current)); return indexes.join('.');
    }) });
    assert.equal(selectedRow.blocks[0].text, 'Region: Region 8. Sales: 80');
    assert.deepEqual(errors, [], 'reader scope fixtures should not produce page errors');
    console.log('PASS: view/detail/section scopes, scoped resume fingerprints, mutation invalidation, hidden-view rejection, active tab selection, explicit large-table/code/visual details, selected table row');
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
