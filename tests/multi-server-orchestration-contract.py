#!/usr/bin/env python3
"""Two isolated Studio runtimes. Paired transport fixture, no live accounts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
import copy
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sqlite3
import tempfile
import threading
import time
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

    def review_repo(self):
        repo = self.root / 'remote' / 'review-repo'
        repo.mkdir()
        def git(*args, **kwargs):
            return subprocess.check_output(['git', '-C', str(repo), *args], text=True, **kwargs).strip()
        git('init', '-q', '-b', 'worker')
        git('config', 'user.name', 'Fixture')
        git('config', 'user.email', 'fixture@example.test')
        git('config', 'commit.gpgSign', 'false')
        (repo / 'file').write_text('base')
        git('add', 'file')
        git('commit', '-qm', 'base')
        return repo, git

    def test_review_status_disables_filters_from_included_config(self):
        repo, git = self.review_repo()
        (repo / '.gitattributes').write_text('file filter=fixture.driver diff=fixture.driver\n')
        git('add', '.gitattributes')
        git('commit', '-qm', 'Attributes')
        marker, program = self.root / 'filter-executed', self.root / 'filter-program'
        program.write_text('#!/bin/sh\ntouch "' + str(marker) + '"\ncat\n')
        program.chmod(0o700)
        config = self.root / 'external-config'
        config.write_text('[filter "fixture.driver"]\n clean = "' + str(program) + '"\n required = true\n')
        git('config', 'include.path', str(config))
        (repo / 'file').write_text('work')  # Same size forces Git to compare filtered content.
        git('status', '--porcelain=v1')
        self.assertTrue(marker.exists(), 'The unsafe control must execute the clean filter')
        marker.unlink()
        result = self.network['remote'].receive('home', {'requestId': 'filtered-status', 'action': 'git',
            'payload': {'cwd': str(repo), 'argv': ['status', '--porcelain=v1']}})
        self.assertFalse(marker.exists())
        self.assertEqual(result['outcome'], 'applied')
        self.assertEqual(result['value']['exitCode'], 0)
        self.assertIn('file', result['value']['output'])
        # A broken include makes the effective driver list unknowable.
        config.write_text('[invalid config\n')
        refused = self.network['remote'].receive('home', {'requestId': 'broken-filter-config', 'action': 'git',
            'payload': {'cwd': str(repo), 'argv': ['status', '--porcelain=v1']}})
        self.assertEqual(refused['outcome'], 'not_applied')

    def test_review_fetch_disables_alternate_refs_programs(self):
        repo, git = self.review_repo()
        target = self.root / 'home' / 'shared-target'
        subprocess.run(['git', 'clone', '-q', '--shared', str(repo), str(target)], check=True)
        (repo / 'file').write_text('Remote work')
        git('add', 'file')
        git('commit', '-qm', 'Remote work')
        worker = self.spawn(cwd=str(repo))['agents'][0]['id']
        marker, program = self.root / 'alternate-executed', self.root / 'alternate-program'
        program.write_text('#!/bin/sh\ntouch "' + str(marker) + '"\nexit 0\n')
        program.chmod(0o700)
        subprocess.run(['git', '-C', str(target), 'config', 'core.alternateRefsCommand', str(program)], check=True)
        # Prove the marker crosses Git's alternate object boundary without hardening.
        subprocess.run(['git', '-C', str(target), 'fetch', '-q', str(repo), 'worker'], check=True)
        self.assertTrue(marker.exists())
        marker.unlink()
        # The first fetch uses the already-present commit through a local file fetch.
        first = self.network['home'].tools(self.lead, {'action': 'fetch', 'server': 'remote', 'agent_id': worker,
            'branch': 'worker', 'destination': str(target)}, 'safe-shared-local')
        self.assertEqual(first['bytes'], 0)
        self.assertFalse(marker.exists())
        # A new commit requires a bundle fetch into the same shared repository.
        (repo / 'file').write_text('More remote work')
        git('add', 'file')
        git('commit', '-qm', 'More remote work')
        second = self.network['home'].tools(self.lead, {'action': 'fetch', 'server': 'remote', 'agent_id': worker,
            'branch': 'worker', 'destination': str(target)}, 'safe-shared-bundle')
        self.assertGreater(second['bytes'], 0)
        self.assertEqual(second['commit'], git('rev-parse', 'HEAD'))
        self.assertFalse(marker.exists())

    def test_review_old_parent_terminal_snapshot_releases_slot(self):
        worker = self.spawn()['agents'][0]['id']
        self.home.stop(self.lead['id'], True)
        self.drain('home')
        self.home.send(self.lead['id'], 'Resume the team', 'resume-before-terminal', resume=True)
        self.drain('home')
        self.drain('remote')
        proxy = self.home.agent(worker)
        self.assertFalse(proxy['inFlight'])
        self.assertFalse(proxy['remoteReservation'])
        self.assertFalse(proxy['autoWake'])
        # Released slots permit an unrelated worker at concurrency one.
        with self.home.lock, self.home.db() as db:
            lead = self.home.agent(self.lead['id'], db)
            lead['concurrency'] = 1
            self.home.put(db, 'agents', lead)
        self.lead = self.home.agent(self.lead['id'])
        self.assertEqual(self.spawn(key='worker-after-old-terminal')['outcome'], 'applied')

    def test_review_input_queue_keeps_order_during_backoff(self):
        worker = self.spawn()['agents'][0]['id']
        other = self.spawn(key='other-order-worker')['agents'][0]['id']
        self.home.send(worker, 'First', 'ordered-first', resume=True)
        self.home.send(worker, 'Second', 'ordered-second', resume=True)
        self.home.send(other, 'Independent', 'ordered-other', resume=True)
        service = self.network['home']
        first_id = identity('input', 'ordered-first')
        second_id = identity('input', 'ordered-second')
        original = service.transport.request
        def offline_first(server, envelope, **kwargs):
            if envelope['requestId'] == first_id:
                raise TimeoutError('First input has no receipt')
            return original(server, envelope, **kwargs)
        with patch.object(service.transport, 'request', side_effect=offline_first):
            service.tick()
            f.f.eventually(lambda: not service._running)
        with self.remote.read_db() as db:
            self.assertIsNone(db.execute('SELECT 1 FROM runtime_events WHERE text=?', ('Second',)).fetchone())
            self.assertIsNotNone(db.execute('SELECT 1 FROM runtime_events WHERE text=?', ('Independent',)).fetchone())
        # Direct retry of a later receipt must honor the same order.
        self.assertEqual(service.deliver(second_id)['outcome'], 'unknown')
        with self.home.db() as db:
            db.execute('UPDATE runtime_server_outbox SET next_at=0 WHERE id=?', (first_id,))
        service.tick()
        f.f.eventually(lambda: not service._running)
        # The scheduler can deliver Second on its next pass after First completes.
        service.tick()
        f.f.eventually(lambda: not service._running)
        with self.remote.read_db() as db:
            inputs = [row[0] for row in db.execute('SELECT text FROM runtime_events WHERE agent=? AND text IN (?,?) ORDER BY rowid',
                (worker, 'First', 'Second'))]
        self.assertEqual(inputs, ['First', 'Second'])

    def test_review_receive_db_wait_does_not_block_tick_claim(self):
        service = self.network['remote']
        service._pruned_at = time.time()
        entered, tick_done = threading.Event(), threading.Event()
        errors = []
        original_db = self.remote.db
        @contextmanager
        def waiting_db(*args, **kwargs):
            entered.set()
            with original_db(*args, **kwargs) as db:
                yield db
        def receive():
            try:
                service.receive('home', {'requestId': 'writer-wait', 'action': 'folders',
                    'payload': {'cwd': str(self.root / 'remote')}})
            except Exception as error:
                errors.append(error)
        def tick():
            service.tick()
            tick_done.set()
        writer = sqlite3.connect(self.remote.db_path, timeout=1)
        writer.execute('BEGIN IMMEDIATE')
        threads = []
        try:
            with patch.object(self.remote, 'db', waiting_db), patch.object(self.remote, 'delivery_executor'):
                threads.append(threading.Thread(target=receive))
                threads[-1].start()
                self.assertTrue(entered.wait(2))
                threads.append(threading.Thread(target=tick))
                threads[-1].start()
                responsive = tick_done.wait(.25)
        finally:
            writer.rollback()
            writer.close()
            for thread in threads:
                thread.join(5)
            service._running = False
        self.assertTrue(responsive, 'A SQLite writer must not block the scheduler claim')
        self.assertFalse(errors)
        self.assertFalse(any(thread.is_alive() for thread in threads))

    def test_review_git_read_never_executes_repo_gpg(self):
        repo, git = self.review_repo()
        marker = self.root / 'gpg-executed'
        program = self.root / 'gpg-program'
        program.write_text('#!/bin/sh\ntouch "' + str(marker) + '"\nexit 1\n')
        program.chmod(0o700)
        raw = git('cat-file', 'commit', 'HEAD')
        header, body = raw.split('\n\n', 1)
        signed = header + '\ngpgsig -----BEGIN PGP SIGNATURE-----\n fixture\n -----END PGP SIGNATURE-----\n\n' + body + '\n'
        sha = git('hash-object', '-t', 'commit', '-w', '--stdin', input=signed)
        git('update-ref', 'refs/heads/worker', sha)
        git('config', 'log.showSignature', 'true')
        git('config', 'gpg.program', str(program))
        git('config', 'gpg.format', 'openpgp')
        git('log', '-n', '1')
        self.assertTrue(marker.exists(), 'The unsafe control must execute the configured program')
        marker.unlink()
        for index, argv in enumerate((['log', '-n', '1'], ['show', '--no-patch', sha])):
            receipt = self.network['remote'].receive('home', {'requestId': 'safe-git-' + str(index),
                'action': 'git', 'payload': {'cwd': str(repo), 'argv': argv}})
            self.assertEqual(receipt['outcome'], 'applied')
        self.assertFalse(marker.exists())

    def test_review_fetch_never_executes_insteadof_transport(self):
        repo, _ = self.review_repo()
        worker = self.spawn(cwd=str(repo))['agents'][0]['id']
        target = self.root / 'home' / 'ext-target'
        target.mkdir()
        subprocess.run(['git', '-C', str(target), 'init', '-q'], check=True)
        marker, program = self.root / 'ext-executed', self.root / 'ext-program'
        program.write_text('#!/bin/sh\ntouch "' + str(marker) + '"\nexit 1\n')
        program.chmod(0o700)
        subprocess.run(['git', '-C', str(target), 'config', 'protocol.ext.allow', 'always'], check=True)
        subprocess.run(['git', '-C', str(target), 'config', 'url.ext::' + str(program) + ' .insteadOf',
            tempfile.gettempdir() + '/'], check=True)
        subprocess.run(['git', '-C', str(target), 'config', '--add', 'url.ext::' + str(program) + ' .insteadOf',
            str(Path(tempfile.gettempdir()).resolve()) + '/'], check=True)
        refused = None
        try:
            self.network['home'].tools(self.lead, {'action': 'fetch', 'server': 'remote', 'agent_id': worker,
                'branch': 'worker', 'destination': str(target)}, 'safe-fetch')
        except (ValueError, subprocess.CalledProcessError) as error:
            refused = error
        self.assertFalse(marker.exists())
        self.assertIsInstance(refused, ValueError)
        self.assertIn('URL rewrite', str(refused))

    def test_review_transport_exception_does_not_block_other_peers(self):
        class AccessError(Exception):
            status = 503
        service = self.network['home']
        with self.home.db() as db:
            service.queue(db, 'offline', 'projects', {}, 'bad-peer')
            service.queue(db, 'healthy', 'projects', {}, 'good-peer')
        def request(server, envelope, **kwargs):
            if server == 'offline':
                raise AccessError('Unavailable')
            return {'requestId': envelope['requestId'], 'outcome': 'applied', 'value': {}}
        with patch.object(service.transport, 'request', side_effect=request):
            service.tick()
            f.f.eventually(lambda: not service._running)
            with self.home.read_db() as db:
                self.assertEqual(db.execute('SELECT state FROM runtime_server_outbox WHERE id=?', ('good-peer',)).fetchone()[0], 'complete')
            with patch('codex_multi_server_orchestration.time.time', return_value=1000):
                service.deliver('bad-peer')
                with self.home.read_db() as db:
                    first = db.execute('SELECT next_at FROM runtime_server_outbox WHERE id=?', ('bad-peer',)).fetchone()[0]
                service.deliver('bad-peer')
                with self.home.read_db() as db:
                    second = db.execute('SELECT next_at FROM runtime_server_outbox WHERE id=?', ('bad-peer',)).fetchone()[0]
                self.assertGreater(second, first)
                self.assertLessEqual(second, 1300)

    def test_review_stop_marker_is_not_native_completion_evidence(self):
        worker = self.spawn()['agents'][0]['id']
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            a.update(status='running', inFlight=True, turnId='active-native-turn')
            self.remote.put(db, 'agents', a)
        self.home.stop(worker, True)
        with patch.object(self.remote, 'interrupt', side_effect=OSError('Crash before native interrupt')) as interrupt:
            self.drain('home')
            envelope = self.transports['home'].calls[-1]
            self.assertEqual(self.network['remote'].receive('home', envelope)['outcome'], 'unknown')
            self.assertEqual(interrupt.call_count, 1)
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            a.update(inFlight=False, turnId=None)
            self.remote.put(db, 'agents', a)
        self.assertEqual(self.network['remote'].receive('home', envelope)['outcome'], 'applied')

    def test_review_stopped_snapshot_clears_slot_and_new_admission_counts_all_slots(self):
        worker = self.spawn()['agents'][0]['id']
        with self.home.lock, self.home.db() as db:
            a = self.home.agent(worker, db)
            a.update(autoWake=False, inFlight=True, remoteReservation=True)
            self.home.put(db, 'agents', a)
            root = self.home.agent(self.lead['id'], db)
            root['concurrency'] = 1
            self.home.put(db, 'agents', root)
        origin = a['remoteWorker']
        self.network['home'].receive('remote', {'requestId': 'stopped-snapshot', 'action': 'state', 'payload': {
            'link': origin['link'], 'worker': worker, 'sequence': 1000,
            'record': {'status': 'paused', 'inFlight': False, 'epoch': 1}}})
        self.assertFalse(self.home.agent(worker)['remoteReservation'])
        self.home.send(worker, 'Resume', 'slot-resume', resume=True)
        local = self.home.create({'name': 'Local', 'prompt': 'Inspect', 'role': 'reviewer'}, parent=self.lead['id'], defer=True)
        with self.home.lock, self.home.db() as db:
            local.update(status='running', inFlight=True, autoWake=True)
            self.home.put(db, 'agents', local)
            a = self.home.agent(worker, db)
            a['remoteReservation'] = True  # Delayed reservation flag without an active slot.
            self.home.put(db, 'agents', a)
        result = self.network['home'].receive('remote', {'requestId': 'new-slot', 'action': 'admit',
            'payload': {'link': origin['link'], 'worker': worker, 'epoch': 1}})
        self.assertEqual(result['outcome'], 'unknown')

    def test_review_stop_waits_for_uncertain_native_input_slot(self):
        worker = self.spawn()['agents'][0]['id']
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            db.execute('INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)',
                ('busy-native-input', worker, 'followup', 'Uncertain native input', 'uncertain', time.time(), a['epoch'], None, None))
            a.update(status='completed', inFlight=False, turnId=None,
                startAttempt={'activeAtReservation': True, 'epoch': a['epoch'], 'events': ['busy-native-input']})
            self.remote.put(db, 'agents', a)
            self.assertIn(worker, {slot['id'] for slot in self.remote.dispatch_active_slots(db)})
        self.home.stop(worker, True)
        self.assertEqual(self.drain('home')[-1]['outcome'], 'unknown')
        envelope = self.transports['home'].calls[-1]
        self.drain()
        self.assertTrue(self.home.agent(worker)['inFlight'])
        with self.remote.lock, self.remote.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id='busy-native-input'")
            a = self.remote.agent(worker, db)
            a.pop('startAttempt', None)
            self.remote.put(db, 'agents', a)
        self.drain()
        self.assertFalse(self.home.agent(worker)['inFlight'])
        self.assertEqual(self.network['remote'].receive('home', envelope)['outcome'], 'applied')

    def test_review_input_cannot_resume_after_concurrent_stop(self):
        worker = self.spawn()['agents'][0]['id']
        self.home.send(worker, 'Next task', 'racing-input', resume=True)
        original = self.remote.send
        def race(*args, **kwargs):
            self.remote.stop(worker, True)
            return original(*args, **kwargs)
        with patch.object(self.remote, 'send', side_effect=race):
            results = self.drain('home')
        self.assertEqual(results[-1]['outcome'], 'not_applied')
        self.assertFalse(self.remote.agent(worker)['autoWake'])

    def test_review_admission_counts_busy_slots_and_waits_for_old_admission(self):
        worker = self.spawn()['agents'][0]['id']
        local = self.home.create({'name': 'Local', 'prompt': 'Inspect', 'role': 'reviewer'}, parent=self.lead['id'], defer=True)
        with self.home.lock, self.home.db() as db:
            root = self.home.agent(self.lead['id'], db)
            root['concurrency'] = 1
            self.home.put(db, 'agents', root)
            local.update(status='running', inFlight=True, autoWake=True)
            self.home.put(db, 'agents', local)
        origin = self.home.agent(worker)['remoteWorker']
        envelope = {'requestId': 'busy-slot', 'action': 'admit',
            'payload': {'link': origin['link'], 'worker': worker, 'epoch': 0}}
        with self.subTest(slots='two active slots'):
            self.assertEqual(self.network['home'].receive('remote', envelope)['outcome'], 'unknown')
        with self.home.lock, self.home.db() as db:
            local.update(status='completed', inFlight=False)
            self.home.put(db, 'agents', local)
            a = self.home.agent(worker, db)
            a['remoteAdmissionRequest'] = 'old-admission'
            self.home.put(db, 'agents', a)
        envelope['requestId'] = 'next-slot'
        with self.subTest(slots='old admission still active'):
            self.assertEqual(self.network['home'].receive('remote', envelope)['outcome'], 'unknown')
        self.network['home'].receive('remote', {'requestId': 'old-turn-terminal', 'action': 'state', 'payload': {
            'link': origin['link'], 'worker': worker, 'sequence': 1000,
            'record': {'status': 'completed', 'inFlight': False, 'epoch': 0, 'admissionId': 'old-admission'}}})
        self.assertEqual(self.network['home'].receive('remote', envelope)['outcome'], 'applied')

    def test_review_explicit_resume_rebinds_parent_epoch(self):
        task = self.home.work_action(self.lead['id'], {'action': 'create', 'title': 'Inspect'}, 'resume-task', actor=self.lead['id'])
        worker = self.spawn(task_id=task['id'])['agents'][0]['id']
        self.home.stop(self.lead['id'], True)
        self.drain('home')
        self.drain()
        self.home.send(self.lead['id'], 'Resume the team', 'resume-parent', resume=True)
        self.home.send(worker, 'Resume the task', 'resume-child', resume=True)
        self.drain('home')
        with self.remote.lock, self.remote.db() as db:
            self.network['remote'].admission(db, self.remote.agent(worker, db))
        self.drain()
        with self.remote.read_db() as db:
            self.assertTrue(self.network['remote'].admission(db, self.remote.agent(worker, db)))
        actor = self.remote.agent(worker)
        result = self.network['remote'].worker_call(actor, 'task', {'action': 'get', 'task_id': task['id']}, 'resume-get')
        self.assertEqual(result['id'], task['id'])
        self.assertIn('id', self.remote.chat_message(worker, 'parent', 'Resumed', 'resume-message', actor['epoch']))
        self.assertEqual(self.network['remote'].worker_call(actor, 'context', {'topic': 'plan'}, 'resume-context')['topic'], 'plan')

    def test_review_remote_input_preserves_delivery_modes(self):
        worker = self.spawn()['agents'][0]['id']
        for delivery in ('queue', 'steer', 'after_tool', 'after_turn'):
            self.home.send(worker, 'Task ' + delivery, 'delivery-' + delivery, resume=True, delivery=delivery)
        self.drain('home')
        with self.remote.read_db() as db:
            modes = [json.loads(row[0])['delivery'] for row in db.execute('SELECT record FROM runtime_event_meta WHERE id IN ('
                'SELECT id FROM runtime_events WHERE agent=? AND text LIKE ?)', (worker, 'Task %'))]
        self.assertCountEqual(modes, ['queue', 'steer', 'after_tool', 'after_turn'])

    def test_review_resume_waits_for_delayed_stop_and_parent_update(self):
        worker = self.spawn()['agents'][0]['id']
        self.home.stop(self.lead['id'], True)
        self.home.send(self.lead['id'], 'Resume the team', 'delayed-parent', resume=True)
        self.home.send(worker, 'Delayed resume', 'delayed-worker', resume=True)
        with self.home.read_db() as db:
            envelopes = [json.loads(row[0]) for row in db.execute('SELECT body FROM runtime_server_outbox ORDER BY rowid')]
        stop = next(e for e in envelopes if e['action'] == 'stop')
        message = next(e for e in envelopes if e['action'] == 'input' and e['payload']['text'] == 'Delayed resume')
        service = self.network['remote']
        self.assertEqual(service.receive('home', message)['outcome'], 'unknown')
        with self.remote.read_db() as db:
            self.assertIsNone(db.execute('SELECT 1 FROM runtime_events WHERE id=?', (message['requestId'],)).fetchone())
        self.assertEqual(service.receive('home', stop)['outcome'], 'applied')
        self.assertEqual(service.receive('home', message)['outcome'], 'unknown')
        rebind = next(e for e in envelopes if e['action'] == 'rebind')
        self.assertEqual(service.receive('home', rebind)['outcome'], 'applied')
        original = self.remote.send
        def lose_result(*args, **kwargs):
            original(*args, **kwargs)
            raise OSError('Lost the committed input result')
        with patch.object(self.remote, 'send', side_effect=lose_result) as send:
            self.assertEqual(service.receive('home', message)['outcome'], 'unknown')
            self.assertEqual(service.receive('home', message)['outcome'], 'applied')
            self.assertEqual(send.call_count, 1)
        self.assertTrue(self.remote.agent(worker)['autoWake'])
        with self.remote.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events WHERE id=?', (message['requestId'],)).fetchone()[0], 1)

    def test_review_remote_input_rejects_assets_before_acceptance(self):
        worker = self.spawn()['agents'][0]['id']
        with self.assertRaisesRegex(ValueError, 'attachment'):
            self.home.send(worker, 'Attachment', 'asset-input', assets=['asset-one'], resume=True)
        with self.home.read_db() as db:
            self.assertIsNone(db.execute('SELECT 1 FROM runtime_events WHERE id=?', ('asset-input',)).fetchone())
            self.assertIsNone(db.execute('SELECT 1 FROM runtime_event_meta WHERE id=?', ('asset-input',)).fetchone())

    def test_review_incremental_fetch_releases_bundle_and_does_not_store_chunk_data(self):
        repo, git = self.review_repo()
        (repo / 'old-blob').write_bytes(os.urandom(2 * 1024 * 1024))
        git('add', 'old-blob')
        git('commit', '-qm', 'Existing history')
        target = self.root / 'home' / 'incremental-target'
        subprocess.run(['git', 'clone', '-q', str(repo), str(target)], check=True)
        (repo / 'new-blob').write_bytes(os.urandom(350 * 1024))
        git('add', 'new-blob')
        git('commit', '-qm', 'New work')
        worker = self.spawn(cwd=str(repo))['agents'][0]['id']
        def used_bytes(runtime):
            with runtime.read_db() as db:
                pages = db.execute('PRAGMA page_count').fetchone()[0] - db.execute('PRAGMA freelist_count').fetchone()[0]
                return pages * db.execute('PRAGMA page_size').fetchone()[0]
        initial_bytes = {runtime.root: used_bytes(runtime) for runtime in (self.home, self.remote)}
        result = self.network['home'].tools(self.lead, {'action': 'fetch', 'server': 'remote', 'agent_id': worker,
            'branch': 'worker', 'destination': str(target)}, 'incremental-fetch')
        with self.subTest(bound='incremental'):
            self.assertLess(result['bytes'], 512 * 1024)
            self.assertGreater(result['bytes'], 256 * 1024)
        self.assertEqual(result['commit'], git('rev-parse', 'HEAD'))
        with self.subTest(bound='release'):
            self.assertEqual(list((self.remote.root / 'server-exports').glob('*.bundle')), [])
        for runtime in (self.home, self.remote):
            with self.subTest(bound='database size', server=runtime.root):
                self.assertLess(used_bytes(runtime) - initial_bytes[runtime.root], 256 * 1024)
            with runtime.read_db() as db:
                for table in ('outbox', 'inbox'):
                    results = [row[0] for row in db.execute('SELECT result FROM runtime_server_' + table + ' WHERE result IS NOT NULL')]
                    with self.subTest(bound='chunk data', server=runtime.root, table=table):
                        self.assertFalse(any('"data":' in result for result in results))
                        self.assertLess(sum(map(len, results)), 64 * 1024)
        same = self.network['home'].tools(self.lead, {'action': 'fetch', 'server': 'remote', 'agent_id': worker,
            'branch': 'worker', 'destination': str(target)}, 'already-fetched')
        self.assertEqual(same['commit'], result['commit'])
        self.assertEqual(same['bytes'], 0)

    def test_review_prune_preserves_request_tombstones_and_state_sequence(self):
        worker = self.spawn()['agents'][0]['id']
        self.home.send(worker, 'One input', 'aged-input', resume=True)
        self.drain('home')
        envelope = self.transports['home'].calls[-1]
        old = time.time() - 8 * 86400
        export = self.remote.root / 'server-exports' / 'stale.bundle'
        export.parent.mkdir(exist_ok=True)
        export.write_bytes(b'expired')
        os.utime(export, (old, old))
        with patch('codex_multi_server_orchestration.time.time', return_value=time.time() + 8 * 86400):
            for runtime in (self.home, self.remote):
                runtime.multi_server().tick()
                f.f.eventually(lambda: not runtime.multi_server()._running)
        self.assertFalse(export.exists())
        self.assertEqual(self.network['remote'].receive('home', envelope)['outcome'], 'unknown')
        calls = len(self.transports['home'].calls)
        self.assertTrue(self.spawn()['expired'])
        self.assertEqual(len(self.transports['home'].calls), calls)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.spawn(prompt='A changed expired request')
        self.assertEqual(self.network['home'].tools(self.lead, {'action': 'receipt', 'server': 'remote',
            'request_id': envelope['requestId']}, 'expired-receipt')['state'], 'expired')
        changed = copy.deepcopy(envelope)
        changed['payload']['text'] = 'Different content'
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.network['remote'].receive('home', changed)
        with self.remote.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events WHERE id=?', (envelope['requestId'],)).fetchone()[0], 1)
        self.drain()
        previous = self.home.agent(worker).get('remoteStateSequence', 0)
        with self.remote.lock, self.remote.db() as db:
            a = self.remote.agent(worker, db)
            a.update(status='completed', inFlight=False)
            self.remote.put(db, 'agents', a)
        self.drain()
        self.assertGreater(self.home.agent(worker)['remoteStateSequence'], previous)

if __name__ == '__main__':
    unittest.main(verbosity=2)
