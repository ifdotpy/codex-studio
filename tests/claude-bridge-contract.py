#!/usr/bin/env python3
"""Exercise the bridge protocol with a deterministic SDK. No model calls."""
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
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
 let abort=new AbortController();
 return {
  supportedModels:async()=>[{value:'default',displayName:'Default',supportsEffort:true,supportedEffortLevels:['low','medium','high']}],
  accountInfo:async()=>({email:fs.existsSync(options.cwd+'/.wrong-account')?'different@example.test':'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[{name:'compact',description:'Compact history'}]}),
  usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET:async()=>({rate_limits_available:true,subscription_type:'max',rate_limits:{five_hour:{utilization:11,resets_at:'2026-09-22T08:00:00Z'},seven_day:{utilization:4},model_scoped:[{display_name:'Fable',utilization:7}]}}),
  setModel:async model=>{if(model==='reject-model')throw new Error('Native model rejected');},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},stopTask:async()=>{},
  close(){abort.abort();},interrupt:async()=>abort.abort(),
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
   if(text==='background')yield {type:'system',subtype:'task_started',task_id:'worker-1',description:'Worker',task_type:'agent'};
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
   yield {type:'stream_event',event:{type:'message_start',message:{id:'answer'}}};
   yield {type:'stream_event',event:{type:'content_block_delta',delta:{type:'text_delta',text:'Visible answer'}}};
   yield {type:'assistant',message:{id:'answer',content:[{type:'text',text:'Visible answer'},{type:'thinking',thinking:'Visible reasoning',signature:'PRIVATE_SIGNATURE'}]}};
   yield {type:'result',subtype:'success',usage:{input_tokens:10,output_tokens:2},result:'Visible answer'};
   if(text==='background'){
    yield {type:'assistant',parent_tool_use_id:'agent-tool',uuid:'late-worker',message:{id:'late-answer',content:[{type:'text',text:'Late worker text'}]}};
    yield {type:'system',subtype:'task_notification',task_id:'worker-1',status:'completed',summary:'Worker finished'};
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
        (root / 'node_modules').symlink_to(ROOT / 'scripts/claude_bridge/node_modules', target_is_directory=True)
        self.proc = subprocess.Popen([shutil.which('node'), str(root / 'bridge.mjs'), str(root / 'state')],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, 'STUDIO_CLAUDE_ACCOUNT':'test@example.test'})
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

    def test_history_version_tracks_content_and_survives_read(self):
        before = self.call('thread/read', {'threadId':self.thread})['thread']
        again = self.call('thread/read', {'threadId':self.thread})['thread']
        self.assertEqual(before['historyVersion'], again['historyVersion'])
        self.turn('hello', 'version-one'); self.completed()
        after = self.call('thread/read', {'threadId':self.thread})['thread']
        self.assertNotEqual(before['historyVersion'], after['historyVersion'])
        self.assertEqual(after['historyVersion'], self.call('thread/read', {'threadId':self.thread})['thread']['historyVersion'])

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

    def test_plan_never_executes_after_generic_approval(self):
        self.approval='accept'
        self.turn('plan','plan')
        self.assertEqual(self.completed()['status'],'completed')
        history=self.call('thread/read',{'threadId':self.thread,'includeTurns':True})['thread']['turns']
        self.assertIn('A visible plan',json.dumps(history))


if __name__ == '__main__': unittest.main()
