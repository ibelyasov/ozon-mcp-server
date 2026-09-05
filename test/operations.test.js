import test from 'node:test';
import assert from 'node:assert/strict';
import { createOperations, productPath } from '../src/operations.js';
const description = {widgetStates:{'webDescription-1':JSON.stringify({richAnnotation:'Product details'})}};
test('product paths constrain origin and normalize suffixes', () => {
  for (const value of ['123456','widget-123456','https://www.ozon.ru/product/widget-123456/reviews/?x=1']) assert.match(productPath(value), /^\/product\/(widget-)?123456\/$/);
  assert.equal(productPath('https://ozon.ru/product/%D0%BC%D1%8B%D1%88%D1%8C-123/'), '/product/мышь-123/');
  for (const value of ['https://evil.test/product/123456/', '//evil.test', '../cart', 'https://ozon.ru/cart', '/product/123%2f456', 'https://user@ozon.ru/product/123/']) assert.throws(()=>productPath(value));
});
test('search validates range before fetch and passes cancellation', async () => {
  let calls=0; const c=new AbortController();
  const ops=createOperations(async (path,{signal})=>{calls++;assert.equal(signal,c.signal);assert.match(path,/currency_price=10.000%3B20.000/);return {widgetStates:{}};});
  await assert.rejects(ops.search({query:'a',priceMin:20,priceMax:10})); assert.equal(calls,0);
  const result=await ops.search({query:'a',priceMin:10,priceMax:20},{signal:c.signal});assert.ok(result.warnings.includes('SEARCH_WIDGET_MISSING'));
});
test('description on base page skips redundant requests', async () => {
  let calls=0;const ops=createOperations(async()=>{calls++;return description;});
  const r=await ops.details({product:'123456'});assert.equal(calls,1);assert.equal(r.description.text,'Product details');
});
test('optional description failure is reported, cancellation is not swallowed', async () => {
  let calls=0;const ops=createOperations(async()=>{if(calls++)throw Error('403');return {widgetStates:{}};});
  const result=await ops.details({product:'123456'});assert.ok(result.warnings.includes('DESCRIPTION_FETCH_FAILED'));
  const c=new AbortController();const cancelOps=createOperations(async()=>{c.abort(Error('cancelled'));return {};});
  await assert.rejects(cancelOps.details({product:'123456'},{signal:c.signal}),/cancelled/);
});
