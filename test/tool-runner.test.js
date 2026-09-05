import test from 'node:test';
import assert from 'node:assert/strict';
import {createToolRunner} from '../src/tool-runner.js';
test('serializes tools and skips cancelled queued requests', async()=>{
 const run=createToolRunner({timeoutMs:1000});let release;const events=[];
 const a=run('a',async()=>{events.push('a');await new Promise(r=>release=r);events.push('done');return {};});
 await new Promise(r=>setImmediate(r));const c=new AbortController();const b=run('b',async()=>{events.push('b');return {};},{signal:c.signal});c.abort();release();await Promise.all([a,b]);assert.deepEqual(events,['a','done']);
});
test('timeout aborts actual operation', async()=>{
 const run=createToolRunner({timeoutMs:10});let aborted=false;
 const r=await run('slow',signal=>new Promise((_,reject)=>signal.addEventListener('abort',()=>{aborted=true;reject(signal.reason);},{once:true})));
 assert.equal(r.isError,true);assert.equal(aborted,true);assert.match(r.content[0].text,/timed out/);
});
test('oversized output is an explicit error, normal output remains valid JSON',async()=>{
 const run=createToolRunner({maxText:20});assert.equal((await run('large',async()=>({text:'x'.repeat(30)}))).isError,true);
 assert.deepEqual(JSON.parse((await run('small',async()=>({ok:1}))).content[0].text),{ok:1});
});

test('bounds error output as well as success output',async()=>{
 const run=createToolRunner({maxText:60});const r=await run('bad',async()=>{throw Error('x'.repeat(100000));});
 assert.equal(r.isError,true);assert.ok(r.content[0].text.length<=60);
});
