"""The project vault in the sidebar (docs/vault.md): the VM writer's report of each imported
layr project, kept on the host so that the sidebar never waits for the VM, and pulls of the
latest vault state into a new Mac folder through the layr-desk command."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time
from typing import Any

INTERVAL = 120.0
PULL_TIMEOUT = 3600
PULLS_KEPT = 50


def _folder(state_dir: Path, project_id: str) -> Path:
    return state_dir / 'layr-projects' / project_id


def _load(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(path.read_text())
    except FileNotFoundError:
        return {}
    return data if isinstance(data, dict) else {}


def _write(path: Path, data: dict[str, Any]) -> None:
    temporary = path.with_name(path.name + '.tmp')
    temporary.write_text(json.dumps(data, sort_keys=True))
    os.replace(temporary, path)


def store_site(state_dir: Path, project_id: str, site: str) -> None:
    """The Desk site that serves the project's vault (from layr-desk's deploy command)."""
    if not site.startswith('https://') or any(c.isspace() for c in site):
        raise ValueError('The Desk site must be an https URL')
    folder = _folder(state_dir, project_id)
    folder.mkdir(parents=True, exist_ok=True)
    _write(folder / 'vault-site.json', {'site': site.rstrip('/') + '/'})


def vault_view(state_dir: Path, project_id: str) -> dict[str, Any] | None:
    """The sidebar's vault report: host records only, no VM call. None before the first
    check. The time of the last check stays out, so an unchanged view keeps its version."""
    folder = _folder(state_dir, project_id)
    try:
        status = _load(folder / 'vault.json')
        site = _load(folder / 'vault-site.json').get('site')
    except (OSError, ValueError):
        return {'projectId': project_id, 'state': 'error', 'reason': 'The vault records are unreadable'}
    if not status:
        return None
    if not status.get('configured'):
        return {'projectId': project_id, 'state': 'none'}
    view: dict[str, Any] = {'projectId': project_id, 'state': 'error' if status.get('lastError') else 'ok',
                            'prefix': status['prefix']}
    if status.get('lastPush'):
        view['lastPush'] = status['lastPush']
    if status.get('lastError'):
        view['reason'] = str(status['lastError'])[:300]
    if isinstance(site, str):
        view['site'] = site
    return view


def _record(client: Any, project_id: str) -> dict[str, Any] | None:
    """The writer's report from the VM, or None when the VM cannot say now."""
    from codex_linux_vm import LinuxVMError
    try:
        result = client.call('vault.status', {'projectId': project_id}, timeout=30)
    except LinuxVMError as error:
        return {'configured': False} if error.code == 'not_found' else None
    return {'configured': True, 'prefix': result['prefix'], 'lastPush': result.get('lastPush'),
            'lastError': result.get('lastError')}


def views(state_dir: Path) -> dict[str, dict[str, Any] | None]:
    """The sidebar's vault report of every imported project."""
    return {record.parent.name: vault_view(state_dir, record.parent.name)
            for record in sorted((state_dir / 'layr-projects').glob('*/project.json'))}


def check(client: Any) -> bool:
    """Ask the VM about the vault of every imported project; True when a view changed."""
    changed = False
    for record in sorted((client.state_dir / 'layr-projects').glob('*/project.json')):
        try:
            project_id = json.loads(record.read_text())['projectId']
        except (OSError, ValueError, KeyError, TypeError):
            continue
        folder = record.parent
        before = vault_view(client.state_dir, folder.name)
        status = _record(client, project_id)
        if status is None or status == _load(folder / 'vault.json'):
            continue
        _write(folder / 'vault.json', status)
        changed = changed or vault_view(client.state_dir, folder.name) != before
    return changed


def tick(runtime: Any) -> None:
    """Every INTERVAL seconds, check the vaults in the runtime's worker pool: never under the
    runtime lock, never twice at once, and not while the VM is stopped."""
    import platform
    now = time.monotonic()
    if platform.system() != 'Darwin' or runtime.__dict__.get('_vault_busy') \
            or now - runtime.__dict__.get('_vault_checked', -INTERVAL) < INTERVAL:
        return
    from codex_linux_workspaces import client as vm_client
    try:
        client = vm_client(runtime)
    except (RuntimeError, OSError, ValueError):
        return  # Without a VM client there is no layr project to check.
    runtime._vault_busy, runtime._vault_checked = True, now

    def run() -> None:
        try:
            if client.status().get('state') == 'running':
                check(client)
            # A new report or a new Desk site (codex_vault.py site) changes what the sidebar shows.
            current = views(client.state_dir)
            if current != runtime.__dict__.get('_vault_views'):
                from codex_vm_agents import refresh_readiness
                refresh_readiness(runtime)
                runtime._vault_views = current
        except Exception as error:  # noqa: BLE001 - the next tick tries again
            print('vault check skipped: ' + type(error).__name__ + ': ' + str(error)[:300], file=sys.stderr)
        finally:
            runtime._vault_busy = False
    runtime.pool.submit(run)


def layr_desk() -> str:
    """The layr-desk command: STUDIO_LAYR_DESK_BIN, else PATH, else cargo's folder."""
    found = os.environ.get('STUDIO_LAYR_DESK_BIN') or shutil.which('layr-desk')
    if found:
        return found
    cargo = Path.home() / '.cargo/bin/layr-desk'
    if cargo.exists():
        return str(cargo)
    raise RuntimeError('The layr-desk command is not installed (cargo install --path crates/layr-desk)')


def _target(parent: Path, prefix: str) -> Path:
    stamp = time.strftime('%Y-%m-%d %H.%M')
    for number in range(1, 100):
        target = parent / (f'{prefix} {stamp}' + ('' if number == 1 else f' {number}'))
        if not os.path.lexists(target):
            return target
    raise RuntimeError('No free folder name for the pull')


def start_pull(runtime: Any, project_id: str, request_id: str, to: str | None = None) -> dict[str, Any]:
    """Pull the latest vault state into a new folder below `to` (Downloads by default). A
    repeated request id returns the same job, so a lost reply never starts a second pull."""
    pulls: dict[str, dict[str, Any]] = runtime.__dict__.setdefault('_vault_pulls', {})
    lock: threading.Lock = runtime.__dict__.setdefault('_vault_pulls_lock', threading.Lock())
    with lock:
        if request_id in pulls:
            return dict(pulls[request_id])
        from codex_linux_workspaces import client as vm_client
        view = vault_view(vm_client(runtime).state_dir, project_id)
        if not view or view['state'] == 'none':
            raise ValueError('The project has no vault')
        if not view.get('site'):
            raise ValueError('The Desk site of this vault is not known; run codex_vault.py site')
        parent = Path(to).expanduser() if to else Path.home() / 'Downloads'
        if not parent.is_absolute() or not parent.is_dir():
            raise ValueError('The pull folder must be an existing folder')
        target = _target(parent, view['prefix'])
        command = [layr_desk(), 'pull', '--site', view['site'], '--project', view['prefix'],
                   '--to', str(target), '--json']
        job: dict[str, Any] = {'requestId': request_id, 'state': 'running', 'folder': str(target)}
        pulls[request_id] = job
        while len(pulls) > PULLS_KEPT:
            pulls.pop(next(iter(pulls)))

    def run() -> None:
        try:
            done = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                  timeout=PULL_TIMEOUT)
            if done.returncode:
                lines = (done.stderr or done.stdout).strip().splitlines()
                update = {'state': 'failed', 'error': (lines[-1] if lines else 'layr-desk failed').removeprefix('Error: ')[:500]}
            else:
                result = json.loads(done.stdout)
                update = {'state': 'done', 'seq': result['seq'], 'files': result['files'], 'bytes': result['bytes']}
        except (OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
            update = {'state': 'failed', 'error': f'{type(error).__name__}: {str(error)[:400]}'}
        with lock:
            job.update(update)
    runtime.pool.submit(run)
    return dict(job)
