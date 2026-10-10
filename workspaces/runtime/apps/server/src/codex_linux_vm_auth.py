"""Host-owned OAuth refresh. Only access credentials cross the VM boundary."""
import base64
import copy
import json
import hashlib
import math
from pathlib import Path
import select
import shutil
import subprocess
import threading
import time

_LOCKS = {}
_GATE = threading.Lock()
REFRESH_WINDOW = 300


def account_snapshot(runtime, key):
    store = runtime.accounts
    if hasattr(store, '_row'):
        with store.lock:
            account = copy.deepcopy(store._row(key))
    else:
        account = store.get(key)
    if (account.get('deleted') or account.get('disconnected') or account.get('duplicateOf')
            or account.get('status') == 'changed'):
        raise ValueError('The selected host account is unavailable')
    return account


def _same_account(first, second):
    return all(first.get(name) == second.get(name) for name in (
        'provider', 'home', 'accountId', 'email', 'claudeOptions',
        'deleted', 'disconnected', 'duplicateOf', '_credentialIdentity'))


def _lock(runtime, key):
    with _GATE:
        return _LOCKS.setdefault((str(getattr(runtime, 'root', '')), key), threading.RLock())


def _jwt(token):
    try:
        part = token.split('.')[1]
        value = json.loads(base64.urlsafe_b64decode(part + '=' * (-len(part) % 4)))
        if not isinstance(value, dict):
            raise ValueError()
        return value
    except (ValueError, TypeError, IndexError):
        raise ValueError('The host access token has invalid expiry metadata') from None


def codex_access(runtime, key, *, refresh=False, proactive=True):
    from codex_linux_vm_credentials import _read_file
    with _lock(runtime, key):
        account = account_snapshot(runtime, key)
        home = Path(account.get('home') or runtime.accounts.home(key))
        def read():
            try:
                value = json.loads(_read_file(home / 'auth.json'))
                if value.get('OPENAI_API_KEY'):
                    expected = account.get('_credentialIdentity')
                    observed = 'api:' + hashlib.sha256(value['OPENAI_API_KEY'].encode()).hexdigest()
                    if expected and expected != observed:
                        raise ValueError('The host API key identity changed')
                    return {'type': 'apiKey', 'apiKey': value['OPENAI_API_KEY']}, None
                tokens = value['tokens']
                claims = _jwt(tokens['access_token'])
                identity = tokens.get('account_id') or claims.get('https://api.openai.com/auth', {}).get('chatgpt_account_id')
                if not identity or identity != account.get('accountId'):
                    raise ValueError('The host Codex account identity changed')
                expires = claims['exp']
                if isinstance(expires, bool) or not isinstance(expires, (int, float)) or not math.isfinite(expires):
                    raise ValueError()
                return {'type': 'chatgptAuthTokens', 'accessToken': tokens['access_token'],
                        'chatgptAccountId': identity}, expires
            except (ValueError, TypeError, KeyError, AttributeError):
                raise ValueError('The host Codex credential is invalid or its account changed') from None
        payload, expires = read()
        if expires is not None and (refresh or proactive and expires <= time.time() + REFRESH_WINDOW):
            # Reuse the account's host AppServer and its native refresh/store locks.
            runtime.connect(key).call('account/read', {'refreshToken': True}, timeout=8 if refresh else 45)
            payload, expires = read()
        if not _same_account(account_snapshot(runtime, key), account):
            raise ValueError('The host account changed during access token read')
        if expires is not None and expires <= time.time() + 30:
            raise ValueError('The host Codex access token expired; host refresh did not produce a usable token')
        return payload


def raw_claude(account):
    from codex_claude import subscription_env
    from codex_linux_vm_credentials import _read_file, claude_keychain_service, MAX_CREDENTIAL_BYTES
    import sys
    configured = subscription_env(account).get('CLAUDE_CONFIG_DIR')
    path = Path(configured or Path.home() / '.claude') / '.credentials.json'
    data = None
    if sys.platform == 'darwin':
        result = subprocess.run(['security', 'find-generic-password', '-s',
                                 claude_keychain_service(configured), '-w'],
                                capture_output=True, timeout=10)
        if result.returncode == 0:
            if len(result.stdout) > MAX_CREDENTIAL_BYTES:
                raise ValueError('The Claude Keychain credential exceeds its size limit')
            data = result.stdout
    if data is None:
        if not path.is_file():
            raise ValueError('The host Claude credential file or Keychain entry is missing')
        data = _read_file(path)
    try:
        value = json.loads(data)['claudeAiOauth']
        if (not isinstance(value['accessToken'], str) or not value['accessToken']
                or type(value['expiresAt']) not in (int, float) or not math.isfinite(value['expiresAt'])
                or not isinstance(value['scopes'], list) or 'user:inference' not in value['scopes']):
            raise ValueError()
        return value
    except (ValueError, TypeError, KeyError, AttributeError):
        raise ValueError('The host Claude credential has invalid access token metadata') from None


def refresh_claude(account):
    """Hold an unsubmitted SDK query until the native CLI saves its refresh."""
    from codex_claude import installed, subscription_env
    node = shutil.which('node') or '/opt/homebrew/bin/node'
    env = subscription_env(account)
    env['STUDIO_CLAUDE_BIN'] = installed(account) or 'claude'
    env['STUDIO_CLAUDE_ACCOUNT'] = account.get('email') or ''
    from codex_layout import CLAUDE_BRIDGE_ROOT
    script = CLAUDE_BRIDGE_ROOT / 'refresh-auth.mjs'
    if not script.is_file():
        script = Path(__file__).resolve().parent / 'claude_bridge' / 'refresh-auth.mjs'
    if not script.is_file():
        raise FileNotFoundError('The Claude bridge refresh entrypoint is unavailable.')
    deadline = time.monotonic() + 90
    process = subprocess.Popen([node, str(script)], env=env, stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    try:
        readable, _, _ = select.select([process.stdout], [], [], min(60, max(0, deadline-time.monotonic())))
        try:
            ready = json.loads(process.stdout.readline()) if readable else None
        except (ValueError, TypeError):
            ready = None
        if ready != {'ready': True}:
            raise ValueError('The host Claude refresh process could not verify the selected account')
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise ValueError('The host Claude refresh process stopped before it saved a fresh access token')
            if raw_claude(account)['expiresAt'] > (time.time() + REFRESH_WINDOW) * 1000:
                process.stdin.write(b'done\n')
                process.stdin.flush()
                if process.wait(timeout=max(1, deadline-time.monotonic())) != 0:
                    raise ValueError('The host Claude account changed during token refresh')
                return
            time.sleep(0.25)
        raise ValueError('The host Claude refresh did not produce a fresh access token within 90 seconds')
    finally:
        # EOF asks the helper to close its idle query. No user prompt was supplied.
        process.stdin.close()
        try:
            process.wait(timeout=40)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        process.stdout.close()


def claude_access(runtime, key):
    from codex_claude import auth_metadata
    with _lock(runtime, key):
        account = account_snapshot(runtime, key)
        value = raw_claude(account)
        if value['expiresAt'] <= (time.time() + REFRESH_WINDOW + 15) * 1000:
            # A short auth-status process must never enter the CLI refresh window.
            # Wait at most 15 seconds, then let the held query own that refresh.
            while value['expiresAt'] > (time.time() + REFRESH_WINDOW) * 1000:
                time.sleep(max(0, min(0.25, value['expiresAt']/1000-time.time()-REFRESH_WINDOW)))
            refresh_claude(account)
            value = raw_claude(account)
        if value['expiresAt'] <= (time.time() + REFRESH_WINDOW + 15) * 1000:
            raise ValueError('The host Claude refresh did not produce a token outside its refresh window')
        # This status probe runs only after refresh, outside its five-minute window.
        metadata = auth_metadata(account, force=True)
        if metadata.get('status') != 'ready' or metadata.get('accountId') != account.get('accountId'):
            raise ValueError('The Claude account identity changed; refresh the host sign-in')
        if not _same_account(account_snapshot(runtime, key), account):
            raise ValueError('The host account changed during access token read')
        # Allowlist fields. Never forward refreshToken or other credential material.
        return {name: value[name] for name in ('accessToken', 'expiresAt', 'scopes',
            'subscriptionType', 'rateLimitTier') if name in value}


def bootstrap_codex(runtime, server, key):
    payload = codex_access(runtime, key, proactive=False)
    server.call('account/login/start', payload, timeout=30)


def refresh_request(runtime, message, key, connection):
    """Answer external-token requests only for this Linux account connection."""
    previous = (message.get('params') or {}).get('previousAccountId')
    try:
        if previous != account_snapshot(runtime, key).get('accountId'):
            raise ValueError('The Linux token request has a different account identity')
        payload = codex_access(runtime, key, refresh=True)
        if payload['type'] != 'chatgptAuthTokens' or previous != payload['chatgptAccountId']:
            raise ValueError('The Linux token request has a different account identity')
        payload.pop('type')
        response = {'id': message['id'], 'result': payload}
    except Exception:
        response = {'id': message['id'], 'error': {'code': -32000,
            'message': 'Host access token refresh failed. Check the host sign-in. The guest has no refresh token.'}}
    runtime.reply(response, key, connection)
