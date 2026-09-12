#!/usr/bin/env python3
"""Regressions for transport stalls, receipt truth, and transaction boundaries."""
import concurrent.futures
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest
from unittest.mock import patch
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import AppServer, Runtime

def load(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
fixture = load('runtime-contract')

class CriticalRuntimeContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.server = self.runtime.connect()

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def test_pipe_backpressure_has_a_deadline(self):
        read_fd, write_fd = os.pipe()
        server = AppServer.__new__(AppServer)
        server.lock = threading.RLock()
        server.write_lock = threading.RLock()
        server.closed = False
        server.transport_error = None
        server.log = tempfile.TemporaryFile()
        stdin = os.fdopen(write_fd, 'w', encoding='utf8')
        server.proc = types.SimpleNamespace(stdin=stdin, poll=lambda: None, terminate=lambda: None)
        finished = threading.Event()
        errors = []
        def write():
            try:
                server.write({'data': 'x' * 2_000_000})
            except Exception as error:
                errors.append(error)
            finally:
                finished.set()
        with patch.object(AppServer, 'WRITE_TIMEOUT', .1, create=True):
            worker = threading.Thread(target=write, daemon=True)
            worker.start()
            bounded = finished.wait(.7)
            os.close(read_fd)
            worker.join(2)
        stdin.close()
        server.log.close()
        self.assertTrue(bounded, 'Native stdin blocked past the write deadline')
        self.assertTrue(errors)

    def test_busy_writer_rejects_unsent_request_without_leaking_future(self):
        from codex_runtime import SubmissionRejected
        server = AppServer.__new__(AppServer)
        server.lock = threading.RLock()
        server.write_lock = threading.RLock()
        server.sequence, server.pending = 0, {}
        entered, release = threading.Event(), threading.Event()
        def hold():
            with server.write_lock:
                entered.set()
                release.wait(2)
        worker = threading.Thread(target=hold)
        worker.start()
        self.assertTrue(entered.wait(1))
        try:
            with patch.object(AppServer, 'WRITE_TIMEOUT', .02):
                with self.assertRaises(SubmissionRejected):
                    server.submit('turn/start', {})
            self.assertEqual(server.pending, {})
        finally:
            release.set()
            worker.join(2)

    def test_real_pipe_preserves_large_utf8_json_frame(self):
        read_fd, write_fd = os.pipe()
        server = AppServer.__new__(AppServer)
        server.lock = threading.RLock()
        server.write_lock = threading.RLock()
        server.closed, server.transport_error = False, None
        stdin = os.fdopen(write_fd, 'w', encoding='utf8')
        server.proc = types.SimpleNamespace(stdin=stdin, poll=lambda: None)
        result = []
        def read():
            with os.fdopen(read_fd, 'r', encoding='utf8') as stream:
                result.append(json.loads(stream.readline()))
        worker = threading.Thread(target=read)
        worker.start()
        payload = {'text': 'Привет 🌍' * 10000}
        try:
            server.write(payload)
            worker.join(2)
            self.assertEqual(result, [payload])
        finally:
            stdin.close()
            worker.join(2)

    def test_explicit_rejection_resolves_uncertain_delivery(self):
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Task'}, defer=True)
        key = a['id']
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(key, db)
            a.update(status='starting', inFlight=True, startAttempt={'id': 'attempt', 'epoch': a['epoch'],
                     'accountKey': 'default', 'connectionId': self.runtime.connection_ids['default'],
                     'submitted': True, 'events': [key + ':initial']})
            self.runtime.put(db, 'agents', a)
            db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                       (key + ":initial", key, "user", "Task", "uncertain", time.time(), a["epoch"], None, None))
        self.runtime.start_error(key, 'attempt', RuntimeError('Invalid turn parameters'), unknown=False)
        self.assertEqual(self.runtime.delivery_receipt(key + ':initial')['status'], 'failed')
        self.assertFalse(self.runtime.agent(key)['inFlight'])

    def test_successful_start_with_local_failure_keeps_unknown_delivery(self):
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Task'}, draft=True)
        attempt = {'id': 'attempt', 'epoch': a['epoch'], 'accountKey': 'default',
                   'connectionId': self.runtime.connection_ids['default'],
                   'submitted': True, 'events': [a['id'] + ':initial']}
        with self.runtime.lock, self.runtime.db() as db:
            a.update(status='starting', inFlight=True, startAttempt=attempt)
            self.runtime.put(db, 'agents', a)
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       (attempt['events'][0], a['id'], 'user', 'Task', 'dispatching', time.time(), a['epoch'], None, None))
        future = concurrent.futures.Future()
        future.set_result({'turn': {'id': 'accepted-native-turn'}})
        with patch.object(self.runtime, 'start_accepted', side_effect=sqlite3.OperationalError('commit failed')):
            self.runtime.start_result(a['id'], attempt, future)
        self.assertEqual(self.runtime.delivery_receipt(attempt['events'][0])['status'], 'uncertain')
        self.assertTrue(self.runtime.agent(a['id'])['inFlight'])

    def test_lost_approval_reply_cannot_be_sent_again(self):
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Task',
                                 'permissionMode': 'default'}, defer=True)
        a = self.runtime.prepare(a)
        self.runtime.request({'id': 700, 'method': 'item/commandExecution/requestApproval',
                              'params': {'threadId': a['threadId'], 'command': 'git status'}})
        request = self.runtime.snapshot()['requests'][0]
        original = self.server.write
        def lost_ack(message):
            original(message)
            raise OSError('pipe failed after delivery')
        with patch.object(self.server, 'write', lost_ack):
            with self.assertRaises(OSError):
                self.runtime.answer(request['id'], {'decision': 'accept'})
        count = len(self.server.responses)
        with self.assertRaisesRegex(ValueError, 'uncertain'):
            self.runtime.answer(request['id'], {'decision': 'accept'})
        with self.assertRaisesRegex(ValueError, 'different answer'):
            self.runtime.answer(request['id'], {'decision': 'decline'})
        self.assertEqual(len(self.server.responses), count)
        with self.runtime.db() as db:
            record = json.loads(db.execute('SELECT record FROM runtime_requests WHERE id=?',
                                           (request['id'],)).fetchone()[0])
        self.assertEqual(record['status'], 'uncertain')

    def test_unsent_approval_remains_answerable(self):
        from codex_runtime import SubmissionRejected
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Task'}, defer=True)
        a = self.runtime.prepare(a)
        self.runtime.request({'id': 701, 'method': 'item/commandExecution/requestApproval',
                              'params': {'threadId': a['threadId'], 'command': 'git status'}})
        request = self.runtime.snapshot()['requests'][0]
        with patch.object(self.runtime, 'reply', side_effect=SubmissionRejected('input busy; not submitted')):
            with self.assertRaises(SubmissionRejected):
                self.runtime.answer(request['id'], {'decision': 'accept'})
        self.assertEqual(self.runtime.answer(request['id'], {'decision': 'decline'})['status'], 'answered')
        self.assertEqual(self.server.responses[-1], {'id': 701, 'result': {'decision': 'decline'}})

    def test_worker_adopts_exact_worktree_after_metadata_failure(self):
        repo = Path(self.temp.name) / 'project'
        repo.mkdir()
        def git(*args):
            return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.DEVNULL).decode().strip()
        git('init')
        git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', 'commit', '--allow-empty', '-m', 'Base')
        lead = self.runtime.create({'name': 'Lead', 'cwd': str(repo), 'prompt': 'Task'}, draft=True)
        worker = self.runtime.create({'name': 'Worker', 'prompt': 'Task', 'role': 'implementer'}, lead['id'], defer=True)
        original = self.runtime.put
        def fail_metadata(db, table, record):
            if table == 'agents' and record['id'] == worker['id'] and record.get('worktreeReady'):
                raise sqlite3.OperationalError('metadata write failed')
            return original(db, table, record)
        with patch.object(self.runtime, 'put', fail_metadata):
            with self.assertRaises(sqlite3.OperationalError):
                self.runtime.prepare(worker)
        directory = repo / '.worktrees' / 'codex-agents' / worker['id']
        self.assertTrue(directory.is_dir())
        (directory / 'preserve.txt').write_text('unsaved worker change')
        recovered = self.runtime.prepare(self.runtime.agent(worker['id']))
        self.assertTrue(recovered['worktreeReady'])
        self.assertEqual(Path(recovered['cwd']).resolve(), directory.resolve())
        self.assertEqual((directory / 'preserve.txt').read_text(), 'unsaved worker change')
        self.assertEqual(git('worktree', 'list', '--porcelain').count('worktree '), 2)

    def test_worker_does_not_adopt_a_different_worktree_branch(self):
        repo = Path(self.temp.name) / 'project'
        repo.mkdir()
        def git(*args):
            return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.DEVNULL)
        git('init')
        git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', 'commit', '--allow-empty', '-m', 'Base')
        lead = self.runtime.create({'name': 'Lead', 'cwd': str(repo), 'prompt': 'Task'}, draft=True)
        worker = self.runtime.create({'name': 'Worker', 'prompt': 'Task', 'role': 'implementer'}, lead['id'], defer=True)
        directory = repo / '.worktrees' / 'codex-agents' / worker['id']
        git('worktree', 'add', '-b', 'unrelated-user-branch', str(directory), 'HEAD')
        (directory / 'preserve.txt').write_text('user content')
        with self.assertRaisesRegex(ValueError, 'identity differs'):
            self.runtime.prepare(worker)
        self.assertFalse(self.runtime.agent(worker['id'])['worktreeReady'])
        self.assertEqual((directory / 'preserve.txt').read_text(), 'user content')

    def test_failed_preparation_commit_does_not_poison_loaded_cache(self):
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Task'}, defer=True)
        operation = {'id': 'prepare', 'method': 'thread/start', 'agent': a['id'], 'epoch': a['epoch'], 'accountKey': 'default',
                     'connectionId': self.runtime.connection_ids['default'], 'threadId': None,
                     'cwd': a['cwd'], 'settings': self.runtime.preparation_settings(a),
                     'future': concurrent.futures.Future()}
        with self.runtime.lock, self.runtime.db() as db:
            a['prepareAttempt'] = operation['id']
            self.runtime.put(db, 'agents', a)
        original = self.runtime.db
        class FailCommit:
            def __init__(self, db): self.db = db
            def __getattr__(self, name): return getattr(self.db, name)
            def commit(self): raise sqlite3.OperationalError('simulated disk I/O error at commit')
        @contextmanager
        def failed_commit():
            with original() as db:
                proxy = FailCommit(db)
                yield proxy
                proxy.commit()
        result = concurrent.futures.Future()
        result.set_result({'thread': {'id': 'new-native-thread'}})
        with patch.object(self.runtime, 'db', failed_commit):
            self.runtime.prepared_result(operation, result)
        self.assertIsInstance(operation['future'].exception(), sqlite3.OperationalError)
        self.assertNotIn(a['id'], self.runtime.loaded, 'Uncommitted thread was cached as prepared')
        self.assertIsNone(self.runtime.agent(a['id'])['threadId'])

if __name__ == '__main__': unittest.main()
