"""Restore an exact supervised native child without starting an older binary."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_native_runtime as native
import codex_process_supervisor as supervisor
from codex_native_binary import REQUIRED_COMPANIONS, bundle_digest
from codex_runtime import AppServer, Runtime

spec = importlib.util.spec_from_file_location('retained_supervisor_fixture', ROOT / 'tests/process-supervisor-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Contract(unittest.TestCase):
    def setUp(self):
        self.case = fixture.ProcessSupervisorContract(methodName='runTest')
        self.case.setUp()
        self.addCleanup(self.case.cleanup)
        self.root = self.case.root
        self.home = self.root / 'account-home'
        self.home.mkdir()
        self.account = 'profile-retained'
        self.handle = 'account:' + self.account
        self.old = self.bundle(self.case.binary.read_bytes(), '0.1.0')
        self.new = self.bundle(self.case.binary.read_bytes() + b'\n# newer\n', '0.2.0')
        self.command = [self.old['path'], 'app-server', '--listen', 'stdio://',
                        '-c', 'cli_auth_credentials_store="file"']
        # These tests model a host-launched fake process and separately stub
        # its argv reader. Do not let an installed Linux VM prefix change that
        # modeled launch command.
        self.provider_command_patch = patch('codex_runtime.provider_process_command',
                                            side_effect=lambda command: command)
        self.provider_command_patch.start()
        self.addCleanup(self.provider_command_patch.stop)
        self.first = AppServer(self.root, lambda _: None, lambda _: None, lambda: None,
                               home=self.home, isolated=True, executable=self.old['path'],
                               supervisor_handle=self.handle, supervisor_root=self.root)
        self.case.servers.append(self.first)
        self.pid = int(self.case.pid_file.read_text())
        self.first.close()
        self.rt = SimpleNamespace(root=self.root, lock=threading.RLock(), start_lock=threading.RLock(),
            closed=False, factory=AppServer, servers={}, connection_ids={}, offline_accounts=set(),
            offline=False, _publish_desktop_resource=lambda: None,
            refresh_workspace_volatile=lambda: None,
            accounts=SimpleNamespace(get=lambda _: {'provider': 'codex'}, home=lambda *a, **kw: self.home),
            notification=lambda *a: None, request=lambda *a: None, disconnected=lambda *a: None,
            commit_supervisor_event=lambda *a: None, supervisor_event_applied=lambda *a: False,
            supervisor_reattached=lambda *a: None, supervisor_monitor_bindings=lambda *a: [],
            supervisor_monitor_result=lambda *a: None)
        self.updates = native.NativeRuntimeUpdates(self.rt)
        self.updates.state.update(selected=copy.deepcopy(self.new), status='ready')
        self.updates.maybe_check = lambda: None
        self.rt.native_runtime_updates = self.updates
        # The script fixture has an interpreter prefix in its OS argv. The
        # real argv reader has a separate OS-process contract below.
        self.argv_patch = patch.object(supervisor, 'process_launch_command', return_value=list(self.command), create=True)
        self.argv_patch.start()
        self.addCleanup(self.argv_patch.stop)

    def bundle(self, data, version):
        contents = {'codex': data, **{name: b'fixture companion' for name in REQUIRED_COMPANIONS}}
        hashes = {name: hashlib.sha256(value).hexdigest() for name, value in contents.items()}
        digest = bundle_digest(hashes)
        directory = self.root / 'native-runtime' / 'builds' / digest
        directory.mkdir(parents=True)
        for name, value in contents.items():
            path = directory / name
            path.write_bytes(value)
            path.chmod(0o700)
        return {'path': str(directory / 'codex'), 'sha256': hashes['codex'], 'bundleSha256': digest,
                'version': version, 'checks': {'fixture': True}}

    def connect(self):
        result = Runtime.connect(self.rt, self.account)
        self.case.servers.append(result)
        return result

    def operations(self):
        return [json.loads(line)['method'] for line in (self.root / 'native-ops.jsonl').read_text().splitlines()]

    def test_newer_selection_reattaches_exact_old_child_without_open_or_input_replay(self):
        actions = []
        original = supervisor.ProcessProxy.call
        def call(proxy, action, **values):
            actions.append(action)
            return original(proxy, action, **values)
        with patch.object(supervisor.ProcessProxy, 'call', call):
            server = self.connect()
        self.assertTrue(server.supervisor_resumed)
        self.assertEqual(server.proc.generation, self.first.proc.generation)
        self.assertEqual(int(self.case.pid_file.read_text()), self.pid)
        self.assertEqual(server.native_binary['path'], self.old['path'])
        self.assertEqual(self.updates.selected(), self.new)
        self.assertNotIn('open', actions)
        self.assertIn('status', actions)
        self.assertEqual(self.operations().count('initialize'), 1)
        self.assertNotIn('turn/start', self.operations())
        self.assertEqual(server.call('model/list', {})['data'][0]['model'], 'fake')

    def test_child_exit_after_selection_never_opens_or_starts_a_replacement(self):
        original = native.executable_for
        def select(*args, **kwargs):
            result = original(*args, **kwargs)
            os.kill(self.pid, __import__('signal').SIGKILL)
            fixture.wait_for(lambda: supervisor.process_start_time(self.pid) is None)
            return result
        actions = []
        call_original = supervisor.ProcessProxy.call
        def call(proxy, action, **values):
            actions.append(action)
            return call_original(proxy, action, **values)
        with patch.object(native, 'executable_for', select), patch.object(supervisor.ProcessProxy, 'call', call):
            with self.assertRaisesRegex(RuntimeError, 'retained|existing|changed'):
                self.connect()
        self.assertNotIn('open', actions)
        self.assertEqual(int(self.case.pid_file.read_text()), self.pid)
        self.assertEqual(self.operations().count('initialize'), 1)

    def test_proven_exited_child_uses_newest_for_a_fresh_generation(self):
        os.kill(self.pid, __import__('signal').SIGKILL)
        fixture.wait_for(lambda: supervisor.process_start_time(self.pid) is None)
        server = self.connect()
        self.assertFalse(server.supervisor_resumed)
        self.assertEqual(server.native_binary, self.new)
        self.assertEqual(server.proc.generation, self.first.proc.generation + 1)
        self.assertNotEqual(int(self.case.pid_file.read_text()), self.pid)
        self.assertEqual(self.operations().count('initialize'), 2)

    def test_changed_account_environment_still_rejects_without_open(self):
        self.rt.accounts.home = lambda *a, **kw: self.root / 'different-home'
        with self.assertRaisesRegex(RuntimeError, 'launch settings changed'):
            self.connect()
        self.assertEqual(int(self.case.pid_file.read_text()), self.pid)
        self.assertEqual(self.operations().count('initialize'), 1)

    def test_unverified_process_identity_stays_unknown(self):
        with patch.object(supervisor, 'process_start_matches', return_value=False):
            with self.assertRaisesRegex(RuntimeError, 'Cannot verify'):
                self.connect()
        self.assertEqual(self.updates.selected(), self.new)

    def test_changed_argv_tail_is_rejected(self):
        supervisor.process_launch_command.return_value = [self.old['path'], 'different-command']
        with self.assertRaisesRegex(RuntimeError, 'launch settings changed'):
            self.connect()

    def test_tampered_retained_companion_is_rejected(self):
        Path(self.old['path']).with_name(REQUIRED_COMPANIONS[0]).write_bytes(b'changed')
        with self.assertRaisesRegex(RuntimeError, 'bundle|executable'):
            self.connect()
        self.assertEqual(int(self.case.pid_file.read_text()), self.pid)

    def test_other_account_and_unscoped_selection_keep_newest(self):
        self.assertEqual(native.executable_for(self.rt), self.new)
        self.assertEqual(native.executable_for(self.rt, account_key='fresh-account', home=self.home), self.new)

    def test_unchanged_selection_keeps_existing_script_launch_path(self):
        self.updates.state['selected'] = copy.deepcopy(self.old)
        with patch.object(supervisor, 'process_launch_command', side_effect=AssertionError('Unchanged launch needs no argv migration')):
            server = self.connect()
        self.assertTrue(server.supervisor_resumed)
        self.assertEqual(server.native_binary, self.old)
        self.assertEqual(int(self.case.pid_file.read_text()), self.pid)

    def test_changed_generation_after_status_is_rejected_without_open(self):
        original = supervisor.ProcessProxy.call
        actions = []
        def call(proxy, action, **values):
            actions.append(action)
            result = original(proxy, action, **values)
            if action == 'status':
                with sqlite3.connect(self.root / 'supervisor.sqlite3') as db:
                    db.execute('UPDATE handles SET generation=generation+1 WHERE id=?', (self.handle,))
            return result
        with patch.object(supervisor.ProcessProxy, 'call', call):
            with self.assertRaisesRegex(RuntimeError, 'Cannot verify'):
                self.connect()
        self.assertNotIn('open', actions)
        self.assertEqual(int(self.case.pid_file.read_text()), self.pid)

    def test_missing_initialize_receipt_never_replays_initialize(self):
        with sqlite3.connect(self.root / 'supervisor.sqlite3') as db:
            db.execute('UPDATE handles SET init_result=NULL WHERE id=?', (self.handle,))
        with self.assertRaisesRegex(RuntimeError, 'initialization receipt is unavailable'):
            self.connect()
        self.assertEqual(self.operations().count('initialize'), 1)

    def test_retained_child_has_no_in_memory_owner_is_rejected(self):
        original = supervisor.ProcessProxy.call
        actions = []
        def call(proxy, action, **values):
            actions.append(action)
            if action == 'status':
                raise RuntimeError('Unknown supervisor handle')
            return original(proxy, action, **values)
        with patch.object(supervisor.ProcessProxy, 'call', call):
            with self.assertRaisesRegex(RuntimeError, 'Unknown supervisor handle'):
                self.connect()
        self.assertNotIn('open', actions)

    def test_symlink_companion_is_rejected_without_following_it(self):
        helper = Path(self.old['path']).with_name(REQUIRED_COMPANIONS[0])
        original = helper.read_bytes()
        helper.unlink()
        outside = self.root / 'outside-helper'
        outside.write_bytes(original)
        outside.chmod(0o700)
        helper.symlink_to(outside)
        with self.assertRaisesRegex(RuntimeError, 'invalid executable'):
            self.connect()
        self.assertEqual(int(self.case.pid_file.read_text()), self.pid)

    def test_existing_operation_receipt_and_cursors_survive_reattach(self):
        first = AppServer(self.root, lambda _: None, lambda _: None, lambda: None,
                          home=self.home, isolated=True, executable=self.old['path'],
                          supervisor_handle=self.handle, supervisor_root=self.root)
        self.case.servers.append(first)
        first.wait(first.submit('model/list', {}, operation_id='retained-catalog-once'), timeout=3)
        first.close()
        with sqlite3.connect(self.root / 'supervisor.sqlite3') as db:
            receipt = db.execute('SELECT * FROM operations WHERE handle=? AND operation_id=?',
                                  (self.handle, 'retained-catalog-once')).fetchone()
            acknowledged = db.execute('SELECT acknowledged FROM handles WHERE id=?', (self.handle,)).fetchone()[0]
        server = self.connect()
        self.assertGreaterEqual(server.proc.cursor, acknowledged)
        with sqlite3.connect(self.root / 'supervisor.sqlite3') as db:
            self.assertEqual(db.execute('SELECT * FROM operations WHERE handle=? AND operation_id=?',
                                       (self.handle, 'retained-catalog-once')).fetchone(), receipt)
        self.assertEqual(self.operations().count('model/list'), 1)
        self.assertEqual(self.operations().count('initialize'), 1)


    def test_rejected_attach_closes_the_unowned_log(self):
        from codex_log_rotation import RotatingLog
        logs = []

        def log(*args, **kwargs):
            result = RotatingLog(*args, **kwargs)
            logs.append(result)
            return result

        try:
            with patch('codex_log_rotation.RotatingLog', side_effect=log), \
                 patch.object(supervisor, 'attach', side_effect=RuntimeError('Retained child disappeared')):
                with self.assertRaisesRegex(RuntimeError, 'Retained child disappeared'):
                    AppServer(self.root, lambda _: None, lambda _: None, lambda: None,
                              home=self.home, isolated=True, executable=self.old['path'],
                              supervisor_handle=self.handle, supervisor_root=self.root)
            self.assertEqual(len(logs), 1)
            self.assertTrue(logs[0].stream.closed)
        finally:
            for result in logs:
                result.close()



class OperatorCloseContract(unittest.TestCase):
    def setUp(self):
        self.case = fixture.ProcessSupervisorContract(methodName='runTest')
        self.case.setUp()
        self.addCleanup(self.case.cleanup)
        provider_command_patch = patch('codex_runtime.provider_process_command',
                                       side_effect=lambda command: command)
        provider_command_patch.start()
        self.addCleanup(provider_command_patch.stop)
        self.replacement_model_list = None
        self.replacement_transport_error = None
        if self._testMethodName == 'test_verified_operator_close_allows_a_new_launch_signature':
            self._prepare_verified_operator_close_replacement()

    def close(self, handle):
        row = next(row for row in supervisor.status(self.case.root)['handles'] if row['id'] == handle)
        return supervisor.admin_close_handle(self.case.root, handle, row['pid'],
                                             row['startTime'], row['signature'])

    def test_close_waits_for_the_server_to_kill_a_child_that_ignores_term(self):
        raw = self.case.binary.read_text()
        self.case.binary.write_text(raw.replace('for line in sys.stdin:',
            'import signal\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nfor line in sys.stdin:', 1))
        server = self.case.server(handle='test:slow-operator-close')
        pid = int(self.case.pid_file.read_text())
        result = self.close('test:slow-operator-close')
        self.assertEqual(result, {'closed': True, 'handle': 'test:slow-operator-close', 'pid': pid})
        self.assertIsNone(supervisor.process_start_time(pid))

    def _prepare_verified_operator_close_replacement(self):
        first = self.case.server(handle='test:operator-replace')
        pid = int(self.case.pid_file.read_text())
        self.assertEqual(first.call('model/list', {})['data'][0]['model'], 'fake')

        ack_entered = threading.Event()
        release_ack = threading.Event()
        ack_finished = threading.Event()
        replacement_opened = threading.Event()
        replacement_reader_parked = threading.Event()
        release_replacement_reader = threading.Event()
        replacement_reader_failed = threading.Event()
        self.addCleanup(release_ack.set)
        self.addCleanup(release_replacement_reader.set)
        original_ack = first.proc.ack

        def delayed_ack(sequence):
            ack_entered.set()
            if not release_ack.wait(90):
                raise RuntimeError('fixture did not release the supervisor ACK')
            original_ack(sequence)
            ack_finished.set()

        first.proc.ack = delayed_ack
        pending = first.submit('model/list', {})
        self.assertEqual(first.wait(pending, timeout=30)['data'][0]['model'], 'fake')
        self.assertTrue(ack_entered.wait(30), 'native model/list response was not read for ACK')

        closed = self.close('test:operator-replace')
        self.assertEqual(closed['handle'], 'test:operator-replace')
        other = self.case.root / 'new-native'
        other.write_text(self.case.binary.read_text() + '\n# replacement\n')
        other.chmod(0o700)
        handle = 'test:operator-replace'

        original_next_event = supervisor.ProcessProxy.next_event

        def gate_replacement_reader(proxy):
            is_replacement = (proxy.handle == handle
                              and proxy.generation > first.proc.generation)
            if is_replacement:
                replacement_reader_parked.set()
                if not release_replacement_reader.wait(90):
                    raise RuntimeError('fixture did not release the replacement reader')
            try:
                return original_next_event(proxy)
            except Exception:
                if is_replacement:
                    replacement_reader_failed.set()
                raise

        next_event_patch = patch.object(supervisor.ProcessProxy, 'next_event',
                                         gate_replacement_reader)
        next_event_patch.start()
        self.addCleanup(next_event_patch.stop)

        def open_replacement(stderr_sink):
            proxy = supervisor.attach(self.case.root, handle,
                [str(other), 'app-server', '--listen', 'stdio://'], dict(os.environ),
                stderr_sink=stderr_sink)
            # The fake's known initialize result lets AppServer finish setup
            # while its first reader call is held at the test-controlled gate.
            proxy.initialize_result = {'userAgent': 'fake-model/1.0.0'}
            replacement_opened.set()
            return proxy

        second = AppServer(self.case.root, lambda _: None, lambda _: None, lambda _: None,
                           executable=str(other), supervisor_handle=handle,
                           process_factory=open_replacement)
        self.case.servers.append(second)
        self.assertTrue(replacement_opened.wait(30),
                        'replacement supervisor open did not capture its cursor')
        self.assertTrue(replacement_reader_parked.wait(30),
                        'replacement reader did not reach the controlled replay gate')
        self.assertFalse(second.supervisor_resumed)
        self.assertEqual(second.proc.generation, first.proc.generation + 1)
        pending_replacement_model_list = second.submit('model/list', {})

        # The replacement ProcessProxy has captured its cursor, but its reader
        # has not polled yet. Commit the old proxy's real ACK before that poll.
        release_ack.set()
        self.assertTrue(ack_finished.wait(90), 'real supervisor ACK action did not finish')
        release_replacement_reader.set()
        self.assertTrue(replacement_reader_failed.wait(30),
                        'replacement reader did not surface the stale-cursor response')
        new_pid = fixture.wait_for(lambda: (
            candidate if (candidate := int(self.case.pid_file.read_text())) != pid else None), timeout=30)
        self.assertNotEqual(new_pid, pid)

        try:
            self.replacement_model_list = second.wait(pending_replacement_model_list, timeout=30)
        except RuntimeError as error:
            expected = ('Native provider transport failed; outcome unknown: Supervisor next failed: '
                        'Supervisor replay cursor is stale or ahead of the journal')
            if str(error) != expected:
                raise
            self.replacement_transport_error = str(error)

    @unittest.expectedFailure  # Product defect: stale ACK cursor after operator close.
    def test_verified_operator_close_allows_a_new_launch_signature(self):
        self.assertEqual(self.replacement_model_list, {'data': [{'model': 'fake'}]},
                         self.replacement_transport_error)

    def test_operator_closed_record_never_replaces_a_still_live_child(self):
        first = self.case.server(handle='test:operator-still-live')
        pid = int(self.case.pid_file.read_text())
        with sqlite3.connect(self.case.root / 'supervisor.sqlite3') as db:
            db.execute('UPDATE handles SET closed_at=?,closed_reason=? WHERE id=?',
                       (1, 'Closed by operator after verified test/unknown-client ownership',
                        'test:operator-still-live'))
        isolated = supervisor.Supervisor(self.case.root)
        with self.assertRaisesRegex(RuntimeError, 'native outcome remains unknown'):
            isolated.open_handle('test:operator-still-live', [str(self.case.binary)], dict(os.environ), None)
        self.assertIsNotNone(supervisor.process_start_time(pid))
        self.assertEqual(isolated.children, {})

    def test_operator_close_without_a_start_identity_cannot_start_a_replacement(self):
        self.case.server(handle='test:operator-missing-start')
        self.close('test:operator-missing-start')
        for start in ['', 'not-a-process-start', 'nan', '0.000000']:
            with self.subTest(start=start):
                with sqlite3.connect(self.case.root / 'supervisor.sqlite3') as db:
                    db.execute('UPDATE child_identities SET start_time=? WHERE handle=?',
                               (start, 'test:operator-missing-start'))
                isolated = supervisor.Supervisor(self.case.root)
                with patch.object(supervisor.subprocess, 'Popen') as spawn:
                    with self.assertRaisesRegex(RuntimeError, 'native outcome remains unknown'):
                        isolated.open_handle('test:operator-missing-start', ['different-native'], dict(os.environ), None)
                    spawn.assert_not_called()


class ProcessArgumentsContract(unittest.TestCase):
    def test_actual_os_arguments_are_read_without_a_probe_process(self):
        command = ['/bin/sleep', '30']
        child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                 stderr=subprocess.DEVNULL)
        try:
            fixture.wait_for(lambda: supervisor.process_start_time(child.pid) is not None)
            self.assertEqual(supervisor.process_launch_command(child.pid), command)
        finally:
            child.terminate()
            child.wait(timeout=3)


if __name__ == '__main__':
    unittest.main()
