#!/usr/bin/env python3
"""Opt-in native auth proof with fake tokens and a local HTTPS service.

Use an already provisioned test VM. No request reaches a provider endpoint.
The host refresh query has no model prompt. Remove only the fixture profiles.
"""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

import argparse
import os, sys, json, tempfile, ssl, subprocess, threading, http.server, time, base64, io, uuid
from types import SimpleNamespace
from unittest.mock import patch
from pathlib import Path
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_linux_vm import connect
from codex_linux_vm_exec import execute
from codex_linux_vm_auth import refresh_claude, raw_claude
from codex_linux_vm_credentials import claude_keychain_service, profile_path, sync_credentials
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--state-dir', required=True)
parser.add_argument('--helper', required=True)
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix='studio-guest-auth-') as d:
    root = Path(d).resolve()
    cert = root / 'cert.pem'
    key = root / 'key.pem'
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-keyout', str(key), '-out', str(cert), '-subj', '/CN=api.anthropic.com', '-addext', 'subjectAltName=DNS:platform.claude.com,DNS:api.anthropic.com,DNS:claude.ai'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    calls = []

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def log_message(self, *args):
            pass

        def do_CONNECT(self):
            self.send_response(200)
            self.end_headers()
            try:
                self.connection = ctx.wrap_socket(self.connection, server_side=True)
            except (ConnectionError, ssl.SSLError):
                self.close_connection = True
                return
            self.rfile = self.connection.makefile('rb')
            self.wfile = self.connection.makefile('wb')
            self.close_connection = False
            while not self.close_connection:
                self.handle_one_request()
            self.close_connection = True

        def reply(self, status, value):
            data = json.dumps(value).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(data)
            self.close_connection = True

        def do_POST(self):
            request = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            calls.append(('POST', self.path))
            if self.path == '/v1/oauth/token':
                assert json.loads(request)['refresh_token'] == 'sk-ant-ort01-fixture-old'
                self.reply(200, {'access_token': 'sk-ant-oat01-fixture-fresh', 'refresh_token': 'sk-ant-ort01-fixture-new', 'expires_in': 3600, 'scope': 'user:inference user:profile'})
                return
            if self.path.startswith('/v1/messages') and self.headers.get('Authorization') == 'Bearer sk-ant-oat01-fixture-fresh':
                events = [('message_start', {'type': 'message_start', 'message': {'id': 'msg_fixture', 'type': 'message', 'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [], 'stop_reason': None, 'stop_sequence': None, 'usage': {'input_tokens': 1, 'output_tokens': 0}}}), ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}), ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': 'fixture-ok'}}), ('content_block_stop', {'type': 'content_block_stop', 'index': 0}), ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn', 'stop_sequence': None}, 'usage': {'output_tokens': 1}}), ('message_stop', {'type': 'message_stop'})]
                data = ''.join(('event: ' + name + '\ndata: ' + json.dumps(value) + '\n\n' for name, value in events)).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(data)))
                self.send_header('Connection', 'close')
                self.end_headers()
                self.wfile.write(data)
                self.close_connection = True
            else:
                self.reply(401, {'type': 'error', 'error': {'type': 'authentication_error', 'message': 'The fixture access token expired'}})

        def do_GET(self):
            calls.append(('GET', self.path))
            self.reply(200, {'account': {'uuid': '11111111-1111-4111-8111-111111111111', 'email': 'fixture@example.test'}, 'organization': {'uuid': '22222222-2222-4222-8222-222222222222', 'organization_type': 'claude_max', 'rate_limit_tier': 'default_claude_max_5x', 'billing_type': 'stripe_subscription'}})
    server = http.server.ThreadingHTTPServer(('0.0.0.0', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    remote = connect(args.state_dir, helper=args.helper)

    def command(argv):
        out, err = (io.BytesIO(), io.BytesIO())
        code = execute(remote, None, '/home/studio', argv, output=out, error=err)
        if code:
            raise RuntimeError(err.getvalue().decode()[:500])
        return out.getvalue().decode()
    gateway = json.loads(command(['ip', '-json', 'route', 'show', 'default']))[0]['gateway']
    fixture_key = 'host-fixture-' + uuid.uuid4().hex
    relative = profile_path(fixture_key, 'claude')
    profile = '/home/studio/' + relative

    def push(token, expires):
        credential = {'claudeAiOauth': {'accessToken': token, 'expiresAt': expires * 1000, 'scopes': ['user:inference', 'user:profile'], 'subscriptionType': 'max', 'rateLimitTier': 'default_claude_max_5x'}}
        assert 'refreshToken' not in json.dumps(credential)
        remote.request('credentials.put', {'files': [{'path': relative + '/.credentials.json', 'data': base64.b64encode(json.dumps(credential).encode()).decode()}, {'path': relative + '/ca.pem', 'data': base64.b64encode(cert.read_bytes()).decode()}, {'path': relative + '/.claude.json', 'data': base64.b64encode(json.dumps({'hasCompletedOnboarding': True, 'oauthAccount': {'accountUuid': '11111111-1111-4111-8111-111111111111', 'emailAddress': 'fixture@example.test', 'organizationUuid': '22222222-2222-4222-8222-222222222222'}}).encode()).decode()}]})
    script = r'''import os,subprocess,json,sys
p=sys.argv[1];env=dict(os.environ)
for k in list(env):
 if k.startswith(('ANTHROPIC_','CLAUDE_CODE_OAUTH_')):env.pop(k,None)
env.update(CLAUDE_CONFIG_DIR=p,HTTPS_PROXY=sys.argv[2],HTTP_PROXY=sys.argv[2],NO_PROXY='',NODE_EXTRA_CA_CERTS=p+'/ca.pem',SSL_CERT_FILE=p+'/ca.pem',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1',CLAUDE_CODE_OAUTH_401_WAIT_MS='0')
r=subprocess.run(['claude','-p','Say fixture-ok','--output-format','json','--model','claude-sonnet-4-6','--max-turns','1','--tools','','--settings','{"disableAllHooks":true}'],env=env,capture_output=True,text=True,timeout=40)
print(json.dumps({'code':r.returncode,'stdout':r.stdout,'stderr':r.stderr}))'''
    try:
        push('sk-ant-oat01-fixture-expired', time.time() - 60)
        before = json.loads(command(['python3', '-c', script, profile, f'http://{gateway}:{server.server_port}']))
        assert before['code'] != 0 and ('expired' in before['stdout'] or 'authentication' in before['stdout'].lower()), before
        assert not any((path == '/v1/oauth/token' for method, path in calls)), calls
        host = root / 'host-profile'
        host.mkdir(mode=448)
        value = {'accessToken': 'sk-ant-oat01-fixture-old', 'refreshToken': 'sk-ant-ort01-fixture-old', 'expiresAt': (time.time() - 1) * 1000, 'scopes': ['user:inference', 'user:profile'], 'subscriptionType': 'max', 'rateLimitTier': 'default_claude_max_5x'}
        (host / '.credentials.json').write_text(json.dumps({'claudeAiOauth': value}))
        (host / '.credentials.json').chmod(384)
        (host / '.claude.json').write_text(json.dumps({'hasCompletedOnboarding': True, 'oauthAccount': {'accountUuid': '11111111-1111-4111-8111-111111111111', 'emailAddress': 'fixture@example.test', 'organizationUuid': '22222222-2222-4222-8222-222222222222', 'billingType': 'stripe_subscription', 'accountCreatedAt': '2025-01-01', 'subscriptionCreatedAt': '2025-01-01', 'ccOnboardingFlags': {}}}))
        account = {'provider': 'claude', 'accountId': 'claude:fixture@example.test', 'email': 'fixture@example.test', 'claudeOptions': {'configDir': str(host)}}
        runtime = SimpleNamespace(root=root, accounts=SimpleNamespace(get=lambda key: account))
        local_proxy = f'http://127.0.0.1:{server.server_port}'
        with patch.dict(os.environ, {'HTTPS_PROXY': local_proxy, 'HTTP_PROXY': local_proxy, 'NO_PROXY': '', 'NODE_EXTRA_CA_CERTS': str(cert), 'SSL_CERT_FILE': str(cert), 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'CLAUDE_CODE_SKIP_UPDATE_CHECK': '1'}):
            refresh_claude(account)
            fresh = raw_claude(account)
            assert fresh['accessToken'] == 'sk-ant-oat01-fixture-fresh'
            assert fresh['refreshToken'] == 'sk-ant-ort01-fixture-new'
            sync_credentials(runtime, remote, fixture_key)
        assert sum((path == '/v1/oauth/token' for method, path in calls)) == 1, calls
        after = json.loads(command(['python3', '-c', script, profile, f'http://{gateway}:{server.server_port}']))
        assert after['code'] == 0 and 'fixture-ok' in after['stdout'], after
        assert sum((path == '/v1/oauth/token' for method, path in calls)) == 1, calls
        print(json.dumps({'guestExpiredAuthError': True, 'guestOAuthRefreshRequests': 0, 'hostRefreshedWithoutModelPrompt': True, 'hostRotationSaved': True, 'pushThenRetryRestores': True, 'fakeRequests': calls}))
    finally:
        if sys.platform == 'darwin' and 'host' in locals():
            subprocess.run(['security', 'delete-generic-password', '-s', claude_keychain_service(str(host))], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
        command(['python3', '-c', 'import shutil,sys;shutil.rmtree(sys.argv[1])', profile])
        server.shutdown()
