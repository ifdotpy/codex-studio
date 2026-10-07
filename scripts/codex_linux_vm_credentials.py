"""Copy account credentials into private guest profile files, without logs."""
from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path
import subprocess
import sys


MAX_CREDENTIAL_BYTES = 1024 * 1024


def claude_keychain_service(config_dir=None):
    service = 'Claude Code-credentials'
    if config_dir:
        # Observed with the installed Claude CLI on this Mac. Keep the
        # selected CLI directory string, without a trailing slash.
        service += '-' + hashlib.sha256(str(config_dir).rstrip('/').encode()).hexdigest()[:8]
    return service


def profile_path(account_key, provider):
    key = hashlib.sha256(account_key.encode()).hexdigest()[:32]
    return ('.claude' if provider == 'claude' else '.codex') + '/studio-accounts/' + key


def _read_file(path):
    with Path(path).open('rb') as stream:
        data = stream.read(MAX_CREDENTIAL_BYTES + 1)
    if len(data) > MAX_CREDENTIAL_BYTES:
        raise ValueError('The account credential file exceeds the guest limit')
    return data


def read_credentials(runtime, account_key):
    account = runtime.accounts.get(account_key)
    provider = account.get('provider', 'codex')
    if provider == 'codex':
        data = _read_file(Path(runtime.accounts.home(account_key)) / 'auth.json')
        destination = profile_path(account_key, provider) + '/auth.json'
        try:
            value = json.loads(data)
            if not isinstance(value, dict):
                raise ValueError()
        except (ValueError, TypeError) as error:
            raise ValueError('The Codex credential file is invalid') from error
    elif provider == 'claude':
        from codex_claude import auth_metadata, subscription_env
        before = auth_metadata(account, force=True)
        if before.get('status') != 'ready' or before.get('accountId') != account.get('accountId'):
            raise ValueError('The Claude account identity changed; refresh the host sign-in')
        configured = subscription_env(account).get('CLAUDE_CONFIG_DIR')
        home = Path(configured or Path.home() / '.claude')
        path = home / '.credentials.json'
        if path.is_file():
            data = _read_file(path)
        elif sys.platform == 'darwin':
            # Read the selected profile Keychain service. Capture
            # bytes only; never place the token in arguments, receipts or logs.
            completed = subprocess.run(['security', 'find-generic-password', '-s',
                                        claude_keychain_service(configured), '-w'],
                                       capture_output=True, timeout=10)
            if completed.returncode or len(completed.stdout) > MAX_CREDENTIAL_BYTES:
                raise ValueError('The Claude Keychain credential cannot be read')
            data = completed.stdout.strip()
        else:
            raise ValueError('The Claude profile needs a credential file for Linux VM use')
        try:
            value = json.loads(data)
            if not isinstance(value.get('claudeAiOauth', {}).get('accessToken'), str):
                raise ValueError()
        except (ValueError, TypeError, AttributeError) as error:
            raise ValueError('The Claude credential file is invalid') from error
        after = auth_metadata(account, force=True)
        if after.get('status') != 'ready' or after.get('accountId') != before.get('accountId'):
            raise ValueError('The Claude account changed during credential copy')
        destination = profile_path(account_key, provider) + '/.credentials.json'
    else:
        raise ValueError('This provider does not support Linux VM credentials')
    return {'path': destination, 'data': base64.b64encode(data).decode('ascii')}


def sync_credentials(runtime, client, account_key, *, previous_hash=None):
    file = read_credentials(runtime, account_key)
    digest = hashlib.sha256(json.dumps(file, sort_keys=True).encode()).hexdigest()
    if digest != previous_hash:
        identity = 'credentials:' + hashlib.sha256(account_key.encode()).hexdigest()[:24] + ':' + digest
        client.request('credentials.put', {'files': [file]}, request_id=identity, timeout=15)
    return digest


def tick(runtime):
    """Refresh changed host tokens in a bounded background job."""
    import time
    with runtime.lock:
        now = time.monotonic()
        if (runtime.closed or not runtime.__dict__.get('linux_servers')
                or runtime.__dict__.get('_linux_credentials_busy')
                or now < runtime.__dict__.get('_linux_credentials_next', 0)):
            return
        runtime._linux_credentials_next = now + 30
        runtime._linux_credentials_busy = True
    def refresh():
        try:
            from codex_linux_workspaces import client
            with runtime.read_db() as db:
                accounts = {agent.get('accountKey', 'default') for agent in runtime.records(db, 'agents')
                            if agent.get('environment') == 'linux' and agent.get('imageWorkspaceReady')
                            and not agent.get('deletedAt')}
            for account in accounts:
                hashes = runtime.__dict__.setdefault('linux_credential_hashes', {})
                try:
                    digest = sync_credentials(runtime, client(runtime), account, previous_hash=hashes.get(account))
                    hashes[account] = digest
                    runtime.__dict__.setdefault('linux_credential_errors', {}).pop(account, None)
                except Exception as error:
                    # Keep tokens and file content out of diagnostics.
                    runtime.__dict__.setdefault('linux_credential_errors', {})[account] = type(error).__name__
        finally:
            with runtime.lock:
                runtime._linux_credentials_busy = False
    try:
        runtime.pool.submit(refresh)
    except Exception:
        with runtime.lock:
            runtime._linux_credentials_busy = False
        raise
