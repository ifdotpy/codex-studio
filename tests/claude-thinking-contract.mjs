import assert from 'node:assert/strict';
import {test} from 'node:test';
import {requiresThinking,thinkingFlag} from '../scripts/claude_bridge/thinking.mjs';

test('aliases follow each account catalog and explicit IDs keep their own version',()=>{
  const current=[{value:'default',resolvedModel:'claude-opus-5-5'},
    {model:'opus[1m]',description:'Opus 5.5 with 1M context · Best for everyday tasks'},
    {model:'fable',description:'Fable 5.1 · Most capable'}];
  for(const id of ['default','opus[1m]','fable','claude-opus-5-5','claude-fable-5-1[1m]'])assert.equal(requiresThinking(id,current),true,id);
  assert.equal(requiresThinking('default',[{value:'default',resolvedModel:'claude-opus-5'}]),false);
  for(const id of ['claude-opus-5','claude-opus-4-8','claude-sonnet-5','custom-model','default'])assert.equal(requiresThinking(id,[]),false,id);
  assert.equal(requiresThinking('custom',[{model:'custom',description:'Use Fable 5.1 for difficult tasks'}]),false);
});
test('fresh and persistent queries preserve the native default differently',()=>{
  assert.deepEqual(thinkingFlag('sonnet',[],undefined),{});
  assert.deepEqual(thinkingFlag('sonnet',[],undefined,true),{alwaysThinkingEnabled:null});
  assert.deepEqual(thinkingFlag('sonnet',[],false,true),{alwaysThinkingEnabled:false});
  assert.deepEqual(thinkingFlag('claude-opus-5-5',[],false),{alwaysThinkingEnabled:true});
});
