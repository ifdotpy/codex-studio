#!/usr/bin/env python3
"""Team cleanup boundaries with isolated SQLite and no native mutations."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

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

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from studio_api.testing import read_runtime_state
from codex_agent_management import manage_agent, _missing_transferred_history, _finished
from codex_efficiency import EfficiencyMixin, digest, finished_worktree_ids, remember_context_manifest
from codex_tool_requests import RequestMixin

class Store(EfficiencyMixin, RequestMixin):
    from codex_runtime import Runtime
    agent_connection = Runtime.agent_connection
    ensure_image_workspace = Runtime.ensure_image_workspace
    workspace_exec_prefix = Runtime.workspace_exec_prefix

    def __init__(self, path):
        self.path=path; self.lock=threading.RLock(); self.preparations={}; self.claims={}; self.calls=[]
        self.closed=False
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
    def server_for(self,account_key,connection_id=None):
        if connection_id is not None:
            for agent_id,current in self.__dict__.get('linux_connection_ids',{}).items():
                if current == connection_id:
                    with self.db() as db: agent=self.agent(agent_id,db)
                    if agent.get('accountKey','default') != account_key: return None
                    return self.__dict__.get('linux_servers',{}).get(agent_id)
        return self.servers.get(account_key)
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
    def test_list_and_inspect_do_not_measure_worktree_sizes(self):
        with (
            patch('os.scandir', side_effect=AssertionError('Unexpected folder scan')),
            patch('os.walk', side_effect=AssertionError('Unexpected folder walk')),
        ):
            listed = self.call('list')
            archived = self.call('list_archived')
            inspected = self.call('inspect')
        self.assertNotIn('disk', listed)
        self.assertNotIn('disk', archived)
        self.assertNotIn('disk', inspected)
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
    def test_archive_does_not_load_other_workers_or_boards(self):
        from unittest.mock import Mock
        self.worker(threadId=None, worktreeReady=False)
        original_records = self.rt.records
        def scoped_records(db, table):
            if table in {'agents', 'requests', 'work'}:
                raise AssertionError('Archive loaded the whole workspace')
            return original_records(db, table)
        self.rt.records = scoped_records
        self.rt.release_failed_work = Mock(return_value=[])
        self.rt._release_work_after = 42
        result = self.call('archive')
        self.assertEqual(result['status'], 'archived')
        self.assertEqual(result['worktree']['state'], 'none')
        self.assertFalse(result['agent']['agentArchive']['cleanupPending'])
        self.rt.release_failed_work.assert_not_called()
        self.assertEqual(self.rt._release_work_after, 42)
        self.assertTrue(self.call('archive')['replayed'])
        with self.rt.db() as db:
            self.assertEqual(self.rt.agent('foreign', db)['status'], 'completed')

    def test_archive_without_native_or_worktree_has_no_external_cleanup(self):
        self.worker(threadId=None, worktreeReady=False)
        with patch('codex_agent_management._cleanup_worktree', side_effect=AssertionError('No worktree needs cleanup')):
            result = self.call('archive')
        self.assertEqual(result['status'], 'archived')
        self.assertFalse(result['agent']['agentArchive']['cleanupPending'])
        self.assertEqual(self.rt.calls, [])
        with self.rt.db() as db:
            self.assertEqual(self.rt.agent('worker', db)['epoch'], 2)
            self.assertEqual(self.rt.records(db, 'items')[0]['text'], 'Preserved result')

    def test_archive_with_saved_cleanup_uses_existing_cleanup_path(self):
        for field in ('worktreeCleanup', 'cleanedWorktree'):
            with self.subTest(field=field):
                self.worker(threadId=None, worktreeReady=False, **{field:{'root':'saved'}})
                with patch('codex_agent_management._cleanup_worktree', return_value={'state':'kept'}) as cleanup:
                    result = self.call('archive')
                self.assertEqual(result['worktree']['state'], 'kept')
                cleanup.assert_called_once()
                with self.rt.db() as db:
                    a = self.rt.agent('worker', db)
                    for key in ('agentArchive', 'deletedAt', 'worktreeCleanup', 'cleanedWorktree'):
                        a.pop(key, None)
                    a.update(epoch=1, autoWake=True)
                    self.rt.put(db, 'agents', a)

    def test_scoped_archive_keeps_blockers_without_status_and_all_counts(self):
        self.worker(threadId=None)
        self.update('work', {'id':'missing-status', 'owner':'worker'})
        result = self.call('archive')
        self.assertEqual(result['blockers'], [{'kind':'assigned_work', 'count':1, 'ids':['missing-status']}])
        with self.rt.db() as db:
            db.execute('DELETE FROM runtime_work')
        for number in range(25):
            self.update('requests', {'id':f'question-{number}', 'agent':'worker', 'status':'pending'})
        self.update('requests', {'id':'unrelated', 'agent':'foreign', 'status':'pending'})
        result = self.call('archive')
        self.assertEqual(result['blockers'], [{'kind':'questions', 'count':25,
                                             'ids':[f'question-{number}' for number in range(20)]}])
        with self.rt.db() as db:
            self.assertNotIn('deletedAt', self.rt.agent('worker', db))
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
    def test_system_error_and_stale_release_receipt_allow_fresh_archive(self):
        self.worker(status='failed', lastCompletedTurn='done', startAttempt={'turnId':'done'},
                    turnId=None, nativeRelease={'id':'old', 'phase':'blocked',
                                                'targetEpoch':0, 'threadId':'worker'})
        self.rt.native_state='systemError'
        self.assertEqual(self.call('archive')['status'],'archived')
        self.assertEqual(self.rt.calls,[('thread/read','worker')])
    def test_linux_worker_archive_reads_guest_thread_status(self):
        self.worker(status='failed', environment='linux', imageWorkspaceReady=True,
                    lastCompletedTurn='done', startAttempt={'turnId':'done'}, turnId=None,
                    nativeRelease={'id':'old', 'phase':'blocked', 'targetEpoch':0})
        guest_calls=[]
        class Guest:
            def call(self, method, params, timeout):
                guest_calls.append((method, params['threadId']))
                return {'thread': {'id': params['threadId'], 'status': {'type': 'systemError'}}}
        self.rt.__dict__['linux_connection_ids']={'worker':'guest-connection'}
        self.rt.__dict__['linux_servers']={'worker':Guest()}
        with patch('codex_linux_workspaces.dispose', return_value={'freedBytes': 0}):
            self.assertEqual(self.call('archive')['status'],'archived')
        self.assertEqual(guest_calls,[('thread/read','worker')])
        self.assertEqual(self.rt.calls,[])
    def test_finished_linux_worker_without_guest_connection_uses_durable_completion(self):
        self.worker(status='completed', environment='linux', imageWorkspaceReady=True,
                    lastCompletedTurn='done', lastCompletedTurnStatus='completed',
                    startAttempt=None, turnId=None, inFlight=False)
        host_calls=[]
        class Host:
            def call(self, method, params, timeout):
                host_calls.append((method, params['threadId']))
                raise ValueError('Thread is not known to the host provider')
        self.rt.servers['default']=Host()
        with patch('codex_linux_workspaces.dispose', return_value={'freedBytes': 0}):
            self.assertEqual(self.call('archive')['status'],'archived')
        self.assertEqual(host_calls,[('thread/read','worker')])
        self.assertNotIn('worker', self.rt.__dict__.get('linux_connection_ids', {}))
        self.assertNotIn('worker', self.rt.__dict__.get('linux_servers', {}))
    def test_active_linux_thread_still_blocks_archive(self):
        self.worker(status='completed', environment='linux', imageWorkspaceReady=True,
                    lastCompletedTurn='done', lastCompletedTurnStatus='completed',
                    startAttempt=None, turnId=None, inFlight=False)
        self.rt.native_state='active'
        self.rt.__dict__['linux_connection_ids']={'worker':'guest-connection'}
        self.rt.__dict__['linux_servers']={'worker':self.rt.servers['default']}
        self.assertEqual(self.call('archive')['blockers'][0]['kind'],'native_state_unconfirmed')
    def test_finished_linux_worker_without_terminal_turn_status_stays_blocked(self):
        self.worker(status='completed', environment='linux', imageWorkspaceReady=True,
                    lastCompletedTurn='done', startAttempt=None, turnId=None, inFlight=False)
        self.rt.servers.clear()
        self.assertEqual(self.call('archive')['blockers'][0]['kind'],'native_state_unconfirmed')
    def test_completed_native_turn_allows_archive_when_account_is_offline(self):
        self.worker(lastCompletedTurn='done', lastCompletedTurnStatus='completed',
                    startAttempt={'turnId':'done'}, turnId=None)
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


class ArchiveBacklogContract(Contract):
    def task(self, key, status='running', **values):
        record = {'id': key, 'rootId': 'lead', 'owner': 'worker', 'status': status,
                  'version': 7, 'title': key, 'description': 'Keep this task',
                  'dependencies': ['dependency'], 'results': [{'id': 'evidence'}],
                  'decisions': [{'resultId': 'evidence', 'decision': 'reject'}],
                  'labels': ['keep'], **values}
        self.update('work', record)
        return record

    def rows(self):
        with self.rt.db() as db:
            return {row['id']: row for row in self.rt.records(db, 'work')}

    def test_default_keeps_assigned_work_and_opt_in_returns_open_tasks_once(self):
        originals = {status: self.task(status, status) for status in
                     ('ready', 'running', 'blocked', 'review', 'accepted', 'cancelled')}
        self.assertEqual(self.call('archive')['blockers'][0]['kind'], 'assigned_work')
        self.assertEqual(self.rows(), originals)
        archived = self.call('archive', unassign_work=True)
        self.assertEqual(archived['status'], 'archived')
        self.assertEqual(archived['unassignedWork'], ['ready', 'running', 'blocked', 'review'])
        rows = self.rows()
        for status in ('ready', 'running', 'blocked', 'review'):
            self.assertIsNone(rows[status]['owner'])
            self.assertEqual(rows[status]['status'], 'ready')
            self.assertEqual(rows[status]['version'], 8)
            for field in ('title', 'description', 'dependencies', 'results', 'decisions', 'labels'):
                self.assertEqual(rows[status][field], originals[status][field])
        for status in ('accepted', 'cancelled'):
            self.assertEqual(rows[status], originals[status])
        replay = self.call('archive', unassign_work=True)
        self.assertTrue(replay['replayed'])
        self.assertEqual(replay['unassignedWork'], archived['unassignedWork'])
        self.assertEqual(self.rows(), rows)
        self.assertEqual(self.rt.calls, [('thread/read', 'worker')])

    def test_archive_and_task_updates_roll_back_together(self):
        original = self.task('open')
        put = self.rt.put
        def fail(db, table, row):
            if table == 'agents' and row['id'] == 'worker' and row.get('agentArchive'):
                raise ValueError('fixture storage failure')
            return put(db, table, row)
        self.rt.put = fail
        with self.assertRaisesRegex(ValueError, 'fixture storage failure'):
            self.call('archive', unassign_work=True)
        self.assertEqual(self.rows()['open'], original)
        with self.rt.db() as db:
            self.assertNotIn('agentArchive', self.rt.agent('worker', db))

    def test_opt_in_checks_all_tasks_scope_before_first_update(self):
        good = self.task('good')
        bad = self.task('foreign', rootId='peer')
        with self.assertRaisesRegex(ValueError, 'another team'):
            self.call('archive', unassign_work=True)
        self.assertEqual(self.rows(), {'good': good, 'foreign': bad})

    def test_opt_in_does_not_stop_active_or_unknown_work(self):
        original = self.task('open')
        for table, record in [('monitors', {'id': 'm', 'agent': 'worker', 'status': 'running'}),
                              ('tasks', {'id': 't', 'agent': 'worker', 'status': 'unknown'}),
                              ('tool_requests', {'id': 'r', 'agent': 'worker', 'stage': 'running', 'outcome': 'unknown'})]:
            with self.subTest(table=table):
                self.update(table, record)
                self.assertEqual(self.call('archive', unassign_work=True)['status'], 'blocked')
                self.assertEqual(self.rows()['open'], original)
                with self.rt.db() as db:
                    self.assertEqual(self.rt.records(db, table), [record])
                    db.execute('DELETE FROM runtime_' + table)
        self.rt.native_state = 'active'
        self.assertEqual(self.call('archive', unassign_work=True)['status'], 'blocked')
        self.assertEqual(self.rows()['open'], original)

    def test_opt_in_rechecks_stop_connection_monitor_and_assignment_after_native_read(self):
        original = self.task('open')
        native = self.rt.servers['default']
        call = native.call
        for race in ('stop', 'connection', 'server', 'monitor'):
            with self.subTest(race=race):
                self.worker(epoch=1, status='completed', autoWake=True)
                self.rt.connection_ids['default'] = 'connection'
                self.rt.servers['default'] = native
                def inspect(method, params, timeout):
                    result = call(method, params, timeout)
                    if race == 'stop': self.worker(epoch=2, autoWake=False, status='paused')
                    elif race == 'connection': self.rt.connection_ids['default'] = 'new'
                    elif race == 'server': self.rt.servers['default'] = object()
                    else: self.update('monitors', {'id': 'new', 'agent': 'worker', 'status': 'running'})
                    return result
                native.call = inspect
                if race == 'monitor':
                    self.assertEqual(self.call('archive', unassign_work=True)['status'], 'blocked')
                else:
                    with self.assertRaisesRegex(ValueError, 'changed during native inspection'):
                        self.call('archive', unassign_work=True)
                self.assertEqual(self.rows()['open'], original)
                with self.rt.db() as db: db.execute('DELETE FROM runtime_monitors')
        self.worker(epoch=1, status='completed', autoWake=True)
        self.rt.connection_ids['default'] = 'connection'
        self.rt.servers['default'] = native
        def reassign(method, params, timeout):
            result = call(method, params, timeout)
            self.task('open', owner='peer')
            self.task('new')
            return result
        native.call = reassign
        result = self.call('archive', unassign_work=True)
        self.assertEqual(result['unassignedWork'], ['new'])
        self.assertEqual(self.rows()['open']['owner'], 'peer')

    def test_opt_in_requires_a_boolean_and_supported_action(self):
        with self.assertRaisesRegex(ValueError, 'boolean'):
            self.call('archive', unassign_work='true')
        with self.assertRaisesRegex(ValueError, 'only to archive'):
            self.call('inspect', unassign_work=True)

    def test_opt_in_no_thread_fast_path_is_atomic_and_preserves_other_tasks(self):
        original = self.task('other', owner='peer')
        self.task('open')
        self.worker(threadId=None)
        result = self.call('archive', unassign_work=True)
        self.assertEqual(result['unassignedWork'], ['open'])
        self.assertEqual(self.rows()['other'], original)
        self.assertEqual(self.rt.calls, [])


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

    def partial_removal(self, change=None):
        import codex_agent_management as management
        root = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        cache = root / 'build' / 'cache.bin'
        cache.parent.mkdir()
        cache.write_bytes(b'saved cache')
        git = management._git
        def remove(repo, *args):
            if args[:3] == ('worktree', 'remove', '--force'):
                admin = Path((root / '.git').read_text().strip().split(': ', 1)[1])
                (root / 'tracked.txt').unlink()
                shutil.rmtree(admin)
                if change: change(root, cache)
                raise subprocess.CalledProcessError(1, ['git', *args])
            return git(repo, *args)
        with patch.object(management, '_git', side_effect=remove):
            result = self.call('archive')
        self.assertEqual(result['status'], 'archived')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertTrue(root.exists())
        self.assertNotIn(str(root), self.git('worktree', 'list', '--porcelain'))
        return root, cache

    def test_git_removal_failure_saves_bounded_stderr_errno_actor_time_and_identity(self):
        import codex_agent_management as management
        root = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        head = self.git('rev-parse', 'HEAD', cwd=root)
        git = management._git
        for failure in (subprocess.CalledProcessError(128, ['git', 'worktree', 'remove'],
                        stderr=b'Permission denied while removing worktree\n' + b'x' * 5000),
                        PermissionError(13, 'Permission denied')):
            with self.subTest(failure=type(failure).__name__):
                def refuse(repo, *args):
                    if args[:3] == ('worktree', 'remove', '--force'):
                        raise failure
                    return git(repo, *args)
                before = management.time.time()
                with patch.object(management, '_git', side_effect=refuse):
                    result = self.call('archive')
                after = management.time.time()
                self.assertEqual(result['worktree']['state'], 'kept')
                self.assertIn('Permission denied', result['worktree']['reason'])
                with self.rt.db() as db:
                    agent = self.rt.agent('worker', db)
                saved = agent['worktreeCleanup']
                self.assertEqual(saved['root'], str(root.resolve()))
                self.assertEqual(saved['head'], head)
                self.assertEqual(saved['branch'], 'codex-agent/worker')
                self.assertTrue(agent['agentArchive']['cleanupPending'])
                self.assertEqual(saved['error']['by'], 'lead')
                self.assertLessEqual(before, saved['error']['at'])
                self.assertLessEqual(saved['error']['at'], after)
                if isinstance(failure, subprocess.CalledProcessError):
                    self.assertEqual(saved['error']['returncode'], 128)
                    self.assertEqual(len(saved['error']['stderr']), 4096)
                else:
                    self.assertEqual(saved['error']['errno'], 13)
                self.assertEqual((root / 'tracked.txt').read_text(), 'initial\n')
        self.assertEqual(self.call('archive')['worktree']['state'], 'removed')

    def test_partial_removal_preserves_proof_and_recovers_exact_remaining_files(self):
        root, cache = self.partial_removal()
        head = self.git('rev-parse', 'codex-agent/worker')
        with self.rt.db() as db:
            saved = self.rt.agent('worker', db)['worktreeCleanup']
        self.assertEqual(saved['head'], head)
        self.assertEqual(saved['root'], str(root.resolve()))
        self.assertIn('gitFile', saved)
        self.assertTrue(saved.get('error'))
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'removed')
        self.assertFalse(root.exists())
        self.assertEqual(self.git('rev-parse', 'refs/codex-agents/archive/worker'), head)
        self.assertTrue(self.call('archive')['replayed'])
        self.assertEqual(self.call('restore')['status'], 'restored')
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=root), head)
        self.assertFalse(cache.exists())

    def test_partial_removal_keeps_changed_new_and_replaced_entries(self):
        root, cache = self.partial_removal()
        changed = root / '.gitignore'
        saved = changed.read_bytes()
        changed.write_bytes(b'changed tracked file')
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertEqual(changed.read_bytes(), b'changed tracked file')
        self.assertEqual(cache.read_bytes(), b'saved cache')

    def test_partial_removal_keeps_new_file_before_any_unlink(self):
        root, cache = self.partial_removal()
        (root / 'new-user-file').write_text('Keep me')
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        self.assertEqual(cache.read_bytes(), b'saved cache')
        self.assertEqual((root / 'new-user-file').read_text(), 'Keep me')
        self.assertTrue((root / '.gitignore').exists())

    def test_partial_removal_keeps_replaced_root_and_does_not_follow_symlinks(self):
        root, cache = self.partial_removal()
        moved = root.with_name('saved-leftovers')
        root.rename(moved)
        root.mkdir()
        (root / 'important').write_text('Keep me')
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        self.assertEqual((root / 'important').read_text(), 'Keep me')
        shutil.rmtree(root)
        root.symlink_to(moved, target_is_directory=True)
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        self.assertEqual((moved / 'build' / 'cache.bin').read_bytes(), b'saved cache')

    def test_partial_removal_requires_exact_ref_epoch_saved_proof_and_no_new_monitor(self):
        root, cache = self.partial_removal()
        with self.rt.db() as db:
            agent = self.rt.agent('worker', db)
        self.worker(epoch=agent['epoch'] + 1)
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        self.worker(epoch=agent['epoch'])
        self.update('monitors', {'id': 'm', 'agent': 'worker', 'status': 'running'})
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        with self.rt.db() as db: db.execute('DELETE FROM runtime_monitors')
        head = self.git('rev-parse', 'refs/codex-agents/archive/worker')
        self.git('commit', '--allow-empty', '-qm', 'different head')
        self.git('update-ref', 'refs/codex-agents/archive/worker', self.git('rev-parse', 'HEAD'))
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        self.git('update-ref', 'refs/codex-agents/archive/worker', head)
        with self.rt.db() as db:
            a = self.rt.agent('worker', db)
            (root / '.git').unlink()
            self.rt.put(db, 'agents', a)
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        self.assertEqual(cache.read_bytes(), b'saved cache')

    def legacy_partial_removal(self):
        root = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        cache = root / 'build' / 'cache.bin'
        cache.parent.mkdir()
        cache.write_bytes(b'old cache')
        admin = Path((root / '.git').read_text().strip().split(': ', 1)[1])
        (root / 'tracked.txt').unlink()
        shutil.rmtree(admin)
        head = self.git('rev-parse', 'codex-agent/worker')
        self.git('update-ref', 'refs/codex-agents/archive/worker', head)
        self.worker(epoch=2, deletedAt=123, autoWake=False, status='paused',
                    agentArchive={'at': 123, 'by': 'lead', 'reason': 'Finished worker cleanup',
                                  'epoch': 2, 'cleanupPending': False})
        return root, cache, head

    def test_legacy_half_removed_worktree_repairs_registration_before_git_removal(self):
        root, cache, head = self.legacy_partial_removal()
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'removed', result)
        self.assertFalse(root.exists())
        self.assertEqual(self.git('rev-parse', 'refs/codex-agents/archive/worker'), head)
        self.assertEqual(self.git('rev-parse', 'codex-agent/worker'), head)
        self.assertEqual(self.call('restore')['status'], 'restored')
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=root), head)
        self.assertFalse(cache.exists())
        self.assertEqual(list(root.parent.glob('.studio-archive-repair-*')), [])

    def test_legacy_registration_repair_preserves_modified_and_untracked_files(self):
        root, cache, head = self.legacy_partial_removal()
        (root / '.gitignore').write_text('changed tracked file')
        (root / 'new-user-file').write_text('Keep me')
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertEqual((root / '.gitignore').read_text(), 'changed tracked file')
        self.assertEqual((root / 'new-user-file').read_text(), 'Keep me')
        self.assertEqual(cache.read_bytes(), b'old cache')
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=root), head)
        self.assertIn(str(root.resolve()), self.git('worktree', 'list', '--porcelain'))
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')

    def test_legacy_git_link_fifo_rejects_without_blocking_the_runner(self):
        code = """
import importlib.util, json, os, sys
from pathlib import Path
path = Path(sys.argv[1])
sys.path.insert(0, str(path.parent))
spec = importlib.util.spec_from_file_location('management_fixture', path)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
case = fixture.RealWorktreeContract()
case.setUp()
try:
    root, cache, head = case.legacy_partial_removal()
    (root / '.git').unlink()
    os.mkfifo(root / '.git')
    result = case.call('archive')
    assert result['worktree']['state'] == 'kept', result
    assert 'regular file' in result['worktree']['reason'], result
    assert cache.read_bytes() == b'old cache'
    print('special Git link kept')
finally:
    case.tearDown()
"""
        result = subprocess.run([sys.executable, '-B', '-c', code, str(Path(__file__).resolve())],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('special Git link kept', result.stdout)

    def test_legacy_repair_requires_original_git_link_archive_and_branch_identity(self):
        root, cache, head = self.legacy_partial_removal()
        link = (root / '.git').read_text()
        (root / '.git').write_text('gitdir: /unrelated/path\n')
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        (root / '.git').write_text(link)
        self.worker(branch='different')
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        self.worker(branch='codex-agent/worker')
        self.worker(agentArchive={'at': 456, 'epoch': 2, 'cleanupPending': False})
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')
        self.assertEqual(cache.read_bytes(), b'old cache')
        self.assertNotIn(str(root.resolve()), self.git('worktree', 'list', '--porcelain'))

    def test_tracked_change_before_removal_never_reaches_git_remove(self):
        import codex_agent_management as management
        root = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        check = management._worktree_removal_check
        def gated(info):
            (root / 'tracked.txt').write_text('new user data')
            return check(info)
        with patch.object(management, '_worktree_removal_check', side_effect=gated):
            result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertEqual((root / 'tracked.txt').read_text(), 'new user data')
        self.assertEqual(self.call('archive')['worktree']['state'], 'kept')

    def test_unverified_dirty_files_remain_after_registration_disappears(self):
        import codex_agent_management as management
        root = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        admin = Path((root / '.git').read_text().strip().split(': ', 1)[1])
        check = management._worktree_removal_check
        def gated(info):
            (root / 'tracked.txt').write_text('new user data')
            shutil.rmtree(admin)
            return check(info)
        with patch.object(management, '_worktree_removal_check', side_effect=gated):
            first = self.call('archive')
        self.assertEqual(first['worktree']['state'], 'kept')
        second = self.call('archive')
        self.assertEqual(second['worktree']['state'], 'kept')
        self.assertEqual((root / 'tracked.txt').read_text(), 'new user data')

    def test_legacy_repair_keeps_another_registered_branch_checkout(self):
        root, cache, head = self.legacy_partial_removal()
        other = self.repo / '.worktrees' / 'codex-agents' / 'other'
        self.git('worktree', 'add', '-q', str(other), 'codex-agent/worker')
        result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'kept')
        self.assertIn('another registered worktree', result['worktree']['reason'])
        self.assertEqual(cache.read_bytes(), b'old cache')
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=other), head)

    def test_bulk_opt_in_returns_tasks_to_backlog_before_worktree_cleanup(self):
        task = {'id': 'open', 'rootId': 'lead', 'owner': 'worker', 'status': 'review',
                'version': 1, 'results': [{'id': 'result'}]}
        self.update('work', task)
        self.assertEqual(self.call('archive_finished')['archived'], 0)
        result = self.call('archive_finished', unassign_work=True)
        self.assertEqual(result['archived'], 1)
        self.assertEqual(result['unassignedWork'], ['open'])
        with self.rt.db() as db:
            work = self.rt.records(db, 'work')[0]
            self.assertEqual(work['results'], task['results'])
            self.assertIsNone(work['owner'])
            self.assertEqual(work['status'], 'ready')
        self.assertFalse((self.repo / '.worktrees' / 'codex-agents' / 'worker').exists())

    def image_workspace(self):
        from types import ModuleType
        mount = Path(self.tmp.name) / 'image'
        image_repo = mount / 'repo'
        mount.mkdir()
        subprocess.run(['git', 'clone', '-q', '--shared', str(self.repo), str(image_repo)], check=True)
        self.worker(cwd=str(image_repo), worktree=False, worktreeReady=False,
                    imageWorkspace=True, imageWorkspaceReady=True, imageWorkspacePhase='ready',
                    imageWorkspaceRepo=str(self.repo),
                    imageWorkspaceMount=str(mount))
        engine = ModuleType('codex_workspace_images')
        engine.workspace_bytes = lambda _agent: 4096
        engine.ensure_mounted = lambda _agent: {'mount': str(mount), 'path': str(image_repo)}
        engine.exec_prefix = lambda: []
        engine.list_workspaces = lambda: [{'id': 'worker', 'repositories': [{'path': '.'}]}]
        engine.archive_workspace = lambda _agent: {'freedBytes': 0, 'state': 'archived'}
        return mount, image_repo, engine

    def test_image_archive_detaches_without_git_status_or_collect(self):
        from unittest.mock import Mock
        _mount, image_repo, engine = self.image_workspace()
        (image_repo / 'draft.txt').write_text('unfinished')
        engine.archive_workspace = Mock(return_value={'freedBytes': 0, 'state': 'archived'})
        engine.workspace_bytes = Mock(side_effect=AssertionError('Unexpected folder size check'))
        calls = []
        def observe_archive(agent_id):
            with self.rt.db() as db:
                saved = self.rt.agent(agent_id, db)['cleanedImageWorkspace']
            calls.append(saved)
            return {'freedBytes': 0, 'state': 'archived'}
        engine.archive_workspace.side_effect = observe_archive
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            result = self.call('archive')
        self.assertEqual(result['workspace']['state'], 'archived')
        self.assertEqual(calls[0]['phase'], 'archiving')
        self.assertTrue((image_repo / 'draft.txt').is_file())
        engine.archive_workspace.assert_called_once_with('worker')
        with self.rt.db() as db:
            saved = self.rt.agent('worker', db)['cleanedImageWorkspace']
        self.assertIsNone(saved['bytes'])
        self.assertEqual(saved['freedBytes'], 0)
        engine.workspace_bytes.assert_not_called()

    def test_archive_crash_retry_finishes_detach_without_losing_saved_record(self):
        mount, image_repo, engine = self.image_workspace()
        engine.workspace_bytes = lambda _agent: 123
        from unittest.mock import Mock
        engine.archive_workspace = Mock(side_effect=[RuntimeError('injected detach failure'),
                                                       {'freedBytes': 0, 'state': 'archived'}])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            first = self.call('archive')
            self.assertEqual(first['workspace']['state'], 'kept')
            with self.rt.db() as db:
                saved = self.rt.agent('worker', db)['cleanedImageWorkspace']
                self.assertEqual(saved['phase'], 'archiving')
            retry = self.call('archive')
        self.assertEqual(retry['workspace']['state'], 'archived')
        self.assertEqual(engine.archive_workspace.call_count, 2)
        with self.rt.db() as db:
            saved = self.rt.agent('worker', db)['cleanedImageWorkspace']
        self.assertEqual(saved['phase'], 'archived')

    def test_clean_archive_keeps_ref_and_restore_recreates_checkout(self):
        path = self.repo / '.worktrees' / 'codex-agents' / 'worker'
        head = self.git('rev-parse', 'HEAD', cwd=path)
        (path / 'build').mkdir()
        (path / 'build' / 'cache').write_text('ignored bytes')
        import codex_agent_management as management
        run = management.subprocess.run
        def no_measurement(command, **kwargs):
            self.assertNotEqual(command[0], 'du', 'Archive must not measure its folder')
            return run(command, **kwargs)
        with patch.object(management.subprocess, 'run', side_effect=no_measurement):
            result = self.call('archive')
        self.assertEqual(result['worktree']['state'], 'removed')
        self.assertIsNone(result['worktree']['bytes'])
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
            self.assertNotIn(worker['id'],[a['id'] for a in read_runtime_state(rt)['agents']])
            epoch=rt.agent(worker['id'])['epoch']
            invoke(9202,'orchestration_agent_manage',archive)
            self.assertEqual(epoch,rt.agent(worker['id'])['epoch'])
            invoke(9203,'orchestration_agent_manage',{'action':'restore','agent_id':worker['id']})
            self.assertIn(worker['id'],[a['id'] for a in read_runtime_state(rt)['agents']])
            self.assertFalse(rt.agent(worker['id'])['autoWake'])
        finally:case.tearDown()

    def test_native_opt_in_archive_receipt_does_not_repeat_task_release_after_restore(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        case = fixture.RuntimeContract()
        case.setUp()
        try:
            rt = case.runtime
            lead = case.lead()
            rt.resource_action = lambda: {'state': {'claims': {}, 'queue': {}}}
            worker = rt.create({'name': 'Archive backlog', 'prompt': 'Inspect', 'role': 'reviewer'},
                               lead['id'], defer=True)
            task = rt.work_action(lead['id'], {'action': 'create', 'title': 'Keep task',
                                              'owner': worker['id']}, actor=lead['id'])
            with rt.lock, rt.db() as db:
                current = rt.agent(worker['id'], db)
                current.update(status='completed', inFlight=False, autoWake=False)
                rt.put(db, 'agents', current)
                db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?", (worker['id'],))
            args = {'action': 'archive', 'agent_id': worker['id'], 'reason': 'Keep the backlog',
                    'unassign_work': True, 'request_id': 'archive-backlog-once'}
            def invoke(number):
                rt.dynamic({'id': number, 'params': {'threadId': lead['threadId'], 'callId': str(number),
                    'tool': 'orchestration_agent_manage', 'arguments': args}})
                response = next(r for r in reversed(rt.server.responses) if r['id'] == number)['result']
                self.assertTrue(response['success'], response)
                return response
            first = invoke(9300)
            archived_epoch = rt.agent(worker['id'])['epoch']
            self.assertEqual(invoke(9300), first)
            self.assertEqual(rt.agent(worker['id'])['epoch'], archived_epoch)
            result = manage_agent(rt, lead['id'], {'action': 'restore', 'agent_id': worker['id']}, lead['epoch'])
            self.assertEqual(result['status'], 'restored')
            rt.work_action(lead['id'], {'action': 'update', 'task_id': task['id'],
                'owner': lead['id']}, actor=lead['id'])
            with rt.db() as db:
                before = dict(db.execute('SELECT * FROM runtime_work WHERE id=?', (task['id'],)).fetchone())
            self.assertEqual(invoke(9300), first)
            self.assertFalse(rt.agent(worker['id']).get('deletedAt'))
            with rt.db() as db:
                after = dict(db.execute('SELECT * FROM runtime_work WHERE id=?', (task['id'],)).fetchone())
            self.assertEqual(after, before)
        finally:
            case.tearDown()

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
