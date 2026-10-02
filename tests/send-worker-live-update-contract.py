#!/usr/bin/env python3
"""A live source patch must also protect dispatcher frames that already run."""
import importlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
from codex_source import source_function

fixture_spec = importlib.util.spec_from_file_location(
    'persistence_fixture', Path(__file__).with_name('supervisor-persistence-retry-contract.py'))
fixture = importlib.util.module_from_spec(fixture_spec)
fixture_spec.loader.exec_module(fixture)


def reviewed_patch():
    update = importlib.import_module('codex_send_worker_reliability_update')
    for name in update.SOURCES:
        importlib.import_module(name)
    return update


def frame_contains(worker, code):
    current = sys._current_frames().get(worker.ident)
    while current is not None:
        if current.f_code is code:
            return True
        current = current.f_back
    return False


def function_snapshot(update):
    functions = []
    for name, source in update.SOURCES.items():
        module = sys.modules[name]
        for item in source['functions']:
            current = module
            for part in item['path']:
                current = getattr(current, part, None)
                if current is None:
                    break
            actual = getattr(current, '__wrapped__', current) if item['manager'] else current
            functions.append((name, tuple(item['path']), id(actual),
                              id(getattr(actual, '__code__', None)),
                              id(getattr(actual, '__defaults__', None)),
                              id(getattr(actual, '__kwdefaults__', None)),
                              dict(getattr(actual, '__kwdefaults__', None) or {})))
    return functions


def native_fixture(runtime, directory, *, busy=False):
    agent = runtime.create({'name': 'Existing worker', 'cwd': directory, 'prompt': ''},
                           draft=True, defer=True)
    with runtime.lock, runtime.db() as db:
        agent.update(threadId='thread', turnId='turn', status='running', inFlight=True,
                     autoWake=False, events=0)
        runtime.put(db, 'agents', agent)
    delivered, lookups, commits = [], [], []

    def notify(message):
        delivered.append(message['_studioSupervisorSequence'])
        runtime.notification(message)

    def lookup(sequence):
        lookups.append(sequence)
        if busy and len(lookups) <= 2:
            raise fixture.busy_error()
        return runtime.supervisor_event_applied('fixture:account', sequence)

    def commit(message, sequence):
        commits.append(sequence)
        if busy and len(commits) <= 2:
            raise fixture.busy_error()
        runtime.commit_supervisor_event('fixture:account', message, sequence, 'default', None)

    def durable():
        with runtime.read_db() as db:
            row = db.execute('SELECT sequence FROM runtime_supervisor_cursor WHERE handle=?',
                             ('fixture:account',)).fetchone()
            return row[0] if row else 0

    dispatch = fixture.DispatchFixture(notify, lambda _: None, lookup, commit, durable)
    server = dispatch.server
    server.dispatcher = dispatch.worker
    server.proc.root = Path(directory)
    server.proc.generation = 7
    server.proc.pid = os.getpid()
    runtime.servers['default'] = server
    runtime.server = server
    runtime.connection_ids['default'] = 'existing-connection'
    return agent, dispatch, delivered, lookups, commits


def close_fixture(runtime, dispatch):
    dispatch.close()
    # The synthetic transport owns no native process or command threads. Remove
    # it after identity assertions so Runtime.close only closes its real owners.
    runtime.servers.pop('default', None)
    runtime.server = None
    runtime.close()


def active_frame_case():
    update = reviewed_patch()
    baseline = subprocess.run(
        ['git', 'show', '2e2eb39:scripts/codex_runtime.py'], cwd=ROOT,
        text=True, capture_output=True, check=True).stdout
    old_dispatch, _ = source_function(baseline, ('AppServer', 'dispatch'),
                                      vars(codex_runtime), '<baseline-2e2eb39>')
    live = codex_runtime.AppServer.dispatch
    saved = live.__code__, live.__defaults__, live.__kwdefaults__
    with tempfile.TemporaryDirectory(prefix='studio-active-dispatch-patch-') as directory:
        runtime = fixture.QuietRuntime(Path(directory), server_factory=lambda *args: None)
        agent, dispatch, delivered, lookups, commits = native_fixture(runtime, directory, busy=True)
        import codex_connection_recovery as recovery
        timer = recovery.start(runtime, interval=30)
        try:
            live.__code__, live.__defaults__, live.__kwdefaults__ = (
                old_dispatch.__code__, old_dispatch.__defaults__, old_dispatch.__kwdefaults__)
            dispatch.start([])
            dispatch.wait(lambda: frame_contains(dispatch.worker, old_dispatch.__code__))
            identities = (os.getpid(), runtime.servers, runtime.connection_ids,
                          runtime.server, dispatch.server.proc, dispatch.server.proc.pid)
            before_callbacks = (dispatch.server.supervisor_commit, dispatch.server.supervisor_event_applied)
            with patch.object(codex_runtime.subprocess, 'Popen',
                              side_effect=AssertionError('A live patch must not replace native processes')):
                first = update.apply(runtime)
                assert first['status'] in {'applied', 'already_applied'}, first
                assert frame_contains(dispatch.worker, old_dispatch.__code__), 'The existing frame must remain active'
                assert codex_runtime.AppServer.dispatch.__code__ is not old_dispatch.__code__
                assert dispatch.server.supervisor_commit is not before_callbacks[0]
                assert dispatch.server.supervisor_event_applied is not before_callbacks[1]
                assert runtime._connection_recovery_service is timer
                callbacks = (dispatch.server.supervisor_commit, dispatch.server.supervisor_event_applied)
                second = update.apply(runtime)
                assert second['status'] in {'applied', 'already_applied'}, second
                assert callbacks == (dispatch.server.supervisor_commit, dispatch.server.supervisor_event_applied)
                assert runtime._connection_recovery_service is timer
                assert timer.thread.is_alive()
                with dispatch.server.callback_lock:
                    for event in [fixture.fragment(1, 'a'), fixture.fragment(2, 'b'),
                                  {'method': 'fixture/terminal', '_studioSupervisorSequence': 3}]:
                        dispatch.server.enqueue(dispatch.server.notification, event)
                dispatch.wait(lambda: dispatch.server.proc.cursor == 3 or dispatch.server.transport_error)
                assert dispatch.server.transport_error is None, dispatch.server.transport_error
                assert not dispatch.server.proc.terminated
                assert not dispatch.server.closed
                assert delivered == [1, 2, 3], delivered
                assert lookups == [1, 1, 1, 3], lookups
                assert commits == [2, 2, 2, 3], commits
                assert dispatch.server.proc.acks == [2, 3]
                assert frame_contains(dispatch.worker, old_dispatch.__code__)
                assert identities == (os.getpid(), runtime.servers, runtime.connection_ids,
                                      runtime.server, dispatch.server.proc, dispatch.server.proc.pid)
                assert runtime.servers is identities[1]
                assert runtime.connection_ids is identities[2]
                assert runtime.server is identities[3]
                assert dispatch.server.proc is identities[4]
                assert runtime.connection_ids == {'default': 'existing-connection'}
                assert not runtime.offline and not runtime.offline_accounts
                with runtime.read_db() as db:
                    row = db.execute('SELECT record FROM runtime_items WHERE id=?',
                                     (agent['id'] + ':response',)).fetchone()
                    assert json.loads(row[0])['text'] == 'ab'
                with runtime.analytics_db() as db:
                    count = db.execute('SELECT coalesce(sum(count),0) FROM analytics_notifications '
                                       'WHERE agent=? AND method=?',
                                       (agent['id'], 'item/agentMessage/delta')).fetchone()[0]
                    assert count == 2, count
            print(json.dumps({'status': 'pass', 'case': 'active-frame', 'pid': os.getpid(),
                              'callbacks': delivered, 'lookups': lookups, 'commits': commits,
                              'acks': dispatch.server.proc.acks, 'timerPreserved': True}))
        finally:
            close_fixture(runtime, dispatch)
            live.__code__, live.__defaults__, live.__kwdefaults__ = saved


def unknown_signature_case():
    update = reviewed_patch()
    with tempfile.TemporaryDirectory(prefix='studio-unknown-live-function-') as directory:
        runtime = fixture.QuietRuntime(Path(directory), server_factory=lambda *args: None)
        _, dispatch, _, _, _ = native_fixture(runtime, directory)
        import codex_connection_recovery as recovery
        timer = recovery.start(runtime, interval=30)
        live = codex_runtime.AppServer.dispatch
        saved = live.__code__, live.__defaults__, live.__kwdefaults__

        def foreign_dispatch(self):
            raise AssertionError('Unreviewed dispatcher')

        try:
            live.__code__ = foreign_dispatch.__code__
            before = function_snapshot(update)
            callbacks = (dispatch.server.supervisor_commit, dispatch.server.supervisor_event_applied)
            identities = (runtime.servers, runtime.connection_ids, runtime.server, dispatch.server.proc)
            try:
                update.apply(runtime)
            except RuntimeError as error:
                assert 'dispatch' in str(error), error
            else:
                raise AssertionError('An unknown live function must be rejected')
            assert function_snapshot(update) == before, 'Rejection must precede every function mutation'
            assert callbacks == (dispatch.server.supervisor_commit, dispatch.server.supervisor_event_applied)
            assert identities == (runtime.servers, runtime.connection_ids, runtime.server, dispatch.server.proc)
            assert runtime.servers is identities[0]
            assert runtime.connection_ids is identities[1]
            assert runtime.server is identities[2]
            assert dispatch.server.proc is identities[3]
            assert runtime.connection_ids == {'default': 'existing-connection'}
            assert runtime._connection_recovery_service is timer
            assert timer.thread.is_alive()
            assert not getattr(dispatch.server, '_studio_persistence_retry_installed', False)
            print(json.dumps({'status': 'pass', 'case': 'unknown-signature',
                              'functionsUnchanged': len(before), 'timerPreserved': True}))
        finally:
            live.__code__, live.__defaults__, live.__kwdefaults__ = saved
            close_fixture(runtime, dispatch)


def cold_recovery_case():
    update = reviewed_patch()
    baseline = subprocess.run(
        ['git', 'show', '2e2eb39:scripts/codex_connection_recovery.py'], cwd=ROOT,
        text=True, capture_output=True, check=True).stdout
    import codex_connection_recovery as recovery
    original_namespace = dict(vars(recovery))
    with tempfile.TemporaryDirectory(prefix='studio-cold-recovery-patch-') as directory:
        runtime = fixture.QuietRuntime(Path(directory), server_factory=lambda *args: None)
        _, dispatch, _, _, _ = native_fixture(runtime, directory)
        identity = runtime.servers, runtime.connection_ids, runtime.server, dispatch.server.proc
        manager = None
        try:
            # Retain the module object and its source identity. Execute the actual
            # old module so newly added helpers and the timer class are absent.
            metadata = {name: value for name, value in original_namespace.items()
                        if name.startswith('__')}
            vars(recovery).clear()
            vars(recovery).update(metadata)
            exec(compile(baseline, recovery.__file__, 'exec', dont_inherit=True), vars(recovery))
            assert not hasattr(recovery, 'ConnectionRecovery')
            assert not hasattr(recovery, 'start')
            assert getattr(runtime, '_connection_recovery_service', None) is None
            update.apply(runtime)
            manager = runtime._connection_recovery_service
            for name in ('__init__', 'run', 'close'):
                method = getattr(recovery.ConnectionRecovery, name)
                assert method.__globals__ is vars(recovery), 'Timer method uses detached globals: ' + name
            update.apply(runtime)
            assert runtime._connection_recovery_service is manager
            assert manager.thread.is_alive()
            assert runtime.servers is identity[0]
            assert runtime.connection_ids is identity[1]
            assert runtime.server is identity[2]
            assert dispatch.server.proc is identity[3]
            # A later code replacement must be visible to the existing timer.
            # The probe is added to the maintained module, never the fresh copy.
            tick = recovery.tick
            saved_tick = tick.__code__, tick.__defaults__, tick.__kwdefaults__
            probe = threading.Event()
            recovery._studio_tick_probe = probe

            def later_tick(runtime, agents):
                _studio_tick_probe.set()

            try:
                tick.__code__, tick.__defaults__, tick.__kwdefaults__ = later_tick.__code__, None, None
                manager.interval = .01
                assert probe.wait(3), 'The existing timer did not call the later patched maintained tick'
            finally:
                tick.__code__, tick.__defaults__, tick.__kwdefaults__ = saved_tick
            assert not getattr(runtime, '_connection_recovery_error', None)
            print(json.dumps({'status': 'pass', 'case': 'cold-recovery',
                              'maintainedTimerGlobals': True, 'laterTickObserved': True}))
        finally:
            if manager is not None:
                manager.close()
            vars(recovery).clear()
            vars(recovery).update(original_namespace)
            close_fixture(runtime, dispatch)


class SendWorkerLiveUpdate(unittest.TestCase):
    def run_case(self, name):
        result = subprocess.run([sys.executable, '-B', str(Path(__file__).resolve()), '--child', name],
                                cwd=ROOT, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        value = json.loads(result.stdout.strip().splitlines()[-1])
        self.assertEqual(value['status'], 'pass')
        self.assertEqual(value['case'], name)

    def test_existing_dispatch_frame_retries_without_replacing_connection(self):
        self.run_case('active-frame')

    def test_unknown_signature_rejects_before_any_mutation(self):
        self.run_case('unknown-signature')

    def test_cold_recovery_timer_uses_maintained_module_globals(self):
        self.run_case('cold-recovery')


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--child':
        {'active-frame': active_frame_case, 'unknown-signature': unknown_signature_case,
         'cold-recovery': cold_recovery_case}[sys.argv[2]]()
    else:
        unittest.main()
