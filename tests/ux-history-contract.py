#!/usr/bin/env python3
"""Isolated history navigation, branch, and deferred settings contracts."""
import importlib.util
import base64
from contextlib import contextmanager
import json
from pathlib import Path
import unittest
import uuid
from unittest.mock import patch
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_transcript_history import search_history, history_item

class HistoryContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    start = f.WorkspaceContract.start

    def seed(self, count=310):
        lead = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            for i in range(count):
                self.runtime.item(db, lead['id'], f'item-{i:04}', 'assistant', f'history sentence {i}', turnId=f'turn-{i}')
                # Equal timestamps must not lose or duplicate items across pages.
                db.execute('UPDATE runtime_items SET created=1 WHERE id=?', (lead['id']+f':item-{i:04}',))
        return lead

    def test_all_pages_stable_scope_and_around_forward(self):
        lead = self.seed()
        other = self.seed(2)
        snapshot = self.runtime.transcript(lead['id'])
        page = snapshot
        identities = []
        while True:
            identities += [item['id'] for item in page['items']]
            if not page['nextCursor']: break
            page = self.runtime.transcript(lead['id'], before=page['nextCursor'])
        self.assertEqual(len(identities),310)
        self.assertEqual(len(set(identities)),310)
        self.assertEqual(self.runtime.transcript(lead['id']),snapshot)
        anchor = lead['id']+':item-0100'
        middle = self.runtime.transcript(lead['id'],around=anchor,limit=20)
        self.assertIn(anchor,[item['id'] for item in middle['items']])
        later = self.runtime.transcript(lead['id'],after=middle['nextAfterCursor'],limit=20)
        self.assertGreater(later['items'][0]['id'],middle['items'][-1]['id'])
        with self.assertRaises(ValueError):
            self.runtime.transcript(lead['id'],around=other['id']+':item-0000')

    def test_search_uses_indexed_full_text_addresses(self):
        lead = self.seed(500)
        self.seed(500)
        original = self.runtime.db
        steps = [0]
        @contextmanager
        def bounded_database():
            with original() as db:
                def progress():
                    steps[0] += 100
                    return int(steps[0] > 100000)
                db.set_progress_handler(progress,100)
                try:
                    yield db
                finally:
                    db.set_progress_handler(None,0)
        with patch.object(self.runtime,'db',bounded_database):
            self.assertEqual(search_history(self.runtime,lead['id'],'no match')['results'],[])
        self.assertLess(steps[0],100000)

    def test_full_search_and_expanded_input_identity(self):
        lead = self.seed()
        with self.runtime.lock,self.runtime.db() as db:
            self.runtime.item(db,lead['id'],'batch','user','combined',inputs=[{'id':'request','kind':'user','text':'exact needle'}],turnId='batch-turn')
        found = search_history(self.runtime,lead['id'],'sentence 0')['results']
        self.assertEqual(found[0]['id'],lead['id']+':item-0000')
        result = search_history(self.runtime,lead['id'],'exact needle')['results'][0]
        self.assertEqual(result['id'],lead['id']+':request')
        page = self.runtime.transcript(lead['id'],around=result['id'])
        self.assertIn(result['sourceId'],[item['id'] for item in page['items']])
        with self.runtime.lock,self.runtime.db() as db:
            db.execute("UPDATE runtime_items SET record=json_set(record,'$.afterRestore',1) WHERE id=?",(result['sourceId'],))
        self.assertEqual(search_history(self.runtime,lead['id'],'exact needle')['results'],[])

    def test_exact_input_read_preserves_long_original_prompt(self):
        lead = self.lead()
        event_id = str(uuid.uuid4())
        text = 'Original text ' * 2000 + 'End'
        self.runtime.send(lead['id'],text,message_id=event_id)
        with self.runtime.lock,self.runtime.db() as db:
            self.runtime.item(db,lead['id'],'long-batch','user',text,
                inputs=[{'id':event_id,'kind':'user','text':text}],turnId='history-turn')
        visible = self.runtime.transcript(lead['id'])['items'][0]['inputs'][0]
        self.assertTrue(visible['truncated'])
        resolved = history_item(self.runtime,lead['id'],lead['id']+':'+event_id)
        self.assertEqual(resolved['text'],text)
        self.assertFalse(resolved['truncated'])
        self.assertEqual(resolved['sourceId'],lead['id']+':long-batch')

    def test_branch_before_first_and_later_preserves_source_and_retry(self):
        for preceding in ([], [{'id':'earlier'}]):
            lead = self.start(self.lead())
            self.runtime.server.complete(lead['threadId'],lead['turnId'])
            with self.runtime.lock,self.runtime.db() as db:
                if preceding:
                    self.runtime.item(db,lead['id'],'earlier-message','user','prior prompt',turnId='earlier')
                    db.execute('UPDATE runtime_items SET created=0 WHERE id=?',(lead['id']+':earlier-message',))
                self.runtime.item(db,lead['id'],'chosen','user','edit this',turnId=lead['turnId'])
            asset = self.runtime.upload_asset({'agent':lead['id'],'name':'draft.txt','base64':base64.b64encode(b'keep file').decode()})
            with self.runtime.lock,self.runtime.db() as db:
                row = db.execute('SELECT record FROM runtime_items WHERE id=?',(lead['id']+':chosen',)).fetchone()
                record = json.loads(row[0])
                record['assets'] = [asset]
                db.execute('UPDATE runtime_items SET record=? WHERE id=?',(json.dumps(record),record['id']))
            before = self.runtime.transcript(lead['id'])
            request = {'id':str(uuid.uuid4()),'message_id':lead['id']+':chosen','before':True}
            original = self.runtime.server.call
            def call(method,params,timeout=60):
                if method=='thread/read': return {'thread':{'id':lead['threadId'],'turns':preceding+[{'id':lead['turnId']}]}}
                return original(method,params,timeout)
            with patch.object(self.runtime.server,'call',side_effect=call):
                branch = self.runtime.branch_conversation(lead['id'],request)
                calls = list(self.runtime.server.calls)
                self.assertEqual(self.runtime.branch_conversation(lead['id'],request),branch)
                self.assertEqual(self.runtime.server.calls,calls)
            self.assertEqual(branch['draft']['text'],'edit this')
            copied = self.runtime.asset_record(branch['draft']['assets'][0]['id'])
            self.assertEqual(copied['agent'],branch['id'])
            self.assertNotEqual(copied['id'],asset['id'])
            self.assertEqual(Path(copied['path']).read_bytes(),b'keep file')
            self.assertNotIn('edit this',[x['text'] for x in self.runtime.transcript(branch['id'])['items']])
            self.assertEqual(self.runtime.transcript(lead['id']),before)
            self.assertEqual(branch['status'],'idle')
            with self.assertRaises(ValueError):
                self.runtime.branch_conversation(lead['id'],{**request,'before':False})

    def test_edit_second_input_retains_prior_user_text_in_review_draft(self):
        lead = self.start(self.lead())
        self.runtime.server.complete(lead['threadId'],lead['turnId'])
        with self.runtime.lock,self.runtime.db() as db:
            self.runtime.item(db,lead['id'],'batch','user','combined',turnId=lead['turnId'],
                inputs=[{'id':'first','kind':'user','text':'Keep these requirements'},
                        {'id':'second','kind':'user','text':'Edit this request'}])
        original = self.runtime.server.call
        def call(method,params,timeout=60):
            if method == 'thread/read':
                return {'thread':{'turns':[{'id':lead['turnId']}]}}
            return original(method,params,timeout)
        with patch.object(self.runtime.server,'call',side_effect=call):
            branch = self.runtime.branch_conversation(lead['id'],
                {'id':str(uuid.uuid4()),'message_id':lead['id']+':second','before':True})
        self.assertEqual(branch['draft']['prefixText'],'Keep these requirements')
        self.assertEqual(branch['draft']['text'],'Edit this request')
        self.assertEqual(self.runtime.transcript(branch['id'])['items'],[])

    def test_native_branch_cannot_reuse_original_thread_identity(self):
        lead = self.start(self.lead())
        self.runtime.server.complete(lead['threadId'],lead['turnId'])
        with self.runtime.lock,self.runtime.db() as db:
            self.runtime.item(db,lead['id'],'chosen','user','edit',turnId=lead['turnId'])
        request = {'id':str(uuid.uuid4()),'message_id':lead['id']+':chosen','before':True}
        original = self.runtime.server.call
        calls = []
        def call(method,params,timeout=60):
            if method == 'thread/read':
                return {'thread':{'turns':[{'id':'previous'},{'id':lead['turnId']}]}}
            if method == 'thread/fork':
                calls.append(params)
                return {'thread':{'id':lead['threadId']}}
            return original(method,params,timeout)
        with patch.object(self.runtime.server,'call',side_effect=call):
            with self.assertRaisesRegex(ValueError,'new thread'):
                self.runtime.branch_conversation(lead['id'],request)
            with self.assertRaises(ValueError):
                self.runtime.branch_conversation(lead['id'],request)
        self.assertEqual(len(calls),1)
        self.assertEqual(self.runtime.agent(lead['id'])['threadId'],lead['threadId'])
        self.assertEqual(len(self.runtime.snapshot()['agents']),1)

    def test_unknown_native_branch_result_never_forks_again(self):
        lead = self.start(self.lead())
        self.runtime.server.complete(lead['threadId'],lead['turnId'])
        with self.runtime.lock,self.runtime.db() as db:
            self.runtime.item(db,lead['id'],'chosen','user','edit',turnId=lead['turnId'])
        source_thread = self.runtime.agent(lead['id'])['threadId']
        request = {'id':str(uuid.uuid4()),'message_id':lead['id']+':chosen','before':True}
        original = self.runtime.server.call
        calls = []
        def call(method,params,timeout=60):
            if method == 'thread/read':
                return {'thread':{'turns':[{'id':'previous'},{'id':lead['turnId']}]}}
            if method == 'thread/fork':
                calls.append(params)
                raise TimeoutError('Native response lost')
            return original(method,params,timeout)
        with patch.object(self.runtime.server,'call',side_effect=call):
            with self.assertRaises(TimeoutError):
                self.runtime.branch_conversation(lead['id'],request)
            with self.assertRaises(ValueError):
                self.runtime.branch_conversation(lead['id'],request)
        self.assertEqual(len(calls),1)
        self.assertEqual(self.runtime.agent(lead['id'])['threadId'],source_thread)

    def test_disconnected_account_replays_exact_selections_but_rejects_changes(self):
        lead = self.lead()
        other = self.lead('Other')
        home = self.root / 'disconnected-account'
        home.mkdir()
        (home / 'auth.json').write_text(json.dumps({'tokens':{'account_id':'isolated-selection','access_token':'fixture'}}))
        account = self.runtime.accounts.register(str(home))
        selected = self.runtime.set_account(lead['id'],account,str(self.project))
        with self.runtime.db() as db:
            project = self.runtime.records(db,'projects')[0]
        request = {'action':'set_account','path':str(self.project),'account_key':account,
                   'expected_revision':project['accountRevision']}
        assigned = self.runtime.projects(request)
        self.runtime.accounts.disconnect(account)
        self.assertEqual(self.runtime.set_account(lead['id'],account),selected)
        self.assertEqual(self.runtime.set_account(lead['id'],account,str(self.project)),selected)
        self.assertEqual(self.runtime.projects(request),assigned)
        with self.assertRaisesRegex(ValueError,'Reconnect'):
            self.runtime.set_account(other['id'],account)
        new_project = self.root / 'new-project'
        new_project.mkdir()
        with self.assertRaisesRegex(ValueError,'Reconnect'):
            self.runtime.set_account(lead['id'],account,str(new_project))
        with self.assertRaisesRegex(ValueError,'Reconnect'):
            self.runtime.projects({**request,'path':str(new_project),'expected_revision':0})

    def test_account_change_requires_new_settings_validation_before_native_start(self):
        lead = self.lead()
        self.runtime.conversation_settings(lead['id'], {'next_turn':True,'effort':'high','request_id':'old-account-choice'})
        home = self.root / 'another-account'
        home.mkdir()
        (home / 'auth.json').write_text(json.dumps({'tokens':{'account_id':'isolated-second','access_token':'fixture'}}))
        account = self.runtime.accounts.register(str(home))
        self.runtime.set_account(lead['id'], account)
        self.runtime.send(lead['id'],'Do work')
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'failed')
        self.assertIn('account changed',self.runtime.agent(lead['id'])['error'])
        self.assertFalse(any(method == 'turn/start' for server in self.runtime.servers.values() for method, _ in server.calls))
        self.runtime.conversation_settings(lead['id'], {'next_turn':True,'effort':'high','request_id':'new-account-choice'})
        current = self.start(lead,'Retry after validation')
        self.assertEqual(current['effort'],'high')
        self.assertNotIn('pendingSettings',current)

    def test_idle_settings_replace_pending_choice(self):
        lead = self.lead()
        self.runtime.conversation_settings(lead['id'], {'next_turn':True,'effort':'high','request_id':'queued-choice'})
        updated = self.runtime.conversation_settings(lead['id'], {'effort':'low'})
        self.assertEqual(updated['effort'],'low')
        self.assertNotIn('pendingSettings',updated)
        self.assertNotIn('pendingSettingsAccountKey',updated)
        retried = self.runtime.conversation_settings(lead['id'], {'next_turn':True,'effort':'high','request_id':'queued-choice'})
        self.assertEqual(retried['effort'],'low')
        self.assertNotIn('pendingSettings',retried)

    def test_queued_settings_preserve_active_then_apply_once(self):
        lead = self.start(self.lead())
        old = self.runtime.agent(lead['id'])
        pending = self.runtime.conversation_settings(lead['id'], {'next_turn':True,'effort':'high','request_id':'settings-test'})
        self.assertEqual(pending['effort'],old['effort'])
        self.assertEqual(pending['pendingSettings']['effort'],'high')
        self.runtime.server.complete(lead['threadId'],lead['turnId'])
        current = self.start(lead,'next task')
        self.assertEqual(current['effort'],'high')
        self.assertNotIn('pendingSettings',current)
        retried = self.runtime.conversation_settings(lead['id'], {'next_turn':True,'effort':'high','request_id':'settings-test'})
        self.assertNotIn('pendingSettings',retried)
        with self.assertRaises(ValueError):
            self.runtime.conversation_settings(lead['id'], {'next_turn':True,'effort':'low','request_id':'settings-test'})

prepare_spec = importlib.util.spec_from_file_location('prepare_fixture_ux', Path(__file__).with_name('prepare-steer-contract.py'))
pf = importlib.util.module_from_spec(prepare_spec)
prepare_spec.loader.exec_module(pf)

class DeferredSettingsContract(unittest.TestCase):
    setUp = pf.PrepareSteerContract.setUp
    tearDown = pf.PrepareSteerContract.tearDown
    create = pf.PrepareSteerContract.create
    pending_prepare = pf.PrepareSteerContract.pending_prepare
    accept_prepare = pf.PrepareSteerContract.accept_prepare

    def test_settings_saved_during_preparation_wait_for_following_turn(self):
        self.server.hold.add('thread/start')
        lead = self.create()
        entry = self.pending_prepare(lead)
        original = self.runtime.agent(lead['id'])['effort']
        self.runtime.conversation_settings(lead['id'], {'next_turn':True,'effort':'high','request_id':'late-setting'})
        self.accept_prepare(entry)
        pf.eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        current = self.runtime.agent(lead['id'])
        self.assertEqual(current['effort'],original)
        self.assertEqual(current['pendingSettings']['effort'],'high')
        self.server.hold.clear()
        self.server.complete(current['threadId'],current['turnId'])
        self.runtime.send(lead['id'],'Next')
        pf.eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        self.assertEqual(self.runtime.agent(lead['id'])['effort'],'high')
        self.assertNotIn('pendingSettings',self.runtime.agent(lead['id']))

if __name__=='__main__': unittest.main()
