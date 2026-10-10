#!/usr/bin/env python3
"""Output wakes publish immediately without repeating whole-roster dispatch."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_runtime
from codex_runtime import Runtime
from codex_scheduler_wake import SchedulerWake, record_needs_dispatch


class ClockWake(SchedulerWake):
    def __init__(self, runtime, steps):
        super().__init__()
        self.runtime, self.steps = runtime, iter(steps)
        self.now = 100.0
        self.waits = []
        self.on_step = None

    def wait(self, timeout=None):
        self.waits.append(timeout)
        step = next(self.steps, None)
        if step is None:
            self.runtime.closed = True
            return False
        elapsed, kind = step
        self.now += elapsed
        if self.on_step:
            self.on_step()
        if kind == 'work':
            self.set()
        elif kind == 'resource':
            self.set_resources()
        return self.is_set()

    @contextmanager
    def installed(self):
        clock = SimpleNamespace(monotonic=lambda: self.now, time=lambda: self.now)
        with patch.object(codex_runtime, 'time', clock), patch.object(codex_runtime, 'startup_memory_mark'):
            yield


class QuietRuntime(Runtime):
    def schedule(self):
        pass


class WorkWakeContract(unittest.TestCase):
    def test_resource_wake_cannot_remove_a_work_request(self):
        event = SchedulerWake()
        event.set()
        event.set_resources()
        self.assertTrue(event.consume_dispatch())
        self.assertFalse(event.is_set())
        event.set_resources()
        self.assertFalse(event.consume_dispatch())

    def test_concurrent_set_during_consume_remains_pending(self):
        event = SchedulerWake()
        event.set_resources()
        entered, release = threading.Event(), threading.Event()
        original = threading.Event.clear
        result = []

        def gated_clear(current):
            if current is event:
                entered.set()
                self.assertTrue(release.wait(2))
            original(current)

        with patch.object(threading.Event, 'clear', gated_clear):
            consumer = threading.Thread(target=lambda: result.append(event.consume_dispatch()))
            consumer.start()
            self.assertTrue(entered.wait(2))
            setter = threading.Thread(target=event.set)
            setter.start()
            release.set()
            consumer.join(2)
            setter.join(2)
            self.assertFalse(consumer.is_alive())
            self.assertFalse(setter.is_alive())
        self.assertEqual(result, [False])
        self.assertTrue(event.is_set())
        self.assertTrue(event.consume_dispatch())

    def test_only_live_output_changes_skip_dispatch(self):
        actor = dict(status='running', inFlight=True, epoch=1, autoWake=True, accountKey='account')
        self.assertFalse(record_needs_dispatch('agents', actor,
            {**actor, 'events': 2, 'lastEvent': 'now', 'activity': {'phase': 'writing'}, 'tail': 'text'}))
        changes = {'status': 'queued', 'inFlight': False, 'autoWake': False, 'epoch': 2,
                   'accountKey': 'other', 'startAttempt': {'submitted': True}, 'contextRepair': {'phase': 'pending'},
                   'nativeFailureHold': True, 'accountTransferId': 'transfer', 'activeTools': [],
                   'futureSchedulingField': True}
        for field, value in changes.items():
            self.assertTrue(record_needs_dispatch('agents', actor, {**actor, field: value}), field)
        self.assertTrue(record_needs_dispatch('agents', None, actor))
        self.assertTrue(record_needs_dispatch('agents', {'status': 'queued'}, {'status': 'queued', 'tail': 'text'}))
        monitor = dict(status='running', activityGeneration=1)
        self.assertFalse(record_needs_dispatch('monitors', monitor, {**monitor, 'tail': 'text', 'activityGeneration': 2}))
        self.assertTrue(record_needs_dispatch('monitors', monitor, {**monitor, 'stallTimeoutSeconds': 10}))
        self.assertTrue(record_needs_dispatch('rules', {'nextAt': 10}, {'nextAt': 11}))

    def drive(self, steps, dispatch=None, on_wake=None):
        rt = Runtime.__new__(Runtime)
        rt.closed = False
        rt.root = Path(self.directory.name)
        clock = ClockWake(rt, steps)
        clock.on_step = on_wake
        rt.changed = clock
        seen, published = [], []
        rt._retry_dirty_workspace_refresh = lambda: None
        rt._publish_committed_resource_changes = lambda: published.append(clock.now)
        rt.dispatch = dispatch or (lambda **_options: seen.append(clock.now))
        for name in ('monitors_tick', 'rules_tick', 'capacity_tick', 'usage_resume_tick',
                     'accepted_archive_tick', 'retry_monitor_results', 'runtime_maintenance_tick', 'turn_item_links_tick'):
            setattr(rt, name, lambda: None)
        with clock.installed():
            rt.schedule()
        self.assertIsNone(rt.scheduler_error)
        return seen, published

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='scheduler-work-wake-')
        self.addCleanup(self.directory.cleanup)

    def test_resource_storm_publishes_each_wake_without_dispatch_storm(self):
        seen, published = self.drive([(.001, 'resource')] * 100)
        self.assertEqual(len(seen), 1)
        self.assertEqual(len(published), 100)

    def test_work_and_resources_cannot_postpone_full_reconciliation(self):
        seen, published = self.drive([(.001, 'resource'), (4.9, 'work'), (.11, 'resource')])
        self.assertEqual(len(seen), 3)
        self.assertEqual(len(published), 3)
        self.assertLess(seen[-1] - seen[0], 5.02)

    def test_periodic_fallback_sees_external_writes_without_a_local_event(self):
        deliveries = []
        with sqlite3.connect(Path(self.directory.name) / 'external.sqlite3') as reader:
            reader.execute('CREATE TABLE input(id TEXT PRIMARY KEY,status TEXT)')
            reader.commit()
            calls = []

            def dispatch(**_options):
                calls.append(True)
                pending = reader.execute("SELECT id FROM input WHERE status='pending'").fetchall()
                deliveries.extend(key for key, in pending)
                reader.execute("UPDATE input SET status='delivered' WHERE status='pending'")
                reader.commit()
                if len(calls) == 1:
                    with sqlite3.connect(Path(self.directory.name) / 'external.sqlite3') as writer:
                        writer.execute("INSERT INTO input VALUES('exact-input','pending')")

            self.drive([(.001, 'resource'), (4.9, 'resource'), (.11, 'resource'), (5.1, 'resource')], dispatch)
        self.assertEqual(deliveries, ['exact-input'])

    def test_external_agent_change_with_journal_gap_gets_full_reconciliation(self):
        spec = importlib.util.spec_from_file_location('gap_roster_fixture',
            Path(__file__).with_name('scheduler-roster-cache-cpu-contract.py'))
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        case = fixture.RosterCache('test_direct_agent_insert_update_delete_changes_membership')
        case.setUp()
        self.addCleanup(case.tearDown)
        case.runtime.db_path = case.path
        case.runtime.install_scheduler_change_tracking(case.db)
        case.add(status='running')
        seen = []

        def dispatch(**_options):
            seen.append(case.roster()[0]['status'])
            if len(seen) == 1:
                writer = case.connect()
                writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval')")
                writer.execute('DELETE FROM runtime_scheduler_agent_changes')
                writer.commit()

        self.drive([(.001, 'resource'), (4.9, 'resource'), (.11, 'resource')], dispatch)
        self.assertEqual(seen, ['running', 'approval'])
        self.assertEqual(len(case.scans), 2)

    def test_committed_output_is_resource_only_but_terminal_state_is_work(self):
        rt = QuietRuntime(Path(self.directory.name), server_factory=object)
        self.addCleanup(rt.close)
        lead = rt.create({'name': 'Fixture', 'cwd': self.directory.name, 'prompt': 'Work'}, defer=True)
        with rt.lock, rt.db() as db:
            a = rt.agent(lead['id'], db)
            a.update(status='running', inFlight=True, turnId='turn')
            rt.put(db, 'agents', a)
        rt.changed.consume_dispatch()
        with rt.lock, rt.db() as db:
            a = rt.agent(lead['id'], db)
            a.update(events=a['events'] + 1, tail='output', activity={'phase': 'writing', 'at': time.time()})
            rt.put(db, 'agents', a)
        self.assertTrue(rt.changed.is_set())
        self.assertFalse(rt.changed.consume_dispatch())
        with rt.lock, rt.db() as db:
            a = rt.agent(lead['id'], db)
            a.update(status='completed', inFlight=False, turnId=None)
            rt.put(db, 'agents', a)
        self.assertTrue(rt.changed.consume_dispatch())

    def test_rollback_does_not_promote_output_to_work(self):
        rt = QuietRuntime(Path(self.directory.name), server_factory=object)
        self.addCleanup(rt.close)
        lead = rt.create({'name': 'Fixture', 'cwd': self.directory.name, 'prompt': 'Work'}, defer=True)
        rt.changed.consume_dispatch()
        with rt.db() as db:
            a = rt.agent(lead['id'], db)
            a['nativeFailureHold'] = True
            rt.put(db, 'agents', a)
            db.rollback()
            rt._stage_transcript_resource(db, a['id'])
        self.assertTrue(rt.changed.is_set())
        self.assertFalse(rt.changed.consume_dispatch())

    def test_direct_event_sql_wakes_work_only_after_commit(self):
        rt = QuietRuntime(Path(self.directory.name), server_factory=object)
        self.addCleanup(rt.close)
        lead = rt.create({'name': 'Fixture', 'cwd': self.directory.name, 'prompt': 'Work'}, defer=True)
        rt.changed.consume_dispatch()
        with rt.db() as db:
            db.execute("INSERT INTO runtime_events VALUES(?,?,'monitor_exit','result','pending',?,1,NULL,NULL)",
                       ('native-result', lead['id'], time.time()))
            self.assertFalse(rt.changed.is_set())
            db.commit()
            self.assertTrue(rt.changed.consume_dispatch())
        with rt.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE id='native-result'").fetchone()[0], 1)

    def test_real_monitor_output_keeps_work_clear_and_exit_wakes_once(self):
        rt = QuietRuntime(Path(self.directory.name), server_factory=object)
        self.addCleanup(rt.close)
        lead = rt.create({'name': 'Fixture', 'cwd': self.directory.name, 'prompt': 'Work'}, defer=True)
        with rt.lock, rt.db() as db:
            actor = rt.agent(lead['id'], db)
            actor.update(autoWake=True, status='waiting')
            rt.put(db, 'agents', actor)
        monitor = rt.monitor(lead['id'], {'command': 'fixture command'}, key='output-fixture')
        with rt.lock, rt.db() as db:
            monitor.update(status='running')
            rt.put(db, 'monitors', monitor)
        rt.connection_ids['default'] = 'fixture-connection'
        rt.changed.consume_dispatch()
        self.assertFalse(rt.changed.is_set())
        chunks = [b'first line\n', 'готово\n'.encode()]
        for chunk in chunks:
            rt.output({'processId': monitor['id'], 'deltaBase64': base64.b64encode(chunk).decode()},
                      'default', 'fixture-connection')
            self.assertFalse(rt.changed.consume_dispatch())
            self.assertFalse(rt.changed.is_set())
        expected = b''.join(chunks)
        self.assertEqual(Path(monitor['log']).read_bytes(), expected)
        with rt.read_db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?',
                                          (monitor['id'],)).fetchone()[0])
            payload = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='monitor' AND id=?",
                                            (monitor['id'],)).fetchone()[0])
        self.assertEqual(saved['bytes'], len(expected))
        self.assertEqual(saved['tail'], expected.decode())
        self.assertEqual(saved['activityGeneration'], len(chunks))
        self.assertEqual(payload['value']['bytes'], len(expected))
        self.assertEqual(payload['value']['tail'], expected.decode())
        rt.finish_monitor(monitor['id'], 0, None)
        self.assertTrue(rt.changed.consume_dispatch())
        self.assertFalse(rt.changed.is_set())
        rt.finish_monitor(monitor['id'], 0, None)
        with rt.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE id=?",
                                        ('monitor:' + monitor['id'],)).fetchone()[0], 1)

    def test_output_storm_large_roster_cpu_and_correctness(self):
        # The existing roster fixture owns a real private journal and deep-copy cache.
        spec = importlib.util.spec_from_file_location('roster_fixture',
            Path(__file__).with_name('scheduler-roster-cache-cpu-contract.py'))
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        case = fixture.RosterCache('test_unrelated_write_reuses_committed_roster_and_preserves_copies')
        case.setUp()
        self.addCleanup(case.tearDown)
        rows = []
        for index in range(2048):
            actor = dict(id=str(index), rootId='root', parentId='root', isLead=False, name=str(index),
                         epoch=1, status='running' if index < 64 else 'completed', inFlight=index < 64,
                         autoWake=False, prompt='x' * 4096, metadata={'nested': list(range(32))})
            rows.append((actor['id'], json.dumps(actor)))
        case.db.executemany('INSERT INTO runtime_agents VALUES(?,?)', rows)
        case.db.commit()
        case.runtime.db_path = case.path
        case.runtime.install_scheduler_change_tracking(case.db)
        case.roster()
        checksums = []
        measured = []
        updates = []
        def output_update():
            updates.append(True)
            case.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.tail',?,'$.events',?) WHERE id='0'",
                            ('new output', len(updates)))
            case.db.commit()
        for kind in ('work', 'resource'):
            observed = []
            def dispatch(**_options):
                actors = case.roster()
                observed.extend(a['id'] for a in actors)
            began = time.thread_time()
            self.drive([(.001, kind)] * 100, dispatch, output_update)
            measured.append((time.thread_time() - began) * 1000)
            checksums.append(hashlib.sha256(json.dumps(sorted(set(observed))).encode()).hexdigest())
        self.assertEqual(checksums[0], checksums[1])
        self.assertEqual(len(set(observed)), 64)
        self.assertEqual(case.roster()[0]['events'], 200)
        print(json.dumps({'fixture': '2048-actors-64-active-100-output-wakes',
                          'baselineCpuMs': round(measured[0], 3), 'resourceWakeCpuMs': round(measured[1], 3),
                          'correctnessSha256': checksums[0], 'baselineDispatches': 100, 'resourceDispatches': 1}))


if __name__ == '__main__':
    unittest.main()
