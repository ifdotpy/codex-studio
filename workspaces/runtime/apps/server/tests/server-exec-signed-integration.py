#!/usr/bin/env python3
"""Lead server commands through two signed runtimes and real supervisors."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('signed_exec_fixture', Path(__file__).with_name('multi-server-signed-integration.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
eventually = fixture.fixture.f.eventually
from codex_process_supervisor import status
from codex_server_exec import job_path
from codex_process_supervisor import process_start_time, process_start_matches


class Commands(unittest.TestCase):
    tool = fixture.SignedIntegration.tool
    drain = fixture.SignedIntegration.drain
    drive = fixture.SignedIntegration.drive
    exchange = fixture.SignedIntegration.exchange
    events = fixture.SignedIntegration.events
    def setUp(self):
        if os.name != 'nt':
            short_root = patch('tempfile.tempdir', '/tmp')
            short_root.start()
            self.addCleanup(short_root.stop)
        fixture.SignedIntegration.setUp(self)
        invitation = self.a.local({'action': 'create_invite', 'requestId': 'exec-invite'})['invitation']
        self.b.local({'action': 'accept_invite', 'invitation': invitation, 'requestId': 'exec-accept'})
        self.supervisors = []
        self.addCleanup(self.stop_supervisors)
        for endpoint in (self.a, self.b):
            process = subprocess.Popen([sys.executable, '-B', str(SERVER_SOURCE_ROOT / "codex_process_supervisor.py"),
                                        '--state', str(endpoint.state)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.supervisors.append(process)
            deadline = time.monotonic() + 5
            while True:
                try:
                    status(endpoint.state)
                    break
                except (OSError, ConnectionError):
                    if time.monotonic() > deadline:
                        self.fail('The isolated supervisor did not start')
                    time.sleep(.02)
        self.lead = self.a.runtime.new_lead({'cwd': str(self.a.folder)})
        self.lead = self.a.runtime.prepare(self.lead)
        self.sequence = 0

    def stop_supervisors(self):
        for endpoint in (self.a, self.b):
            service = endpoint.runtime.__dict__.get('_cross_server_service')
            if service and service._command_service:
                manager = service.commands()
                with endpoint.runtime.read_db() as db:
                    jobs = db.execute('SELECT id,owner,actor FROM runtime_server_exec').fetchall()
                for job in jobs:
                    try:
                        manager.control(job['owner'], 'exec_cancel', {'actor': job['actor'], 'handle': job['id']}, 'fixture-cancel:' + job['id'])
                    except Exception:
                        pass
                endpoint.runtime.close()
        for process in self.supervisors:
            process.terminate()
            process.wait(timeout=5)

    def call(self, action, key=None, **arguments):
        self.sequence += 1
        key = key or 'exec-tool-' + str(self.sequence)
        value = self.tool(self.a, self.lead, 'orchestration_servers',
                          {'action': action, **arguments, 'request_id': key}, key)
        if value.get('outputRef'):
            text, offset = '', 0
            while True:
                self.sequence += 1
                page = self.tool(self.a, self.lead, 'orchestration_read',
                    {'output_ref': value['outputRef'], 'offset': offset}, 'read-saved-' + str(self.sequence))
                text += page['text']
                offset = page['nextOffset']
                if offset is None:
                    return json.JSONDecoder().raw_decode(text)[0]
        return value

    def start(self, command, key='start-command', **arguments):
        return self.call('exec', key, server=self.b.server_id, cwd=str(self.b.folder), command=command,
                         **arguments)

    def read(self, handle, **arguments):
        return self.call('exec_read', server=self.b.server_id, handle=handle, **arguments)['value']

    def finish(self, handle, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = self.read(handle)
            if value['status'] not in {'starting', 'running'}:
                self.drain()
                return value
            time.sleep(.03)
        self.fail('The isolated command did not finish')

    def test_local_remote_exec_env_shell_retry_and_audit(self):
        args = {'cwd': str(self.a.folder), 'command': [sys.executable, '-c',
                'import os,sys;print(os.environ["STUDIO_EXEC_TEST"]);print("error",file=sys.stderr)'],
                'env': {'STUDIO_EXEC_TEST': 'a value'}, 'timeout': 3}
        local = self.call('exec', 'local-command', **args)
        self.assertEqual(local['outcome'], 'applied')
        self.assertEqual(local['value']['serverId'], self.a.server_id)
        self.assertEqual(local['value']['stdout'], 'a value\n')
        self.assertEqual(local['value']['stderr'], 'error\n')
        self.assertEqual(local['value']['exitCode'], 0)
        self.assertEqual(self.call('exec', 'local-command', **args), local)
        marker = self.b.folder / 'command-count'
        command = [sys.executable, '-c', 'from pathlib import Path;p=Path("command-count");p.write_text(p.read_text()+"x" if p.exists() else "x");print("remote")']
        remote = self.start(command, timeout=3)
        self.assertEqual(remote['value']['stdout'], 'remote\n')
        self.assertEqual(self.start(command, timeout=3), remote)
        self.assertEqual(marker.read_text(), 'x')
        shell = self.start('printf "%s" "$STUDIO_EXEC_TEST"', 'shell-command', timeout=3,
                           env={'STUDIO_EXEC_TEST': 'quote " and spaces'})
        self.assertEqual(shell['value']['stdout'], 'quote " and spaces')
        with self.b.runtime.read_db() as db:
            row = dict(db.execute('SELECT * FROM runtime_server_exec_audit WHERE id=?', (remote['value']['handle'],)).fetchone())
        self.assertEqual(row['actor'], self.lead['id'])
        self.assertEqual(row['source_server'], self.a.server_id)
        self.assertEqual(row['server'], self.b.server_id)
        self.assertEqual(len(row['argv_hash']), 64)
        self.assertIsNotNone(row['end'])
        self.assertEqual(row['exit_code'], 0)
        self.assertNotIn('remote', json.dumps(row))

    def test_timeout_kills_the_command_group(self):
        response = self.start([sys.executable, '-u', '-c',
            'import subprocess,sys,time;p=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]);print(p.pid,flush=True);time.sleep(60)'], timeout=15)
        value = response['value']
        handle = value['handle']
        deadline = time.monotonic() + 12
        while value['status'] in {'starting', 'running'} and not value.get('stdout'):
            self.assertLess(time.monotonic(), deadline, value)
            time.sleep(.1)
            value = self.read(handle)
        if value['status'] in {'starting', 'running'}:
            value = self.finish(handle, timeout=20)
        self.assertTrue(value['timedOut'], value)
        self.assertEqual(value['signal'], signal.SIGKILL)
        self.assertLess(value['duration'], 20)
        pid = int(value['stdout'].strip())
        deadline = time.monotonic() + 2
        while True:
            state = subprocess.run(['ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True).stdout.strip()
            if not state or state.startswith('Z'):
                break
            self.assertLess(time.monotonic(), deadline, state)
            time.sleep(.02)

    def test_output_keeps_head_and_tail_and_stays_bounded(self):
        response = self.start([sys.executable, '-c', 'import sys;sys.stdout.write("HEAD"+"x"*20000+"TAIL");sys.stderr.write("ERROR"+"y"*20000+"END")'],
                              timeout=3, output_limit=64)
        value = response['value']
        head = value['stdout']
        tail = self.read(value['handle'], stdout_offset=value['stdoutNextOffset'])
        self.assertTrue(head.startswith('HEAD'))
        self.assertTrue(tail['stdout'].endswith('TAIL'))
        self.assertTrue(value['stdoutTruncated'])
        self.assertTrue(value['stderrTruncated'])
        self.assertGreater(tail['stdoutGapBytes'], 19000)
        snapshot = job_path(self.b.state / 'server-command-output', value['handle'], '.output.json')
        self.assertLess(snapshot.stat().st_size, 2048)
        self.assertEqual(snapshot.stat().st_mode & 0o777, 0o600)

    def test_long_handle_stream_input_cancel_and_one_event(self):
        response = self.start([sys.executable, '-u', '-c', 'import sys,time;print("ready");print(sys.stdin.readline().strip());time.sleep(60)'], timeout=120)
        handle = response['value']['handle']
        self.assertIn(response['value']['status'], {'starting', 'running'})
        deadline = time.monotonic() + 5
        while 'ready' not in self.read(handle)['stdout']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.03)
        args = {'server': self.b.server_id, 'handle': handle, 'input': 'once\n'}
        first = self.call('exec_input', 'send-once', **args)
        self.assertEqual(self.call('exec_input', 'send-once', **args), first)
        deadline = time.monotonic() + 5
        while 'once' not in self.read(handle)['stdout']:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.03)
        self.call('exec_cancel', 'cancel-once', server=self.b.server_id, handle=handle)
        result = self.finish(handle)
        self.assertEqual(result['status'], 'cancelled')
        self.assertEqual(result['stdout'].count('once'), 1)
        self.drain()
        self.drive(lambda: bool(self.events(self.a, self.lead['id'], kind='monitor_exit')))
        notices = self.events(self.a, self.lead['id'], kind='monitor_exit')
        self.assertEqual(len(notices), 1)
        self.assertEqual(json.loads(notices[0]['text'])['id'], handle)

    def test_restart_reattaches_without_repeating_command(self):
        marker = self.b.folder / 'restart-count'
        response = self.start([sys.executable, '-u', '-c',
            'from pathlib import Path;import time;p=Path("restart-count");p.write_text(p.read_text()+"x" if p.exists() else "x");print("before");time.sleep(3);print("after")'])
        handle = response['value']['handle']
        deadline = time.monotonic() + 5
        while not marker.exists():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        before = status(self.b.state)['handles'][0]['pid']
        from codex_process_supervisor import process_launch_command
        config = json.loads(job_path(self.b.state / 'server-command-output', handle, '.config.json').read_text())
        self.assertEqual(process_launch_command(before), config['adapter'])
        self.b.restart()
        self.assertEqual(status(self.b.state)['handles'][0]['pid'], before)
        value = self.finish(handle)
        self.assertEqual(value['exitCode'], 0, (value, (self.b.state / 'orchestration-errors.log').read_text() if (self.b.state / 'orchestration-errors.log').exists() else ''))
        self.assertIn('before', value['stdout'])
        self.assertIn('after', value['stdout'])
        self.assertEqual(marker.read_text(), 'x')
        with self.b.runtime.read_db() as db:
            count = db.execute('SELECT count(*) FROM runtime_server_exec').fetchone()[0]
        self.assertEqual(count, 1)
        self.assertGreater(before, 0)

    def test_missing_supervisor_refuses_start(self):
        self.supervisors[1].terminate()
        self.supervisors[1].wait(timeout=5)
        response = self.start([sys.executable, '-c', 'raise SystemExit(0)'], timeout=3)
        self.assertEqual(response['outcome'], 'not_applied')
        self.assertIn('supervisor is unavailable', response['error'])

    def test_lost_reply_uses_the_saved_target_receipt(self):
        self.drop_action = 'exec'
        command = [sys.executable, '-c', 'from pathlib import Path;p=Path("lost-count");p.write_text(p.read_text()+"x" if p.exists() else "x")']
        response = self.start(command, timeout=3)
        self.assertEqual(response['outcome'], 'unknown')
        key = response['requestId']
        recovered = self.a.runtime.multi_server().deliver(key)
        self.assertEqual(recovered['outcome'], 'applied')
        self.assertEqual((self.b.folder / 'lost-count').read_text(), 'x')
        requests = [json.loads(r['raw']) for r in self.wire if r['target'] == fixture.ROUTE and json.loads(r['raw'])['action'] == 'exec']
        self.assertEqual([r['requestId'] for r in requests], [key])
        probes = [json.loads(r['raw']) for r in self.wire if r['target'] == fixture.ROUTE and json.loads(r['raw'])['action'] == 'exec_receipt']
        self.assertEqual(len(probes), 1)
        self.assertEqual(probes[0]['payload']['handle'], key)

    def test_missing_start_evidence_after_crash_never_replays(self):
        manager = self.b.runtime.multi_server().commands()
        attach = manager._attach
        def interrupted(*args, **kwargs):
            attach(*args, **kwargs)
            raise RuntimeError('fixture crash after supervisor open')
        marker = self.b.folder / 'crash-count'
        with patch.object(manager, '_watch'), patch.object(manager, '_attach', side_effect=interrupted):
            response = self.start([sys.executable, '-c', 'from pathlib import Path;p=Path("crash-count");p.write_text(p.read_text()+"x" if p.exists() else "x")'])
        key = response['requestId']
        self.assertEqual(response['outcome'], 'unknown')
        output = job_path(manager.folder, key, '.output.json')
        deadline = time.monotonic() + 5
        while not output.exists() or json.loads(output.read_text())['record']['status'] != 'completed':
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.02)
        output.unlink()
        self.assertEqual(self.a.runtime.multi_server().deliver(key)['outcome'], 'unknown')
        self.b.restart()
        self.assertEqual(self.a.runtime.multi_server().deliver(key)['outcome'], 'unknown')
        self.b.restart()
        self.assertEqual(self.a.runtime.multi_server().deliver(key)['outcome'], 'unknown')
        self.assertEqual(marker.read_text(), 'x')
        with self.b.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_server_exec').fetchone()[0], 1)
            self.assertEqual(json.loads(db.execute('SELECT record FROM runtime_server_exec').fetchone()[0])['status'], 'unknown')

    def test_nonlead_and_other_lead_cannot_control_the_handle(self):
        child = self.a.runtime.new_lead({'cwd': str(self.a.folder)})
        child['isLead'] = False
        with self.a.runtime.db() as db:
            self.a.runtime.put(db, 'agents', child)
        child = self.a.runtime.prepare(child)
        key = 'worker-command'
        self.a.runtime.dynamic({'id': key, 'params': {'threadId': child['threadId'], 'callId': key,
            'tool': 'orchestration_servers', 'arguments': {'action': 'exec', 'cwd': str(self.a.folder),
            'command': ['false'], 'request_id': key}}})
        eventually(lambda: any(row['id'] == key for row in self.a.runtime.server.responses))
        response = next(row for row in self.a.runtime.server.responses if row['id'] == key)['result']
        self.assertFalse(response['success'])
        with self.assertRaisesRegex(PermissionError, 'active lead'):
            self.a.runtime.multi_server().tools({**child, 'isLead': True}, {'action': 'exec',
                'cwd': str(self.a.folder), 'command': ['false']}, 'forged')
        handle = self.start(['true'], timeout=3)['value']['handle']
        other = self.a.runtime.prepare(self.a.runtime.new_lead({'cwd': str(self.a.folder)}))
        with self.assertRaises(PermissionError):
            self.a.runtime.multi_server().tools(other, {'action': 'exec_read', 'server': self.b.server_id, 'handle': handle}, 'foreign')

    def test_large_output_uses_bounded_pages_and_prunes_private_files(self):
        response = self.start([sys.executable, '-c', 'import sys;sys.stdout.buffer.write(b"x"*5000000);sys.stderr.buffer.write(b"y"*5000000)'],
                              timeout=3, output_limit=4 * 1024 * 1024)
        self.assertEqual(response['outcome'], 'applied', response)
        value = response['value']
        self.assertLessEqual(len(value['stdout'].encode()) + len(value['stderr'].encode()), 65536)
        tail = self.read(value['handle'], stdout_offset=4_999_990, stderr_offset=4_999_990)
        self.assertEqual(tail['stdout'], 'x' * 10)
        self.assertEqual(tail['stderr'], 'y' * 10)
        self.assertTrue(tail['stdoutTruncated'])
        manager = self.b.runtime.multi_server().commands()
        output = job_path(manager.folder, value['handle'], '.output.json')
        self.assertLess(output.stat().st_size, 2 * 4 * 1024 * 1024 + 2048)
        # Wait until the observer saves the terminal record and detaches.
        eventually(lambda: value['handle'] not in manager.active)
        manager.prune(time.time() + 8 * 86400)
        self.assertFalse(output.exists())
        with self.b.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_server_exec_audit').fetchone()[0], 1)

    def test_close_stdin_without_text_and_nonzero_exit(self):
        response = self.start([sys.executable, '-u', '-c', 'import sys;print("EOF:"+sys.stdin.read());sys.exit(7)'])
        handle = response['value']['handle']
        self.call('exec_input', 'close-only', server=self.b.server_id, handle=handle, close_stdin=True)
        value = self.finish(handle)
        self.assertEqual(value['stdout'], 'EOF:\n')
        self.assertEqual(value['exitCode'], 7)
        self.assertIsNone(value['signal'])

    def test_input_backpressure_is_bounded_and_cancel_still_works(self):
        response = self.start([sys.executable, '-c', 'import time;time.sleep(60)'])
        handle = response['value']['handle']
        for number in range(32):
            value = self.call('exec_input', 'full-input-' + str(number), server=self.b.server_id,
                              handle=handle, input='x' * 65536)
            self.assertEqual(value['outcome'], 'applied')
        refused = self.call('exec_input', 'overflow-input', server=self.b.server_id, handle=handle, input='x')
        self.assertEqual(refused['outcome'], 'not_applied')
        self.assertIn('32 input requests', refused['error'])
        self.call('exec_cancel', 'cancel-full-input', server=self.b.server_id, handle=handle)
        self.assertEqual(self.finish(handle)['status'], 'cancelled')

    def test_rejects_invalid_limits_and_oversized_encoded_request_before_acceptance(self):
        service = self.a.runtime.multi_server()
        for arguments in ({'cwd': 'relative'}, {'timeout': 1801}, {'timeout': True},
                          {'output_limit': 4194305}, {'env': {'BAD-NAME': 'x'}},
                          {'command': ['true', '\x01' * 128_000]}):
            with self.subTest(arguments=list(arguments)):
                with self.assertRaises(ValueError):
                    service.tools(self.lead, {'action': 'exec', 'cwd': str(self.a.folder),
                        'command': ['true'], **arguments}, 'invalid-command')
        with self.a.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_server_outbox').fetchone()[0], 0)

    def test_review_receipt_is_private_to_the_command_lead(self):
        response = self.start([sys.executable, '-c', 'print("private-command-output")'], timeout=3)
        other = self.a.runtime.prepare(self.a.runtime.new_lead({'cwd': str(self.a.folder)}))
        with self.assertRaises(PermissionError):
            self.a.runtime.multi_server().tools(other, {'action': 'receipt', 'server': self.b.server_id,
                'request_id': response['requestId']}, 'foreign-receipt')
        receipt = self.a.runtime.request_action(self.lead['id'], {'action': 'get', 'request_id': 'start-command'})
        with self.assertRaises(ValueError):
            self.a.runtime.model_read(other['id'], {'output_ref': receipt['id']})
        self.drive(lambda: bool(self.events(self.a, self.lead['id'], kind='monitor_exit')))
        notice = self.events(self.a, self.lead['id'], kind='monitor_exit')[0]
        with self.assertRaises(ValueError):
            self.a.runtime.model_read(other['id'], {'output_ref': 'event:' + notice['id']})
        local = self.call('exec', 'local-receipt-command', cwd=str(self.a.folder), command=['true'], timeout=3)
        self.assertEqual(self.call('receipt', local['requestId'])['result']['value']['exitCode'], 0)

    def test_review_new_sessions_cannot_survive_exec(self):
        for mode in ('normal', 'cancel', 'timeout'):
            with self.subTest(mode=mode):
                marker = self.b.folder / ('escaped-' + mode)
                child = 'import os,time;from pathlib import Path;Path(' + repr(str(marker)) + ').write_text(str(os.getpid()));time.sleep(60)'
                parent = ('import subprocess,sys,time;from pathlib import Path;'
                    'subprocess.Popen([sys.executable,"-c",' + repr(child) + '],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);'
                    '\nwhile not Path(' + repr(str(marker)) + ').exists(): time.sleep(.01)\n' +
                    ('time.sleep(60)' if mode != 'normal' else ''))
                response = self.start([sys.executable, '-c', parent], 'escaped-' + mode,
                                      timeout=1 if mode == 'timeout' else 120)
                eventually(lambda: marker.exists() and marker.read_text().strip())
                pid = int(marker.read_text())
                began = process_start_time(pid)
                def clean(pid=pid, began=began):
                    if began and process_start_matches(pid, began):
                        os.kill(pid, signal.SIGKILL)
                self.addCleanup(clean)
                handle = response['value']['handle']
                if mode == 'cancel':
                    self.call('exec_cancel', 'cancel-escaped', server=self.b.server_id, handle=handle)
                value = response['value'] if mode == 'timeout' else self.finish(handle)
                self.assertIsNone(process_start_time(pid), (mode, pid, value))
                self.assertGreaterEqual(value['stoppedDescendants'], 1)

    def test_review_completed_adapters_release_supervisor_rows(self):
        for number in range(10):
            handle = self.start(['true'], 'retire-' + str(number), timeout=3)['value']['handle']
            eventually(lambda: handle not in self.b.runtime.multi_server().commands().active)
        import sqlite3
        with sqlite3.connect(self.b.state / 'supervisor.sqlite3') as db:
            for table in ('handles', 'operations', 'events', 'child_identities'):
                self.assertEqual(db.execute('SELECT count(*) FROM ' + table).fetchone()[0], 0, table)

    def test_review_config_crash_and_delivered_envelopes_do_not_keep_env(self):
        import codex_server_exec
        write = codex_server_exec.atomic_json
        def interrupted(path, value):
            write(path, value)
            if path.suffix == '.json' and path.name.endswith('.config.json'):
                raise RuntimeError('fixture crash after private config')
        with patch('codex_server_exec.atomic_json', side_effect=interrupted):
            response = self.start(['true'], 'config-crash', env={'PRIVATE_EXEC_VALUE': 'secret-fixture-env'})
        self.assertEqual(response['outcome'], 'unknown')
        with self.b.runtime.read_db() as db:
            self.assertIsNotNone(db.execute('SELECT 1 FROM runtime_server_exec WHERE id=?', (response['requestId'],)).fetchone())
        with self.a.runtime.read_db() as db:
            body = db.execute('SELECT body FROM runtime_server_outbox WHERE id=?', (response['requestId'],)).fetchone()[0]
        self.assertNotIn('secret-fixture-env', body)
        self.assertEqual(self.a.runtime.multi_server().deliver(response['requestId'])['outcome'], 'unknown')

    def test_review_output_pages_expire_with_the_snapshot(self):
        self.drop_action = 'exec'
        initial = self.start([sys.executable, '-c', 'print("retained-fixture-output")'], timeout=3)
        self.assertEqual(initial['outcome'], 'unknown')
        handle = self.a.runtime.multi_server().deliver(initial['requestId'])['value']['handle']
        with patch('time.time', return_value=time.time() + 6 * 86400):
            page = self.call('exec_read', 'retained-page', server=self.b.server_id, handle=handle)
        native = self.a.runtime.request_action(self.lead['id'], {'action': 'get', 'request_id': 'retained-page'})
        copied = self.a.runtime.model_read(self.lead['id'], {'output_ref': native['id']})
        self.assertIn('retained-fixture-output', copied['text'])
        self.tool(self.a, self.lead, 'orchestration_read', {'output_ref': native['id']}, 'copied-page')
        future = time.time() + 8 * 86400
        with patch('time.time', return_value=future):
            saved = self.a.runtime.model_read(self.lead['id'], {'output_ref': native['id']})
            self.assertNotIn('retained-fixture-output', json.dumps(saved))
            self.assertNotIn('retained-fixture-output', json.dumps(self.a.runtime.request_action(
                self.lead['id'], {'action': 'get', 'request_id': 'retained-page'})))
            self.a.runtime.multi_server().prune()
            self.b.runtime.multi_server().prune()
            receipt = self.call('receipt', page['requestId'], server=self.b.server_id)
        self.assertNotIn('retained-fixture-output', json.dumps(receipt))
        for endpoint in (self.a, self.b):
            with endpoint.runtime.read_db() as db:
                results = db.execute('SELECT result FROM runtime_server_' + ('outbox' if endpoint == self.a else 'inbox')).fetchall()
            self.assertNotIn('retained-fixture-output', json.dumps([r[0] for r in results]))
        with self.a.runtime.read_db() as db:
            for alias in ('retained-page', 'copied-page'):
                key = db.execute('SELECT request FROM runtime_tool_request_aliases WHERE agent=? AND alias=?',
                                 (self.lead['id'], alias)).fetchone()[0]
                self.assertNotIn('retained-fixture-output', db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (key,)).fetchone()[0])

    def test_review_normal_exit_stops_descendants_that_hold_output_pipes(self):
        command = 'import subprocess,sys;subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"],start_new_session=True)'
        value = self.start([sys.executable, '-c', command], 'pipe-descendant', timeout=3)['value']
        self.assertFalse(value['timedOut'])
        self.assertLess(value['duration'], 3)
        self.assertGreaterEqual(value['stoppedDescendants'], 1)

    def test_review_binary_tail_does_not_stall_the_cursor(self):
        from codex_server_exec import Buffer
        buffer = Buffer(100)
        buffer.append(b'abc\xff\xe2')
        self.assertEqual(buffer.read(0, 100)['nextOffset'], 5)

    def test_review_utf8_page_cursors_preserve_valid_characters(self):
        handle = self.start([sys.executable, '-c', 'print("€"*30000,end="")'], timeout=3)['value']['handle']
        text, offset = '', 0
        while offset < 90000:
            value = self.read(handle, stdout_offset=offset)
            self.assertGreater(value['stdoutNextOffset'], offset)
            offset = value['stdoutNextOffset']
            text += value['stdout']
        self.assertEqual(text, '€' * 30000)

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS marker proof')
    def test_second_review_marker_cannot_prove_an_unrelated_process(self):
        from codex_server_exec import Descendants
        accepted = time.time()
        foreign = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(60)'],
                                   env={**os.environ, 'STUDIO_EXEC_ID': 'public-handle'})
        def close_foreign():
            if foreign.poll() is None:
                foreign.kill()
            foreign.wait(timeout=3)
        self.addCleanup(close_foreign)
        tree = Descendants('public-handle', accepted)
        self.addCleanup(tree.close)
        tree.marked_processes()
        self.assertNotIn(foreign.pid, tree.known)
        response = self.start([sys.executable, '-c', 'print("ok")'], timeout=3)
        config = json.loads(job_path(self.b.state / 'server-command-output', response['value']['handle'], '.config.json').read_text())
        secret = config['markerSecret']
        self.assertGreaterEqual(len(bytes.fromhex(secret)), 16)
        self.assertNotEqual(secret, response['value']['handle'])
        self.assertNotIn(secret, json.dumps(response))
        self.assertNotIn(secret, json.dumps(self.events(self.a, self.lead['id'], kind='monitor_exit')))

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS lost fork proof')
    def test_second_review_markerless_double_fork_reports_unknown_cleanup(self):
        marker = self.b.folder / 'markerless-child'
        child = 'import os,time;from pathlib import Path;Path(' + repr(str(marker)) + ').write_text(str(os.getpid()));time.sleep(60)'
        script = ('import os,time;'
            '\nif os.fork()==0:\n'
            ' if os.fork()==0:\n'
            '  os.environ.pop("STUDIO_EXEC_ID",None);os.setsid();os.execvpe(' + repr(sys.executable) + ','
                + repr([sys.executable, '-c', child]) + ',os.environ)\n'
            ' os._exit(0)\n'
            'time.sleep(.7)')
        response = self.start([sys.executable, '-c', script], 'lost-double-fork', timeout=3)
        eventually(marker.exists)
        pid = int(marker.read_text())
        began = process_start_time(pid)
        def cleanup():
            if began and process_start_matches(pid, began):
                os.kill(pid, signal.SIGKILL)
        self.addCleanup(cleanup)
        value = response['value']
        if process_start_time(pid) is not None:
            self.assertGreater(value['cleanupUnknownForks'], 0, value)
            self.assertEqual(value['status'], 'unknown')

    def test_second_review_bootstrap_obeys_timeout_and_cancel(self):
        startup = self.b.folder / 'startup'
        startup.mkdir()
        for mode in ('timeout', 'cancel'):
            with self.subTest(mode=mode):
                marker = self.b.folder / ('bootstrap-' + mode)
                (startup / 'sitecustomize.py').write_text('import os,time\nfrom pathlib import Path\nPath(' + repr(str(marker)) + ').write_text(str(os.getpid()))\ntime.sleep(60)\n')
                response = self.start(['true'], 'bootstrap-' + mode, timeout=1 if mode == 'timeout' else 120,
                                      env={'PYTHONPATH': str(startup)})
                eventually(marker.exists)
                pid = int(marker.read_text())
                began = process_start_time(pid)
                def cleanup(pid=pid, began=began):
                    if began and process_start_matches(pid, began):
                        os.kill(pid, signal.SIGKILL)
                self.addCleanup(cleanup)
                if mode == 'cancel':
                    self.call('exec_cancel', 'cancel-bootstrap', server=self.b.server_id, handle=response['value']['handle'])
                deadline = time.monotonic() + 2
                while process_start_time(pid) is not None and time.monotonic() < deadline:
                    time.sleep(.02)
                self.assertIsNone(process_start_time(pid), mode)
                value = self.finish(response['value']['handle'])
                self.assertEqual(value['status'], 'cancelled' if mode == 'cancel' else 'completed')
                self.assertEqual(value['timedOut'], mode == 'timeout')

    def test_second_review_detached_completion_retires_the_unread_journal(self):
        response = self.start([sys.executable, '-u', '-c', 'import time;print("before");time.sleep(1);print("after")'],
                              'detached-completion', timeout=120)
        handle = response['value']['handle']
        manager = self.b.runtime.multi_server().commands()
        manager.close()
        output = job_path(manager.folder, handle, '.output.json')
        eventually(lambda: output.exists() and json.loads(output.read_text())['record']['status'] == 'completed')
        self.b.runtime.multi_server()._command_service = None
        restored = self.b.runtime.multi_server().commands()
        restored.prune(time.time())
        import sqlite3
        with sqlite3.connect(self.b.state / 'supervisor.sqlite3') as db:
            self.assertEqual(db.execute('SELECT count(*) FROM handles').fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT count(*) FROM events').fetchone()[0], 0)
            self.assertEqual(db.execute('SELECT count(*) FROM child_identities').fetchone()[0], 0)
        self.assertEqual(self.read(handle)['stdout'], 'before\nafter\n')

    def test_second_review_live_utf8_waits_for_the_remaining_bytes(self):
        response = self.start([sys.executable, '-u', '-c',
            'import os,sys;os.write(1,b"\\xe2");sys.stdin.readline();os.write(1,b"\\x82\\xac")'],
            'live-utf8', timeout=120)
        handle = response['value']['handle']
        snapshot = job_path(self.b.state / 'server-command-output', handle, '.output.json')
        eventually(lambda: snapshot.exists() and json.loads(snapshot.read_text())['stdout']['total'] == 1)
        page = self.read(handle)
        self.assertEqual(page['stdout'], '')
        self.assertEqual(page['stdoutNextOffset'], 0)
        self.call('exec_input', 'release-live-utf8', server=self.b.server_id, handle=handle, input='go\n')
        value = self.finish(handle)
        self.assertEqual(value['stdout'], '€')
        self.assertEqual(value['stdoutNextOffset'], 3)

    def test_second_review_private_marker_is_redacted_across_chunks(self):
        from codex_server_exec import MarkerRedactor
        private = 'fixture-private-marker'
        redactor = MarkerRedactor(private)
        output = redactor.feed(b'before fixture-private-')
        output += redactor.feed(b'marker after', final=True)
        self.assertEqual(output, b'before [private marker] after')
        value = self.start([sys.executable, '-c', 'import os;print(os.environ["STUDIO_EXEC_ID"])'],
                           'print-marker', timeout=3)['value']
        self.assertEqual(value['stdout'], '[private marker]\n')
        config = json.loads(job_path(self.b.state / 'server-command-output', value['handle'], '.config.json').read_text())
        self.assertTrue(config['markerSecret'] not in json.dumps(value))

    def test_second_review_linux_pidfd_pins_the_signal_target(self):
        from codex_server_exec import Descendants
        tree = Descendants('pidfd-fixture', time.time())
        self.addCleanup(tree.close)
        actions = []
        with patch('sys.platform', 'linux'), patch('os.pidfd_open', create=True, side_effect=lambda pid: actions.append(('open', pid)) or 77), \
                patch('codex_server_exec.descendant_matches', side_effect=lambda pid, birth: actions.append(('birth', pid, birth)) or True), \
                patch('signal.pidfd_send_signal', create=True, side_effect=lambda fd, signum: actions.append(('signal', fd, signum))), \
                patch('os.close', side_effect=lambda fd: actions.append(('close', fd))):
            self.assertTrue(tree.signal_process(123, 'old-birth', signal.SIGSTOP))
        self.assertEqual(actions, [('open', 123), ('birth', 123, 'old-birth'), ('signal', 77, signal.SIGSTOP), ('close', 77)])


if __name__ == '__main__':
    unittest.main(verbosity=2)
