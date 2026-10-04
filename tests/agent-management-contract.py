#!/usr/bin/env python3
"""Team cleanup boundaries with isolated SQLite and no native mutations."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_agent_management import manage_agent, _missing_transferred_history, _finished
from codex_efficiency import EfficiencyMixin, digest, finished_worktree_ids, remember_context_manifest
from codex_tool_requests import RequestMixin

class Store(EfficiencyMixin, RequestMixin):
    def __init__(self, path):
        self.path=path; self.lock=threading.RLock(); self.preparations={}; self.claims={}; self.calls=[]
        self.native_state='idle'; self.connection_ids={'default':'connection'}
        self.scheduler=threading.current_thread()
        owner=self
        class Native:
            def call(self, method, params, timeout):
                owner.calls.append((method,params['threadId']))
                return {'thread':{'id':params['threadId'],'status':{'type':owner.native_state}}}
        self.servers={'default':Native()}
        with self.db() as db:
            for table in ['agents','monitors','tasks','requests','work','tool_requests','items']:
                db.execute(f'CREATE TABLE runtime_{table} (id TEXT PRIMARY KEY, record TEXT)')
            db.execute("CREATE TABLE runtime_events (id TEXT PRIMARY KEY, agent TEXT, epoch INT, kind TEXT DEFAULT 'user', status TEXT)")
            db.execute('CREATE TABLE runtime_account_transfers (id TEXT PRIMARY KEY, record TEXT)')
            db.execute('CREATE TABLE runtime_tool_results (id TEXT PRIMARY KEY, result TEXT)')
            db.execute('CREATE TABLE runtime_completed_turns (id TEXT PRIMARY KEY)')
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
    def test_missing_empty_transfer_history_is_reported_without_replaying(self):
        db=sqlite3.connect(':memory:');db.row_factory=sqlite3.Row
        db.execute('CREATE TABLE runtime_account_transfers (id TEXT, record TEXT)')
        db.execute('CREATE TABLE runtime_events (agent TEXT, epoch INT, kind TEXT, status TEXT)')
        aid='worker';tid='target-thread';transfer_id='transfer-empty'
        transfer={'id':transfer_id,'status':'completed','members':{aid:{
            'phase':'completed','nativeMethod':'thread/start','sourceThreadId':None,
            'result':{'thread':{'id':tid}}}}}
        db.execute('INSERT INTO runtime_account_transfers VALUES (?,?)',(transfer_id,json.dumps(transfer)))
        db.execute('INSERT INTO runtime_events VALUES (?,?,?,?)',(aid,1,'user','failed'))
        agent={'id':aid,'epoch':1,'threadId':tid,'error':f'no rollout found for thread id {tid}',
               'accountHistory':[{'transferId':transfer_id,'threadId':None,'targetThreadId':tid}]}
        result=_missing_transferred_history(db,agent)
        self.assertEqual(result['status'],'history_missing')
        self.assertFalse(result['replayed'])
        self.assertEqual(result['evidence']['events'],{'user:failed':1})
        self.assertEqual(db.execute('SELECT status FROM runtime_events').fetchone()[0],'failed')
        db.close()
    def test_recover_routes_proven_empty_transfer_to_native_replacement(self):
        from unittest.mock import Mock
        from codex_account_transfer import transfer_store
        transfer_id='transfer-empty';tid='target-thread'
        transfer={'id':transfer_id,'status':'pending','members':{'worker':{
            'phase':'completed','nativeMethod':'thread/start','sourceThreadId':None,
            'result':{'thread':{'id':tid}}}}}
        self.update('agents', {'id':'worker','name':'worker','parentId':'lead','rootId':'lead',
                               'isLead':False,'autoWake':True,'epoch':1,'status':'failed',
                               'inFlight':False,'threadId':tid,'error':f'no rollout found for thread id {tid}',
                               'accountHistory':[{'transferId':transfer_id,'threadId':None,
                                                  'targetThreadId':tid}]})
        with self.rt.db() as db:
            db.execute('INSERT INTO runtime_account_transfers VALUES (?,?)',
                       (transfer_id,json.dumps(transfer)))
        replacement = Mock()
        replacement.recover_empty_transferred_thread.return_value={'status':'recovered','replayed':False}
        with patch('codex_account_transfer.transfer_store', return_value=replacement):
            result=self.call('recover')
        self.assertEqual(result, {'status':'recovered','replayed':False})
        replacement.recover_empty_transferred_thread.assert_called_once_with('worker')
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
    def test_list_and_inspect_include_cached_worktree_disk_use(self):
        disk = {'totalBytes': 4096, 'allWorkersBytes': 8192, 'limitBytes': 8192,
                'warning': True, 'unmeasured': 0}
        def view(_runtime, agents, **_kwargs):
            return ({a['id']: {'state': 'ready', 'bytes': 4096} for a in agents}, disk)
        with patch('codex_worktree_disk.management_view', side_effect=view):
            listed = self.call('list')
            inspected = self.call('inspect')
        self.assertEqual(listed['items'][0]['worktreeDisk']['bytes'], 4096)
        self.assertEqual(listed['disk'], disk)
        self.assertEqual(inspected['agent']['worktreeDisk']['bytes'], 4096)
        self.assertEqual(inspected['disk'], disk)
        with self.assertRaisesRegex(ValueError, 'Only the active orchestrator'):
            manage_agent(self.rt, 'worker', {'action': 'list'}, 1)
    def test_each_unfinished_operation_blocks_archive(self):
        for table,row in [('monitors',{'id':'m','agent':'worker','status':'running'}),
                          ('tasks',{'id':'t','agent':'worker','status':'running'}),
                          ('requests',{'id':'q','agent':'worker','status':'pending'}),
                          ('work',{'id':'w','owner':'worker','status':'review'}),
                          ('tool_requests',{'id':'r','agent':'worker','stage':'running','outcome':'unknown'})]:
            with self.subTest(table=table):
                self.update(table,row);self.assertEqual(self.call('archive')['status'],'blocked')
                with self.rt.db() as db:db.execute(f'DELETE FROM runtime_{table}')
        self.worker(inFlight=True);self.assertFalse(self.call('inspect')['canArchive'])
        self.assertEqual(self.call('archive')['status'],'blocked')
    def test_finished_unknown_receipt_is_preserved_without_false_success(self):
        self.update('tool_requests', {'id':'unknown-monitor','agent':'worker',
                                      'stage':'failed','outcome':'unknown'})
        archived = self.call('archive')
        self.assertEqual(archived['status'], 'archived')
        self.assertEqual(archived['agent']['agentArchive']['unknownToolRequests'], ['unknown-monitor'])
        with self.rt.db() as db:
            self.assertEqual(self.rt.records(db,'tool_requests')[0]['outcome'],'unknown')
    def test_dead_command_with_completed_turn_is_settled_unknown(self):
        self.update('tasks', {'id':'dead','agent':'worker','status':'running','kind':'command',
                              'turnId':'done','processId':'12345'})
        with self.rt.db() as db:db.execute("INSERT INTO runtime_completed_turns VALUES ('worker:done')")
        with patch('codex_agent_management.os.kill', side_effect=ProcessLookupError):
            self.assertEqual(self.call('archive')['status'],'archived')
        with self.rt.db() as db:
            task=self.rt.records(db,'tasks')[0]
            self.assertEqual(task['status'],'lost')
            self.assertIn('Outcome unknown',task['error'])
    def test_live_command_keeps_archive_blocked(self):
        self.update('tasks', {'id':'live','agent':'worker','status':'running','kind':'command',
                              'turnId':'done','processId':str(os.getpid())})
        with self.rt.db() as db:db.execute("INSERT INTO runtime_completed_turns VALUES ('worker:done')")
        self.assertEqual(self.call('archive')['blockers'][0]['kind'],'background_tasks')
    def test_unknown_input_blocks_archive_but_legacy_claim_does_not(self):
        with self.rt.db() as db:db.execute("INSERT INTO runtime_events VALUES ('e','worker',1,'user','uncertain')")
        self.assertEqual(self.call('archive')['status'],'blocked')
        with self.rt.db() as db:db.execute('DELETE FROM runtime_events')
        self.rt.claims={'build':{'worker':'worker'}}
        self.assertEqual(self.call('archive')['status'],'archived')
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
    def test_completed_native_turn_allows_archive_when_account_is_offline(self):
        self.worker(lastCompletedTurn='done', startAttempt={'turnId':'done'}, turnId=None)
        self.rt.servers.clear()
        self.assertEqual(self.call('archive')['status'],'archived')
    def test_bulk_archives_finished_worker_without_worktree(self):
        self.worker(status='failed', worktreeReady=False, lastCompletedTurn='done',
                    startAttempt={'turnId':'done'}, turnId=None)
        self.rt.servers.clear()
        result = self.call('archive_finished')
        self.assertEqual(result['archived'], 1)
        self.assertEqual(result['kept'], [])
    def test_active_native_thread_blocks_archive_despite_completed_turn_record(self):
        self.worker(lastCompletedTurn='done', startAttempt={'turnId':'done'}, turnId=None)
        self.rt.native_state='active'
        self.assertEqual(self.call('archive')['blockers'][0]['kind'],'native_state_unconfirmed')
    def test_malformed_native_state_blocks_archive(self):
        self.rt.servers['default'] = type('Malformed', (), {
            'call': lambda self, method, params, timeout: {'thread': None}})()
        self.assertEqual(self.call('archive')['blockers'][0]['kind'],'native_state_unconfirmed')
    def test_missing_receipt_ledger_cannot_authorize_archive(self):
        with self.rt.db() as db:
            db.execute('DROP TABLE runtime_tool_requests')
        with self.assertRaises(sqlite3.OperationalError):
            self.call('archive')
        with self.rt.db() as db:
            self.assertNotIn('deletedAt', self.rt.agent('worker', db))
        self.assertEqual(self.rt.calls, [])

    def test_restoration_of_paused_worker_does_not_use_active_limit(self):
        self.call('archive')
        with self.rt.db() as db:
            a=self.rt.agent('lead',db);a['maxAgents']=1;self.rt.put(db,'agents',a)
        self.assertEqual(self.call('restore')['status'],'restored')


class RealWorktreeContract(Contract):
    def setUp(self):
        super().setUp()
        self.repo = Path(self.tmp.name) / 'project'
        self.repo.mkdir()
        self.git('init', '-q')
        self.git('config', 'user.email', 'fixture@example.test')
        self.git('config', 'user.name', 'Fixture')
        (self.repo / 'tracked.txt').write_text('initial\n')
        (self.repo / '.gitignore').write_text('build/\n.worktrees/\n')
        self.git('add', 'tracked.txt', '.gitignore')
        self.git('commit', '-qm', 'initial')
        self.checkout('worker')

    def git(self, *args, cwd=None):
        return subprocess.run(['git', '-C', str(cwd or self.repo), *args], check=True,
                              capture_output=True, text=True, timeout=30).stdout.strip()

    def checkout(self, key, status='completed'):
        path = self.repo / '.worktrees' / 'codex-agents' / key
        self.git('worktree', 'add', '-q', '-b', 'codex-agent/' + key, str(path))
        with self.rt.db() as db:
            try: a = self.rt.agent(key, db)
            except ValueError:
                a = {'id': key, 'name': key, 'parentId': 'lead', 'rootId': 'lead',
                     'isLead': False, 'autoWake': False, 'epoch': 1, 'threadId': key,
                     'inFlight': False}
            a.update(cwd=str(path), branch='codex-agent/' + key, worktree=True,
                     worktreeReady=True, status=status)
            self.rt.put(db, 'agents', a)
        return path

    def image_workspace(self):
        from types import ModuleType
        mount = Path(self.tmp.name) / 'image'
        image_repo = mount / 'repo'
        mount.mkdir()
        subprocess.run(['git', 'clone', '-q', '--shared', str(self.repo), str(image_repo)], check=True)
        self.worker(cwd=str(image_repo), worktree=False, worktreeReady=False,
                    imageWorkspace=True, imageWorkspaceReady=True, imageWorkspacePhase='ready',
                    imageWorkspaceRepo=str(self.repo), imageWorkspaceRelative='.',
                    imageWorkspaceMount=str(mount))
        engine = ModuleType('codex_workspace_images')
        engine.workspace_bytes = lambda _agent: 4096
        engine.ensure_mounted = lambda _agent: {'mount': str(mount), 'repoPath': str(image_repo)}
        engine.exec_prefix = lambda: []
        engine.collect = lambda _agent: {'state': 'collected'}
        engine.remove_workspace = lambda _agent: {'freedBytes': 4096}
        return mount, image_repo, engine

    def test_image_archive_keeps_uncommitted_and_untracked_changes(self):
        from unittest.mock import Mock
        mount, image_repo, engine = self.image_workspace()
        (image_repo / 'draft.txt').write_text('unfinished')
        engine.collect = Mock(return_value={'state': 'collected'})
        engine.remove_workspace = Mock(return_value={'freedBytes': 4096})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            result = self.call('archive')
        self.assertEqual(result['workspace']['state'], 'kept')
        self.assertIn('uncommitted or untracked', result['workspace']['reason'])
        self.assertTrue((image_repo / 'draft.txt').is_file())
        engine.collect.assert_called_once_with('worker')
        engine.remove_workspace.assert_not_called()

    def test_conflict_archive_restores_from_the_collected_raw_head(self):
        from unittest.mock import Mock
        mount, image_repo, engine = self.image_workspace()
        (self.repo / 'raw-only.txt').write_text('raw result\n')
        self.git('add', 'raw-only.txt')
        self.git('commit', '-qm', 'raw agent commit')
        raw_head = self.git('rev-parse', 'HEAD')
        self.git('update-ref', 'refs/studio/agents/worker/raw', raw_head)
        engine.collect = Mock(return_value={
            'state': 'conflict', 'conflict': 'raw-only.txt',
            'rawRef': 'refs/studio/agents/worker/raw'})
        engine.remove_workspace = Mock(return_value={'freedBytes': 4096})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            archived = self.call('archive')
            self.assertEqual(archived['workspace']['state'], 'conflict')
            with self.rt.db() as db:
                saved = self.rt.agent('worker', db)['cleanedImageWorkspace']
            self.assertEqual(saved['head'], raw_head)
            restored_repo = Path(self.tmp.name) / 'restored' / 'repo'
            restored_repo.mkdir(parents=True)
            engine.create_workspace = Mock(return_value={
                'mount': str(restored_repo.parent), 'repoPath': str(restored_repo),
                'branch': 'codex-agent/worker', 'snapshotCommit': None})
            restored = self.call('restore')
        self.assertEqual(restored['status'], 'restored')
        engine.create_workspace.assert_called_once_with(
            str(self.repo), 'worker', start_commit=raw_head)

    def test_clean_archive_keeps_ref_and_restore_recreates_checkout(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        head = self.git('rev-parse', 'HEAD', cwd=path)
        (path / 'build').mkdir()
        (path / 'build' / 'cache').write_text('ignored bytes')
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'removed')
        self.assertGreater(result['worktree']['bytes'], 0)
        self.assertFalse(path.exists())
        self.assertEqual(self.git('rev-parse', 'refs/codex-agents/archive/worker'), head)
        self.assertEqual(self.git('rev-parse', 'refs/heads/codex-agent/worker'), head)
        restored = self.call('restore')
        self.assertEqual(restored['status'], 'restored')
        self.assertTrue(path.is_dir())
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=path), head)
        self.assertFalse((path / 'build' / 'cache').exists())

    def test_missing_worktree_archives_branch_and_prunes_registration(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        sibling = self.checkout('sibling')
        head = self.git('rev-parse', 'HEAD', cwd=path)
        shutil.rmtree(path)
        result = self.call('archive')
        self.assertEqual(result['status'], 'archived')
        self.assertEqual(result['worktree']['state'], 'missing')
        self.assertEqual(result['worktree']['reason'], 'worktree folder missing; branch saved to archive ref')
        self.assertEqual(self.git('rev-parse', 'refs/codex-agents/archive/worker'), head)
        self.assertEqual(self.git('rev-parse', 'refs/heads/codex-agent/worker'), head)
        self.assertTrue(sibling.is_dir())
        self.assertIn(str(sibling.resolve()), self.git('worktree', 'list', '--porcelain'))
        self.assertNotIn(str(path), self.git('worktree', 'list', '--porcelain'))
        with self.rt.db() as db:
            agent = self.rt.agent('worker', db)
            self.assertFalse(agent['worktreeReady'])
            self.assertTrue(agent['cleanedWorktree']['missing'])
        restored = self.call('restore')
        self.assertEqual(restored['status'], 'restored')
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=path), head)

    def test_missing_worktree_without_branch_archives_with_no_save_note(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        shutil.rmtree(path)
        self.git('update-ref', '-d', 'refs/heads/codex-agent/worker')
        result = self.call('archive')
        self.assertEqual(result['status'], 'archived')
        self.assertEqual(result['worktree']['state'], 'missing')
        self.assertEqual(result['worktree']['reason'], 'worktree folder missing; nothing to save')
        self.assertNotIn('refs/codex-agents/archive/worker', self.git('show-ref'))
        self.assertNotIn(str(path), self.git('worktree', 'list', '--porcelain'))
        restored = self.call('restore')
        self.assertEqual(restored['status'], 'blocked')
        self.assertIn('no verified ref', restored['reason'])
        self.assertFalse(path.exists())

    def test_restore_refuses_archived_missing_worktree_without_saved_evidence(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        shutil.rmtree(path)
        self.worker(deletedAt=1, agentArchive={'at': 1, 'epoch': 1}, worktreeReady=True)
        result = self.call('restore')
        self.assertEqual(result['status'], 'blocked')
        self.assertIn('no verified restore ref', result['reason'])
        with self.rt.db() as db:
            self.assertEqual(self.rt.agent('worker', db)['deletedAt'], 1)

    def test_maintenance_report_lists_live_worker_with_missing_folder(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        shutil.rmtree(path)
        report = self.call('maintenance_report')['worktrees']
        self.assertIn({'id': 'worker', 'reason': 'worktree folder missing', 'folderMissing': True}, report)

    def test_bulk_archives_mix_of_missing_present_and_blocked_worktrees(self):
        missing = self.checkout('missing')
        present = self.checkout('present')
        active = self.checkout('active')
        shutil.rmtree(missing)
        self.git('update-ref', '-d', 'refs/heads/codex-agent/missing')
        with self.rt.db() as db:
            row = self.rt.agent('active', db)
            row.update(status='completed', inFlight=True)
            self.rt.put(db, 'agents', row)
        result = self.call('archive_finished')
        self.assertEqual(result['archived'], 3)
        self.assertEqual(result['notes'], [{'id': 'missing', 'reason': 'worktree folder missing; nothing to save'}])
        self.assertFalse(present.exists())
        self.assertTrue(active.exists())
        self.assertTrue(any(item['id'] == 'active' and 'active_turn' in item['reason']
                            for item in result['kept']))

    def test_pruning_missing_worktree_does_not_remove_existing_folder_or_branch(self):
        missing = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        existing = self.checkout('existing')
        branch_head = self.git('rev-parse', 'refs/heads/codex-agent/existing')
        shutil.rmtree(missing)
        self.call('archive')
        self.assertTrue(existing.is_dir())
        self.assertEqual(self.git('rev-parse', 'refs/heads/codex-agent/existing'), branch_head)

    def test_dirty_and_untracked_worktrees_stay_with_exact_reason(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        (path / 'tracked.txt').write_text('changed\n')
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertIn('tracked or untracked', result['worktree']['reason'])
        self.assertTrue(path.exists())
        self.assertEqual(self.call('restore')['status'], 'restored')
        (path / 'tracked.txt').write_text('initial\n')
        (path / 'new.txt').write_text('new')
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertIn('tracked or untracked', result['worktree']['reason'])

    def test_changed_branch_and_detached_head_are_removable_and_restorable(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        self.git('switch', '-c', 'reviewed-branch', cwd=path)
        changed = self.call('archive')
        self.assertEqual(changed['worktree']['state'], 'removed')
        self.assertEqual(self.call('restore')['agent']['status'], 'paused')
        self.assertEqual(self.git('branch', '--show-current', cwd=path), 'reviewed-branch')
        self.git('switch', '--detach', cwd=path)
        detached = self.call('archive')
        self.assertEqual(detached['worktree']['state'], 'removed')
        self.call('restore')
        self.assertEqual(self.git('branch', '--show-current', cwd=path), '')

    def test_bulk_skips_active_and_keeps_deleted_worktrees(self):
        second = self.checkout('second')
        third = self.checkout('third')
        self.worker(inFlight=True)
        result = self.call('archive_finished')
        self.assertEqual(result['archived'], 2)
        self.assertFalse(second.exists())
        self.assertFalse(third.exists())
        self.assertTrue(any(item['id'] == 'worker' and 'active_turn' in item['reason']
                            for item in result['kept']))
        self.assertEqual(self.call('archive_finished')['archived'], 0)
        self.assertTrue((self.repo / '.worktrees' / 'codex-agents' / 'worker').exists())
        self.worker(inFlight=False, deletedAt=1)
        report = self.call('maintenance_report')['worktrees']
        self.assertEqual(report, [{'id': 'worker', 'reason': 'removable', 'folderMissing': False}])
        self.assertTrue((self.repo / '.worktrees' / 'codex-agents' / 'worker').exists())

    def test_bulk_archives_clean_worktree_with_unknown_receipt(self):
        self.update('tool_requests', {'id':'unknown-monitor','agent':'worker',
                                      'tool':'orchestration_monitor',
                                      'stage':'failed','outcome':'unknown'})
        result = self.call('archive_finished')
        self.assertEqual(result['archived'], 1)
        self.assertFalse((self.repo / '.worktrees' / 'codex-agents' / 'worker').exists())
        with self.rt.db() as db:
            receipt = self.rt.agent('worker', db)['agentArchive']
        self.assertEqual(receipt['unknownToolRequests'], ['unknown-monitor'])

    def test_inspect_reconciles_failed_review_without_child(self):
        self.update('tool_requests', {'id':'worker:review:preflight','agent':'worker',
            'tool':'orchestration_review','stage':'failed','outcome':'unknown',
            'result':{'success':False,'contentItems':[{'type':'inputText','text':'not a repository'}]}})
        inspected = self.call('inspect')
        self.assertTrue(inspected['canArchive'])
        self.assertEqual(inspected['blockers'], [])
        with self.rt.db() as db:
            receipt = self.rt.records(db,'tool_requests')[0]
        self.assertEqual(receipt['outcome'], 'not_applied')
        self.assertEqual(receipt['result']['contentItems'][0]['text'], 'not a repository')

    def test_non_lead_cannot_bulk_archive(self):
        with self.assertRaisesRegex(ValueError, 'Only the active orchestrator'):
            manage_agent(self.rt, 'worker', {'action': 'archive_finished'}, 1)

    def test_nested_registered_worktree_is_never_removed_with_ignored_files(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        child = path / '.worktrees' / 'codex-agents' / 'child'
        self.git('worktree', 'add', '-q', '-b', 'codex-agent/child', str(child))
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertIn('nested', result['worktree']['reason'])
        self.assertTrue(child.exists())

    def test_path_outside_agent_worktree_folder_is_never_removed(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        self.worker(cwd=str(self.repo))
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertIn('outside', result['worktree']['reason'])
        self.assertTrue(path.exists())

    def test_retry_finishes_a_removed_worktree_with_saved_cleanup_stage(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        head = self.git('rev-parse', 'HEAD', cwd=path)
        with self.rt.db() as db:
            a = self.rt.agent('worker', db)
            a.update(deletedAt=1, autoWake=False, status='paused', epoch=2,
                     agentArchive={'at': 1, 'epoch': 2, 'cleanupPending': True},
                     worktreeCleanup={'root': str(path.resolve()), 'repo': str(self.repo.resolve()),
                                      'relative': '.', 'branch': 'codex-agent/worker',
                                      'head': head, 'bytes': 4096,
                                      'identity': [2, str(path), 1]})
            self.rt.put(db, 'agents', a)
        self.git('update-ref', 'refs/codex-agents/archive/worker', head)
        self.git('worktree', 'remove', '--force', str(path))
        result = self.call('archive')
        self.assertEqual(result['worktree'], {'state': 'removed', 'bytes': 4096})
        with self.rt.db() as db:
            self.assertFalse(self.rt.agent('worker', db)['worktreeReady'])

    def test_restore_adopts_exact_checkout_after_lost_database_result(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        self.assertEqual(self.call('archive')['worktree']['state'], 'removed')
        self.git('worktree', 'add', str(path), 'codex-agent/worker')
        self.assertEqual(self.call('restore')['status'], 'restored')
        with self.rt.db() as db:
            self.assertTrue(self.rt.agent('worker', db)['worktreeReady'])

class RuntimeRouteContract(unittest.TestCase):
    def test_native_tool_preserves_receipts_and_ui_visibility(self):
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
                result=next(r for r in reversed(rt.server.responses) if r['id']==number)['result']
                self.assertTrue(result['success'],str(result))
                return result
            first_bulk=invoke(9200,'orchestration_agent_manage',{'action':'archive_finished'})
            self.assertEqual(invoke(9200,'orchestration_agent_manage',{'action':'archive_finished'}),first_bulk)
            invoke(9201,'orchestration_agent_manage',{'action':'inspect','agent_id':worker['id']})
            archive = {'action':'archive','agent_id':worker['id'],'reason':'Reviewed fixture'}
            invoke(9202,'orchestration_agent_manage',archive)
            self.assertNotIn(worker['id'],[a['id'] for a in rt.snapshot()['agents']])
            epoch=rt.agent(worker['id'])['epoch']
            invoke(9202,'orchestration_agent_manage',archive)
            self.assertEqual(epoch,rt.agent(worker['id'])['epoch'])
            invoke(9203,'orchestration_agent_manage',{'action':'restore','agent_id':worker['id']})
            self.assertIn(worker['id'],[a['id'] for a in rt.snapshot()['agents']])
            self.assertFalse(rt.agent(worker['id'])['autoWake'])
        finally:case.tearDown()

    def test_lead_reminder_starts_at_three_and_deduplicates_same_set(self):
        import importlib.util
        spec=importlib.util.spec_from_file_location('runtime_fixture',Path(__file__).with_name('runtime-contract.py'))
        f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)
        case=f.RuntimeContract();case.setUp()
        try:
            rt=case.runtime;lead=case.lead()
            workers=[rt.create({'name':f'Worker {i}','prompt':'Review','role':'reviewer'},
                               lead['id'],defer=True) for i in range(3)]
            with rt.lock,rt.db() as db:
                for worker in workers:
                    a=rt.agent(worker['id'],db)
                    a.update(status='completed',autoWake=False,worktreeReady=True)
                    rt.put(db,'agents',a)
                third=rt.agent(workers[2]['id'],db)
                third['worktreeReady']=False;rt.put(db,'agents',third)
                actor=rt.agent(lead['id'],db)
                rt.enqueue(db,actor,'user','Continue','reminder-two')
                self.assertNotIn('archive_finished',rt.model_turn_context(db,actor,'reminder-two'))
                third['worktreeReady']=True;rt.put(db,'agents',third)
                rt.enqueue(db,actor,'user','Continue','reminder-three')
                self.assertIn('Studio: 3 finished workers keep worktrees.',
                              rt.model_turn_context(db,actor,'reminder-three'))
                db.execute("UPDATE runtime_events SET status='delivered' WHERE id='reminder-three'")
                remember_context_manifest(db, actor['id'], 'reminder-three')
                rt.enqueue(db,actor,'user','Continue','reminder-repeat')
                self.assertNotIn('archive_finished',rt.model_turn_context(db,actor,'reminder-repeat'))
                actor['compactions']=1
                rt.enqueue(db,actor,'user','Continue','reminder-compacted')
                self.assertNotIn('archive_finished',rt.model_turn_context(db,actor,'reminder-compacted'))
        finally:case.tearDown()

    def test_reminder_query_matches_full_scan_for_worker_states(self):
        import importlib.util
        spec=importlib.util.spec_from_file_location('runtime_fixture',Path(__file__).with_name('runtime-contract.py'))
        f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)
        case=f.RuntimeContract();case.setUp()
        try:
            rt=case.runtime;lead=case.lead()
            states=[('completed',False,False,True,False),
                    ('failed',False,False,True,False),
                    ('interrupted',False,False,True,False),
                    ('paused',False,False,True,False),
                    ('completed',False,False,True,True),
                    ('paused',True,False,True,False),
                    ('running',True,False,True,False),
                    ('completed',False,True,True,False),
                    ('completed',False,False,False,False)]
            workers=[rt.create({'name':f'Worker {i}','prompt':'Review','role':'reviewer'},
                               lead['id'],defer=True) for i in range(len(states))]
            with rt.lock,rt.db() as db:
                for worker,(status,auto_wake,deleted,ready,archived) in zip(workers,states):
                    a=rt.agent(worker['id'],db)
                    a.update(status=status,autoWake=auto_wake,worktreeReady=ready,archived=archived)
                    if deleted:
                        a['deletedAt']=1
                    rt.put(db,'agents',a)
                old=sorted(a['id'] for a in rt.records(db,'agents')
                           if a['rootId']==lead['id'] and a['id']!=lead['id']
                           and not a.get('deletedAt') and a.get('worktreeReady') and _finished(a))
                self.assertEqual(finished_worktree_ids(db,lead['id']),old)
                self.assertEqual(len(old),5)
                actor=rt.agent(lead['id'],db)
                rt.enqueue(db,actor,'user','Continue','reminder-mixed')
                text=rt.model_turn_context(db,actor,'reminder-mixed')
                self.assertIn(f'Studio: {len(old)} finished workers keep worktrees.',text)
                meta=json.loads(db.execute('SELECT record FROM runtime_event_meta WHERE id=?',
                                           ('reminder-mixed',)).fetchone()[0])
                self.assertEqual(meta['contextManifest']['versions']['worktreeReminder'],digest(old))
        finally:case.tearDown()

if __name__=='__main__':unittest.main()
