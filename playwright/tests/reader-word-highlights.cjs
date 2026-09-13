// Run with: node playwright/tests/reader-word-highlights.cjs
const {chromium}=require('playwright');
const fs=require('node:fs'),http=require('node:http'),assert=require('node:assert/strict'),path=require('node:path');
(async()=>{
 const bridge=fs.readFileSync(path.join(__dirname,'../../assets/reader-bridge.js'),'utf8');
 const text='A quiet sentence repeats through the selected passage. '.repeat(15);
 const html='<main><p id="a">Beginning <em>'+text+'</em></p><p id="b">Final<br>destination and café 😀 words.</p></main><script>'+bridge+'</script>';
 const server=http.createServer((req,res)=>{res.setHeader('Content-Type','text/html; charset=utf-8');res.end('<iframe sandbox="allow-scripts" style="width:95vw;height:90vh"></iframe><script>window.messages=[];addEventListener("message",e=>messages.push(e.data));document.querySelector("iframe").srcdoc='+JSON.stringify(html).replace(/<\//g,'<\\/')+'</script>')});
 await new Promise(r=>server.listen(0,'127.0.0.1',r));let browser;
 try{
  browser=await chromium.launch({headless:true,args:['--no-sandbox']});const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));await page.goto('http://127.0.0.1:'+server.address().port);const frame=page.frames().find(f=>f!==page.mainFrame());await frame.waitForFunction(()=>typeof CSS.highlights?.set==='function');
  const send=message=>page.evaluate(m=>document.querySelector('iframe').contentWindow.postMessage(m,'*'),message);
  const marked=name=>frame.evaluate(n=>{const h=CSS.highlights.get(n);return h?[...h].map(r=>r.toString()).join(''):null},name);
  await frame.evaluate(()=>{document.body.focus();const r=document.createRange();r.selectNodeContents(document.querySelector('main'));getSelection().removeAllRanges();getSelection().addRange(r)});await page.waitForTimeout(50);
  await send({type:'reader:extract',requestId:'selection-1',mode:'selection'});await page.waitForFunction(()=>messages.some(m=>m.requestId==='selection-1'));const content=await page.evaluate(()=>messages.find(m=>m.requestId==='selection-1'));
  const whole=content.blocks[0].text;assert.ok(whole.length>600);assert.ok(whole.includes('Final destination'));
  await send({type:'reader:highlight',id:'selection'});await frame.waitForFunction(()=>CSS.highlights.has('artifact-reader-passage'));
  const offset=whole.indexOf('Final destination');const chunk=whole.slice(offset);
  await send({type:'reader:word',id:'selection',text:chunk,offset,start:6,end:17});await frame.waitForFunction(()=>CSS.highlights.has('artifact-reader-word'));assert.equal(await marked('artifact-reader-word'),'destination');assert.equal(await marked('artifact-reader-passage'),null);assert.equal(await frame.evaluate(()=>getSelection().toString()),'');
  // Clearing native selection while the iframe owns focus must not erase the playback map.
  await page.waitForTimeout(50);const cafe=chunk.indexOf('café');await send({type:'reader:word',id:'selection',text:chunk,offset,start:cafe,end:cafe+4});await page.waitForTimeout(50);assert.equal(await marked('artifact-reader-word'),'café');
  await send({type:'reader:word',id:'selection',text:'mismatched text',offset:0,start:0,end:3});await frame.waitForFunction(()=>!CSS.highlights.has('artifact-reader-word'));assert.ok(await marked('artifact-reader-passage'));
  await send({type:'reader:highlight',id:null});await frame.waitForFunction(()=>CSS.highlights.size===0);
  await send({type:'reader:extract',requestId:'page-1',mode:'page'});await page.waitForFunction(()=>messages.some(m=>m.requestId==='page-1'));const block=await page.evaluate(()=>messages.find(m=>m.requestId==='page-1').blocks.find(b=>b.text.startsWith('Final')));await send({type:'reader:highlight',id:block.id});await send({type:'reader:word',id:block.id,text:block.text,offset:0,start:6,end:17});await frame.waitForFunction(()=>CSS.highlights.has('artifact-reader-word'));assert.equal(await marked('artifact-reader-word'),'destination');
  await frame.evaluate(()=>document.querySelector('#b').firstChild.data='Changed');await frame.waitForFunction(()=>CSS.highlights.size===0);assert.deepEqual(errors,[]);
  console.log(JSON.stringify({longSelection:true,chunkOffsets:true,multipleParagraphs:true,lineBreak:true,nestedMarkup:true,nativeSelectionClear:true,mappingFallback:true,stop:true,mutation:true,errors}));
 }finally{await browser?.close();server.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
