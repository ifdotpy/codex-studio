"""Copy account credentials into private guest profile files, without logs."""
from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path


MAX_CREDENTIAL_BYTES = 1024 * 1024


def claude_keychain_service(config_dir=None):
    service = 'Claude Code-credentials'
    if config_dir:
        # Observed with the installed Claude CLI on this computer. Keep the
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
    from codex_linux_vm_auth import account_snapshot, claude_access, codex_access
    account = account_snapshot(runtime, account_key)
    provider = account.get('provider', 'codex')
    if provider == 'codex':
        codex_access(runtime, account_key)
        # Clear any legacy copied credential. External auth is installed through RPC.
        value = {}
        destination = profile_path(account_key, provider) + '/auth.json'
    elif provider == 'claude':
        value = {'claudeAiOauth': claude_access(runtime, account_key)}
        destination = profile_path(account_key, provider) + '/.credentials.json'
    else:
        raise ValueError('This provider does not support Linux VM credentials')
    data = json.dumps(value, sort_keys=True).encode()
    return {'path': destination, 'data': base64.b64encode(data).decode('ascii')}


def sync_credentials(runtime, client, account_key, *, previous_hash=None):
    file = read_credentials(runtime, account_key)
    from codex_linux_vm_auth import account_snapshot, codex_access
    identity = file
    if account_snapshot(runtime, account_key).get('provider', 'codex') == 'codex':
        identity = [file, codex_access(runtime, account_key, proactive=False)]
    digest = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    from codex_linux_vm_git import sync_git_identity
    sync_git_identity(runtime, client, force=previous_hash is None)
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
                    from codex_linux_vm_auth import bootstrap_codex
                    for agent_id, server in list(runtime.__dict__.get('linux_servers', {}).items()):
                        agent = runtime.agent(agent_id)
                        if (agent.get('accountKey', 'default') == account
                                and agent.get('provider', 'codex') == 'codex'
                                and runtime.__dict__.setdefault('linux_provider_credential_hashes', {}).get(agent_id) != digest):
                            bootstrap_codex(runtime, server, account)
                            runtime.linux_provider_credential_hashes[agent_id] = digest
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
