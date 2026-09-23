#!/usr/bin/env python3
"""Radio uses the real dispatch and native notification paths with fake model I/O."""
import importlib.util
from pathlib import Path
import unittest
import uuid

spec = importlib.util.spec_from_file_location('radio_runtime_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_peer_teams import manage

class RadioRuntime(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    agent_update = f.WorkspaceContract.agent_update

    def setup_room(self):
        self.left = self.agent_update(self.lead('Left'), draft=False, status='idle', autoWake=True)
        self.right = self.agent_update(self.lead('Right'), draft=False, status='idle', autoWake=True)
        self.team = str(uuid.uuid4())
        manage(self.runtime, {'action':'save', 'path':str(self.project), 'team_id':self.team,
            'name':'Shared', 'members':[self.left['id'],self.right['id']], 'expected_revision':0,'request_id':str(uuid.uuid4())})
        self.base = {'action':'radio','path':str(self.project),'team_id':self.team}
        self.room = manage(self.runtime, {**self.base,'radio_action':'open','request_id':str(uuid.uuid4())})['room']

    def test_direct_creation_starts_one_shared_conversation(self):
        result=manage(self.runtime, {'action':'radio','radio_action':'create',
            'request_id':str(uuid.uuid4()),'path':str(self.project),'name':'Design discussion',
            'participants':[{'account_key':'default','model':'gpt-6-astra'},
                            {'account_key':'default','model':'gpt-6-sol'}]})
        self.room=result['room']
        self.left,self.right=[self.runtime.agent(key) for key in self.room['members']]
        self.base={'action':'radio','path':str(self.project),'team_id':self.room['radio']['teamId']}
        self.assertEqual([m for m,_ in self.runtime.server.calls if m=='turn/start'],[])
        for agent in (self.left,self.right):
            self.assertEqual(self.runtime.chat_read(self.room['id'],agent['id'])['room']['id'],self.room['id'])
        outsider=self.lead('Outside')
        with self.assertRaises(ValueError):
            self.runtime.chat_read(self.room['id'],outsider['id'])
        self.command('send',text='Compare options.',target='both',rounds=1)
        first=self.active(self.left)
        self.finish(first,'First view.')
        second=self.active(self.right)
        self.finish(second,'Second view.')
        self.runtime.dispatch()
        page=self.runtime.chat_read(self.room['id'])
        self.assertEqual([m['text'] for m in page['messages']],['Compare options.','First view.','Second view.'])
        self.assertEqual(page['room']['radio']['status'],'idle')

    def command(self, action, **data):
        self.room = manage(self.runtime, {**self.base,'radio_action':action,
            'expected_revision':self.room['radio']['revision'], 'request_id':str(uuid.uuid4()), **data})['room']
        return self.room

    def active(self, agent):
        self.runtime.dispatch()
        f.eventually(lambda: bool(self.runtime.agent(agent['id']).get('turnId')))
        return self.runtime.agent(agent['id'])

    def finish(self, active, text):
        self.runtime.server.complete(active['threadId'],active['turnId'],text)
        f.eventually(lambda: not self.runtime.agent(active['id']).get('inFlight'))

    def test_two_replies_receive_shared_text_and_then_stop(self):
        self.setup_room()
        self.command('send',text='Compare both approaches.',target='both',rounds=1)
        first = self.active(self.left)
        self.assertFalse(self.runtime.agent(self.right['id']).get('inFlight'))
        self.runtime.server.notify({'method':'item/completed','params':{'threadId':first['threadId'],
            'turnId':first['turnId'],'item':{'type':'agentMessage','id':'commentary','text':'First agent commentary.','phase':'commentary'}}})
        self.finish(first,'First agent final answer.')
        second = self.active(self.right)
        calls = [params for method,params in self.runtime.server.calls if method=='turn/start']
        prompt = calls[-1]['input'][0]['text']
        for text in ('Compare both approaches.','First agent commentary.','First agent final answer.'):
            self.assertIn(text,prompt)
        self.finish(second,'Second agent answer.')
        self.runtime.dispatch()
        self.runtime.dispatch()
        self.assertEqual(len([1 for method,_ in self.runtime.server.calls if method=='turn/start']),2)
        page=self.runtime.chat_read(self.room['id'])
        self.assertEqual([m['text'] for m in page['messages']], ['Compare both approaches.',
            'First agent commentary.','First agent final answer.','Second agent answer.'])
        self.assertEqual(page['room']['radio']['status'],'idle')

    def test_stop_interrupts_only_current_shared_turn(self):
        self.setup_room()
        self.command('send',text='Work on this.',target='both',rounds=1)
        first=self.active(self.left)
        self.room=self.runtime.chat_read(self.room['id'])['room']
        self.command('stop')
        self.runtime.dispatch()
        f.eventually(lambda: not self.runtime.agent(first['id']).get('inFlight'))
        self.runtime.dispatch()
        self.assertTrue(self.runtime.agent(first['id'])['autoWake'])
        self.assertFalse(self.runtime.agent(self.right['id']).get('inFlight'))
        self.assertEqual(len([1 for method,_ in self.runtime.server.calls if method=='turn/start']),1)

    def test_long_stream_and_peer_message_guard(self):
        self.setup_room()
        self.command('send',text='Explain in detail.',target=self.left['id'],rounds=1)
        first=self.active(self.left)
        with self.assertRaisesRegex(ValueError, 'shared automatically'):
            self.runtime.chat_message(first['id'],self.right['id'],'Duplicate reply',str(uuid.uuid4()))
        chunks=['a'*19000,'b'*5000,'c'*3000]
        for chunk in chunks:
            self.runtime.server.notify({'method':'item/agentMessage/delta','params':{
                'threadId':first['threadId'],'turnId':first['turnId'],'itemId':'long-stream','delta':chunk}})
        page=self.runtime.chat_read(self.room['id'])
        self.assertEqual(page['messages'][-1]['text'], ''.join(chunks))
        self.finish(first,'Done.')
        self.runtime.dispatch()
        page=self.runtime.chat_read(self.room['id'])
        self.assertEqual(page['messages'][-2]['text'], ''.join(chunks))

    def test_private_steer_is_rejected_but_shared_question_can_be_answered(self):
        self.setup_room()
        self.command('send',text='Shared question.',target='both',rounds=1)
        first=self.active(self.left)
        for delivery in ('steer','after_tool'):
            with self.assertRaisesRegex(ValueError, 'shared chat'):
                self.runtime.send(first['id'],'Private unrelated task',str(uuid.uuid4()),delivery=delivery)
        self.runtime.server.notify({'method':'item/completed','params':{'threadId':first['threadId'],
            'turnId':first['turnId'],'item':{'type':'agentMessage','id':'question','text':'Choose one.',
                'questions':[{'title':'Which option?', 'options':['One','Two']}]}}})
        key=first['id']+':question:question'
        self.assertEqual(self.runtime.answer(key,{'answers':{'0':{'answers':['One']}}})['status'],'answered')
        steers=[params for method,params in self.runtime.server.calls if method=='turn/steer']
        self.assertEqual(len(steers),1)
        self.assertIn('One',steers[0]['input'][0]['text'])

    def test_completed_question_holds_floor_and_resumes_in_shared_chat(self):
        self.setup_room()
        self.command('send',text='Shared question.',target='both',rounds=1)
        first=self.active(self.left)
        self.runtime.server.notify({'method':'item/completed','params':{'threadId':first['threadId'],
            'turnId':first['turnId'],'item':{'type':'agentMessage','id':'question','text':'Choose one.',
                'questions':[{'title':'Which option?', 'options':['One','Two']}]}}})
        self.finish(first,'I need your choice.')
        self.runtime.dispatch()
        self.assertFalse(self.runtime.agent(self.right['id']).get('inFlight'))
        key=first['id']+':question:question'
        self.runtime.answer(key,{'answers':{'0':{'answers':['One']}}})
        next_turn=self.active(self.left)
        self.assertNotEqual(first['turnId'],next_turn['turnId'])
        self.assertFalse(self.runtime.agent(self.right['id']).get('inFlight'))
        self.finish(next_turn,'Choice received.')
        second=self.active(self.right)
        prompt=[p for m,p in self.runtime.server.calls if m=='turn/start'][-1]['input'][0]['text']
        self.assertIn('Which option?\\nOne',prompt)
        self.finish(second,'Agreed.')
        self.runtime.dispatch()
        self.assertEqual(self.runtime.chat_read(self.room['id'])['room']['radio']['status'],'idle')

    def test_question_keeps_ordinary_queue_out_of_held_turn(self):
        self.setup_room()
        self.command('send',text='Shared question.',target='both',rounds=1)
        first=self.active(self.left)
        self.runtime.server.notify({'method':'item/completed','params':{'threadId':first['threadId'],
            'turnId':first['turnId'],'item':{'type':'agentMessage','id':'question','text':'Choose one.',
                'questions':[{'title':'Which option?'}]}}})
        self.runtime.send(first['id'],'Independent work',str(uuid.uuid4()),delivery='queue')
        self.finish(first,'Waiting for your choice.')
        self.runtime.dispatch()
        self.runtime.dispatch()
        self.assertFalse(self.runtime.agent(first['id']).get('inFlight'))
        self.assertEqual(len([1 for m,_ in self.runtime.server.calls if m=='turn/start']),1)
        page=self.runtime.chat_read(self.room['id'])
        self.assertEqual(page['room']['radio']['status'],'waiting')
        self.assertIsNone(page['room']['radio']['error'])
        self.assertEqual(self.runtime.agent(first['id'])['status'],'queued')
        self.runtime.answer(first['id']+':question:question',{'answers':{'0':{'answers':['One']}}})
        private=self.active(self.left)
        self.finish(private,'Private result.')
        resumed=self.active(self.left)
        self.finish(resumed,'Shared answer.')
        second=self.active(self.right)
        self.finish(second,'Agreed.')
        self.runtime.dispatch()
        page=self.runtime.chat_read(self.room['id'])
        self.assertNotIn('Private result.',[m['text'] for m in page['messages']])
        self.assertEqual(page['room']['radio']['status'],'idle')

if __name__=='__main__': unittest.main()
