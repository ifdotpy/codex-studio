#!/usr/bin/env python3
"""Durable transfer controls and explicit executor refusals on both HTTP backends."""
from __future__ import annotations
from test_isolation import isolate_supervisor_environment
import json
import os
from pathlib import Path
import sqlite3
import sys
import unittest
import uuid

isolate_supervisor_environment()
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'src'))
from codex_account_transfer import AccountTransfers
from codex_sync_entities import register_functions
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class AccountTransferHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend=start_contract_backend(cls)
        cls.backend.stop()
        cls.operations={}
        cls.selected_cwd=cls.backend.cwd/'selected'
        cls.selected_cwd.mkdir()
        with sqlite3.connect(cls.backend.state/'canvas.sqlite3') as db:
            register_functions(db)
            db.create_function('sync_invalidate_agent',1,lambda _:None)
            class Ports:
                def put(self,db,table,record): db.execute('INSERT OR REPLACE INTO runtime_'+table+'(id,record) VALUES(?,?)',(record['id'],json.dumps(record)))
                def agent(self,key,db): return json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?',(key,)).fetchone()[0])
            ports=Ports(); transfers=object.__new__(AccountTransfers); transfers.rt=ports
            for name in ('cancel','finish','refuse','replay'):
                key=str(uuid.uuid4()); cls.operations[name]=key
                agent=dict(id=name,rootId=name,epoch=0,status='idle',name=name,isLead=True,autoWake=False,accountKey='default',cwd=str(cls.backend.cwd),model='gpt-6-astra',accountTransferId=key)
                ports.put(db,'agents',agent)
                operation=dict(id=key,leadId=name,targetAccountKey='default',status='pending',scope='team',created=1.0,
                    members={name:dict(phase='blocked',sourceAccountKey='default',lazy=False,error='Fixture receipt requires attention',archiveInvalidated=name=='refuse')})
                transfers.save(db,operation)
            ports.put(db,'agents',dict(id='empty',rootId='empty',epoch=0,status='idle',name='Empty',isLead=True,autoWake=False,accountKey='default',cwd=str(cls.backend.cwd),model='gpt-6-astra',projectFolder='old',projectFolderRevision=2))
            ports.put(db,'agents',dict(id='selection-fail',rootId='selection-fail',epoch=0,status='idle',name='Selection failure',isLead=True,autoWake=False,accountKey='default',cwd=str(cls.backend.cwd),model='gpt-6-astra'))
        cls.backend.start()

    def headers(self):
        return {'Origin':self.backend.origin,'X-Canvas-Token':self.backend.session['token']}
    def post(self,body,path='/api/agents/account-transfer'):
        return self.backend.request('POST',path,body=body,headers=self.headers())
    def stored(self,name):
        with sqlite3.connect(self.backend.state/'canvas.sqlite3') as db:
            return db.execute('SELECT record FROM runtime_account_transfers WHERE id=?',(self.operations[name],)).fetchone()[0]
    def test_cancel_and_finish_history_preserve_member_receipt(self):
        for name,action in [('finish','finish_history'),('cancel','cancel')]:
            result=self.post(dict(request_id=self.operations[name],action=action))
            self.assertEqual(result.status,200,result.body)
            self.assertEqual(result.json()['status'],'cancelled' if name=='cancel' else 'pending')
            self.assertEqual(result.json()['members'][name]['phase'],'blocked')
            if name=='finish': self.assertTrue(result.json()['finishHistory'])
    def test_invalidated_retry_refuses_without_writes(self):
        before=self.stored('refuse')
        result=self.post(dict(request_id=self.operations['refuse'],action='retry'))
        self.assertEqual(result.status,400,result.body)
        self.assertEqual(self.stored('refuse'),before)
    def test_exact_request_replay_and_content_conflict(self):
        before=self.stored('replay')
        result=self.post(dict(request_id=self.operations['replay'],id='replay',account_key='default',scope='team'))
        self.assertEqual(result.status,200,result.body)
        self.assertEqual(result.json()['id'],self.operations['replay'])
        self.assertEqual(self.stored('replay'),before)
        conflict=self.post(dict(request_id=self.operations['replay'],id='replay',account_key='default',scope='subagents'))
        self.assertEqual(conflict.status,400,conflict.body)
        self.assertEqual(self.stored('replay'),before)
    def test_same_account_selection_is_read_only(self):
        before=self.stored('replay')
        result=self.post(dict(id='replay',account_key='default'),'/api/agents/account')
        self.assertEqual(result.status,200,result.body)
        self.assertEqual(self.stored('replay'),before)
    def test_empty_chat_selection_changes_folder_once(self):
        body=dict(id='empty',account_key='default',cwd=str(self.selected_cwd))
        for _ in range(2):
            result=self.post(body,'/api/agents/account')
            self.assertEqual(result.status,200,result.body)
            self.assertEqual(result.json()['cwd'],str(self.selected_cwd))
        with sqlite3.connect(self.backend.state/'canvas.sqlite3') as db:
            agent=json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='empty'").fetchone()[0])
            project=db.execute('SELECT record FROM runtime_projects WHERE id=?',(str(self.selected_cwd),)).fetchone()
        self.assertEqual(agent['projectFolderRevision'],3)
        self.assertNotIn('projectFolder',agent)
        self.assertIsNotNone(project)
    def test_selection_transaction_rolls_back_after_project_write(self):
        directory=self.backend.cwd/'refused-project'
        directory.mkdir()
        with sqlite3.connect(self.backend.state/'canvas.sqlite3') as db:
            before=db.execute("SELECT record FROM runtime_agents WHERE id='selection-fail'").fetchone()[0]
            db.execute("CREATE TRIGGER selection_fail AFTER INSERT ON runtime_projects WHEN NEW.id LIKE '%/refused-project' BEGIN SELECT RAISE(ABORT,'injected project failure'); END")
        try:
            result=self.post(dict(id='selection-fail',account_key='default',cwd=str(directory)),'/api/agents/account')
            self.assertEqual(result.status,400,result.body)
            with sqlite3.connect(self.backend.state/'canvas.sqlite3') as db:
                self.assertEqual(db.execute("SELECT record FROM runtime_agents WHERE id='selection-fail'").fetchone()[0],before)
                self.assertIsNone(db.execute('SELECT record FROM runtime_projects WHERE id=?',(str(directory),)).fetchone())
        finally:
            with sqlite3.connect(self.backend.state/'canvas.sqlite3') as db:
                db.execute('DROP TRIGGER selection_fail')
    def test_new_request_needs_executor_without_writes(self):
        if not os.environ.get('STUDIO_HTTP_BACKEND_COMMAND'):
            self.skipTest('Rust executor boundary witness')
        before=self.stored('replay')
        with sqlite3.connect(self.backend.state/'canvas.sqlite3') as db:
            rows=db.execute('SELECT id,record FROM runtime_account_transfers ORDER BY id').fetchall()
        result=self.post(dict(request_id=str(uuid.uuid4()),id='replay',account_key='default',scope='team'))
        self.assertEqual(result.status,501,result.body)
        self.assertEqual(result.json()['error'],'Not implemented: requires account-transfer/provider-history-executor')
        self.assertEqual(self.stored('replay'),before)
        with sqlite3.connect(self.backend.state/'canvas.sqlite3') as db:
            self.assertEqual(db.execute('SELECT id,record FROM runtime_account_transfers ORDER BY id').fetchall(),rows)
    def test_validation_is_located(self):
        result=self.post(dict(request_id='',action='retry'))
        self.assertEqual(result.status,400,result.body)

if __name__=='__main__': unittest.main()
