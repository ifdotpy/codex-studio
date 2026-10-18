#!/usr/bin/env python3
"""A full disk holds agents and new turns, then resumes them without input replay.

Free space is injected at the codex_disk_space sampling boundary and provider errors at the
fake app-server. No test fills a real disk.
"""

from test_isolation import isolate_supervisor_environment

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_SOURCE_ROOT

isolate_supervisor_environment()
from runtime_test_environment import make_runtime

import importlib.util
import json
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_disk_space as disk
from codex_runtime import Runtime
from codex_native_errors import NativeRpcError
from codex_shared_values import DISK_FULL_MAX_RESUMES, DISK_FULL_RECHECK_SECONDS
from server_test_waits import HANG_GUARD_SECONDS

spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
assert spec and spec.loader
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

MIB = 1024**2
GIB = 1024**3
INCIDENT = ("ENOSPC: no space left on device, rename '/state/account-servers/claude-profile-ee03/"
            "sessions/55f43293.meta.json.7c1e.tmp' -> '/state/account-servers/claude-profile-ee03/"
            "sessions/55f43293.meta.json'")


class DiskFullContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.free = 100 * GIB
        sampler = patch('codex_disk_space.free_bytes', side_effect=lambda _root: self.free)
        sampler.start()
        self.addCleanup(sampler.stop)
        self.runtime = make_runtime(Runtime, self.root, fixture.FakeServer)
        self.server = self.runtime.connect()
        self.starts_seen = threading.Condition()
        original_notify = self.server._notify

        def notify(message):
            result = original_notify(message)
            if message.get('method') == 'turn/started':
                with self.starts_seen:
                    self.starts_seen.notify_all()
            return result

        notify_patch = patch.object(self.server, '_notify', side_effect=notify)
        notify_patch.start()
        self.addCleanup(notify_patch.stop)
        agent = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Original task'})
        self.key = agent['id']
        self.wait_turns(1)

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def agent(self):
        return self.runtime.agent(self.key)

    def starts(self):
        return [params for method, params in self.server.calls if method == 'turn/start']

    def wait_turns(self, count):
        deadline = time.monotonic() + HANG_GUARD_SECONDS
        with self.starts_seen:
            while len([p for p in self.starts()]) < count or not self.agent().get('turnId'):
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, f'turn/start {count} was not observed')
                self.starts_seen.wait(min(0.05, remaining))

    def set_free(self, free, *, clear=False):
        """Change the injected free space. clear also ends an observed-error hold."""
        self.free = free
        state = self.runtime.__dict__.get('_disk_full_state')
        if state is not None:
            state['sampledAt'] = None
            if clear:
                state['observedUntil'] = 0.0

    def fail(self, message=INCIDENT):
        agent = self.agent()
        self.server.notify({'method': 'turn/completed', 'params': {'threadId': agent['threadId'],
            'turn': {'id': agent['turnId'], 'status': 'failed', 'error': {'message': message}}}})
        return self.agent().get('capacityRetry')

    def due(self):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['capacityRetry']['dueAt'] = time.time() - 1
            self.runtime.capacity_save(db, agent, agent['capacityRetry'])
            self.runtime.put(db, 'agents', agent)

    def test_disk_full_turn_failure_holds_then_resumes_once_with_empty_input(self):
        self.set_free(100 * MIB)
        retry = self.fail()
        self.assertEqual((retry['cause'], retry['status'], retry['maxAttempts']),
                         ('diskFull', 'scheduled', DISK_FULL_MAX_RESUMES))
        self.assertEqual(retry['reason'],
                         'Disk is full: 100 MiB free. Studio resumes this agent when free space returns.')
        self.assertTrue(self.runtime.worker_continuation_pending(self.agent()),
                        'the held agent does not report a stopped child')
        with self.runtime.db() as db:
            projected = json.loads(db.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (self.key,)).fetchone()[0])["value"]["capacityRetry"]
        self.assertEqual((projected['cause'], projected['reason']), ('diskFull', retry['reason']),
                         'the renderer receives the cause and the reason of the hold')
        with self.runtime.db() as db:
            notices = [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_items WHERE agent=? AND id LIKE '%native-notice:error:%'",
                (self.key,))]
        self.assertEqual([notice['text'] for notice in notices], [disk.ERROR_MESSAGE],
                         'the chat shows the clear disk-full message, not the raw error')

        for _ in range(3):
            self.due()
            self.runtime.capacity_tick()
            current = self.agent()['capacityRetry']
            self.assertEqual(current['status'], 'scheduled')
            self.assertFalse(current.get('claimedAt'))
            self.assertAlmostEqual(current['dueAt'] - time.time(), DISK_FULL_RECHECK_SECONDS, delta=2)
            self.assertNotIn('waits', current, 'a free-space wait does not grow its delay')
        self.assertEqual(len(self.starts()), 1, 'no turn starts while the disk is full')
        with patch.object(self.runtime, 'capacity_run') as run:
            self.set_free(1500 * MIB, clear=True)
            self.due()
            self.runtime.capacity_tick()
            run.assert_not_called()
        self.assertEqual(self.agent()['capacityRetry']['reason'],
                         'Disk is full: 1500 MiB free. Studio resumes this agent when free space returns.',
                         'below the exit threshold the agent still waits')

        self.set_free(10 * GIB, clear=True)
        self.due()
        self.runtime.capacity_tick()
        self.wait_turns(2)
        self.assertEqual(len(self.starts()), 2)
        self.assertEqual(self.starts()[-1]['input'], [], 'the continuation replays no input')
        self.assertNotIn('clientUserMessageId', self.starts()[-1])
        self.runtime.capacity_tick()
        self.assertEqual(len(self.starts()), 2)

    def test_manual_retry_while_disk_full_is_refused_with_the_reason(self):
        self.set_free(100 * MIB)
        retry = self.fail()
        with self.assertRaisesRegex(ValueError, r'^Disk is full: 100 MiB free\.'):
            self.runtime.capacity_retry(self.key, retry['id'], 'retry')
        self.assertEqual(len(self.starts()), 1)

    def test_queued_input_waits_while_disk_full_and_starts_once_after(self):
        agent = self.agent()
        self.server.complete(agent['threadId'], agent['turnId'])
        self.set_free(100 * MIB)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.enqueue(db, self.agent(), 'followup', 'Next task', 'disk-event')
        self.runtime.dispatch()
        with self.runtime.db() as db:
            status = db.execute("SELECT status FROM runtime_events WHERE id='disk-event'").fetchone()[0]
        self.assertEqual(status, 'pending')
        self.assertEqual(self.agent()['status'], 'queued')
        self.assertEqual(len(self.starts()), 1)
        desktop = disk.status(self.root, self.runtime)
        self.assertEqual((desktop['level'], desktop['freeBytes']), ('full', 100 * MIB))
        self.set_free(1500 * MIB)
        self.runtime.dispatch()
        self.assertEqual(len(self.starts()), 1, 'the full state ends only at the exit threshold')
        self.set_free(10 * GIB)
        self.runtime.dispatch()
        self.wait_turns(2)
        self.assertIn('Next task', str(self.starts()[-1]['input']))
        self.runtime.dispatch()
        self.assertEqual(len(self.starts()), 2)

    def test_rejected_unapplied_start_returns_the_exact_input_to_the_queue(self):
        agent = self.agent()
        self.server.complete(agent['threadId'], agent['turnId'])
        original = self.server.call
        rejected = []

        def reject_once(method, params, timeout=60):
            if method == 'turn/start' and not rejected:
                rejected.append(params)
                self.server.calls.append((method, params))
                raise NativeRpcError({'code': -32000,
                    'message': 'Disk is full. Claude did not receive this input. Studio sends it when free space returns.',
                    'data': {'turnStartOutcome': 'not_applied', 'inputSubmitted': False, 'diskFull': True}})
            return original(method, params, timeout)

        with patch.object(self.server, 'call', side_effect=reject_once):
            with self.runtime.lock, self.runtime.db() as db:
                self.runtime.enqueue(db, self.agent(), 'followup', 'Exact input', 'disk-input')
            self.runtime.dispatch()
            deadline = time.monotonic() + HANG_GUARD_SECONDS
            while (not rejected or self.agent()['status'] != 'queued') and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertEqual(self.agent()['status'], 'queued', self.agent().get('error'))
            self.assertIsNone(self.agent().get('error'))
            with self.runtime.db() as db:
                row = db.execute("SELECT status, error FROM runtime_events WHERE id='disk-input'").fetchone()
            self.assertEqual(tuple(row), ('pending', None))
            self.runtime.dispatch()
            self.assertEqual(len(self.starts()), 2, 'the observed disk-full error holds the next start')
            self.set_free(10 * GIB, clear=True)
            self.runtime.dispatch()
            self.wait_turns(3)
        self.assertIn('Exact input', str(self.starts()[-1]['input']))
        self.assertEqual(self.starts()[-1].get('clientUserMessageId'), rejected[0].get('clientUserMessageId'))


if __name__ == '__main__':
    unittest.main()
