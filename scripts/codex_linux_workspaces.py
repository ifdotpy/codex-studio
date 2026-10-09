"""Studio lifecycle and provider routing for guest-owned Linux workspaces."""
from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path
import threading
import uuid

from codex_linux_workspace_sync import archive_source

_LOCKS = {}
_GATE = threading.Lock()


def client(runtime):
    with _GATE:
        value = runtime.__dict__.get('_linux_vm_client')
        if value is None:
            from codex_linux_vm import connect
            value = connect()
            runtime._linux_vm_client = value
        return value


def _save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    from codex_workspace_images import _write_json
    _write_json(path, value)
    path.chmod(0o600)


def _load(path):
    return json.loads(path.read_text()) if path.exists() else {}


def guest_root(source):
    return '/var/lib/codex-studio/projects/' + hashlib.sha256(str(Path(source).resolve()).encode()).hexdigest()


def build_base(runtime, source):
    source = str(Path(source).resolve())
    with _GATE:
        lock = _LOCKS.setdefault((str(runtime.root), source), threading.Lock())
    with lock:
        remote = client(runtime)
        remote.ensure_running()
        directory = runtime.root / 'linux-vm' / 'sources' / hashlib.sha256(source.encode()).hexdigest()
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        state_path = directory / 'state.json'
        state = _load(state_path)
        pending = state.get('pending')
        archive = directory / 'source.tar.gz'
        if pending is None:
            metadata = archive_source(source, archive, state.get('baseline'))
            pending = {**metadata, 'uploadId': uuid.uuid4().hex, 'root': guest_root(source)}
            state['pending'] = pending
            _save(state_path, state)
        identity = pending['uploadId']
        begin = {key: pending[key] for key in ('uploadId', 'root', 'totalBytes', 'sha256', 'mode', 'deletePaths')}
        remote.request('upload.begin', begin, request_id=identity + ':begin', timeout=30)
        with archive.open('rb') as stream:
            sequence = 0
            while data := stream.read(512 * 1024):
                remote.request('upload.chunk', {'uploadId': identity, 'seq': sequence,
                    'data': base64.b64encode(data).decode('ascii')},
                    request_id=identity + ':chunk:' + str(sequence), timeout=30)
                sequence += 1
        remote.request('upload.commit', {'uploadId': identity}, request_id=identity + ':commit', timeout=1810)
        result = remote.request('workspace.startBase', {'root': pending['root'], 'timeoutSeconds': 1800},
                                request_id=identity + ':base', timeout=1810)
        if result.get('state') != 'ready':
            raise RuntimeError(result.get('error') or 'The Linux workspace base is not ready')
        state.update(baseline={'repositories': pending['repositories']})
        state.pop('pending', None)
        _save(state_path, state)
        archive.unlink(missing_ok=True)
        return result


def create(runtime, source, agent_id):
    return client(runtime).request('workspace.create', {'root': guest_root(source), 'agentId': agent_id},
                                   request_id='workspace-create:' + agent_id, timeout=1810)


def ensure(runtime, agent):
    if agent.get("executionMode") == "vm":
        from codex_vm_agents import ensure as ensure_layr
        return ensure_layr(runtime, agent)
    result = client(runtime).request('workspace.status', {'agentId': agent['id']}, timeout=20)
    rows = result.get('workspaces', [])
    if len(rows) != 1:
        raise RuntimeError('The Linux workspace does not exist; its result remains unavailable')
    # Creation also restores an archived mount. The guest engine preserves its snapshot.
    return client(runtime).request('workspace.create', {'root': guest_root(agent['imageWorkspaceRepo']),
        'agentId': agent['id']}, request_id=str(uuid.uuid4()), timeout=1810)


def prefix(agent):
    import sys
    return [sys.executable, str(Path(__file__).with_name('codex_linux_vm_exec.py')),
            '--agent', agent['id'], '--cwd', agent['cwd'], '--']


def dispose(runtime, agent_id, *, remove=False):
    remote = client(runtime)
    agent = runtime.agent(agent_id)
    if agent.get('executionMode') == 'vm':
        from codex_vm_agents import dispose as dispose_layr
        server = runtime.__dict__.get('linux_servers', {}).pop(agent_id, None)
        if server is not None:
            server.close()
        return dispose_layr(runtime, agent)
    server = runtime.__dict__.get('linux_servers', {}).pop(agent_id, None)
    if server is not None:
        server.close()
    handle = 'linux-worker:' + agent_id
    providers = remote.request('provider.list', timeout=20).get('providers', [])
    if any(row.get('handle') == handle and row.get('state') in ('running', 'starting', 'lost') for row in providers):
        remote.request('provider.stop', {'handle': handle}, request_id=str(uuid.uuid4()), timeout=30)
    method = 'workspace.remove' if remove else 'workspace.archive'
    return remote.request(method, {'agentId': agent_id},
                          request_id=('remove:' if remove else 'archive:') + agent_id + ':' + str(agent['epoch']), timeout=130)


def connect_agent(runtime, agent):
    from codex_runtime import AppServer, uid
    from codex_linux_vm_credentials import profile_path, sync_credentials
    from codex_linux_vm_provider import GuestProcessProxy
    agent_id = agent['id']
    account_key = agent.get('accountKey', 'default')
    # Host refresh can call Runtime.connect, which also takes start_lock.
    # Complete it before acquiring that lock for guest process creation.
    digest = runtime.__dict__.get('linux_credential_hashes', {}).get(account_key)
    cached = runtime.__dict__.get('linux_servers', {}).get(agent_id)
    if (cached is None or cached.closed
            or agent_id in runtime.__dict__.get('offline_linux_agents', set())):
        digest = sync_credentials(runtime, client(runtime), account_key)
    with runtime.start_lock:
        if runtime.closed:
            raise RuntimeError('Runtime is stopped')
        servers = runtime.__dict__.setdefault('linux_servers', {})
        offline = runtime.__dict__.setdefault('offline_linux_agents', set())
        server = servers.get(agent_id)
        if server is not None and not server.closed and agent_id not in offline:
            return server
        if server is not None:
            server.close()
        remote = client(runtime)
        # Reconnect must not provision or replace a lost VM while a provider outcome is unknown.
        remote.request('health', timeout=15)
        ensure(runtime, agent)
        from codex_linux_vm_auth import account_snapshot
        account = account_snapshot(runtime, account_key)
        provider = account.get('provider', 'codex')
        runtime.__dict__.setdefault('linux_credential_hashes', {})[account_key] = digest
        profile_relative = profile_path(account_key, provider)
        home = agent.get('layrHome', '/home/studio')
        profile = home + '/' + profile_relative
        handle = 'linux-worker:' + agent_id
        if provider == 'claude':
            from codex_claude import bridge_options
            command = ['node', '/opt/codex-studio/claude_bridge/bridge.mjs',
                       home + '/.codex/studio-bridges/' + agent_id]
            environment = {'CLAUDE_CONFIG_DIR': profile, 'STUDIO_CLAUDE_BIN': 'claude',
                           'STUDIO_CLAUDE_OPTIONS': json.dumps(bridge_options(account)),
                           'STUDIO_CLAUDE_ACCOUNT': account.get('email', '')}
        else:
            command = ['codex', 'app-server', '--listen', 'stdio://', '-c', 'cli_auth_credentials_store="file"']
            environment = {'CODEX_HOME': profile}
        connection = uid()
        runtime.__dict__.setdefault('linux_connection_ids', {})[agent_id] = connection
        offline.discard(agent_id)
        root = runtime.root / 'linux-vm' / 'providers' / agent_id
        root.mkdir(parents=True, exist_ok=True, mode=0o700)
        # Save launch identity before dispatch. A lost start response keeps the same request ID.
        pending_path = root / 'start.json'
        pending = _load(pending_path)
        params = {'transport': 'native', 'handle': handle, 'argv': command,
                  'cwd': agent['cwd'], 'env': environment, 'agentId': agent_id}
        if agent.get('executionMode') == 'vm':
            params.update(layr=True, provider=provider, profile=profile_relative)
        if pending and pending.get('params') != params:
            raise RuntimeError('The pending Linux provider launch has a different identity')
        if not pending:
            pending = {'id': str(uuid.uuid4()), 'params': params}
            _save(pending_path, pending)
        def process_factory(stderr):
            opened = remote.request('provider.start', params, request_id=pending['id'], timeout=30)
            pending_path.unlink(missing_ok=True)
            return GuestProcessProxy(remote, handle, opened, root=runtime.root, stderr_sink=stderr)
        callbacks = (lambda message: runtime.notification(message, account_key, connection),
                     lambda message: runtime.request(message, account_key, connection),
                     lambda: runtime.disconnected(account_key, connection))
        server = AppServer(root, *callbacks, provider=provider,
            provider_options=account if provider == 'claude' else None,
            supervisor_handle=handle, process_factory=process_factory,
            supervisor_commit=lambda message, sequence: runtime.commit_supervisor_event(
                handle, message, sequence, account_key, connection),
            supervisor_event_applied=lambda sequence: runtime.supervisor_event_applied(handle, sequence),
            supervisor_reattached=lambda resumed: runtime.supervisor_reattached(
                account_key, connection, resumed, agent_id=agent_id),
            supervisor_monitor_bindings=lambda proxy: runtime.supervisor_monitor_bindings(account_key, connection, proxy),
            supervisor_monitor_result=lambda binding, future: runtime.supervisor_monitor_result(account_key, connection, binding, future))
        servers[agent_id] = server
        try:
            if provider == 'codex':
                from codex_linux_vm_auth import bootstrap_codex
                bootstrap_codex(runtime, server, account_key)
            runtime.__dict__.setdefault('linux_provider_credential_hashes', {})[agent_id] = digest
        except Exception:
            server.close()
            servers.pop(agent_id, None)
            raise
        return server


def resources(runtime):
    remote = client(runtime)
    host = remote.status()
    settings = remote.get_settings()
    result = {'state': host.get('state', 'unknown'), 'settings': settings,
              'allocatedDiskBytes': host.get('allocatedDiskBytes'),
              'memoryLimitBytes': settings.get('memoryBytes')}
    if host.get('state') == 'running':
        try:
            guest = remote.request('health', timeout=15)
            result.update(disk=guest.get('disk'), memory=guest.get('memory'))
            result['workspaces'] = remote.request('workspace.status', timeout=20).get('workspaces', [])
        except Exception as error:
            result['errorType'] = type(error).__name__
    return result
