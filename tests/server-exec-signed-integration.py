#!/usr/bin/env python3
"""Lead server commands through two signed runtimes and real supervisors."""
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


class Commands(unittest.TestCase):
    tool = fixture.SignedIntegration.tool
    drain = fixture.SignedIntegration.drain
    drive = fixture.SignedIntegration.drive
    exchange = fixture.SignedIntegration.exchange
    events = fixture.SignedIntegration.events
    def setUp(self):
        short_root = patch('tempfile.tempdir', '/tmp')
        short_root.start()
        self.addCleanup(short_root.stop)
        fixture.SignedIntegration.setUp(self)
        invitation = self.a.local({'action': 'create_invite', 'requestId': 'exec-invite'})['invitation']
        self.b.local({'action': 'accept_invite', 'invitation': invitation, 'requestId': 'exec-accept'})
        self.supervisors = []
        self.addCleanup(self.stop_supervisors)
        for endpoint in (self.a, self.b):
            process = subprocess.Popen([sys.executable, '-B', str(Path(__file__).resolve().parents[1] / 'scripts/codex_process_supervisor.py'),
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

    def finish(self, handle):
        deadline = time.monotonic() + 8
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
            'import subprocess,sys,time;p=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]);print(p.pid);time.sleep(60)'], timeout=1)
        value = response['value']
        self.assertTrue(value['timedOut'])
        self.assertEqual(value['signal'], signal.SIGKILL)
        self.assertLess(value['duration'], 4)
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
        self.assertEqual([r['requestId'] for r in requests], [key, key])

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


if __name__ == '__main__':
    unittest.main(verbosity=2)
