import assert from 'node:assert/strict';
import {test} from 'node:test';
import {usageLimits,limitEvent,nativeItem,usageTokens,InputQueue} from '../scripts/claude_bridge/features.mjs';

test('subscription windows use percentages and seconds',()=>{
  const raw={rate_limits_available:true,subscription_type:'max',rate_limits:{
    five_hour:{utilization:11,resets_at:'2026-09-22T08:00:00Z'},seven_day:{utilization:4},
    model_scoped:[{display_name:'Fable',utilization:7}],extra_usage:{is_enabled:true}}};
  const actual=usageLimits(raw);
  assert.equal(actual.rateLimits.primary.usedPercent,11);
  assert.equal(actual.rateLimits.primary.resetsAt,Date.parse(raw.rate_limits.five_hour.resets_at)/1000);
  assert.equal(actual.rateLimits.primary.windowDurationMins,300);
  assert.equal(actual.rateLimits.secondary.windowDurationMins,10080);
  assert.equal(actual.rateLimitsByLimitId['claude-fable'].secondary.usedPercent,7);
  assert.deepEqual(actual.extraUsage,{is_enabled:true});
  assert.throws(()=>usageLimits({rate_limits_available:false}),/did not return/);
});
test('invalid or absent utilization never becomes zero usage',()=>{
  const actual=usageLimits({rate_limits_available:true,rate_limits:{five_hour:{utilization:null,resets_at:'invalid'}}});
  assert.equal(actual.rateLimits.primary.usedPercent,null);
  assert.equal(actual.rateLimits.primary.resetsAt,null);
  assert.ok(actual.rateLimits.secondary==null);
});
test('native events use fractional utilization without mutating cached windows',()=>{
  const initial=usageLimits({rate_limits_available:true,rate_limits:{five_hour:{utilization:11},seven_day:{utilization:4}}});
  const updated=limitEvent(initial,{rateLimitType:'five_hour',utilization:0.25,resetsAt:123});
  assert.equal(updated.rateLimits.primary.usedPercent,25);
  assert.equal(updated.rateLimits.primary.resetsAt,123);
  assert.equal(initial.rateLimits.primary.usedPercent,11);
  assert.equal(updated.rateLimits.secondary.usedPercent,4);
  assert.equal(limitEvent(initial,{rateLimitType:'unrecognized',utilization:0.1}),null);
  assert.equal(limitEvent(initial,{rateLimitType:'five_hour',utilization:null}),null);
});
test('cached and created input tokens remain part of total usage',()=>{
  assert.deepEqual(usageTokens({input_tokens:10,cache_read_input_tokens:20,cache_creation_input_tokens:5,output_tokens:3}),
    {inputTokens:35,cachedInputTokens:20,outputTokens:3,totalTokens:38});
  assert.equal(usageTokens(null),null);
});
test('native tools preserve commands and file edits',()=>{
  const command=nativeItem({id:'bash',name:'Bash',input:{command:'pwd'}},'/work');
  assert.equal(command.type,'commandExecution');assert.equal(command.command,'pwd');assert.equal(command.cwd,'/work');
  const edit=nativeItem({id:'edit',name:'Edit',input:{file_path:'/work/a',old_string:'before',new_string:'after'}},'/work');
  assert.equal(edit.type,'fileChange');assert.equal(edit.changes[0].path,'/work/a');assert.match(edit.changes[0].diff,/before/);
  assert.equal(nativeItem({id:'read',name:'Read',input:{file_path:'/work/a'}},'/work').type,'mcpToolCall');
});
test('persistent queue preserves order and close releases an idle consumer',async()=>{
  const queue=new InputQueue(),iterator=queue[Symbol.asyncIterator]();
  const pending=iterator.next();queue.push('one');queue.push('two');
  assert.deepEqual(await pending,{value:'one',done:false});
  assert.deepEqual(await iterator.next(),{value:'two',done:false});
  const idle=iterator.next();queue.close();assert.equal((await idle).done,true);
  assert.throws(()=>queue.push('late'),/closed/);
});
