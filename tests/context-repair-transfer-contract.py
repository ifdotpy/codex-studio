#!/usr/bin/env python3
"""A transferred repaired context keeps its checked-event provenance."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('wait_fixture', Path(__file__).with_name('context-repair-wait-contract.py'))
f = importlib.util.module_from_spec(spec); spec.loader.exec_module(f)
repair, eventually = f.repair, f.eventually


class TransferRepair(f.ContextWait):
    def transfer(self):
        a = self.agent_update(self.a, accountKey='source-account')
        self.runtime.connection_ids['source-account'] = 'source-connection'
        repaired = repair.repair_idle(self.runtime, a['id'])
        parent = repaired['threadId']
        self.transfer_record = {'id':'transfer-exact','leadId':a['id'],'status':'completed','targetAccountKey':'default','members':{
            a['id']:{'phase':'completed','sourceAccountKey':'source-account','sourceThreadId':parent,
                'source':{'accountKey':'source-account','threadId':parent,'epoch':a['epoch']},
                'result':{'thread':{'id':'transferred-native','forkedFromId':parent}}}}}
        with self.runtime.db() as db:
            db.execute('CREATE TABLE IF NOT EXISTS runtime_account_transfers(id TEXT PRIMARY KEY,record TEXT NOT NULL)')
            db.execute('INSERT INTO runtime_account_transfers VALUES (?,?)',('transfer-exact',json.dumps(self.transfer_record)))
        a = self.agent_update(repaired, accountKey='default',threadId='transferred-native',turnId=None,
            accountHistory=[*repaired.get('accountHistory',[]),{'transferId':'transfer-exact','accountKey':'source-account','threadId':parent}])
        self.server.tid, self.server.no_turns = a['threadId'], True
        return a

    def save_transfer(self, value):
        with self.runtime.db() as db:
            db.execute('UPDATE runtime_account_transfers SET record=? WHERE id=?',(json.dumps(value),'transfer-exact'))

    def test_existing_transfer_wait_starts_original_input_on_same_new_thread(self):
        a = self.transfer()
        initial_forks = len(self.forks())
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('transfer-input',a['id'],'followup','Continue the existing work.','pending',2,a['epoch'],None,None))
        a = self.agent_update(a,status='queued',inFlight=False,startAttempt={'id':'transfer-start','events':['transfer-input'],
            'epoch':a['epoch'],'accountKey':'default','submitted':False,'settingsFixed':True})
        self.agent_update(a,contextRepairWait={'source':repair._identity(a),'events':['transfer-input'],
            'action':None,'actionRequestId':None,'actionIdentity':None,'nextCheckAt':0,
            'error':'Context repair needs a terminal native turn; native status: idle'})
        self.runtime.dispatch()
        eventually(lambda:self.runtime.agent(a['id']).get('turnId')=='resumed-turn')
        current=self.runtime.agent(a['id'])
        self.assertEqual(current['accountKey'],'default')
        self.assertEqual(current['threadId'],'transferred-native')
        self.assertEqual(current['startAttempt']['id'],'transfer-start')
        self.assertEqual(current['startAttempt']['events'],['transfer-input'])
        self.assertEqual(len(self.forks()),initial_forks)
        starts=[p for m,p in self.server.calls if m=='turn/start']
        self.assertEqual(len(starts),1)
        self.assertEqual(starts[0]['threadId'],'transferred-native')
        self.assertEqual(starts[0]['clientUserMessageId'],'transfer-input')

    def test_wrong_or_incomplete_transfer_cannot_carry_checked_events(self):
        original = self.transfer()
        for kind in ('epoch','compactions','account','thread','source','fork-parent','unknown-parent','member-incomplete'):
            with self.subTest(kind=kind):
                a=copy.deepcopy(original); transfer=copy.deepcopy(self.transfer_record)
                if kind=='epoch':a['epoch']+=1
                elif kind=='compactions':a['compactions']=a.get('compactions',0)+1
                elif kind=='account':a['accountKey']='wrong-account'
                elif kind=='thread':a['threadId']='wrong-thread'
                elif kind=='source':transfer['members'][a['id']]['sourceThreadId']='wrong-source'
                elif kind=='fork-parent':transfer['members'][a['id']]['result']['thread']['forkedFromId']='wrong-parent'
                elif kind=='unknown-parent':transfer['status']='unknown'
                elif kind=='member-incomplete':transfer['members'][a['id']]['phase']='submitted'
                self.save_transfer(transfer)
                with self.runtime.db() as db:
                    self.assertEqual([r['id'] for r in repair.verified_events(db,a)],[self.event['id']])

    def test_committed_member_does_not_wait_for_other_team_members(self):
        a=self.transfer()
        for status in ('pending','cancelled'):
            transfer=copy.deepcopy(self.transfer_record)
            transfer['status']=status
            transfer['members']['other-worker']={'phase':'waiting'}
            self.save_transfer(transfer)
            with self.runtime.db() as db:
                self.assertEqual(repair.verified_events(db,a),[])
            self.assertEqual(repair.repair_idle(self.runtime,a['id'])['threadId'],a['threadId'])

    def test_new_event_still_requires_terminal_native_history(self):
        a=self.transfer()
        with self.runtime.db() as db:
            row=dict(self.event,id='new-unchecked-event',turn_id='new-source-turn',created=3)
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                tuple(row[k] for k in ('id','agent','kind','text','status','created','epoch','turn_id','error')))
            self.assertEqual([r['id'] for r in repair.verified_events(db,a)],['new-unchecked-event'])
        initial_forks=len(self.forks())
        with self.assertRaisesRegex(ValueError,'terminal native turn'):
            repair.repair_idle(self.runtime,a['id'])
        self.assertEqual(len(self.forks()),initial_forks)


if __name__=='__main__':
    suite=unittest.TestSuite(TransferRepair(name) for name in TransferRepair.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
