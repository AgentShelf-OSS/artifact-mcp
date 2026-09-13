const { chromium } = require('playwright');
const fs = require('fs');
const bridge = fs.readFileSync(require('node:path').resolve(__dirname, '../../assets/reader-bridge.js'), 'utf8');
const ereader = fs.readFileSync(require('node:path').resolve(__dirname, '../../assets/reader-ereader.js'), 'utf8');
(async () => {
  const browser = await chromium.launch({headless:true,args:['--no-sandbox']});
  try {
  const page = await browser.newPage(); const messages=[];
  await page.exposeFunction('captureReaderMessage', m => messages.push(m)); await page.setContent('<iframe id="book"></iframe>'); const frame=page.frames()[1];
  await page.evaluate(() => window.addEventListener('message', e => window.captureReaderMessage(e.data)));
  const book={chapters:[{part:'Part One',title:'Chapter 1',blocks:[{t:'p',s:'First chapter text.'}]},{part:'Part One',title:'Chapter 2',blocks:[{t:'p',s:'Second chapter text.'}]}]};
  await frame.setContent(`<script id="book" type="application/json">${JSON.stringify(book)}</script><button id="t-contents" data-panel="contents">Contents</button><div class="toc" hidden><a href="#" data-go="0">One</a><a href="#" data-go="1">Two</a></div><main id="cover"><h1>Cover</h1><p>Cover text.</p></main><script>${ereader}</script><script>document.addEventListener('click',function(e){var go=e.target.closest('[data-go]');if(!go)return;e.preventDefault();var i=+go.getAttribute('data-go'),c=CH[i];document.getElementById('cover').outerHTML='<article id="article"><header class="ch-head"><div class="part">'+c.part+'</div><h1>'+c.title+'</h1></header><p data-p="0">'+c.blocks[0].s+'</p></article>';});document.getElementById('t-contents').addEventListener('click',function(){document.querySelector('.toc').hidden=false;});</script><script>${bridge}</script>`);
  await frame.evaluate(chapters => { window.CH = chapters; }, book.chapters);
  await page.evaluate(() => document.getElementById('book').contentWindow.postMessage({type:'reader:resume',requestId:'seek',mode:'page',chapterIndex:1,fingerprint:'wrong'},'*')); await page.waitForTimeout(150);
  if(!messages.some(m=>m.type==='reader:error'&&m.requestId==='seek'))throw new Error('cover seek/change rejection failed'); messages.length=0;
  await page.evaluate(() => document.getElementById('book').contentWindow.postMessage({type:'reader:extract',requestId:'page',mode:'page'},'*')); await page.waitForTimeout(80); const result=messages.find(m=>m.type==='reader:content'&&m.requestId==='page');
  if(!result||result.chapter.index!==1||!result.fingerprint)throw new Error('TOC seek fixture failed'); messages.length=0;
  await frame.locator('p[data-p]').evaluate(n=>{n.textContent='Changed chapter text.'}); await page.evaluate(fp=>document.getElementById('book').contentWindow.postMessage({type:'reader:resume',requestId:'changed',mode:'page',chapterIndex:1,fingerprint:fp},'*'),result.fingerprint); await page.waitForTimeout(100);
  if(!messages.some(m=>m.type==='reader:error'&&m.requestId==='changed'))throw new Error('changed DOM resume was accepted'); messages.length=0;
  await frame.locator('p[data-p]').evaluate((n, text) => { n.textContent = text; }, book.chapters[1].blocks[0].s);
  await page.evaluate(() => document.getElementById('book').contentWindow.postMessage({type:'reader:extract',requestId:'section',mode:'section'},'*')); await page.waitForTimeout(80); const section=messages.find(m=>m.type==='reader:content'&&m.requestId==='section');
  if(!section||section.blocks.length!==2||section.blocks[0].sectionEndOrdinal!==1){console.error(section);throw new Error('section boundary fixture failed');}
  await frame.setContent('<main><section><h1>Outer</h1><p>Outer intro</p><section><h2>Inner</h2><p id="inner">Inner text</p></section></section><section><h2>Sibling</h2><p>Sibling text</p></section></main><script>' + bridge + '</script>');
  messages.length=0;
  await frame.locator('#inner').dispatchEvent('pointerdown');
  await page.evaluate(() => document.getElementById('book').contentWindow.postMessage({type:'reader:extract',requestId:'nested',mode:'section'},'*'));
  await page.waitForTimeout(100);
  const nested=messages.find(m=>m.type==='reader:content'&&m.requestId==='nested');
  if(JSON.stringify(nested?.blocks.map(b=>b.text)) !== JSON.stringify(['Inner','Inner text'])) throw new Error('Nested section leaks outer or sibling text: '+JSON.stringify(nested));
  await frame.setContent('<main><h1>One</h1><p>First</p><h2>Two</h2><p id="two">Second</p><h2>Three</h2><p>Third</p></main><script>' + bridge + '</script>');
  messages.length=0;await frame.locator('#two').dispatchEvent('pointerdown');
  await page.evaluate(() => document.getElementById('book').contentWindow.postMessage({type:'reader:extract',requestId:'headings',mode:'section'},'*'));await page.waitForTimeout(100);
  const headings=messages.find(m=>m.type==='reader:content'&&m.requestId==='headings');
  if(JSON.stringify(headings?.blocks.map(b=>b.text)) !== JSON.stringify(['Two','Second'])) throw new Error('Heading section scope wrong: '+JSON.stringify(headings));
  console.log('reader bridge browser fixtures passed: TOC seek, fingerprint rejection, section bounds');
  } finally { await browser.close(); }
})().catch(e=>{console.error(e);process.exitCode=1});
