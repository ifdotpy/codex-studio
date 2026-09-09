#!/usr/bin/env python3
"""Team cleanup boundaries with isolated SQLite and no native mutations."""
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_agent_management import manage_agent
from codex_efficiency import EfficiencyMixin

class Store(EfficiencyMixin):
    def __init__(self, path):
        self.path=path; self.lock=threading.RLock(); self.preparations={}; self.claims={}; self.calls=[]
        self.native_state='idle'; self.connection_ids={'default':'connection'}
        owner=self
        class Native:
            def call(self, method, params, timeout):
                owner.calls.append((method,params['threadId']))
                return {'thread':{'id':params['threadId'],'status':{'type':owner.native_state}}}
        self.servers={'default':Native()}
        with self.db() as db:
            for table in ['agents','monitors','tasks','requests','work','tool_requests','items']:
                db.execute(f'CREATE TABLE runtime_{table} (id TEXT PRIMARY KEY, record TEXT)')
            db.execute('CREATE TABLE runtime_events (id TEXT PRIMARY KEY, agent TEXT, epoch INT, status TEXT)')
            for key,parent,root,lead in [('lead',None,'lead',True),('worker','lead','lead',False),('peer',None,'peer',True),('foreign','peer','peer',False)]:
                self.put(db,'agents',{'id':key,'name':key,'parentId':parent,'rootId':root,'isLead':lead,
                    'autoWake':True,'epoch':1,'status':'completed','inFlight':False,'threadId':key,'maxAgents':20})
            self.put(db,'items',{'id':'history','agent':'worker','text':'Preserved result'})
    @contextmanager
    def db(self):
        db=sqlite3.connect(self.path); db.row_factory=sqlite3.Row
        try:
            with db: yield db
        finally: db.close()
    def put(self,db,table,row): db.execute(f'INSERT OR REPLACE INTO runtime_{table} VALUES (?,?)',(row['id'],json.dumps(row)))
    def records(self,db,table): return [json.loads(r[0]) for r in db.execute(f'SELECT record FROM runtime_{table}')]
    def agent(self,key,db):
        row=db.execute('SELECT record FROM runtime_agents WHERE id=?',(key,)).fetchone()
        if not row: raise ValueError('Unknown agent')
        return json.loads(row[0])
    def resource_action(self): return {'state':{'claims':self.claims}}
    def reconcile_turn(self,key): self.calls.append(('reconcile',key)); return {'status':'unconfirmed'}

class Contract(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.rt=Store(str(Path(self.tmp.name)/'state.sqlite'))
    def tearDown(self): self.tmp.cleanup()
    def call(self,action,**args):
        return manage_agent(self.rt,'lead',{'action':action,'agent_id':'worker','reason':'Reviewed result',**args},1)
    def update(self,table,row):
        with self.rt.db() as db:self.rt.put(db,table,row)
    def worker(self,**values):
        with self.rt.db() as db:
            a=self.rt.agent('worker',db);a.update(values);self.rt.put(db,'agents',a)
    def test_archive_restore_preserves_history_identity_and_never_starts_work(self):
        result=self.call('archive');self.assertEqual(result['status'],'archived')
        self.assertTrue(self.call('archive')['replayed'])
        self.assertEqual(self.call('list_archived')['total'],1)
        with self.rt.db() as db:
            self.assertEqual(self.rt.records(db,'items')[0]['text'],'Preserved result')
            self.assertEqual(self.rt.agent('worker',db)['threadId'],'worker')
        restored=self.call('restore');self.assertEqual(restored['status'],'restored')
        self.assertFalse(restored['agent']['inFlight']);self.assertEqual(restored['agent']['status'],'paused')
        self.assertEqual(self.call('restore')['status'],'not_archived');self.assertEqual(self.rt.calls,[('thread/read','worker')])
    def test_only_active_lead_can_manage_its_own_workers(self):
        for actor,target,epoch in [('worker','worker',1),('lead','foreign',1),('lead','lead',1),('lead','worker',0)]:
            with self.assertRaises(ValueError):manage_agent(self.rt,actor,{'action':'archive','agent_id':target,'reason':'x'},epoch)
    def test_each_unfinished_operation_blocks_archive(self):
        for table,row in [('monitors',{'id':'m','agent':'worker','status':'running'}),
                          ('tasks',{'id':'t','agent':'worker','status':'running'}),
                          ('requests',{'id':'q','agent':'worker','status':'pending'}),
                          ('work',{'id':'w','owner':'worker','status':'review'}),
                          ('tool_requests',{'id':'r','agent':'worker','stage':'interrupted','outcome':'unknown'})]:
            with self.subTest(table=table):
                self.update(table,row);self.assertEqual(self.call('archive')['status'],'blocked')
                with self.rt.db() as db:db.execute(f'DELETE FROM runtime_{table}')
        self.worker(inFlight=True);self.assertFalse(self.call('inspect')['canArchive'])
        self.assertEqual(self.call('archive')['status'],'blocked')
    def test_unknown_input_and_resource_claim_are_not_discarded(self):
        with self.rt.db() as db:db.execute("INSERT INTO runtime_events VALUES ('e','worker',1,'uncertain')")
        self.assertEqual(self.call('archive')['status'],'blocked')
        with self.rt.db() as db:db.execute('DELETE FROM runtime_events')
        self.rt.claims={'build':{'worker':'worker'}}
        self.assertEqual(self.call('archive')['blockers'][0]['kind'],'resource_claims')
    def test_descendant_must_be_archived_first(self):
        self.update('agents',{'id':'child','parentId':'worker','rootId':'lead','isLead':False})
        self.assertEqual(self.call('archive')['status'],'blocked')
    def test_deleted_or_later_stopped_worker_cannot_be_restored(self):
        self.worker(deletedAt=1)
        with self.assertRaises(ValueError):self.call('restore')
        self.worker(deletedAt=None);self.call('archive');self.worker(epoch=99)
        with self.assertRaises(ValueError):self.call('restore')
    def test_recover_reads_native_state_without_replay(self):
        result=self.call('recover');self.assertEqual(result['recovery']['status'],'unconfirmed')
        self.assertEqual(self.rt.calls,[('reconcile','worker')])
    def test_native_activity_blocks_archive_even_when_local_status_is_completed(self):
        self.rt.native_state='active'
        self.assertEqual(self.call('archive')['blockers'][0]['kind'],'native_state_unconfirmed')
    def test_compatibility_entry_uses_the_same_checks(self):
        result=self.rt.model_context('lead',{'topic':'agent_manage','action':'inspect','agent_id':'worker'})
        self.assertTrue(result['canArchive'])
        with self.assertRaises(ValueError):
            self.rt.model_context('worker',{'topic':'agent_manage','action':'archive','agent_id':'worker'})
    def test_restoration_obeys_team_limit(self):
        self.call('archive')
        with self.rt.db() as db:
            a=self.rt.agent('lead',db);a['maxAgents']=1;self.rt.put(db,'agents',a)
        with self.assertRaisesRegex(ValueError,'limit'):self.call('restore')

class RuntimeRouteContract(unittest.TestCase):
    def test_native_and_existing_thread_routes_preserve_receipts_and_ui_visibility(self):
        import importlib.util
        spec=importlib.util.spec_from_file_location('runtime_fixture',Path(__file__).with_name('runtime-contract.py'))
        f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)
        case=f.RuntimeContract();case.setUp()
        try:
            rt=case.runtime; lead=case.lead()
            rt.resource_action=lambda: {'state': {'claims': {}, 'queue': {}}}
            worker=rt.create({'name':'Archive fixture','prompt':'Review','role':'reviewer'},lead['id'],defer=True)
            with rt.lock,rt.db() as db:
                a=rt.agent(worker['id'],db);a.update(status='completed',inFlight=False,autoWake=False)
                rt.put(db,'agents',a)
                db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?",(a['id'],))
            def invoke(number,tool,args):
                rt.dynamic({'id':number,'params':{'threadId':lead['threadId'],'callId':str(number),'tool':tool,'arguments':args}})
                result=next(r for r in rt.server.responses if r['id']==number)['result']
                self.assertTrue(result['success'],str(result))
            invoke(9201,'orchestration_agent_manage',{'action':'inspect','agent_id':worker['id']})
            bridge={'agent_id':'workspace','text':json.dumps({'tool':'orchestration_context','arguments':{
                'topic':'agent_manage','action':'archive','agent_id':worker['id'],'reason':'Reviewed fixture'}})}
            invoke(9202,'orchestration_send',bridge)
            self.assertNotIn(worker['id'],[a['id'] for a in rt.snapshot()['agents']])
            epoch=rt.agent(worker['id'])['epoch']
            invoke(9202,'orchestration_send',bridge)
            self.assertEqual(epoch,rt.agent(worker['id'])['epoch'])
            invoke(9203,'orchestration_agent_manage',{'action':'restore','agent_id':worker['id']})
            self.assertIn(worker['id'],[a['id'] for a in rt.snapshot()['agents']])
            self.assertFalse(rt.agent(worker['id'])['autoWake'])
        finally:case.tearDown()

if __name__=='__main__':unittest.main()
