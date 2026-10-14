import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';

const bridgeSource = readFileSync(new URL('../assets/reader-bridge.js', import.meta.url), 'utf8');
const bridgeHelpers = bridgeSource.slice(bridgeSource.indexOf('  function sanitizeReaderCast('), bridgeSource.indexOf('  function allowed('));
const {sanitizeReaderCast, resolveReaderRole, splitReaderRoleSegments} = runInNewContext(`${bridgeHelpers};({sanitizeReaderCast,resolveReaderRole,splitReaderRoleSegments})`, {TextEncoder});
const shellSource = readFileSync(new URL('../assets/shell.js', import.meta.url), 'utf8');
const shellHelpers = shellSource.slice(shellSource.indexOf('    function defaultRoleVoices('), shellSource.indexOf('    function styleChanged('));
const {defaultRoleVoices} = runInNewContext(`${shellHelpers};({defaultRoleVoices})`);

test('reader cast validation accepts only the bounded declared contract', () => {
  const good = sanitizeReaderCast(JSON.stringify({version:1, roles:[{id:'narrator', label:' Narrator ', description:'\u0001A guide', suggest:['pocket_alba']}]}));
  assert.deepEqual(JSON.parse(JSON.stringify(good)), {version:1, default:'narrator', roles:[{id:'narrator', label:'Narrator', description:'A guide', suggest:['pocket_alba']}]});
  for (const value of [
    'x'.repeat(16 * 1024 + 1),
    JSON.stringify({version:1,roles:[{id:'Bad',label:'Narrator'}]}),
    JSON.stringify({version:1,roles:[{id:'narrator',label:'Narrator'},{id:'narrator',label:'Again'}]}),
    JSON.stringify({version:1,default:'missing',roles:[{id:'narrator',label:'Narrator'}]}),
    JSON.stringify({version:1,roles:Array.from({length:25},(_,i)=>({id:`r${i}`,label:'Role'}))}),
  ]) assert.equal(sanitizeReaderCast(value), null);
});

test('reader roles use the nearest ancestor and fall back to default', () => {
  const cast = {default:'narrator',roles:[{id:'narrator'},{id:'victor'}]};
  const root = {parentElement:null,getAttribute:()=>null};
  const outer = {parentElement:root,getAttribute:name=>name === 'data-artifact-reader-voice' ? 'victor' : null};
  const inner = {parentElement:outer,getAttribute:name=>name === 'data-artifact-reader-voice' ? 'narrator' : null};
  const unknown = {parentElement:root,getAttribute:name=>name === 'data-artifact-reader-voice' ? 'unknown' : null};
  assert.equal(resolveReaderRole(inner, root, cast), 'narrator');
  assert.equal(resolveReaderRole(outer, root, cast), 'victor');
  assert.equal(resolveReaderRole(unknown, root, cast), 'narrator');
  assert.equal(resolveReaderRole(inner, root, null), null);
});

test('inline role segments preserve pronunciation offsets', () => {
  const segments = splitReaderRoleSegments([
    {text:'Dear ',role:'narrator',positions:[0,1,2,3,4]},
    {text:'Victor',role:'victor',positions:[5,6,7,8,9,10],pronunciations:[{start:0,end:6,text:'Vik-tor'}]},
    {text:', welcome.',role:'narrator',positions:[11,12,13,14,15,16,17,18,19,20]},
  ]);
  assert.deepEqual(JSON.parse(JSON.stringify(segments)), [
    {text:'Dear',role:'narrator',positions:[0,1,2,3]},
    {text:'Victor',role:'victor',positions:[5,6,7,8,9,10],pronunciations:[{start:0,end:6,text:'Vik-tor'}]},
    {text:', welcome.',role:'narrator',positions:[11,12,13,14,15,16,17,18,19,20]},
  ]);
});

test('default role voices prefer suggestions and avoid the main voice', () => {
  const cast = {default:'narrator',roles:[{id:'narrator',suggest:['pocket_alba']},{id:'victor',suggest:['missing']},{id:'letter',suggest:[]}]};
  assert.deepEqual(JSON.parse(JSON.stringify(defaultRoleVoices(cast, [{id:'pocket_alba'},{id:'qwen_ryan'},{id:'qwen_aiden'}], 'qwen_ryan'))),
    {narrator:'',victor:'qwen_aiden',letter:'pocket_alba'});
});
