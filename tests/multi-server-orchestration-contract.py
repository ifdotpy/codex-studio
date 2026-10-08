#!/usr/bin/env python3
"""Two isolated Studio runtimes. Paired transport fixture, no live accounts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('ms_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_multi_server_orchestration import MultiServerService, identity


class FakeTransport:
    def __init__(self, local_server_id, network):
        self.local_server_id, self.network = local_server_id, network
        self.offline, self.drop_reply = False, False
        self.calls = []
    def servers(self):
        return [{'id': s, 'label': s, 'status': 'online'} for s in self.network if s != self.local_server_id]
    def request(self, server, envelope, *, timeout):
        self.calls.append(copy.deepcopy(envelope))
        if self.offline:
            raise OSError('offline')
        result = self.network[server].receive(self.local_server_id, envelope)
        if self.drop_reply:
            self.drop_reply = False
            raise TimeoutError('reply lost')
        return result


class CrossServer(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='studio-multi-server-')
        self.root = Path(self.tmp.name)
        self.network, self.runtimes, self.transports = {}, {}, {}
        for server in ('home', 'remote'):
            folder = self.root / server
            folder.mkdir()
            runtime = f.ControlledRuntime(folder / 'state', f.f.FakeServer)
            runtime.catalog = lambda account='default': copy.deepcopy(f.CATALOG)
            transport = FakeTransport(server, self.network)
            service = MultiServerService(runtime, transport)
            runtime._cross_server_service = service
            self.network[server] = service
            self.runtimes[server], self.transports[server] = runtime, transport
        self.home, self.remote = self.runtimes['home'], self.runtimes['remote']
        self.lead = self.home.create({'name': 'Lead', 'prompt': '', 'cwd': str(self.root / 'home'), 'concurrency': 4}, draft=True)
    def tearDown(self):
        for runtime in self.runtimes.values():
            runtime.close()
        self.tmp.cleanup()
    def spawn(self, key='spawn-one', **fields):
        args = {'server': 'remote', 'agents': [{'name': 'Worker', 'prompt': 'Inspect source', 'role': 'reviewer', 'cwd': str(self.root / 'remote'), **fields}]}
        return self.home.spawn_agents(self.lead, args, key)
    def drain(self, server='remote'):
        with self.runtimes[server].read_db() as db:
            keys = [r[0] for r in db.execute("SELECT id FROM runtime_server_outbox WHERE state='queued' ORDER BY rowid")]
        return [self.network[server].deliver(key) for key in keys]
    def test_spawn_links_and_default_local_path(self):
        result = self.spawn()
        self.assertEqual(result['outcome'], 'applied')
        worker = result['agents'][0]['id']
        self.assertEqual(self.remote.agent(worker)['remoteOrigin']['home'], 'home')
        self.assertEqual(self.home.agent(worker)['remoteWorker']['server'], 'remote')
        from codex_sync_entities import project
        with self.home.read_db() as db:
            dto = project('agent', self.home.agent_entity_view(db, self.home.agent(worker)))
        self.assertEqual(dto['remoteWorker']['server'], 'remote')
        self.assertEqual(self.remote.agent(worker)['cwd'], str((self.root / 'remote').resolve()))
        local = self.home.spawn_agents(self.lead, {'agents': [{'name': 'Local', 'prompt': 'Inspect', 'role': 'reviewer'}]}, 'local-one')
        self.assertNotIn('remoteWorker', self.home.agent(local['agents'][0]['id']))
    def test_lost_spawn_reply_exact_retry_creates_one_worker(self):
        self.transports['home'].drop_reply = True
        first = self.spawn()
        self.assertEqual(first['outcome'], 'unknown')
        result = self.network['home'].deliver(first['remoteRequestId'])
        self.assertEqual(result['outcome'], 'applied')
        with self.remote.read_db() as db:
            count = db.execute("SELECT count(*) FROM runtime_agents WHERE json_type(record,'$.remoteOrigin')='object'").fetchone()[0]
        self.assertEqual(count, 1)
        self.spawn()
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.spawn(prompt='Changed task')
        with patch.object(self.transports['home'], 'servers', return_value=[{'id': 'remote'}, {'id': 'other'}]):
            with self.assertRaisesRegex(ValueError, 'different content'):
                self.home.spawn_agents(self.lead, {'server': 'other', 'agents': [{
                    'name': 'Worker', 'prompt': 'Inspect source', 'role': 'reviewer',
                    'cwd': str(self.root / 'remote')}]}, 'spawn-one')
    def test_offline_spawn_queues_and_reconnects(self):
        self.transports['home'].offline = True
        result = self.spawn()
        self.assertEqual(result['status'], 'offline')
        self.assertTrue(self.home.agent(result['agents'][0]['id'])['inFlight'])
        self.transports['home'].offline = False
        self.drain('home')
        self.assertTrue(self.remote.agent(result['agents'][0]['id'])['remoteOrigin'])
    def test_late_spawn_receipt_preserves_completed_snapshot(self):
        self.transports['home'].drop_reply = True
        result = self.spawn()
        worker = result['agents'][0]['id']
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            a.update(status='completed', inFlight=False)
            self.remote.put(db, 'agents', a)
        self.drain()
        self.assertEqual(self.home.agent(worker)['status'], 'completed')
        self.network['home'].deliver(result['remoteRequestId'])
        self.assertEqual(self.home.agent(worker)['status'], 'completed')
        with self.home.read_db() as db:
            self.assertNotIn(worker, {a['id'] for a in self.home.dispatch_active_slots(db)})
    def test_input_duplicates_keep_one_native_event(self):
        worker = self.spawn()['agents'][0]['id']
        self.home.send(worker, 'Do next task', 'input-one', manual=False, resume=True,
                       sender=self.lead['id'], sender_epoch=self.lead['epoch'])
        self.transports['home'].drop_reply = True
        self.drain('home')
        self.drain('home')
        self.home.send(worker, 'Do next task', 'input-one', manual=False, resume=True)
        with self.remote.read_db() as db:
            rows = db.execute("SELECT text FROM runtime_events WHERE agent=? AND text='Do next task'", (worker,)).fetchall()
        self.assertEqual(len(rows), 1)
    def test_remote_result_delivered_once_after_offline_period(self):
        worker = self.spawn()['agents'][0]['id']
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            a.update(status='completed', inFlight=False)
            self.remote.put(db, 'agents', a)
            self.remote.parent_event(db, a, 'completion-one', 'The work is complete')
        self.transports['remote'].offline = True
        self.drain()
        self.transports['remote'].offline = False
        self.transports['remote'].drop_reply = True
        self.drain()
        self.drain()
        with self.home.read_db() as db:
            rows = db.execute("SELECT text FROM runtime_events WHERE agent=? AND kind='child_result'", (self.lead['id'],)).fetchall()
        self.assertEqual(len(rows), 1)
        self.assertEqual(json.loads(rows[0][0])['result'], 'The work is complete')
        self.assertFalse(self.home.agent(worker)['inFlight'])
    def test_task_board_submission_and_rejection_reaches_remote_worker(self):
        task = self.home.work_action(self.lead['id'], {'action': 'create', 'title': 'Inspect', 'description': 'Inspect the source'}, 'task-create', actor=self.lead['id'])
        worker = self.spawn(task_id=task['id'])['agents'][0]['id']
        self.drain()
        a = self.remote.agent(worker)
        result = self.network['remote'].worker_call(a, 'task', {'action': 'submit', 'task_id': task['id'],
            'result': 'Verified', 'checks': 'Fixture passed', 'revision': 'abc123', 'files': []}, 'remote-submit')
        self.assertEqual(result['status'], 'review')
        self.network['remote'].worker_call(a, 'task', {'action': 'submit', 'task_id': task['id'],
            'result': 'Verified', 'checks': 'Fixture passed', 'revision': 'abc123', 'files': []}, 'remote-submit')
        self.home.model_work(self.lead['id'], {'action': 'reject', 'task_id': task['id'], 'result': 'Check again'}, 'reject-one', self.lead['epoch'])
        self.drain('home')
        with self.remote.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND kind='work_decision'", (worker,)).fetchone()[0], 1)
        with self.home.read_db() as db:
            work = self.home.work_by_id(db, task['id'], self.lead['rootId'])
        self.assertEqual(len(work['results']), 1)
    def test_remote_message_uses_home_chat(self):
        worker = self.spawn()['agents'][0]['id']
        result = self.remote.chat_message(worker, 'parent', 'Check this result', 'message-one', 0)
        self.assertIn('id', result)
        with self.home.read_db() as db:
            rows = db.execute("SELECT text FROM runtime_events WHERE agent=? AND kind='agent_message'", (self.lead['id'],)).fetchall()
        self.assertEqual(len(rows), 1)
    def test_stop_never_restores_home_agent_from_late_state(self):
        worker = self.spawn()['agents'][0]['id']
        self.home.stop(worker, True, sender=self.lead['id'], sender_epoch=self.lead['epoch'])
        self.drain('home')
        self.drain()
        self.assertFalse(self.remote.agent(worker)['autoWake'])
        self.assertFalse(self.home.agent(worker)['autoWake'])
    def test_unknown_principal_and_cross_link_rejected(self):
        with self.assertRaises(PermissionError):
            self.network['remote'].receive('stranger', {'requestId': 'x', 'action': 'projects', 'payload': {}})
        result = self.spawn()
        self.network['home'].transport.servers = lambda: [{'id': 'remote'}, {'id': 'other'}]
        worker = self.home.agent(result['agents'][0]['id'])
        receipt = self.network['home'].receive('other', {'requestId': 'bad-peer', 'action': 'task',
            'payload': {'link': worker['remoteWorker']['link'], 'worker': worker['id'], 'epoch': 0, 'args': {'action': 'list'}}})
        self.assertEqual(receipt['outcome'], 'not_applied')
    def test_restart_preserves_offline_spawn_and_proxy_reservation(self):
        self.transports['home'].offline = True
        result = self.spawn()
        worker = result['agents'][0]['id']
        self.home.close()
        runtime = f.ControlledRuntime(self.root / 'home' / 'state', f.f.FakeServer)
        runtime.catalog = lambda account='default': copy.deepcopy(f.CATALOG)
        transport = FakeTransport('home', self.network)
        service = MultiServerService(runtime, transport)
        runtime._cross_server_service = service
        self.home = self.runtimes['home'] = runtime
        self.network['home'], self.transports['home'] = service, transport
        self.assertTrue(runtime.agent(worker)['autoWake'])
        self.assertTrue(runtime.agent(worker)['inFlight'])
        self.assertEqual(service.deliver(result['remoteRequestId'])['outcome'], 'applied')
        self.assertTrue(self.remote.agent(worker)['remoteOrigin'])

    def test_receiver_recovers_spawn_receipt_from_atomic_effect(self):
        result = self.spawn()
        envelope = self.transports['home'].calls[-1]
        with self.remote.db() as db:
            db.execute("UPDATE runtime_server_inbox SET state='running',result=NULL WHERE id=?", (envelope['requestId'],))
        recovered = self.network['remote'].receive('home', envelope)
        self.assertEqual(recovered['outcome'], 'applied')
        self.assertEqual(recovered['value']['agents'][0]['id'], result['agents'][0]['id'])

    def test_receiver_can_retry_home_context_without_a_saved_read_receipt(self):
        worker = self.spawn()['agents'][0]['id']
        actor = self.remote.agent(worker)
        self.network['remote'].worker_call(actor, 'context', {'topic': 'plan'}, 'read-context')
        envelope = self.transports['remote'].calls[-1]
        with self.home.db() as db:
            db.execute("UPDATE runtime_server_inbox SET state='running',result=NULL WHERE id=?", (envelope['requestId'],))
        recovered = self.network['home'].receive('remote', envelope)
        self.assertEqual(recovered['outcome'], 'applied')
        self.assertEqual(recovered['value']['topic'], 'plan')

    def test_receiver_does_not_repeat_input_when_effect_evidence_is_missing(self):
        worker = self.spawn()['agents'][0]['id']
        origin = self.remote.agent(worker)['remoteOrigin']
        envelope = {'requestId': 'unknown-input', 'action': 'input', 'payload': {
            'link': origin['link'], 'worker': worker, 'kind': 'followup', 'text': 'Do next task',
            'epoch': 0, 'controlEpoch': 0}}
        service = self.network['remote']
        with patch.object(service, '_receive', side_effect=OSError('Uncertain operation')):
            self.assertEqual(service.receive('home', envelope)['outcome'], 'unknown')
        with patch.object(service, '_receive', wraps=service._receive) as receive:
            self.assertEqual(service.receive('home', envelope)['outcome'], 'unknown')
        receive.assert_not_called()
        with self.remote.read_db() as db:
            self.assertIsNone(db.execute('SELECT 1 FROM runtime_events WHERE id=?', ('unknown-input',)).fetchone())

    def test_receiver_recovers_stop_without_incrementing_epoch_again(self):
        worker = self.spawn()['agents'][0]['id']
        self.home.stop(worker, True)
        self.drain('home')
        envelope = self.transports['home'].calls[-1]
        epoch = self.remote.agent(worker)['epoch']
        with self.remote.db() as db:
            db.execute("UPDATE runtime_server_inbox SET state='running',result=NULL WHERE id=?", (envelope['requestId'],))
        recovered = self.network['remote'].receive('home', envelope)
        self.assertEqual(recovered['outcome'], 'applied')
        self.assertEqual(self.remote.agent(worker)['epoch'], epoch)

    def test_execution_admission_waits_for_home_slot(self):
        worker = self.spawn()['agents'][0]['id']
        self.drain()
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            a.update(status='completed', inFlight=False)
            self.remote.put(db, 'agents', a)
        self.drain()
        with self.home.lock, self.home.db() as db:
            root = self.home.agent(self.lead['id'], db)
            root['concurrency'] = 1
            self.home.put(db, 'agents', root)
        local = self.home.create({'name': 'Local', 'prompt': 'Inspect', 'role': 'reviewer'}, parent=self.lead['id'], defer=True)
        with self.home.lock, self.home.db() as db:
            local.update(status='running', inFlight=True, autoWake=True)
            self.home.put(db, 'agents', local)
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            a.update(status='queued', inFlight=False)
            self.remote.put(db, 'agents', a)
            self.assertFalse(self.network['remote'].admission(db, a))
        results = self.drain()
        self.assertEqual(results[-1]['outcome'], 'unknown')
        self.assertFalse(self.home.agent(worker)['inFlight'])
        with self.home.lock, self.home.db() as db:
            local.update(status='completed', inFlight=False)
            self.home.put(db, 'agents', local)
        results = self.drain()
        self.assertEqual(results[-1]['outcome'], 'applied')
        with self.remote.lock, self.remote.db() as db:
            self.assertTrue(self.network['remote'].admission(db, self.remote.agent(worker, db)))
        self.assertTrue(self.home.agent(worker)['inFlight'])

    def test_remote_input_resumes_after_stop_with_control_epoch(self):
        worker = self.spawn()['agents'][0]['id']
        self.home.stop(worker, True)
        self.drain('home')
        self.home.send(worker, 'Resume with a new task', 'resume-one', resume=True)
        self.drain('home')
        self.assertTrue(self.remote.agent(worker)['autoWake'])
        with self.remote.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND text='Resume with a new task'", (worker,)).fetchone()[0], 1)

    def test_dynamic_spawn_uses_stable_native_tool_identity(self):
        self.lead = self.home.prepare(self.lead)
        args = {'request_id': 'cross-batch', 'server': 'remote', 'agents': [{'name': 'Dynamic', 'prompt': 'Inspect',
            'role': 'reviewer', 'cwd': str(self.root / 'remote')}]}
        for call in ('cross-one', 'cross-two'):
            self.home.dynamic({'id': call, 'params': {'threadId': self.lead['threadId'],
                'callId': call, 'tool': 'orchestration_spawn', 'arguments': args}})
        f.f.eventually(lambda: len(self.home.server.responses) >= 2)
        first, second = [json.loads(row['result']['contentItems'][0]['text']) for row in self.home.server.responses[-2:]]
        self.assertEqual(first['agents'], second['agents'])
        self.assertEqual(first['server'], 'remote')

    def test_remote_spawn_failure_releases_assigned_task(self):
        task = self.home.work_action(self.lead['id'], {'action': 'create', 'title': 'Inspect'}, 'create-failure', actor=self.lead['id'])
        result = self.spawn(task_id=task['id'], cwd=str(self.root / 'missing'))
        self.assertEqual(result['outcome'], 'not_applied')
        with self.home.read_db() as db:
            work = self.home.work_by_id(db, task['id'], self.lead['rootId'])
        self.assertIsNone(work['owner'])
        self.assertFalse(self.home.agent(result['agents'][0]['id'])['inFlight'])

    def test_remote_state_cannot_move_back_to_an_older_snapshot(self):
        worker = self.spawn()['agents'][0]['id']
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            a.update(status='completed', inFlight=False)
            self.remote.put(db, 'agents', a)
        with self.remote.read_db() as db:
            snapshots = [json.loads(row[0]) for row in db.execute("SELECT body FROM runtime_server_outbox WHERE json_extract(body,'$.action')='state' ORDER BY rowid")]
        newest = snapshots[-1]
        self.network['home'].receive('remote', newest)
        for old in snapshots[:-1]:
            self.network['home'].receive('remote', old)
        self.assertEqual(self.home.agent(worker)['status'], 'completed')
        self.assertFalse(self.home.agent(worker)['inFlight'])

    def test_native_remote_turn_requires_home_admission(self):
        worker = self.spawn()['agents'][0]['id']
        self.assertEqual(self.home.dispatch(worker), 0)
        self.assertEqual(self.remote.dispatch(worker), 0)
        self.assertFalse(self.remote.agent(worker)['inFlight'])
        self.drain()
        self.remote.dispatch(worker)
        f.f.eventually(lambda: self.remote.agent(worker).get('turnId'))
        running = self.remote.agent(worker)
        self.assertTrue(running['inFlight'])
        starts = [call for call in self.remote.server.calls if call[0] == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.remote.server.complete(running['threadId'], running['turnId'])
        f.f.eventually(lambda: self.remote.agent(worker)['status'] == 'completed')
        self.drain()
        self.assertFalse(self.home.agent(worker)['inFlight'])

    def test_dynamic_remote_task_uses_home_board(self):
        task = self.home.work_action(self.lead['id'], {'action': 'create', 'title': 'Inspect'}, 'dynamic-task', actor=self.lead['id'])
        worker = self.spawn(task_id=task['id'])['agents'][0]['id']
        actor = self.remote.prepare(self.remote.agent(worker))
        self.remote.dynamic({'id': 'worker-task-get', 'params': {'threadId': actor['threadId'], 'callId': 'worker-task-get',
            'tool': 'orchestration_task', 'arguments': {'action': 'get', 'task_id': task['id']}}})
        f.f.eventually(lambda: self.remote.server.responses)
        result = json.loads(self.remote.server.responses[-1]['result']['contentItems'][0]['text'])
        self.assertEqual(result['id'], task['id'])
        self.assertEqual(result['owner'], worker)

    def test_fetch_reservation_does_not_replay_uncertain_git_command(self):
        worker = self.spawn()['agents'][0]['id']
        target = self.root / 'home' / 'fetch-target'
        target.mkdir()
        subprocess.run(['git', '-C', str(target), 'init', '-q'], check=True)
        request = {'worker': worker, 'branch': 'worker', 'destination': str(target.resolve())}
        with self.home.db() as db:
            signature, _ = self.home.operation_receipt(db, 'uncertain-fetch', request)
            db.execute('INSERT INTO runtime_server_fetch VALUES (?,?,?,?,NULL)', ('uncertain-fetch', 'remote', signature, 'fetching'))
        with patch('codex_multi_server_orchestration.subprocess.run', wraps=subprocess.run) as run:
            result = self.network['home'].tools(self.lead, {'action': 'fetch', 'server': 'remote', 'agent_id': worker,
                'branch': 'worker', 'destination': str(target)}, 'uncertain-fetch')
        self.assertEqual(result['outcome'], 'unknown')
        self.assertFalse(any('fetch' in call.args[0] for call in run.call_args_list))

    def test_adapter_preserves_signed_request_identity_and_filters_revocation(self):
        from codex_server_transport import PairedServerTransport
        from unittest.mock import Mock
        access = Mock()
        access.local_server_id = 'home'
        access.servers.return_value = [{'id': 'remote', 'status': 'paired'}, {'id': 'revoked', 'status': 'revoked'}]
        access.request.return_value = {'requestId': 'wire-one', 'outcome': 'applied', 'value': {}}
        runtime = Mock()
        runtime.paired_access.return_value = access
        adapter = PairedServerTransport(runtime)
        self.assertEqual(adapter.local_server_id, 'home')
        self.assertEqual(adapter.servers(), [{'id': 'remote', 'status': 'paired'}])
        envelope = {'requestId': 'wire-one', 'action': 'projects', 'payload': {}}
        adapter.request('remote', envelope, timeout=20)
        access.request.assert_called_once_with('remote', 'POST', '/api/servers/orchestration', envelope,
            request_id='wire-one', timeout=20)

    def test_git_allowlist_and_fetch_checksum(self):
        repo = self.root / 'remote' / 'repo'
        repo.mkdir()
        def git(*args):
            return subprocess.check_output(['git', '-C', str(repo), *args], text=True).strip()
        git('init', '-q', '-b', 'worker')
        git('config', 'user.name', 'Fixture')
        git('config', 'user.email', 'fixture@example.test')
        (repo / 'file').write_text('one')
        git('add', 'file')
        git('commit', '-qm', 'one')
        sha = git('rev-parse', 'HEAD')
        worker = self.spawn(cwd=str(repo))['agents'][0]['id']
        for argv in (['config', 'user.name', 'changed'], ['-c', 'alias.foo=!touch bad', 'foo'], ['show', '--no-patch', '--output=bad']):
            result = self.network['remote'].receive('home', {'requestId': identity(str(argv)), 'action': 'git', 'payload': {'cwd': str(repo), 'argv': argv}})
            self.assertEqual(result['outcome'], 'not_applied')
        target = self.root / 'home' / 'repo'
        target.mkdir()
        subprocess.run(['git', '-C', str(target), 'init', '-q'], check=True)
        result = self.network['home'].tools(self.lead, {'action': 'fetch', 'server': 'remote', 'agent_id': worker,
            'branch': 'worker', 'destination': str(target)}, 'fetch-one')
        self.assertEqual(result['commit'], sha)
        again = self.network['home'].tools(self.lead, {'action': 'fetch', 'server': 'remote', 'agent_id': worker,
            'branch': 'worker', 'destination': str(target)}, 'fetch-one')
        self.assertEqual(again, result)

if __name__ == '__main__':
    unittest.main(verbosity=2)
