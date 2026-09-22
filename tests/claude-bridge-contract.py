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
export const tool=(name,description,schema,call)=>({name,call});
export const createSdkMcpServer=value=>value;
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export function query({prompt,options}){
 let abort=new AbortController();
 return {
  supportedModels:async()=>[{value:'default',displayName:'Default',supportsEffort:true,supportedEffortLevels:['low','medium','high']}],
  accountInfo:async()=>({email:fs.existsSync(options.cwd+'/.wrong-account')?'different@example.test':'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next();
   if(input.done)return;
   const text=input.value.message.content[0].text;
   yield {type:'system',subtype:'init'};
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
   yield {type:'stream_event',event:{type:'message_start',message:{id:'answer'}}};
   yield {type:'stream_event',event:{type:'content_block_delta',delta:{type:'text_delta',text:'Visible answer'}}};
   yield {type:'assistant',message:{id:'answer',content:[{type:'text',text:'Visible answer'}]}};
   yield {type:'result',subtype:'success',usage:{input_tokens:10,output_tokens:2},result:'Visible answer'};
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
        (root / 'bridge.mjs').write_text(source.replace("@anthropic-ai/claude-agent-sdk", "./fake.mjs"))
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
            result = ({'decision':'decline'} if row['method'].endswith('requestApproval') else
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
        self.assertEqual(history[0]['items'][-1]['text'],'Visible answer')
        self.assertTrue(any(x.get('method')=='item/agentMessage/delta' for x in self.notifications))

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
        with self.assertRaisesRegex(ValueError,'queue'): self.call('turn/steer',{'threadId':self.thread})


if __name__ == '__main__': unittest.main()
