#!/usr/bin/env python3
"""Exercise the bridge protocol with a deterministic SDK. No model calls."""
import json
import os
import uuid
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
SDK = r'''
import fs from 'node:fs';
import fsp from 'node:fs/promises';
let beginRace,releaseRace;
const raceStarted=new Promise(resolve=>beginRace=resolve);
const originalRead=fsp.readFile.bind(fsp);
fsp.readFile=async(file,...args)=>{
 if(String(file).endsWith('/race.png')){
  const held=new Promise(resolve=>releaseRace=resolve);beginRace();await held;
 }
 return originalRead(file,...args);
};
export const tool=(name,description,schema,call)=>({name,call});
export const createSdkMcpServer=value=>value;
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
export function query({prompt,options}){
 if(options.systemPrompt&&!(options.disallowedTools||[]).includes('Agent'))throw new Error('Native subagents must stay disabled');
 let abort=new AbortController();
 let outputTotal=0;
 if(options.systemPrompt)fs.appendFileSync(options.cwd+'/.queries',JSON.stringify({resume:options.resume||null,sessionId:options.sessionId||null})+'\n');
 if(options.systemPrompt)fs.appendFileSync(options.cwd+'/.thinking-flags',JSON.stringify({phase:'initial',model:options.model,settings:options.settings})+'\n');
 return {
  supportedModels:async()=>[
   {value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5',supportsEffort:true,supportedEffortLevels:['low','medium','high']},
   {value:'opus[1m]',resolvedModel:'claude-opus-5-5'},
   {value:'sonnet',resolvedModel:'claude-sonnet-5'},
  ],
  accountInfo:async()=>({email:fs.existsSync(options.cwd+'/.wrong-account')?'different@example.test':'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[{name:'compact',description:'Compact history'}]}),
  usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET:async()=>({rate_limits_available:true,subscription_type:'max',rate_limits:{five_hour:{utilization:11,resets_at:'2026-09-22T08:00:00Z'},seven_day:{utilization:4},model_scoped:[{display_name:'Fable',utilization:7}]}}),
  setModel:async model=>{if(model==='reject-model')throw new Error('Native model rejected');if(fs.existsSync(options.cwd+'/.dead-query'))throw new Error('Claude Code process aborted by user');},setPermissionMode:async()=>{},setMcpServers:async servers=>{fs.appendFileSync(options.cwd+'/.mcp-sets',JSON.stringify(Object.keys(servers).map(name=>[name,servers[name].tools.map(t=>t.name)]))+'\n');return {added:[],removed:[],errors:{}};},applyFlagSettings:async settings=>{fs.appendFileSync(options.cwd+'/.thinking-flags',JSON.stringify({phase:'live',settings})+'\n');},stopTask:async()=>{},
  close(){if(options.systemPrompt)fs.appendFileSync(options.cwd+'/.query-closes','closed\n');abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   while(!abort.signal.aborted){
   const input=await prompt.next();
   if(input.done)return;
   if(!/^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/i.test(input.value.uuid))throw new Error('Native input requires UUID');
   let text=input.value.message.content[0].text;
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   if(text==='steer-race'){
    await raceStarted;
    yield {type:'result',subtype:'success',usage:{input_tokens:10,output_tokens:2},result:'Before steer'};
    releaseRace();continue;
   }
   if(text==='steer'){
    yield {type:'stream_event',event:{type:'message_start',message:{id:'waiting'}}};
    yield {type:'stream_event',event:{type:'content_block_delta',delta:{type:'text_delta',text:'Waiting for steer'}}};
    const next=await prompt.next();
    if(!/^[a-f0-9]{8}-(?:[a-f0-9]{4}-){3}[a-f0-9]{12}$/i.test(next.value.uuid))throw new Error('Native steer requires UUID');
    yield {type:'user',isReplay:true,uuid:next.value.uuid};
    text=next.value.message.content[0].text;
    if(text!=='replacement')throw new Error('Wrong steer');
    while(!fs.existsSync(options.cwd+'/.release-steer')&&!abort.signal.aborted)await new Promise(r=>setTimeout(r,5));
   }
   if(text==='plan'){
    const decision=await options.canUseTool('ExitPlanMode',{plan:'A visible plan'},{toolUseID:'plan',signal:abort.signal});
    if(decision.behavior!=='deny')throw new Error('Plan executed without a separate user turn');
   }
   if(text==='permission'){
    const decision=await options.canUseTool('Bash',{command:'echo hello'},{toolUseID:'tool-1',signal:abort.signal});
    if(decision.behavior!=='deny')throw new Error('Expected denial');
   }
   if(text==='question'){
    const decision=await options.canUseTool('AskUserQuestion',{questions:[{question:'Choose?',header:'Choice',options:[{label:'A',description:'First'}]}]},{toolUseID:'ask',signal:abort.signal});
    if(decision.updatedInput.answers['Choose?']!=='A')throw new Error('Missing answer');
   }
   if(text==='tool'){
    const tool=options.mcpServers.studio.tools[0];
    const result=await tool.call({text:'hello'});
    if(result.content[0].text!=='tool-ok')throw new Error('Tool result lost');
   }
   if(text==='wait')await new Promise(resolve=>abort.signal.aborted?resolve():abort.signal.addEventListener('abort',resolve,{once:true}));
   if(text==='fail')throw new Error('Native failure');
   if(text==='/compact')yield {type:'system',subtype:'local_command_output',uuid:'compact-result',content:'Native compact received'};
   if(text==='background'||text==='background-hold')yield {type:'system',subtype:'task_started',task_id:'worker-1',description:'Worker',task_type:'agent'};
   if(text==='claude-limit'){
    const resetsAt=Math.floor(Date.now()/1000)+3600;
    yield {type:'rate_limit_event',rate_limit_info:{status:'rejected',rateLimitType:'five_hour',resetsAt,utilization:1}};
    yield {type:'result',subtype:'error',errors:['Claude usage limit reached (five_hour). Resets at '+new Date(resetsAt*1000).toISOString()+'. The turn waits.']};
    continue;
   }
   if(text==='claude-offline'){
    const offline="API Error: Can't reach the API server \u2014 check your internet or DNS (ENOTFOUND)";
    yield {type:'assistant',error:'unknown',message:{id:'offline',content:[{type:'text',text:offline}]}};
    yield {type:'result',subtype:'success',is_error:true,result:offline};
    continue;
   }
   if(text.startsWith('Continue the previous turn'))fs.writeFileSync(options.cwd+'/.continued',text);
   if(text==='subagent-text'){
    yield {type:'assistant',parent_tool_use_id:'agent-tool',uuid:'worker-message',message:{id:'worker-answer',content:[
      {type:'text',text:'First worker paragraph'},{type:'text',text:'Second worker paragraph'}]}};
   }
   if(text==='snapshots'){
    yield {type:'system',subtype:'local_command_output',uuid:'command-output',content:'Native command output'};
    yield {type:'assistant',message:{id:'snapshot-only',content:[{type:'text',text:'Snapshot without stream'}]}};
    yield {type:'assistant',message:{id:'native-tool',content:[{type:'tool_use',id:'bash-output',name:'Bash',input:{command:'pwd'}}]}};
    yield {type:'user',message:{content:[{type:'tool_result',tool_use_id:'bash-output',content:'/work'}]}};
   }
   if(text==='split-blocks'){
    yield {type:'assistant',uuid:'11111111-1111-4111-8111-111111111111',message:{id:'split-message',content:[{type:'text',text:'First text block.'}],stop_reason:null}};
    yield {type:'assistant',uuid:'22222222-2222-4222-8222-222222222222',message:{id:'split-message',content:[{type:'text',text:'Second text block.'}],stop_reason:null}};
    yield {type:'assistant',uuid:'22222222-2222-4222-8222-222222222222',message:{id:'split-message',content:[{type:'text',text:'Second text block.'}],stop_reason:null}};
    yield {type:'result',subtype:'success',usage:{input_tokens:10,output_tokens:2},result:'First text block. Second text block.'};
    continue;
   }
   if(text==='rate-usage'){
    yield {type:'stream_event',event:{type:'message_start',message:{id:'rate-1'}}};
    yield {type:'stream_event',event:{type:'content_block_delta',delta:{type:'text_delta',text:'Rate answer'}}};
    for(const output of [6,6,8])yield {type:'assistant',message:{id:'rate-1',usage:{input_tokens:1000,output_tokens:output},content:[{type:'text',text:'Rate answer'}]}};
    yield {type:'assistant',message:{id:'rate-2',usage:{input_tokens:2000,output_tokens:12},content:[{type:'text',text:'Second answer'}]}};
    outputTotal+=20;
    yield {type:'result',subtype:'success',usage:{input_tokens:3000,output_tokens:outputTotal},result:'Second answer'};
    continue;
   }
   if(text==='split-stream'){
    yield {type:'stream_event',event:{type:'message_start',message:{id:'stream-blocks'}}};
    yield {type:'stream_event',event:{type:'content_block_delta',index:0,delta:{type:'text_delta',text:'First stream block.'}}};
    yield {type:'assistant',uuid:'11111111-1111-4111-8111-111111111111',message:{id:'stream-blocks',content:[{type:'text',text:'First stream block.'}],stop_reason:null}};
    yield {type:'stream_event',event:{type:'content_block_delta',index:1,delta:{type:'text_delta',text:'Second stream block.'}}};
    yield {type:'assistant',uuid:'22222222-2222-4222-8222-222222222222',message:{id:'stream-blocks',content:[{type:'text',text:'Second stream block.'}],stop_reason:null}};
    yield {type:'result',subtype:'success',usage:{input_tokens:10,output_tokens:2},result:'First stream block. Second stream block.'};
    continue;
   }
   yield {type:'stream_event',event:{type:'message_start',message:{id:'answer'}}};
   yield {type:'stream_event',event:{type:'content_block_delta',delta:{type:'text_delta',text:'Visible answer'}}};
   yield {type:'assistant',message:{id:'answer',content:[{type:'text',text:'Visible answer'},{type:'thinking',thinking:'Visible reasoning',signature:'PRIVATE_SIGNATURE'}]}};
   yield {type:'result',subtype:'success',usage:{input_tokens:10,output_tokens:2},result:'Visible answer'};
   if(text==='background'){
    yield {type:'assistant',parent_tool_use_id:'agent-tool',uuid:'late-worker',message:{id:'late-answer',content:[{type:'text',text:'Late worker text'}]}};
    yield {type:'system',subtype:'task_notification',task_id:'worker-1',status:'completed',summary:'Worker finished'};
   }
   if(text==='background-hold'){
    while(!fs.existsSync(options.cwd+'/.release-background')&&!abort.signal.aborted)await new Promise(r=>setTimeout(r,5));
    if(!abort.signal.aborted)yield {type:'system',subtype:'task_notification',task_id:'worker-1',status:'completed'};
   }
   }
  }
 };
}
'''


class Bridge(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='claude-bridge-test-')
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.root = root
        source = (ROOT / 'scripts/claude_bridge/bridge.mjs').read_text()
        for helper in (ROOT / 'scripts/claude_bridge').glob('*.mjs'):
            (root / helper.name).write_text(helper.read_text().replace("@anthropic-ai/claude-agent-sdk", "./fake.mjs"))
        (root / 'fake.mjs').write_text(SDK)
        dependency_modules = ROOT / 'scripts/claude_bridge/node_modules'
        if dependency_modules.exists():
            (root / 'node_modules').symlink_to(dependency_modules, target_is_directory=True)
        else:
            zod = root / 'node_modules' / 'zod'
            zod.mkdir(parents=True)
            (zod / 'package.json').write_text('{"name":"zod","type":"module","exports":"./index.js"}')
            (zod / 'index.js').write_text('export const z={fromJSONSchema:schema=>schema};')
        self.proc = subprocess.Popen([shutil.which('node'), str(root / 'bridge.mjs'), str(root / 'state')],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, 'STUDIO_CLAUDE_ACCOUNT':'test@example.test',
                 'STUDIO_CLAUDE_IDLE_SECONDS':'1'})
        self.addCleanup(self.close)
        self.rows = queue.Queue()
        threading.Thread(target=lambda: [self.rows.put(json.loads(line)) for line in self.proc.stdout], daemon=True).start()
        self.sequence = 0
        self.notifications = []
        self.approval = 'decline'
        self.thread = self.call('thread/start', {'cwd':str(root),'dynamicTools':[{'name':'echo','description':'Echo',
            'inputSchema':{'type':'object','properties':{'text':{'type':'string'}},'required':['text']}}]})['thread']['id']

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)
        self.proc.stdout.close()
        self.proc.stderr.close()

    def write(self, message):
        self.proc.stdin.write(json.dumps(message)+'\n'); self.proc.stdin.flush()

    def read(self):
        row = self.rows.get(timeout=10)
        if 'id' in row and 'method' in row:
            result = ({'decision':self.approval} if row['method'].endswith('requestApproval') else
                      {'answers':{'0':{'answers':['A']}}} if row['method'].endswith('requestUserInput') else
                      {'success':True,'contentItems':[{'type':'inputText','text':'tool-ok'}]})
            self.write({'id':row['id'],'result':result})
        return row

    def call(self, method, params):
        self.sequence += 1; request = self.sequence
        self.write({'id':request,'method':method,'params':params})
        while True:
            row = self.read()
            if row.get('id') == request:
                if 'error' in row: raise ValueError(row['error']['message'])
                return row['result']
            self.notifications.append(row)

    def turn(self, text, key):
        return self.call('turn/start', {'threadId':self.thread,'clientUserMessageId':key,
            'input':[{'type':'text','text':text}]})

    def completed(self):
        while True:
            row = self.read(); self.notifications.append(row)
            if row.get('method') == 'turn/completed': return row['params']['turn']

    def test_rate_uses_assistant_output_and_subtracts_persistent_query_total(self):
        for index in range(2):
            self.notifications = []
            turn = self.turn('rate-usage', 'rate-' + str(index))
            self.completed()
            samples = [row['params'] for row in self.notifications
                       if row.get('params', {}).get('tokenRateUsage')]
            self.assertEqual([sample['tokenRateUsage']['outputTokens'] for sample in samples], [6, 6, 8, 12])
            final = [row['params'] for row in self.notifications
                     if row.get('method') == 'thread/tokenUsage/updated'][-1]
            self.assertEqual(final['turnOutputTokens'], 20)
            self.assertTrue(all(sample['turnId'] == turn['turn']['id'] for sample in samples + [final]))
            self.assertTrue(all(sample['threadId'] == self.thread for sample in samples + [final]))
            self.assertFalse(any(row.get('method', '').startswith('studio/tokenRate') for row in self.notifications))

    def test_model_list_carries_the_resolved_model_of_each_alias(self):
        rows = {row['model']: row for row in self.call('model/list', {})['data']}
        self.assertEqual(rows['sonnet']['resolvedModel'], 'claude-sonnet-5')
        self.assertEqual(rows['default']['resolvedModel'], 'claude-opus-5-5')

    def test_stream_history_immediate_next_turn_and_duplicate_identity(self):
        first = self.turn('hello','one'); self.assertEqual(self.completed()['status'],'completed')
        self.turn('tool','two'); self.assertEqual(self.completed()['status'],'completed')
        duplicate = self.turn('hello','one'); self.assertEqual(first['turn']['id'],duplicate['turn']['id'])
        with self.assertRaisesRegex(ValueError,'different content'): self.turn('changed','one')
        history = self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns']
        self.assertEqual(len(history),2)
        texts=[item.get('text') for item in history[0]['items']]
        self.assertIn('Visible answer',texts)
        self.assertIn('Visible reasoning',texts)
        final=[i for i in history[0]['items'] if i.get('phase')=='final_answer']
        self.assertEqual(final[-1]['text'],'Visible answer')
        self.assertNotIn('PRIVATE_SIGNATURE',json.dumps(history))
        self.assertTrue(any(x.get('method')=='item/agentMessage/delta' for x in self.notifications))

    def test_idle_query_closes_and_next_turn_resumes_native_history(self):
        self.turn('hello', 'first')
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertEqual(self.call('claude/diagnostics', {})['liveQueries'], 1)
        deadline = time.monotonic() + 4
        while self.call('claude/diagnostics', {})['liveQueries'] and time.monotonic() < deadline:
            time.sleep(.1)
        self.assertEqual(self.call('claude/diagnostics', {})['liveQueries'], 0)
        self.assertEqual(self.call('claude/diagnostics', {})['sessionCache']['cachedSessions'], 0)
        self.turn('hello', 'second')
        self.assertEqual(self.completed()['status'], 'completed')
        starts = [json.loads(line) for line in (self.root / '.queries').read_text().splitlines()]
        self.assertEqual(len(starts), 2)
        self.assertEqual(starts[1]['resume'], starts[0]['sessionId'])
        self.assertEqual(len(self.call('thread/read', {'threadId': self.thread,
                                                     'includeTurns': True})['thread']['turns']), 2)

    def test_unsubscribe_evicts_then_reload_preserves_request_identity(self):
        original = self.turn('hello', 'stable-request')
        self.assertEqual(self.completed()['status'], 'completed')
        self.call('thread/unsubscribe', {'threadId': self.thread})
        self.assertEqual(self.call('claude/diagnostics', {})['sessionCache']['cachedSessions'], 0)
        restored = self.call('thread/read', {'threadId': self.thread, 'includeTurns': True})
        self.assertEqual(restored['thread']['turns'][0]['id'], original['turn']['id'])
        duplicate = self.turn('hello', 'stable-request')
        self.assertEqual(duplicate['turn']['id'], original['turn']['id'])

    def test_background_task_keeps_query_until_task_finishes(self):
        self.turn('background-hold', 'background-hold')
        self.assertEqual(self.completed()['status'], 'completed')
        time.sleep(1.5)
        state = self.call('claude/diagnostics', {})
        self.assertEqual((state['liveQueries'], state['backgroundQueries']), (1, 1))
        (self.root / '.release-background').write_text('done')
        deadline = time.monotonic() + 4
        while self.call('claude/diagnostics', {})['liveQueries'] and time.monotonic() < deadline:
            time.sleep(.1)
        self.assertEqual(self.call('claude/diagnostics', {})['liveQueries'], 0)

    def test_rejected_claude_rate_limit_has_verified_error_kind_and_reset(self):
        self.turn('claude-limit', 'limit')
        failed = self.completed()
        self.assertEqual(failed['status'], 'failed')
        self.assertEqual(failed['error']['codexErrorInfo'], 'rateLimitExceeded')
        self.assertGreater(failed['error']['resetsAt'], time.time())
        self.assertIn('Claude usage limit reached (five_hour). Resets at ', failed['error']['message'])
        self.assertTrue(any(row.get('method') == 'item/completed'
                            and 'Claude usage limit reached (five_hour)' in row.get('params', {}).get('item', {}).get('text', '')
                            for row in self.notifications))

    def test_claude_network_error_is_typed_and_empty_input_continues(self):
        self.turn('claude-offline', 'offline')
        failed = self.completed()
        self.assertEqual(failed['status'], 'failed')
        self.assertEqual(failed['error']['codexErrorInfo'], 'httpConnectionFailed')
        self.assertIn('ENOTFOUND', failed['error']['message'])
        self.call('turn/start', {'threadId': self.thread, 'input': []})
        self.assertEqual(self.completed()['status'], 'completed')
        self.assertTrue(any(self.root.rglob('.continued')))

    def test_dead_idle_query_is_replaced_for_the_next_turn(self):
        self.turn('hello', 'before-dead'); self.assertEqual(self.completed()['status'], 'completed')
        (self.root / '.dead-query').write_text('dead')
        try:
            started = self.turn('hello', 'after-dead')
            self.assertEqual(self.completed()['status'], 'completed')
        finally:
            (self.root / '.dead-query').unlink()
        history = self.call('thread/read', {'threadId': self.thread, 'includeTurns': True})['thread']['turns']
        self.assertEqual(history[-1]['id'], started['turn']['id'])
        self.assertEqual(history[-1]['items'][0]['id'], 'after-dead')

    def test_required_thinking_overrides_saved_off_and_optional_switch_restores_it(self):
        self.call('claude/settings', {'threadId': self.thread, 'settings': {'thinking': False}})
        for index, model in enumerate(('opus[1m]', 'sonnet', 'claude-fable-5-1[1m]')):
            self.call('turn/start', {'threadId': self.thread, 'model': model,
                'clientUserMessageId': 'thinking-' + str(index),
                'input': [{'type': 'text', 'text': 'hello'}]})
            self.assertEqual(self.completed()['status'], 'completed')
        flags = [json.loads(line) for line in (self.root / '.thinking-flags').read_text().splitlines()]
        self.assertEqual([row['phase'] for row in flags], ['initial', 'live', 'live'])
        self.assertEqual([row['settings']['alwaysThinkingEnabled'] for row in flags], [True, False, True])

    def test_required_thinking_switch_clears_override_when_preference_is_unset(self):
        for index, model in enumerate(('default', 'sonnet')):
            self.call('turn/start', {'threadId': self.thread, 'model': model,
                'clientUserMessageId': 'thinking-default-' + str(index),
                'input': [{'type': 'text', 'text': 'hello'}]})
            self.assertEqual(self.completed()['status'], 'completed')
        flags = [json.loads(line) for line in (self.root / '.thinking-flags').read_text().splitlines()]
        self.assertEqual(flags[0]['settings']['alwaysThinkingEnabled'], True)
        self.assertIsNone(flags[1]['settings']['alwaysThinkingEnabled'])

    def test_changed_studio_tools_reach_the_live_session_once(self):
        tool = lambda name: {'name': name, 'description': name, 'inputSchema': {'type': 'object', 'properties': {}}}
        for index, tools in enumerate(([tool('a')], [tool('a'), tool('b')], [tool('a'), tool('b')])):
            self.call('turn/start', {'threadId': self.thread, 'dynamicTools': tools,
                'clientUserMessageId': 'tools-' + str(index), 'input': [{'type': 'text', 'text': 'hello'}]})
            self.assertEqual(self.completed()['status'], 'completed')
        sets = [json.loads(line) for line in (self.root / '.mcp-sets').read_text().splitlines()]
        # The SDK keeps a registered in-process server, so the bridge removes it and adds the new one.
        self.assertEqual(sets, [[], [['studio', ['a', 'b']]]])

    def test_history_version_tracks_content_and_survives_read(self):
        before = self.call('thread/read', {'threadId':self.thread})['thread']
        again = self.call('thread/read', {'threadId':self.thread})['thread']
        self.assertEqual(before['historyVersion'], again['historyVersion'])
        self.turn('hello', 'version-one'); self.completed()
        after = self.call('thread/read', {'threadId':self.thread})['thread']
        self.assertNotEqual(before['historyVersion'], after['historyVersion'])
        self.assertEqual(after['historyVersion'], self.call('thread/read', {'threadId':self.thread})['thread']['historyVersion'])

    def test_metadata_responses_page_and_exclude_large_transcripts(self):
        session_path = self.root / 'state' / 'sessions' / (self.thread + '.json')
        saved = json.loads(session_path.read_text())
        large_id = str(uuid.uuid4())
        large_turns = [{'id': 'large-turn', 'status': 'completed', 'items': [
            {'id': 'large-item', 'type': 'agentMessage', 'text': 'x' * (22 * 1024 * 1024)}]}]
        large_saved = json.loads(json.dumps(saved))
        large_saved['session'].update(id=large_id, turns=large_turns)
        large_path = session_path.parent / (large_id + '.json')
        large_path.write_text(json.dumps(large_saved, separators=(',', ':')))
        large_metadata = json.loads((session_path.parent / (self.thread + '.meta.json')).read_text())
        large_metadata['id'] = large_id
        (session_path.parent / (large_id + '.meta.json')).write_text(json.dumps(large_metadata, separators=(',', ':')))
        for index in range(21):
            thread_id = str(uuid.uuid5(uuid.NAMESPACE_URL, 'claude-page-' + str(index)))
            clone = json.loads(json.dumps(saved))
            clone['session'].update(id=thread_id, turns=[])
            (session_path.parent / (thread_id + '.json')).write_text(json.dumps(clone, separators=(',', ':')))
            sidecar = json.loads((session_path.parent / (self.thread + '.meta.json')).read_text())
            sidecar['id'] = thread_id
            (session_path.parent / (thread_id + '.meta.json')).write_text(json.dumps(sidecar, separators=(',', ':')))

        cached_before_list = self.call('claude/diagnostics', {})['sessionCache']['cachedSessions']
        all_page = self.call('thread/list', {'limit': 50})
        cached_after_list = self.call('claude/diagnostics', {})['sessionCache']['cachedSessions']
        first = self.call('thread/list', {'limit': 5})
        second = self.call('thread/list', {'limit': 5, 'cursor': first['nextCursor']})
        self.assertEqual(len(all_page['data']), 23)
        self.assertEqual(cached_after_list, cached_before_list, 'metadata list does not load session bodies')
        self.assertEqual(len(first['data']), 5)
        self.assertEqual(len(second['data']), 5)
        self.assertEqual(first['nextCursor'], '5')
        self.assertEqual(second['nextCursor'], '10')
        self.assertTrue(all(row['turns'] == [] for row in all_page['data']))

        cached_before_read = self.call('claude/diagnostics', {})['sessionCache']['cachedSessions']
        read = self.call('thread/read', {'threadId': large_id, 'includeTurns': False})
        cached_after_read = self.call('claude/diagnostics', {})['sessionCache']['cachedSessions']
        self.assertEqual(cached_after_read, cached_before_read, 'metadata read does not load session bodies')
        self.assertNotIn('turns', read)
        self.assertEqual(read['thread']['turns'], [])
        full = self.call('thread/read', {'threadId': large_id, 'includeTurns': True})
        self.assertEqual(len(full['thread']['turns'][0]['items'][0]['text']), 22 * 1024 * 1024)
        resumed = self.call('thread/resume', {'threadId': large_id, 'excludeTurns': True})
        self.assertNotIn('turns', resumed)
        self.assertEqual(resumed['thread']['turns'], [])
        included = self.call('thread/resume', {'threadId': large_id, 'excludeTurns': False})
        self.assertEqual(len(included['thread']['turns'][0]['items'][0]['text']), 22 * 1024 * 1024)
        forked = self.call('thread/fork', {'threadId': self.thread, 'excludeTurns': True})
        self.assertNotIn('turns', forked)
        self.assertEqual(forked['thread']['turns'], [])

        list_bytes = len(json.dumps(all_page, separators=(',', ':')).encode())
        read_bytes = len(json.dumps(read, separators=(',', ':')).encode())
        resume_bytes = len(json.dumps(resumed, separators=(',', ':')).encode())
        old_wire = {'historyVersion': '0' * 64, 'id': large_id, 'cwd': large_saved['session'].get('cwd'),
                    'createdAt': large_saved['session'].get('createdAt'),
                    'updatedAt': large_saved['session'].get('updatedAt') or large_saved['session'].get('createdAt'),
                    'preview': large_saved['session'].get('preview') or '', 'name': large_saved['session'].get('name'),
                    'turns': large_turns, 'status': {'type': 'idle'}, 'modelProvider': 'claude'}
        legacy_list_bytes = len(json.dumps({'data': [old_wire] * 1 + all_page['data'][:-1], 'nextCursor': None}, separators=(',', ':')).encode())
        legacy_read = {**large_saved['session'], 'sandbox': None,
                       'thread': {**old_wire, 'turns': []}}
        legacy_resume = {**large_saved['session'], 'sandbox': None, 'thread': old_wire}
        legacy_read_bytes = len(json.dumps(legacy_read, separators=(',', ':')).encode())
        legacy_resume_bytes = len(json.dumps(legacy_resume, separators=(',', ':')).encode())
        self.assertLess(list_bytes, 100_000)
        self.assertLess(read_bytes, 10_000)
        self.assertLess(resume_bytes, 10_000)
        print(json.dumps({'syntheticSessionBytes': 22 * 1024 * 1024,
                          'sessions': 23, 'legacyListLowerBoundBytes': legacy_list_bytes,
                          'listBytes': list_bytes, 'legacyReadBytes': legacy_read_bytes,
                          'readMetadataBytes': read_bytes, 'legacyResumeBytes': legacy_resume_bytes,
                          'resumeExcludeTurnsBytes': resume_bytes}))

    def test_account_change_blocks_prompt_before_model_call(self):
        (self.root / '.wrong-account').touch()
        self.turn('hello','wrong-account')
        result=self.completed()
        self.assertEqual(result['status'],'failed')
        self.assertIn('account or subscription changed',result['error']['message'])
        self.assertFalse(any(x.get('method')=='item/agentMessage/delta' for x in self.notifications))

    def test_denial_question_failure_and_interrupt(self):
        for text in ('permission','question'):
            self.turn(text,text);self.assertEqual(self.completed()['status'],'completed')
        self.turn('fail','failure');self.assertEqual(self.completed()['status'],'failed')
        self.turn('wait','interruption')
        with self.assertRaisesRegex(ValueError,'different Claude turn'):
            self.call('turn/interrupt',{'threadId':self.thread,'turnId':'old-turn'})
        self.call('turn/interrupt',{'threadId':self.thread})
        # A completion can arrive while waiting for the interrupt receipt.
        done=[r['params']['turn'] for r in self.notifications if r.get('method')=='turn/completed']
        if done and done[-1]['status']=='interrupted': result=done[-1]
        else: result=self.completed()
        self.assertEqual(result['status'],'interrupted')
        with self.assertRaisesRegex(ValueError,'different Claude turn'): self.call('turn/steer',{'threadId':self.thread})
        # Like Codex, an interrupt without an active turn changes nothing and says so.
        with self.assertRaisesRegex(ValueError,'^no active turn to interrupt$'):
            self.call('turn/interrupt',{'threadId':self.thread,'turnId':result['id']})

    def test_thread_name_is_stored_like_codex(self):
        self.assertEqual(self.call('thread/name/set',{'threadId':self.thread,'name':'Review 7'}),{})
        self.assertEqual(self.call('thread/read',{'threadId':self.thread})['thread']['name'],'Review 7')
        with self.assertRaisesRegex(ValueError,'must not be empty'):
            self.call('thread/name/set',{'threadId':self.thread,'name':' '})

    def test_failed_setter_does_not_accept_or_duplicate_message(self):
        self.turn('hello','before-setter');self.completed()
        params={'threadId':self.thread,'clientUserMessageId':'retry-setter',
                'input':[{'type':'text','text':'hello'}],'model':'reject-model'}
        with self.assertRaisesRegex(ValueError,'Native model rejected'):
            self.call('turn/start',params)
        history=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns']
        self.assertEqual(len(history),1)
        self.call('turn/start',{**params,'model':'default'})
        self.assertEqual(self.completed()['status'],'completed')
        history=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns']
        self.assertEqual(len(history),2)

    def test_native_command_uses_raw_command_and_keeps_display_input(self):
        wrapped='Studio instructions\nUser: /compact\nClock: now'
        self.call('turn/start',{'threadId':self.thread,'clientUserMessageId':'compact-command',
                  'claudeCommand':'/compact','input':[{'type':'text','text':wrapped}]})
        self.assertEqual(self.completed()['status'],'completed')
        items=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns'][0]['items']
        self.assertEqual(items[0]['content'][0]['text'],wrapped)
        self.assertIn('Native compact received',[i.get('text') for i in items])

    def test_background_text_and_completion_after_parent_result(self):
        first=self.turn('background','background')['turn']['id']
        self.assertEqual(self.completed()['id'],first)
        while not any(r.get('method')=='item/completed' and
                      r.get('params',{}).get('item',{}).get('id')=='task:worker-1' and
                      r['params']['item']['status']=='completed' for r in self.notifications):
            self.notifications.append(self.read())
        state=self.call('claude/state',{'threadId':self.thread})
        self.assertEqual(state['tasks'],[])
        turns=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns']
        self.assertEqual(len(turns),1)
        self.assertEqual(turns[0]['id'],first)
        self.assertEqual(len([i for i in turns[0]['items'] if i['type']=='userMessage']),1)
        self.assertIn('Late worker text','\n'.join(i.get('text','') for i in turns[0]['items']))
        task=next(i for i in turns[0]['items'] if i['id']=='task:worker-1')
        self.assertEqual(task['status'],'completed')

    def test_all_subagent_text_blocks_remain_visible(self):
        self.turn('subagent-text','subagent-text');self.completed()
        items=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns'][0]['items']
        text='\n'.join(i.get('text','') for i in items if i['type']=='agentMessage')
        self.assertIn('First worker paragraph',text)
        self.assertIn('Second worker paragraph',text)

    def test_snapshots_commands_and_native_tool_results(self):
        self.turn('snapshots','snapshots');self.assertEqual(self.completed()['status'],'completed')
        items=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns'][0]['items']
        texts=[i.get('text') for i in items]
        self.assertIn('Native command output',texts)
        self.assertIn('Snapshot without stream',texts)
        command=next(i for i in items if i['id']=='bash-output')
        self.assertEqual(command['type'],'commandExecution')
        self.assertEqual(command['status'],'completed')
        self.assertEqual(command['aggregatedOutput'],'/work')

    def test_unresolved_rollback_blocks_native_mutations(self):
        thread='55555555-5555-4555-8555-555555555555'
        saved={'id':thread,'cwd':str(self.root),'turns':[],'started':False,
               'controlRequests':{'lost-receipt':{'kind':'rollback','status':'submitted','turnId':'old'}}}
        (self.root / 'state' / 'sessions' / (thread+'.json')).write_text(json.dumps(saved))
        for method,extra in [('turn/start',{'clientUserMessageId':'new','input':[{'type':'text','text':'hello'}]}),
                             ('thread/resume',{}),('thread/fork',{}),('claude/settings',{'settings':{}})]:
            with self.subTest(method=method), self.assertRaisesRegex(ValueError,'recovery'):
                self.call(method,{'threadId':thread,**extra})
        self.assertEqual(self.call('thread/read',{'threadId':thread,'includeTurns':True})['thread']['turns'],[])

    def test_native_limits_and_commands(self):
        result=self.call('account/rateLimits/read',{})
        self.assertEqual(result['rateLimits']['primary']['usedPercent'],11)
        self.assertEqual(result['rateLimits']['secondary']['usedPercent'],4)
        self.assertEqual(result['rateLimitsByLimitId']['claude-fable']['secondary']['usedPercent'],7)
        self.assertTrue(any(c['name']=='compact' for c in self.call('claude/commands',{})))

    def test_steer_reserves_turn_before_attachment_read(self):
        first=self.turn('steer-race','race-initial')['turn']['id']
        attachment=self.root / 'race.png';attachment.write_bytes(b'image-fixture')
        params={'threadId':self.thread,'expectedTurnId':first,
                'clientUserMessageId':'44444444-4444-4444-8444-444444444444',
                'input':[{'type':'text','text':'replacement'},{'type':'localImage','path':str(attachment)}]}
        self.assertEqual(self.call('turn/steer',params)['turnId'],first)
        completed=[r for r in self.notifications if r.get('method')=='turn/completed']
        self.assertEqual(completed,[], 'The original turn completed while accepted steer input was pending')
        self.assertEqual(self.completed()['id'],first)
        history=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns']
        self.assertEqual(len(history),1)
        self.assertEqual(len([i for i in history[0]['items'] if i['type']=='userMessage']),2)

    def test_steer_same_turn_and_original_identity(self):
        first=self.turn('steer','initial')['turn']['id']
        params={'threadId':self.thread,'expectedTurnId':first,'clientUserMessageId':'33333333-3333-4333-8333-333333333333',
                'input':[{'type':'text','text':'replacement'}]}
        with self.assertRaisesRegex(ValueError,'different Claude turn'):
            self.call('turn/steer',{**params,'expectedTurnId':'wrong'})
        result=self.call('turn/steer',params)
        self.assertEqual(result['turnId'],first)
        self.assertEqual(self.call('turn/steer',params)['turnId'],first)
        with self.assertRaisesRegex(ValueError,'different content'):
            self.call('turn/steer',{**params,'input':[{'type':'text','text':'different'}]})
        (self.root / '.release-steer').touch()
        self.assertEqual(self.completed()['id'],first)
        history=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns']
        self.assertEqual(len(history),1)
        users=[i for i in history[0]['items'] if i['type']=='userMessage']
        self.assertEqual([i['id'] for i in users],['initial','33333333-3333-4333-8333-333333333333'])

    def test_turn_start_steers_active_turn_and_deduplicates_client_id(self):
        self.assertEqual(self.call('initialize', {})['capabilities']['claudeVersion'], 14)
        first = self.turn('steer', 'start-initial')['turn']['id']
        params = {'threadId': self.thread, 'clientUserMessageId': 'start-followup',
                  'input': [{'type': 'text', 'text': 'replacement'}]}
        answer = self.call('turn/start', params)
        self.assertTrue(answer['steered'])
        self.assertEqual(answer['turn']['id'], first)
        self.assertEqual(self.call('turn/start', params)['turn']['id'], first)
        (self.root / '.release-steer').touch()
        self.assertEqual(self.completed()['id'], first)
        turns = self.call('thread/read', {'threadId': self.thread, 'includeTurns': True})['thread']['turns']
        self.assertEqual([i['id'] for i in turns[0]['items'] if i['type'] == 'userMessage'],
                         ['start-initial', 'start-followup'])

    def test_non_uuid_studio_ids_map_to_stable_sdk_uuids_for_start_and_steer(self):
        studio_id = 'child:worker:turn-123'
        first = self.turn('steer', studio_id)['turn']['id']
        steer_id = 'chat:message:recipient'
        params = {'threadId': self.thread, 'expectedTurnId': first,
                  'clientUserMessageId': steer_id,
                  'input': [{'type': 'text', 'text': 'replacement'}]}
        self.assertEqual(self.call('turn/steer', params)['turnId'], first)
        self.assertEqual(self.call('turn/steer', params)['turnId'], first)
        (self.root / '.release-steer').touch()
        self.assertEqual(self.completed()['status'], 'completed')
        turn = self.call('thread/read', {'threadId': self.thread, 'includeTurns': True})['thread']['turns'][0]
        self.assertEqual(turn['clientUserMessageId'], studio_id)
        users = [item for item in turn['items'] if item['type'] == 'userMessage']
        self.assertEqual([item['id'] for item in users], [studio_id, steer_id])
        for item in users:
            self.assertEqual(str(uuid.UUID(item['nativeId'])), item['nativeId'])
            self.assertEqual(item['nativeId'], str(uuid.uuid5(
                uuid.UUID('8d95e191-763a-4ee2-a462-7d27f981f138'), item['id'])))
        self.assertEqual(self.turn('steer', studio_id)['turn']['id'], first)

    def test_same_message_frames_preserve_each_text_block_and_final_answer(self):
        self.turn('split-blocks', 'split-input')
        self.assertEqual(self.completed()['status'], 'completed')
        turn = self.call('thread/read', {'threadId': self.thread, 'includeTurns': True})['thread']['turns'][0]
        answer = [item for item in turn['items'] if item['id'] == 'split-message']
        self.assertEqual(len(answer), 1)
        self.assertEqual(answer[0]['text'], 'First text block.\nSecond text block.')
        self.assertEqual(answer[0]['phase'], 'final_answer')

    def test_stream_deltas_and_block_snapshots_form_one_final_answer(self):
        self.turn('split-stream', 'stream-input')
        self.assertEqual(self.completed()['status'], 'completed')
        turn = self.call('thread/read', {'threadId': self.thread, 'includeTurns': True})['thread']['turns'][0]
        answer = next(item for item in turn['items'] if item['id'] == 'stream-blocks')
        self.assertEqual(answer['text'], 'First stream block.\nSecond stream block.')
        self.assertEqual(answer['phase'], 'final_answer')
        deltas = [row['params']['delta'] for row in self.notifications
                  if row.get('method') == 'item/agentMessage/delta'
                  and row['params']['itemId'] == 'stream-blocks']
        self.assertEqual(''.join(deltas), answer['text'])

    def test_plan_never_executes_after_generic_approval(self):
        self.approval='accept'
        self.turn('plan','plan')
        self.assertEqual(self.completed()['status'],'completed')
        history=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns']
        self.assertIn('A visible plan',json.dumps(history))


if __name__ == '__main__': unittest.main()
