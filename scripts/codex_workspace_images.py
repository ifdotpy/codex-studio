"""Private folder copies for image workspaces."""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import shutil
import sys
import threading
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Protocol


class WorkspaceBackend(Protocol):
    def supported(self, root: Path) -> tuple[bool, str]: ...
    def current_event_id(self, root: Path) -> Any: ...
    def open_base_staging(self, root: Path, key: str, version: str) -> dict[str, Any]: ...
    def copy_base_tree(self, root: Path, destination: Path, *, excludes: tuple[str, ...]) -> None: ...
    def seal_base(self, staging: dict[str, Any]) -> dict[str, Any]: ...
    def clone_workspace(self, image: Path, agent_dir: Path) -> Path: ...
    def mount_workspace(self, layer: Path, mount: Path, *, base_image: Path | None = None) -> dict[str, Any]: ...
    def sync_delta(self, root: Path, target: Path, token: Any, *, excludes: tuple[str, ...]) -> Any: ...
    def unmount_workspace(self, mount: Path, *, force: bool = False) -> None: ...
    def remove_layer(self, agent_dir: Path) -> None: ...
    def remove_base_version(self, path: Path) -> None: ...
    def private_bytes(self, path: Path) -> int: ...
    def exec_prefix(self) -> list[str]: ...


_STORE_OVERRIDE = 'CODEX_WORKSPACE_STORE'
_DEFAULT_STORE = Path.home() / '.local' / 'state' / 'codex-agents' / 'workspaces'
_backend_instance = None
_backend_lock = threading.Lock()
_build_lock = threading.Lock()
_build_threads: dict[str, threading.Thread] = {}
_build_callbacks: dict[str, list] = {}


def _backend() -> WorkspaceBackend:
    if sys.platform == 'darwin':
        from codex_workspace_macos import Backend
    elif sys.platform.startswith('linux'):
        from codex_workspace_linux import Backend
    else:
        raise RuntimeError('Image workspaces are not supported on this platform')
    return Backend()


def _get_backend():
    global _backend_instance
    if _backend_instance is None:
        with _backend_lock:
            if _backend_instance is None:
                _backend_instance = _backend()
    return _backend_instance


def _store() -> Path:
    return Path(os.environ.get(_STORE_OVERRIDE) or _DEFAULT_STORE).expanduser().resolve()


def _root_path(path) -> Path:
    root = Path(path).expanduser().resolve()
    if not root.is_dir():
        raise ValueError(f'Workspace folder does not exist: {root}')
    return root


def _repo_key(root: Path) -> str:
    return hashlib.sha256(os.fsencode(root.resolve())).hexdigest()[:32]


def _workspace_excludes(root: Path) -> tuple[str, ...]:
    values = {'.worktrees'}
    configured_store = Path(os.environ.get(_STORE_OVERRIDE) or _DEFAULT_STORE).expanduser()
    configured_store = Path(os.path.abspath(configured_store))
    for store in (configured_store, _store()):
        try:
            values.add(store.relative_to(root).as_posix())
        except ValueError:
            pass
    return tuple(sorted(values))


def _safe_id(value) -> str:
    value = str(value)
    if not value or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-' for c in value):
        raise ValueError('Invalid workspace identifier')
    return value


def _read_json(path: Path, default=None):
    try:
        return json.loads(path.read_text())
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return default


def _write_json(path: Path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_name(path.name + f'.{os.getpid()}.{threading.get_ident()}.tmp')
    with temp.open('w') as stream:
        json.dump(value, stream, sort_keys=True, separators=(',', ':'))
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temp, path)
    try:
        fd = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    except OSError:
        pass


@contextmanager
def _file_lock(path: Path, *, blocking=True):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        flags = fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB)
        fcntl.flock(fd, flags)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def _base_dir(key: str) -> Path:
    return _store() / 'bases' / key


def _base_state_path(key: str) -> Path:
    return _base_dir(key) / 'base.json'


def _agent_dir(agent_id: str) -> Path:
    return _store() / 'agents' / _safe_id(agent_id)


def _agent_state_path(agent_id: str) -> Path:
    return _agent_dir(agent_id) / 'agent.json'


def _mount_path(agent_id: str) -> Path:
    return _store() / 'mnt' / _safe_id(agent_id)


def supported(root) -> tuple[bool, str]:
    return _get_backend().supported(_root_path(root))


def base_status(root) -> dict[str, Any]:
    folder = _root_path(root)
    state = _read_json(_base_state_path(_repo_key(folder)), {})
    if not state or state.get('schema') != 2:
        return {'state': 'missing', 'version': None, 'error': None}
    return {key: state.get(key) for key in ('state', 'version', 'error')}


def start_base_build(root, on_done=None, *, retry_failed=False) -> dict[str, Any]:
    folder = _root_path(root)
    ok, reason = supported(folder)
    if not ok:
        result = {'state': 'failed', 'version': None, 'error': reason}
        if on_done:
            _call_callback(on_done, result)
        return result
    exclude_store = getattr(_get_backend(), 'exclude_store', None)
    if exclude_store:
        exclude_store(_store())
    key = _repo_key(folder)
    status = base_status(folder)
    retrying_failed = status['state'] == 'failed'
    if status['state'] == 'ready':
        if on_done:
            _call_callback(on_done, status)
        backend = _get_backend()
        if hasattr(backend, 'base_needs_refresh'):
            with _build_lock:
                thread = _build_threads.get(key)
                if thread is None or not thread.is_alive():
                    thread = threading.Thread(target=_check_base_refresh,
                                              args=(folder, key, status['version']),
                                              name='studio-workspace-refresh-' + key[:8], daemon=True)
                    _build_threads[key] = thread
                    thread.start()
        return status
    if status['state'] == 'failed':
        failed_at = float((_read_json(_base_state_path(key), {}) or {}).get('failedAt') or 0)
        if not retry_failed and time.time() - failed_at < 300:
            if on_done:
                _call_callback(on_done, status)
            return status
    if on_done:
        with _build_lock:
            if key not in _build_threads or not _build_threads[key].is_alive():
                _build_callbacks[key] = []
            _build_callbacks.setdefault(key, []).append(on_done)
    with _build_lock:
        thread = _build_threads.get(key)
        if thread is None or not thread.is_alive():
            thread = threading.Thread(target=_build_base, args=(folder, key, None),
                                      name='studio-workspace-base-' + key[:8], daemon=True)
            _build_threads[key] = thread
            thread.start()
    current = base_status(folder)
    if current['state'] == 'failed' and retrying_failed:
        return {'state': 'building', 'version': current.get('version'), 'error': None}
    return current


def _call_callback(callback, result):
    try:
        callback(result)
    except BaseException:
        pass


def _check_base_refresh(root: Path, key: str, expected_version: str):
    try:
        state = _read_json(_base_state_path(key), {}) or {}
        if (state.get('state') == 'ready' and state.get('version') == expected_version
                and _get_backend().base_needs_refresh(root, state)):
            _build_base(root, key, expected_version)
    except BaseException as exc:
        state = _read_json(_base_state_path(key), {}) or {}
        if state.get('state') == 'ready':
            state['refreshError'] = str(exc)[:2000]
            state['refreshFailedAt'] = time.time()
            _write_json(_base_state_path(key), state)
    finally:
        with _build_lock:
            if _build_threads.get(key) is threading.current_thread():
                _build_threads.pop(key, None)


def _build_base(root: Path, key: str, refresh_from=None):
    result = {}
    staging = None
    base_root = _base_dir(key)
    try:
        with _file_lock(base_root / '.build.lock'):
            prior = _read_json(_base_state_path(key), {}) or {}
            if (prior.get('schema') == 2 and prior.get('state') == 'ready'
                    and Path(prior.get('image', '')).exists()
                    and (refresh_from is None or prior.get('version') != refresh_from)):
                result = prior
            else:
                version = f'v-{time.time_ns()}'
                if refresh_from is None:
                    _write_json(_base_state_path(key), {
                        'schema': 2, 'state': 'building', 'repoRoot': str(root), 'repoKey': key,
                        'version': version, 'error': None, 'startedAt': time.time(),
                    })
                else:
                    prior['refreshingVersion'] = version
                    _write_json(_base_state_path(key), prior)
                backend = _get_backend()
                staging = backend.open_base_staging(root, key, version)
                excludes = _workspace_excludes(root)
                token = staging.get('token')
                backend.copy_base_tree(root, Path(staging['root']), excludes=excludes)
                for _pass in range(12):
                    delta = backend.sync_delta(root, Path(staging['root']), token, excludes=excludes)
                    if not isinstance(delta, dict):
                        delta = {'token': delta, 'changedPaths': []}
                    token = delta.get('token', token)
                    if not delta.get('changedPaths') and not delta.get('scanPaths'):
                        break
                sealed = backend.seal_base(staging)
                result = {
                    'schema': 2, 'state': 'ready', 'repoRoot': str(root), 'repoKey': key,
                    'version': version, 'error': None,
                    'image': str(sealed['image']),
                    'versionPath': str(sealed['versionPath']),
                    'token': token,
                    'excludes': list(excludes),
                    'createdAt': time.time(),
                }
                _write_json(_base_dir(key) / 'versions' / version / 'version.json', result)
                _write_json(_base_state_path(key), result)
                try:
                    _prune_base_versions(key)
                except BaseException as exc:
                    result['pruneError'] = str(exc)[:1000]
                    _write_json(_base_state_path(key), result)
    except BaseException as exc:
        current = _read_json(_base_state_path(key), {}) or {}
        if refresh_from and current.get('state') == 'ready':
            result = {key: value for key, value in current.items() if key != 'refreshingVersion'}
            result.update({'refreshError': str(exc)[:2000], 'refreshFailedAt': time.time()})
            _write_json(_base_state_path(key), result)
        else:
            result = {**current, 'state': 'failed', 'error': str(exc)[:2000], 'failedAt': time.time()}
            _write_json(_base_state_path(key), result)
        if staging:
            try:
                _get_backend().remove_base_version(Path(staging['versionPath']))
            except BaseException:
                pass
    finally:
        with _build_lock:
            callbacks = _build_callbacks.pop(key, [])
            _build_threads.pop(key, None)
    for callback in callbacks:
        _call_callback(callback, {key: result.get(key) for key in ('state', 'version', 'error')})


def _schedule_base_refresh(root: Path, version: str):
    key = _repo_key(root)
    with _build_lock:
        thread = _build_threads.get(key)
        if thread is not None and thread.is_alive():
            return
        thread = threading.Thread(target=_build_base, args=(root, key, version),
                                  name='studio-workspace-refresh-' + key[:8], daemon=True)
        _build_threads[key] = thread
        thread.start()


def _prune_base_versions(key: str):
    state = _read_json(_base_state_path(key), {}) or {}
    used = set()
    for path in (_store() / 'agents').glob('*/agent.json'):
        value = _read_json(path, {}) or {}
        if value.get('repoKey') == key and value.get('baseVersion'):
            used.add(value['baseVersion'])
    versions = _base_dir(key) / 'versions'
    if not versions.exists():
        return
    for candidate in versions.iterdir():
        if candidate.name in used or candidate.name == state.get('version') or candidate.name == state.get('refreshingVersion'):
            continue
        _get_backend().remove_base_version(candidate)


def _base_metadata(key: str, version: str):
    path = _base_dir(key) / 'versions' / version / 'version.json'
    value = _read_json(path, {}) or {}
    return value if value.get('schema') == 2 and value.get('version') == version else None


def _sync_to_latest(root: Path, target: Path, token, excludes):
    current = token
    changed_paths = set()
    scan_paths = set()
    result = {'token': token, 'changedPaths': [], 'scanPaths': []}
    for _pass in range(12):
        delta = _get_backend().sync_delta(root, target, current, excludes=excludes)
        if not isinstance(delta, dict):
            delta = {'token': delta, 'changedPaths': []}
        current = delta.get('token', current)
        changed_paths.update(delta.get('changedPaths') or [])
        scan_paths.update(delta.get('scanPaths') or [])
        flags = {key: (result.get(key, False) or value if isinstance(value, bool) else value)
                 for key, value in delta.items()
                 if key not in {'token', 'changedPaths', 'scanPaths'}}
        result = {
            **result,
            **flags,
            'token': current,
            'changedPaths': sorted(changed_paths),
            'scanPaths': sorted(scan_paths),
        }
        if not delta.get('changedPaths') and not delta.get('scanPaths'):
            break
    return result


def create_workspace(root, agent_id) -> dict[str, str]:
    folder = _root_path(root)
    agent_id = _safe_id(agent_id)
    key = _repo_key(folder)
    agent_dir = _agent_dir(agent_id)
    state_path = _agent_state_path(agent_id)
    mount = _mount_path(agent_id)
    with _file_lock(agent_dir / '.workspace.lock'):
        state = _read_json(state_path, {}) or {}
        if state and state.get('schema') != 2:
            _get_backend().unmount_workspace(mount, force=False)
            _get_backend().remove_layer(agent_dir)
            shutil.rmtree(mount, ignore_errors=True)
            state = {}
        if state and state.get('repoKey') != key:
            raise ValueError('Workspace identifier is already used by another folder')
        if state.get('state') in {'ready', 'archived'}:
            already_ready = True
        else:
            already_ready = False
            base = (_base_metadata(key, state.get('baseVersion'))
                    if state.get('baseVersion') else None)
            if base is None:
                base = _read_json(_base_state_path(key), {}) or {}
                if state.get('image'):
                    _get_backend().unmount_workspace(mount, force=False)
                    _get_backend().remove_layer(agent_dir)
                    agent_dir.mkdir(parents=True, exist_ok=True)
                if base.get('state') != 'ready':
                    raise RuntimeError('Image workspace base is not ready')
                state = {
                    'schema': 2, 'repoRoot': str(folder), 'repoKey': key, 'agentId': agent_id,
                    'baseVersion': base['version'], 'baseImage': base['image'],
                    'image': None, 'mount': str(mount), 'state': 'creating',
                    'createdAt': time.time(),
                }
                _write_json(state_path, state)
            if base.get('state') != 'ready':
                raise RuntimeError('Reserved image workspace base is not ready')
            if not state.get('image'):
                image = _get_backend().clone_workspace(Path(base['image']), agent_dir)
                state['image'] = str(image)
                state['baseImage'] = base['image']
                _write_json(state_path, state)
            _get_backend().mount_workspace(Path(state['image']), mount,
                                          base_image=Path(state.get('baseImage') or base['image']))
            workspace_root = mount / 'repo'
            excludes = tuple(base.get('excludes') or _workspace_excludes(folder))
            delta = _sync_to_latest(folder, workspace_root, base.get('token'), excludes)
            state.update({'path': str(workspace_root), 'mount': str(mount),
                          'token': delta.get('token', base.get('token')),
                          'state': 'ready', 'mounted': True})
            _write_json(state_path, state)
            if delta.get('refreshBase'):
                _schedule_base_refresh(folder, base['version'])
    if already_ready:
        return ensure_mounted(agent_id)
    return {'mount': str(mount), 'path': str(mount / 'repo')}


def ensure_mounted(agent_id) -> dict[str, str]:
    agent_id = _safe_id(agent_id)
    state = _read_json(_agent_state_path(agent_id), {}) or {}
    if not state or not state.get('image'):
        raise ValueError('Unknown image workspace')
    mount = Path(state.get('mount') or _mount_path(agent_id))
    with _file_lock(_agent_dir(agent_id) / '.workspace.lock'):
        _get_backend().mount_workspace(Path(state['image']), mount,
                                       base_image=Path(state.get('baseImage') or _base_metadata(
                                           state['repoKey'], state['baseVersion'])['image']))
        if state.get('state') == 'archived':
            state['state'] = 'ready'
        state['mounted'] = True
        state['path'] = str(mount / 'repo')
        _write_json(_agent_state_path(agent_id), state)
    return {'mount': str(mount), 'path': str(mount / 'repo')}


def archive_workspace(agent_id) -> dict[str, Any]:
    agent_id = _safe_id(agent_id)
    state = _read_json(_agent_state_path(agent_id), {}) or {}
    if not state:
        return {'freedBytes': 0, 'state': 'removed'}
    with _file_lock(_agent_dir(agent_id) / '.workspace.lock'):
        mount = Path(state.get('mount') or _mount_path(agent_id))
        _get_backend().unmount_workspace(mount, force=True)
        state['state'] = 'archived'
        state['mounted'] = False
        _write_json(_agent_state_path(agent_id), state)
    return {'freedBytes': 0, 'state': 'archived'}


def remove_workspace(agent_id, *, force=False) -> dict[str, Any]:
    del force
    agent_id = _safe_id(agent_id)
    agent_dir = _agent_dir(agent_id)
    state = _read_json(_agent_state_path(agent_id), {}) or {}
    if not state:
        return {'freedBytes': 0, 'state': 'removed'}
    with _file_lock(agent_dir / '.workspace.lock'):
        before = workspace_bytes(agent_id)
        mount = Path(state.get('mount') or _mount_path(agent_id))
        _get_backend().unmount_workspace(mount, force=True)
        _get_backend().remove_layer(agent_dir)
        shutil.rmtree(mount, ignore_errors=True)
        if state.get('repoKey'):
            _prune_base_versions(state['repoKey'])
    return {'freedBytes': before or 0, 'state': 'removed'}


def _allocated_bytes(path: Path):
    if not path.exists():
        return 0
    total = 0
    for current, dirs, files in os.walk(path, followlinks=False):
        for name in dirs + files:
            entry = Path(current) / name
            try:
                info = entry.lstat()
            except OSError:
                continue
            if not entry.is_dir() or entry.is_symlink():
                total += getattr(info, 'st_blocks', 0) * 512 or info.st_size
    return total


def workspace_bytes(agent_id) -> int:
    state = _read_json(_agent_state_path(_safe_id(agent_id)), {}) or {}
    if not state or not state.get('image'):
        return 0
    result = _get_backend().private_bytes(Path(state['image']))
    return int(result) if result is not None else _allocated_bytes(Path(state['image']))


def base_bytes(root) -> int:
    folder = _root_path(root)
    state = _read_json(_base_state_path(_repo_key(folder)), {}) or {}
    if not state.get('image'):
        return 0
    image = Path(state['image'])
    result = _get_backend().private_bytes(image)
    return int(result) if result is not None else _allocated_bytes(image)


def list_workspaces() -> list[dict[str, Any]]:
    results = []
    for path in (_store() / 'agents').glob('*/agent.json'):
        value = _read_json(path, {})
        if value:
            results.append(value)
    return results


def exec_prefix() -> list[str]:
    return _get_backend().exec_prefix()
