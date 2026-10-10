#!/usr/bin/env python3
"""Exercise native process login with a local fake CLI, without OAuth requests."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import nullcontext
import copy
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

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_claude
from codex_claude_login import LoginManager, verification_url


class Accounts:
    def __init__(self, profile):
        self.lock = threading.RLock()
        self.profile = profile
        self.registered = []
        self.root = Path(profile['claudeOptions']['configDir']).parent / 'accounts'
        self.root.mkdir(parents=True)

    def _row(self, key):
        if key != 'claude-test':
            raise ValueError('Unknown account')
        return self.profile

    def refresh(self, key, verified_metadata=None):
        if verified_metadata is not None:
            return verified_metadata
        metadata = codex_claude.auth_metadata(self.profile, force=True)
        return {**metadata, 'status': metadata['status'] if metadata['accountId'] == self.profile['accountId'] else 'changed'}

    def reconnect(self, key, verified_metadata=None):
        if verified_metadata is not None:
            self.profile.update(verified_metadata)
        self.profile.pop('disconnected', None)
        return dict(self.profile)

    def register_claude(self, options, label, verified_metadata=None):
        profile = {'provider':'claude', 'claudeOptions':options}
        metadata = verified_metadata or codex_claude.auth_metadata(profile, force=True)
        if metadata['status'] != 'ready':
            raise ValueError('not signed in')
        key = 'claude-added-' + str(len(self.registered) + 1)
        self.registered.append({'accountKey':key, 'options':dict(options), 'label':label, **metadata})
        return key

    def find_claude_config(self, config_dir):
        for account in self.registered:
            if account['options'].get('configDir') == config_dir:
                return {'id':account['accountKey'], **account}
        return None


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
    if (root / 'status-block').exists():
        (root / 'status-pid').write_text(str(os.getpid()))
        while True: time.sleep(1)
    p = root / 'status.json'
    print(p.read_text() if p.exists() else json.dumps({'loggedIn':False}))
    sys.exit(0)
if (root / 'ignore-term').exists(): signal.signal(signal.SIGTERM, signal.SIG_IGN)
(root / 'native-pid').write_text(str(os.getpid()))
assert sys.argv[1:3] == ['auth','login'] and sys.argv[3] == '--claudeai'
if '--email' in sys.argv:
    assert sys.argv[sys.argv.index('--email') + 1] == 'expected@example.com'
    assert sys.argv[4:] == ['--email','expected@example.com']
else:
    assert sys.argv[4:] == []
(root / 'login-args').write_text(json.dumps(sys.argv[1:]))
assert os.environ['BROWSER'] == '/usr/bin/false'
assert 'ANTHROPIC_API_KEY' not in os.environ
with (root / 'launches').open('a') as f: f.write('start\\n')
print('https://claude.com/cai/oauth/authorize?state=secret-state&code_challenge=challenge', flush=True)
print('Paste code here if prompted >', flush=True)
code = input()
with (root / 'deliveries').open('a') as f: f.write('received\\n')
time.sleep(.15)
if code == 'invalid':
    print('Invalid code', flush=True)
    sys.exit(1)
if code == 'expired':
    print('Authorization code expired', flush=True)
    sys.exit(1)
email = 'wrong@example.com' if code == 'wrong' else 'expected@example.com'
(root / 'status.json').write_text(json.dumps({'loggedIn':True,'authMethod':'claude.ai','email':email,'subscriptionType':'max'}))
''')
        self.binary.chmod(0o700)
        self.installed = patch('codex_claude.installed', return_value=str(self.binary))
        self.installed.start()
        native_reader = codex_claude._auth_status
        self.auth_reader = patch('codex_claude._auth_status',
            side_effect=lambda executable, env, interactive=False:
                native_reader(executable, env, interactive=True))
        self.auth_reader.start()
        self.profile = {'provider':'claude', 'email':'expected@example.com', 'accountId':'claude:expected@example.com',
                        'claudeOptions':{'configDir':str(self.config), 'binaryPath':str(self.binary)}}
        self.rt = SimpleNamespace(root=self.root, accounts=Accounts(self.profile), lock=threading.RLock())
        self.rt.accounts.root = self.root / 'accounts'
        self.agents = []
        self.rt.db = lambda: nullcontext(None)
        self.rt.records = lambda db, table: copy.deepcopy(self.agents)
        self.rt.account_agents = lambda db, key: copy.deepcopy(
            [agent for agent in self.agents if agent.get('accountKey', 'default') == key])
        self.rt.put = lambda db, table, record: self.agents.__setitem__(
            next(i for i, row in enumerate(self.agents) if row['id'] == record['id']), record)
        self.login = LoginManager(self.rt, deadline=30)
        self.ids = []

    def tearDown(self):
        for rid in self.ids:
            self.login.cancel(rid)
        end = time.monotonic() + 4
        while any(job.get('process') and job['process'].poll() is None for job in self.login.jobs.values()) and time.monotonic() < end:
            time.sleep(.02)
        time.sleep(.05)
        self.temp.cleanup()
        self.installed.stop()
        self.auth_reader.stop()

    def start(self):
        rid = str(uuid.uuid4())
        self.ids.append(rid)
        self.login.start('claude-test', rid)
        return rid

    def await_status(self, rid, states):
        end = time.monotonic() + 35
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

    def test_login_refreshes_only_inactive_auth_failures_for_same_account(self):
        base = {'id':'first', 'provider':'claude', 'accountKey':'claude-test',
                'status':'failed', 'error':{'message':'Failed to authenticate: OAuth session expired'},
                'nativeStatus':{'phase':'auth'}, 'turnId':'historical-turn'}
        self.agents = [base, {**base,'id':'second','error':"This profile's account changed. Restore its original login or add a separate profile."},
            {**base,'id':'other','accountKey':'another'},
            {**base,'id':'busy','inFlight':True},
            {**base,'id':'running','status':'running'},
            {**base,'id':'network','error':'Network unavailable'}]
        before = copy.deepcopy(self.agents)
        rid = self.start()
        self.await_status(rid, {'pending'})
        self.login.code(rid, 'valid-code')
        receipt = self.await_status(rid, {'ready','error'})
        self.assertEqual(receipt['status'], 'ready')
        self.assertTrue(receipt['chatsRefreshed'])
        for agent in self.agents[:2]:
            self.assertEqual(agent['status'], 'idle')
            self.assertIsNone(agent['error'])
            self.assertNotIn('nativeStatus', agent)
            self.assertEqual(agent['turnId'], 'historical-turn')
        self.assertEqual(self.agents[2:], before[2:])
        # An old successful receipt also reconciles previous chat errors once.
        self.agents = before
        job = self.login.jobs[rid]
        job['receipt'].pop('chatsRefreshed')
        self.login._save(job)
        self.assertTrue(LoginManager(self.rt).status(rid)['chatsRefreshed'])
        self.assertEqual(self.agents[0]['status'], 'idle')

    def test_wrong_account_is_error_and_saved_identity_unchanged(self):
        rid = self.start()
        self.await_status(rid, {'pending'})
        self.login.code(rid, 'wrong')
        result = self.await_status(rid, {'error','ready'})
        self.assertEqual(result['status'], 'error')
        self.assertIn('different Claude account', result['error'])
        self.assertEqual(self.profile['accountId'], 'claude:expected@example.com')
        self.assertEqual(self.rt.accounts.refresh('claude-test')['status'], 'changed')

    def test_disconnected_account_can_reauthenticate_and_reconnect(self):
        self.profile['disconnected'] = True
        rid = str(uuid.uuid4())
        started = self.login.start('claude-test', rid)
        self.ids.append(rid)
        self.assertIn(started['status'], {'starting', 'pending'})
        self.await_status(rid, {'pending'})
        self.login.code(rid, 'valid-code')
        result = self.await_status(rid, {'ready', 'error'})
        self.assertEqual(result['status'], 'ready', result)
        self.assertFalse(self.profile.get('disconnected', False))

    def test_add_account_uses_private_config_and_registers_verified_identity_once(self):
        rid = str(uuid.uuid4())
        started = self.login.start_add('expected@example.com', 'Work', rid)
        config = self.root / 'claude-logins' / ('config-' + rid)
        self.assertEqual(started['status'], 'starting')
        self.assertEqual(config.stat().st_mode & 0o777, 0o700)
        pending = self.await_status(rid, {'pending'})
        self.assertTrue(verification_url(pending['verificationUrl']))
        self.assertEqual(self.login.start_add('expected@example.com', 'Work', rid), pending)
        self.assertEqual(self.login.code(rid, 'one-time-code#state-value')['codeSubmitted'], True)
        self.login.code(rid, 'one-time-code#state-value')
        result = self.await_status(rid, {'ready', 'error'})
        self.assertEqual(result['status'], 'ready', result)
        self.assertEqual(result['accountKey'], 'claude-added-1')
        self.assertEqual(result['email'], 'expected@example.com')
        self.assertEqual(result['plan'], 'max')
        self.assertEqual(self.rt.accounts.registered[0]['label'], 'Work')
        self.assertEqual(self.rt.accounts.registered[0]['options']['configDir'], str(config.resolve()))
        self.assertEqual(json.loads((config / 'login-args').read_text()),
                         ['auth', 'login', '--claudeai', '--email', 'expected@example.com'])
        self.assertEqual((config / 'deliveries').read_text(), 'received\n')
        persisted = (self.root / 'claude-logins' / (rid + '.json')).read_text()
        self.assertNotIn('one-time-code', persisted)
        self.assertNotIn('state-value', persisted)
        self.assertNotIn('secret-state', persisted)

    def test_add_accounts_have_isolated_configs_for_concurrent_logins(self):
        first, second = str(uuid.uuid4()), str(uuid.uuid4())
        self.login.start_add('expected@example.com', 'First', first)
        self.login.start_add(None, 'Second', second)
        self.assertEqual(self.await_status(first, {'pending'})['status'], 'pending')
        self.assertEqual(self.await_status(second, {'pending'})['status'], 'pending')
        first_config = self.root / 'claude-logins' / ('config-' + first)
        second_config = self.root / 'claude-logins' / ('config-' + second)
        self.assertNotEqual(first_config, second_config)
        self.assertEqual(json.loads((first_config / 'login-args').read_text()),
                         ['auth', 'login', '--claudeai', '--email', 'expected@example.com'])
        self.assertEqual(json.loads((second_config / 'login-args').read_text()),
                         ['auth', 'login', '--claudeai'])
        self.login.code(first, 'first-code#state')
        self.login.code(second, 'second-code#state')
        self.assertEqual(self.await_status(first, {'ready', 'error'})['status'], 'ready')
        self.assertEqual(self.await_status(second, {'ready', 'error'})['status'], 'ready')
        self.assertEqual({row['options']['configDir'] for row in self.rt.accounts.registered},
                         {str(first_config.resolve()), str(second_config.resolve())})

    def test_add_wrong_email_hint_and_invalid_code_are_clear_and_do_not_register(self):
        wrong = str(uuid.uuid4())
        self.login.start_add('expected@example.com', 'Wrong', wrong)
        self.await_status(wrong, {'pending'})
        self.login.code(wrong, 'wrong')
        wrong_result = self.await_status(wrong, {'ready', 'error'})
        self.assertEqual(wrong_result['status'], 'error')
        self.assertIn('different Claude account', wrong_result['error'])
        self.assertIn('expected@example.com', wrong_result['error'])
        self.assertEqual(self.rt.accounts.registered, [])

        invalid = str(uuid.uuid4())
        self.login.start_add(None, 'Invalid', invalid)
        self.await_status(invalid, {'pending'})
        self.login.code(invalid, 'invalid')
        invalid_result = self.await_status(invalid, {'ready', 'error'})
        self.assertEqual(invalid_result['status'], 'error')
        self.assertIn('rejected', invalid_result['error'])
        self.assertEqual(self.rt.accounts.registered, [])

        expired = str(uuid.uuid4())
        self.login.start_add(None, 'Expired', expired)
        self.await_status(expired, {'pending'})
        self.login.code(expired, 'expired')
        expired_result = self.await_status(expired, {'ready', 'error'})
        self.assertEqual(expired_result['status'], 'error')
        self.assertIn('expired', expired_result['error'])
        self.assertEqual(self.rt.accounts.registered, [])

    def test_add_cancel_is_durable_and_cleans_unregistered_config(self):
        rid = str(uuid.uuid4())
        self.login.start_add(None, 'Cancelled', rid)
        self.await_status(rid, {'pending'})
        config = self.root / 'claude-logins' / ('config-' + rid)
        self.assertEqual(self.login.cancel(rid)['status'], 'cancelled')
        end = time.monotonic() + 3
        while config.exists() and time.monotonic() < end:
            time.sleep(.02)
        self.assertFalse(config.exists())
        self.assertEqual(LoginManager(self.rt).start_add(None, 'Cancelled', rid)['status'], 'cancelled')

    def test_add_restart_marks_active_request_without_relaunch(self):
        rid = str(uuid.uuid4())
        script = """
import os, time
from pathlib import Path
from types import SimpleNamespace
from codex_claude_login import LoginManager
runtime = SimpleNamespace(root=Path(os.environ['FIXTURE_ROOT']), accounts=SimpleNamespace())
manager = LoginManager(runtime, deadline=10)
rid = os.environ['FIXTURE_REQUEST']
manager.start_add('expected@example.com', 'Restart', rid)
while manager.status(rid)['status'] == 'starting':
    time.sleep(.02)
os._exit(0)
"""
        env = {**os.environ, 'PYTHONPATH':str(SERVER_SOURCE_ROOT),
               'STUDIO_CLAUDE_BIN':str(self.binary), 'FIXTURE_ROOT':str(self.root), 'FIXTURE_REQUEST':rid}
        subprocess.run([sys.executable, '-B', '-c', script], env=env, check=True, timeout=20)
        config = self.root / 'claude-logins' / ('config-' + rid)
        pid = int((config / 'native-pid').read_text())
        end = time.monotonic() + 3
        while time.monotonic() < end:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(.03)
        else:
            self.fail('Native add login survived backend exit')
        restarted = LoginManager(self.rt)
        result = restarted.status(rid)
        self.assertEqual(result['status'], 'error')
        self.assertIn('restarted', result['error'])
        self.assertFalse(config.exists())
        self.assertTrue((self.root / 'claude-logins' / (rid + '.json')).exists())

    def test_add_restart_keeps_config_when_account_was_saved_before_receipt(self):
        rid = str(uuid.uuid4())
        config = self.root / 'claude-logins' / ('config-' + rid)
        config.mkdir(parents=True)
        options = {'binaryPath':str(self.binary), 'configDir':str(config.resolve())}
        key = self.rt.accounts.register_claude(options, 'Recovered', verified_metadata={
            'status':'ready', 'accountId':'claude:expected@example.com',
            'email':'expected@example.com', 'plan':'max',
        })
        job = {'receipt':{'requestId':rid, 'accountKey':'pending-' + rid, 'status':'pending',
                          'email':'expected@example.com'},
               'flow':'add', 'configDir':str(config.resolve()), 'binaryPath':str(self.binary),
               'requestHash':'hash', 'emailHint':'expected@example.com', 'label':'Recovered',
               'cleanupDone':False}
        self.login._save(job)
        result = LoginManager(self.rt).status(rid)
        self.assertEqual(result['status'], 'ready')
        self.assertEqual(result['accountKey'], key)
        self.assertTrue(config.is_dir())

    def test_terminal_unregistered_add_retries_cleanup_on_restart(self):
        rid = str(uuid.uuid4())
        config = self.root / 'claude-logins' / ('config-' + rid)
        config.mkdir(parents=True)
        job = {'receipt':{'requestId':rid, 'accountKey':'pending-' + rid, 'status':'error',
                          'error':'rejected'},
               'flow':'add', 'configDir':str(config.resolve()), 'binaryPath':str(self.binary),
               'requestHash':'hash', 'emailHint':None, 'label':'Rejected', 'cleanupDone':False}
        self.login._save(job)
        restarted = LoginManager(self.rt)
        self.assertEqual(restarted.jobs[rid]['receipt']['status'], 'error')
        self.assertFalse(config.exists())
        saved = json.loads((restarted._path(rid)).read_text())
        self.assertTrue(saved['metadata']['cleanupDone'])

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
        env = {**os.environ, 'PYTHONPATH':str(SERVER_SOURCE_ROOT),
               'FIXTURE_PROFILE':json.dumps(self.profile), 'FIXTURE_ROOT':str(self.root), 'FIXTURE_REQUEST':rid}
        subprocess.run([sys.executable,'-B','-c',script],env=env,check=True,timeout=20)
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

    def test_backend_exit_stops_supervised_auth_status_process(self):
        (self.config / 'status-block').touch()
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
manager.start('claude-test', rid)
while manager.status(rid)['status'] == 'starting': time.sleep(.02)
manager.code(rid, 'valid-code')
status_pid = Path(profile['claudeOptions']['configDir']) / 'status-pid'
end = time.monotonic() + 12
while not status_pid.exists() and time.monotonic() < end: time.sleep(.02)
if not status_pid.exists(): raise SystemExit('status process did not start')
os._exit(0)
"""
        env = {**os.environ, 'PYTHONPATH':str(SERVER_SOURCE_ROOT),
               'STUDIO_CLAUDE_BIN':str(self.binary), 'FIXTURE_PROFILE':json.dumps(self.profile),
               'FIXTURE_ROOT':str(self.root), 'FIXTURE_REQUEST':rid}
        subprocess.run([sys.executable, '-B', '-c', script], env=env, check=True, timeout=60)
        pid = int((self.config / 'status-pid').read_text())
        end = time.monotonic() + 3
        while time.monotonic() < end:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(.03)
        else:
            self.fail('Supervised auth status survived backend exit')

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
        server.server.context._maintenance_last = time.monotonic()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        origin = f'http://127.0.0.1:{server.server_port}'
        def request(path, body=None, headers=None):
            req = urllib.request.Request(origin + path,
                data=json.dumps(body).encode() if body is not None else None,
                headers={'Content-Type':'application/json', **(headers or {})})
            try:
                with urllib.request.urlopen(req, timeout=20) as response:
                    return response.status, json.loads(response.read())
            except urllib.error.HTTPError as error:
                with error:
                    return error.code, json.loads(error.read())
        try:
            token = request('/api/session')[1]['token']
            headers = {'Origin':origin, 'X-Canvas-Token':token}
            path = '/api/accounts/claude/login'
            rid = str(uuid.uuid4())
            body = {'account_key':'claude-test', 'login_id':rid}
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
            self.assertEqual(request(path + '/cancel', {'login_id':rid}, headers)[1]['status'],'cancelled')
        finally:
            server.shutdown()
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive(), 'HTTP server did not stop')
            server.server_close()

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
