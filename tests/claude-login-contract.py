#!/usr/bin/env python3
"""Exercise native process login with a local fake CLI, without OAuth requests."""
import json
import os
from pathlib import Path
import sys
import subprocess
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch
import uuid
import urllib.request
import urllib.error

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import codex_claude
from codex_claude_login import LoginManager, verification_url


class Accounts:
    def __init__(self, profile):
        self.lock = threading.RLock()
        self.profile = profile

    def _row(self, key):
        if key != 'claude-test':
            raise ValueError('Unknown account')
        return self.profile

    def refresh(self, key):
        metadata = codex_claude.auth_metadata(self.profile, force=True)
        return {**metadata, 'status': metadata['status'] if metadata['accountId'] == self.profile['accountId'] else 'changed'}


class LoginTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.config = self.root / 'config'
        self.config.mkdir()
        self.binary = self.root / 'claude'
        self.binary.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys, time, signal
root = pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])
if sys.argv[1:3] == ['auth','status']:
    p = root / 'status.json'
    print(p.read_text() if p.exists() else json.dumps({'loggedIn':False}))
    sys.exit(0)
if (root / 'ignore-term').exists(): signal.signal(signal.SIGTERM, signal.SIG_IGN)
(root / 'native-pid').write_text(str(os.getpid()))
assert sys.argv[1:] == ['auth','login','--claudeai','--email','expected@example.com']
assert os.environ['BROWSER'] == '/usr/bin/false'
assert 'ANTHROPIC_API_KEY' not in os.environ
with (root / 'launches').open('a') as f: f.write('start\\n')
print('https://claude.com/cai/oauth/authorize?state=secret-state&code_challenge=challenge', flush=True)
print('Paste code here if prompted >', flush=True)
code = input()
with (root / 'deliveries').open('a') as f: f.write('received\\n')
time.sleep(.15)
email = 'wrong@example.com' if code == 'wrong' else 'expected@example.com'
(root / 'status.json').write_text(json.dumps({'loggedIn':True,'authMethod':'claude.ai','email':email}))
''')
        self.binary.chmod(0o700)
        self.profile = {'provider':'claude', 'email':'expected@example.com', 'accountId':'claude:expected@example.com',
                        'claudeOptions':{'configDir':str(self.config), 'binaryPath':str(self.binary)}}
        self.rt = SimpleNamespace(root=self.root, accounts=Accounts(self.profile), lock=threading.RLock())
        self.login = LoginManager(self.rt, deadline=3)
        self.ids = []

    def tearDown(self):
        for rid in self.ids:
            self.login.cancel(rid)
        end = time.monotonic() + 4
        while any(job.get('process') and job['process'].poll() is None for job in self.login.jobs.values()) and time.monotonic() < end:
            time.sleep(.02)
        time.sleep(.05)
        self.temp.cleanup()

    def start(self):
        rid = str(uuid.uuid4())
        self.ids.append(rid)
        self.login.start('claude-test', rid)
        return rid

    def await_status(self, rid, states):
        end = time.monotonic() + 5
        while time.monotonic() < end:
            result = self.login.status(rid)
            if result['status'] in states:
                return result
            time.sleep(.02)
        self.fail(str(result))

    def test_success_profile_environment_code_once_and_receipt_replay(self):
        with patch.dict(os.environ, {'ANTHROPIC_API_KEY':'not-allowed'}):
            rid = self.start()
            pending = self.await_status(rid, {'pending'})
            self.assertTrue(verification_url(pending['verificationUrl']))
            self.assertEqual(self.login.start('claude-test', rid), pending)
            self.login.code(rid, 'test-authorization-code')
            self.login.code(rid, 'test-authorization-code')
            receipt = self.await_status(rid, {'ready','error'})
        self.assertEqual(receipt['status'], 'ready', receipt)
        self.assertNotIn('verificationUrl', receipt)
        self.assertEqual((self.config / 'deliveries').read_text(), 'received\n')
        self.assertEqual((self.config / 'launches').read_text(), 'start\n')
        persisted = (self.root / 'claude-logins' / (rid + '.json')).read_text()
        self.assertNotIn('test-authorization-code', persisted)
        self.assertNotIn('secret-state', persisted)
        self.assertEqual(LoginManager(self.rt).start('claude-test', rid), receipt)
        with self.assertRaises(ValueError):
            self.login.start('other', rid)

    def test_wrong_account_is_error_and_saved_identity_unchanged(self):
        rid = self.start()
        self.await_status(rid, {'pending'})
        self.login.code(rid, 'wrong')
        result = self.await_status(rid, {'error','ready'})
        self.assertEqual(result['status'], 'error')
        self.assertIn('different Claude account', result['error'])
        self.assertEqual(self.profile['accountId'], 'claude:expected@example.com')
        self.assertEqual(self.rt.accounts.refresh('claude-test')['status'], 'changed')

    def test_cancel_only_owned_process_and_no_restart_of_receipt(self):
        rid = self.start()
        self.await_status(rid, {'pending'})
        job = self.login.jobs[rid]
        self.assertEqual(self.login.cancel(rid)['status'], 'cancelled')
        job['process'].wait(timeout=3)
        self.assertIsNotNone(job['process'].returncode)
        self.assertEqual(self.login.start('claude-test', rid)['status'], 'cancelled')
        self.assertEqual(self.login.code(rid, 'valid-code')['status'], 'cancelled')

    def test_cancel_stops_native_child_that_ignores_termination(self):
        (self.config / 'ignore-term').touch()
        rid = self.start()
        self.await_status(rid, {'pending'})
        pid = int((self.config / 'native-pid').read_text())
        self.login.cancel(rid)
        end = time.monotonic() + 3
        while time.monotonic() < end:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(.03)
        else:
            self.fail('Native login survived cancellation')
        self.assertEqual(self.login.status(rid)['status'], 'cancelled')

    def test_backend_exit_stops_native_child_and_preserves_receipt(self):
        rid = str(uuid.uuid4())
        script = """
import os, json, threading, time
from pathlib import Path
from types import SimpleNamespace
from codex_claude_login import LoginManager
profile = json.loads(os.environ['FIXTURE_PROFILE'])
rt = SimpleNamespace(root=Path(os.environ['FIXTURE_ROOT']), accounts=SimpleNamespace(
    lock=threading.RLock(), _row=lambda key: profile))
manager = LoginManager(rt, deadline=10)
rid = os.environ['FIXTURE_REQUEST']
manager.start('claude-test',rid)
while manager.status(rid)['status'] == 'starting': time.sleep(.02)
os._exit(0)
"""
        env = {**os.environ, 'PYTHONPATH':str(Path(__file__).resolve().parents[1] / 'scripts'),
               'FIXTURE_PROFILE':json.dumps(self.profile), 'FIXTURE_ROOT':str(self.root), 'FIXTURE_REQUEST':rid}
        subprocess.run([sys.executable,'-B','-c',script],env=env,check=True,timeout=5)
        pid = int((self.config / 'native-pid').read_text())
        end = time.monotonic() + 3
        while time.monotonic() < end:
            try:
                os.kill(pid,0)
            except ProcessLookupError:
                break
            time.sleep(.03)
        else:
            self.fail('Native login survived backend exit')
        self.assertEqual(self.login.status(rid)['status'],'error')
        self.assertIn('restarted',self.login.status(rid)['error'])

    def test_configuration_lock_and_restart_receipt(self):
        rid = self.start()
        self.await_status(rid, {'pending'})
        other = LoginManager(self.rt)
        self.assertEqual(other.status(rid)['status'], 'error')
        with self.assertRaisesRegex(ValueError, 'already active'):
            other.start('claude-test', str(uuid.uuid4()))
        self.assertEqual((self.config / 'launches').read_text(), 'start\n')

    def test_deadline_and_invalid_inputs(self):
        self.login.deadline = .4
        rid = self.start()
        for bad in ['', 'a\nb', 'a\x00b', 'a'*4097, None]:
            with self.assertRaises(ValueError):
                self.login.code(rid, bad)
        with self.assertRaises(ValueError):
            self.login.status('../bad')
        with self.assertRaises(ValueError):
            self.login.status(str(uuid.uuid4()))
        self.assertEqual(self.await_status(rid, {'error'})['status'], 'error')

    def test_http_authentication_routes_and_invalid_body(self):
        from codex_canvas import Canvas, make_server
        canvas = Canvas(self.root)
        canvas.runtime = self.rt
        self.rt._claude_login = self.login
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        origin = f'http://127.0.0.1:{server.server_port}'
        def request(path, body=None, headers=None):
            req = urllib.request.Request(origin + path,
                data=json.dumps(body).encode() if body is not None else None,
                headers={'Content-Type':'application/json', **(headers or {})})
            try:
                with urllib.request.urlopen(req, timeout=3) as response:
                    return response.status, json.loads(response.read())
            except urllib.error.HTTPError as error:
                with error:
                    return error.code, json.loads(error.read())
        try:
            token = request('/api/session')[1]['token']
            headers = {'Origin':origin, 'X-Canvas-Token':token}
            path = '/api/accounts/claude/login'
            rid = str(uuid.uuid4())
            body = {'account_key':'claude-test', 'request_id':rid}
            for route in [path, path + '/code', path + '/cancel']:
                self.assertEqual(request(route, body)[0], 403)
                self.assertEqual(request(route, body, {**headers,'Origin':'https://evil.invalid'})[0],403)
            self.assertEqual(request(path, [], headers)[0], 400)
            self.assertEqual(request(path, {}, headers)[0], 400)
            status, receipt = request(path, body, headers)
            self.ids.append(rid)
            self.assertEqual(status,200)
            self.assertEqual(receipt['accountKey'],'claude-test')
            self.assertEqual(request(path + '?request_id=' + rid)[0],200)
            self.assertEqual(request(path + '/cancel', {'request_id':rid}, headers)[1]['status'],'cancelled')
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_config_change_during_login_does_not_report_success(self):
        rid = self.start()
        self.await_status(rid, {'pending'})
        self.profile['claudeOptions'] = {**self.profile['claudeOptions'], 'configDir':str(self.root / 'another')}
        self.login.code(rid, 'valid-code')
        self.assertEqual(self.await_status(rid, {'ready','error'})['status'],'error')

    def test_url_allowlist(self):
        for bad in ['https://claude.com.evil/cai/oauth/authorize?a=b', 'http://claude.com/cai/oauth/authorize?a=b',
                    'https://user@claude.com/cai/oauth/authorize', 'https://claude.com/other']:
            self.assertFalse(verification_url(bad))


if __name__ == '__main__':
    unittest.main()
