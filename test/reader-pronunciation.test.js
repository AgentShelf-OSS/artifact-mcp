import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
const source=readFileSync(new URL('../assets/shell.js',import.meta.url),'utf8');
const helpers=source.slice(source.indexOf('    function pronunciationPlan('),source.indexOf('    function splitLongText('));
const {pronunciationPlan:plan,pronunciationRange:range}=runInNewContext(helpers+';({pronunciationPlan,pronunciationRange})');
const plain=value=>JSON.parse(JSON.stringify(value));
test('pronunciation preserves original offsets before and after expansion',()=>{
 const p=plan('SQL and SQL.',[{start:0,end:3,text:'S Q L'},{start:8,end:11,text:'sequel'}]);
 assert.equal(p.text,'S Q L and sequel.');
 assert.deepEqual(plain(range(p,2,3)),{start:0,end:3});
 assert.deepEqual(plain(range(p,6,9)),{start:4,end:7});
 assert.deepEqual(plain(range(p,10,16)),{start:8,end:11});
});
test('a replacement split between requests still maps to its whole original term',()=>{
 const p=plan('Name.',[{start:0,end:4,text:'a very long spoken name'}]);
 assert.deepEqual(plain(range(p,0,6)),{start:0,end:4});
 assert.deepEqual(plain(range(p,6,22)),{start:0,end:4});
});
test('invalid or overlapping hints leave narration unchanged',()=>{
 for(const hints of [[{start:0,end:5,text:'wrong'}],[{start:0,end:3,text:'one'},{start:2,end:3,text:'two'}],[{start:0,end:3,text:'\u0000'}],[{start:0,end:3,text:'x'.repeat(201)}]]) assert.equal(plan('SQL',hints).text,'SQL');
});
test('UTF16 offsets preserve emoji and accented display text',()=>{
 const p=plan('😀 René.',[{start:3,end:7,text:'ruh nay'}]);
 assert.equal(p.text,'😀 ruh nay.');assert.deepEqual(plain(range(p,3,6)),{start:3,end:7});
});
