"""Platform-neutral image workspace engine.

Platform backends implement :class:`WorkspaceBackend`. Git object setup and
agent commit collection stay in this module so every backend has identical
merge semantics.
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import subprocess
import sys
import threading
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Protocol


class WorkspaceBackend(Protocol):
    """Storage operations required by the common workspace engine."""

    def supported(self, repo_root: Path) -> tuple[bool, str]: ...

    def current_event_id(self, repo_root: Path) -> Any: ...

    def open_base_staging(self, repo_root: Path, repo_key: str, version: str) -> dict[str, Any]: ...

    def copy_base_tree(self, repo_root: Path, destination: Path, *, excludes: tuple[str, ...]) -> None: ...

    def seal_base(self, staging: dict[str, Any]) -> dict[str, Any]: ...

    def clone_workspace(self, base_image: Path, agent_dir: Path) -> Path: ...

    def mount_workspace(self, layer: Path, mount: Path, *,
                        base_image: Path | None = None) -> dict[str, Any]: ...

    def sync_delta(self, repo_root: Path, target_repo: Path, token: Any, *,
                   excludes: tuple[str, ...]) -> Any: ...

    def unmount_workspace(self, mount: Path, *, force: bool = False) -> None: ...

    def remove_layer(self, agent_dir: Path) -> None: ...

    def remove_base_version(self, path: Path) -> None: ...

    def private_bytes(self, path: Path) -> int: ...

    def exec_prefix(self) -> list[str]: ...


def _backend() -> WorkspaceBackend:
    """Load the host backend lazily so importing this module is portable."""
    import sys

    if sys.platform == "darwin":
        from codex_workspace_macos import Backend
    elif sys.platform.startswith("linux"):
        from codex_workspace_linux import Backend
    else:
        raise RuntimeError("Image workspaces are not supported on this platform")
    return Backend()


_STORE_OVERRIDE = 'CODEX_WORKSPACE_STORE'
_DEFAULT_STORE = Path.home() / '.local' / 'state' / 'codex-agents' / 'workspaces'
_backend_instance = None
_backend_lock = threading.Lock()
_build_lock = threading.Lock()
_build_threads: dict[str, threading.Thread] = {}
_build_callbacks: dict[str, list] = {}
_BASE_REFRESH_DELTA_RATIO = 0.35


def _get_backend():
    global _backend_instance
    if _backend_instance is None:
        with _backend_lock:
            if _backend_instance is None:
                _backend_instance = _backend()
    return _backend_instance


def _store() -> Path:
    return Path(os.environ.get(_STORE_OVERRIDE) or _DEFAULT_STORE).expanduser().resolve()


def _repo_root(path) -> Path:
    result = subprocess.run(['git', '-C', str(path), 'rev-parse', '--show-toplevel'],
                            capture_output=True, text=True, timeout=30)
    if result.returncode == 0:
        return Path(result.stdout.strip()).resolve()
    return Path(path).expanduser().resolve()


def _repo_key(root: Path) -> str:
    return hashlib.sha256(os.fsencode(root.resolve())).hexdigest()[:32]


def _workspace_excludes(repo_root: Path) -> tuple[str, ...]:
    root = Path(repo_root).resolve()
    values = {'.worktrees'}
    try:
        store_relative = _store().relative_to(root)
    except ValueError:
        pass
    else:
        values.add(store_relative.as_posix())
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


def supported(repo_root) -> tuple[bool, str]:
    return _get_backend().supported(Path(repo_root).expanduser().resolve())


def base_status(repo_root) -> dict[str, Any]:
    root = _repo_root(repo_root)
    state = _read_json(_base_state_path(_repo_key(root)), {})
    if not state:
        return {'state': 'missing', 'version': None, 'error': None}
    return {key: state.get(key) for key in ('state', 'version', 'error')}


def start_base_build(repo_root, on_done=None) -> dict[str, Any]:
    root = _repo_root(repo_root)
    ok, reason = supported(root)
    if not ok:
        result = {'state': 'failed', 'version': None, 'error': reason}
        if on_done:
            _call_callback(on_done, result)
        return result
    exclude_store = getattr(_get_backend(), 'exclude_store', None)
    if exclude_store:
        exclude_store(_store())
    key = _repo_key(root)
    status = base_status(root)
    if status['state'] == 'ready':
        if on_done:
            _call_callback(on_done, status)
        backend = _get_backend()
        if hasattr(backend, 'base_needs_refresh'):
            with _build_lock:
                thread = _build_threads.get(key)
                if thread is None or not thread.is_alive():
                    thread = threading.Thread(target=_check_base_refresh, args=(root, key, status['version']),
                                              name='studio-workspace-refresh-' + key[:8], daemon=True)
                    _build_threads[key] = thread
                    thread.start()
        return status
    if status['state'] == 'failed':
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
            thread = threading.Thread(target=_build_base, args=(root, key, None),
                                      name='studio-workspace-base-' + key[:8], daemon=True)
            _build_threads[key] = thread
            thread.start()
    return base_status(root)


def _check_base_refresh(root: Path, key: str, expected_version: str):
    try:
        state = _read_json(_base_state_path(key), {}) or {}
        if (state.get('state') == 'ready' and state.get('version') == expected_version
                and _get_backend().base_needs_refresh(root, state)):
            _build_base(root, key, expected_version)
            return
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


def _call_callback(callback, result):
    try:
        callback(result)
    except BaseException:
        # A runtime observer must not turn a completed base build into failure.
        pass


def _build_base(root: Path, key: str, refresh_from=None):
    callbacks = []
    base_root = _base_dir(key)
    staging = None
    protected = []
    base_id = None
    try:
        with _file_lock(base_root / '.build.lock'):
            state = _read_json(_base_state_path(key), {})
            if (state.get('state') == 'ready' and Path(state.get('image', '')).exists()
                    and (refresh_from is None or state.get('version') != refresh_from)):
                result = state
            else:
                if state.get('state') == 'building' and state.get('version'):
                    stale_path = base_root / 'versions' / state['version']
                    try:
                        _get_backend().remove_base_version(stale_path)
                    except BaseException:
                        pass
                    stale_ref = 'refs/studio/base/' + key + '-' + state['version'][2:]
                    for _rel, user_repo in _git_repositories(root):
                        _git(user_repo, 'update-ref', '-d', stale_ref, check=False)
                if state.get('refreshingVersion'):
                    stale_path = base_root / 'versions' / state['refreshingVersion']
                    try:
                        _get_backend().remove_base_version(stale_path)
                    except BaseException:
                        pass
                    stale_ref = 'refs/studio/base/' + key + '-' + state['refreshingVersion'][2:]
                    for _rel, user_repo in _git_repositories(root):
                        _git(user_repo, 'update-ref', '-d', stale_ref, check=False)
                version = f'v-{time.time_ns()}'
                base_id = key + '-' + version[2:]
                backend = _get_backend()
                if refresh_from is None:
                    _write_json(_base_state_path(key), {
                        'state': 'building', 'repoRoot': str(root), 'repoKey': key,
                        'version': version, 'error': None, 'pid': os.getpid(),
                        'startedAt': time.time(), 'token': None
                    })
                else:
                    state['refreshingVersion'] = version
                    _write_json(_base_state_path(key), state)
                source_repos = _git_repositories(root)
                ref = 'refs/studio/base/' + base_id
                # Pin the current heads before a long tree copy can overlap git gc.
                for rel, user_repo in source_repos:
                    head = _git(user_repo, 'rev-parse', 'HEAD').strip()
                    _git(user_repo, 'update-ref', ref, head)
                    protected.append({'path': str(rel), 'head': head, 'ref': ref})
                if refresh_from and state.get('state') == 'ready':
                    staging = backend.open_base_refresh(root, key, version, state)
                else:
                    staging = backend.open_base_staging(root, key, version)
                token = staging.get('token')
                if refresh_from is None:
                    _write_json(_base_state_path(key), {
                        'state': 'building', 'repoRoot': str(root), 'repoKey': key,
                        'version': version, 'error': None, 'pid': os.getpid(),
                        'startedAt': time.time(), 'token': token
                    })
                base_repo = Path(staging['root'])
                if staging.get('refresh'):
                    token = backend.sync_delta(root, base_repo, token,
                                               excludes=_workspace_excludes(root))
                    staging['token'] = token
                else:
                    backend.copy_base_tree(root, base_repo, excludes=_workspace_excludes(root))
                for rel, user_repo in source_repos:
                    base_repo_path = base_repo / rel
                    if not _is_git_repo(base_repo_path):
                        continue
                    _prepare_repo(base_repo_path, user_repo, refresh=True)
                    copied_head = _git(base_repo_path, 'rev-parse', 'HEAD').strip()
                    _git(user_repo, 'update-ref', ref, copied_head)
                    entry = next((value for value in protected if value['path'] == str(rel)), None)
                    if entry:
                        entry['head'] = copied_head
                sealed = backend.seal_base(staging)
                image = str(sealed['image'])
                result = {
                    'state': 'ready', 'repoRoot': str(root), 'repoKey': key,
                    'version': version, 'error': None, 'image': image,
                    'versionPath': str(sealed['versionPath']),
                    'head': _git(root, 'rev-parse', 'HEAD').strip() if _is_git_repo(root) else None,
                    'token': sealed.get('token', token), 'protectedRefs': protected,
                    'baseId': base_id, 'createdAt': time.time(),
                }
                _write_json(_base_state_path(key), result)
                _write_json(Path(sealed['versionPath']) / 'version.json', result)
                # Older versions are retained while an agent refers to them.
                try:
                    _prune_base_versions(key)
                except BaseException as exc:
                    result['pruneError'] = str(exc)[:1000]
                    _write_json(_base_state_path(key), result)
    except BaseException as exc:
        current = _read_json(_base_state_path(key), {})
        if refresh_from and current.get('state') == 'ready':
            result = {key: value for key, value in current.items() if key != 'refreshingVersion'}
            result.update({'refreshError': str(exc)[:2000], 'refreshFailedAt': time.time()})
        else:
            result = {**current, 'state': 'failed', 'error': str(exc)[:2000], 'failedAt': time.time()}
        _write_json(_base_state_path(key), result)
        if staging:
            try:
                _get_backend().remove_base_version(Path(staging['versionPath']))
            except BaseException:
                pass
        if base_id:
            for item in protected:
                _git(Path(root) / item['path'], 'update-ref', '-d', item['ref'], check=False)
    finally:
        with _build_lock:
            callbacks = _build_callbacks.pop(key, [])
            _build_threads.pop(key, None)
    for callback in callbacks:
        _call_callback(callback, {key: result.get(key) for key in ('state', 'version', 'error')})


def _prune_base_versions(key):
    # Keep the current base. Retention is deliberately conservative because
    # agent layers can still refer to older versions after a refresh.
    root = _base_dir(key)
    current = _read_json(_base_state_path(key), {})
    current_version = current.get('version')
    used = set()
    for agent_state in (_store() / 'agents').glob('*/agent.json'):
        value = _read_json(agent_state, {}) or {}
        if value.get('repoKey') == key and value.get('baseVersion'):
            used.add(value['baseVersion'])
    versions = root / 'versions'
    if not versions.exists():
        return
    for candidate in versions.iterdir():
        if candidate.name == current_version or candidate.name in used:
            continue
        metadata = _read_json(candidate / 'version.json', {}) or {}
        for item in metadata.get('protectedRefs', []):
            user_repo = Path(metadata.get('repoRoot', '')) / item.get('path', '.')
            if _is_git_repo(user_repo):
                _git(user_repo, 'update-ref', '-d', item['ref'], check=False)
        _get_backend().remove_base_version(candidate)


def _command(args, *, view=False, check=False, timeout=120, input=None):
    prefix = _get_backend().exec_prefix() if view else []
    result = subprocess.run([*prefix, *args], input=input,
                            capture_output=True, timeout=timeout, check=False)
    if check and result.returncode:
        detail = result.stderr.decode(errors='replace')
        raise RuntimeError(f"command {' '.join(map(str, args))} failed: {detail[-3000:]}")
    return result


def _git(repo: Path, *args, check=True, timeout=120, input=None, view=False):
    result = _command(['git', '-C', str(repo), *args], input=input,
                      timeout=timeout, view=view)
    if check and result.returncode:
        raise RuntimeError(f"git {' '.join(args)} failed in {repo}: " +
                           result.stderr.decode(errors='replace')[-3000:])
    return result.stdout.decode(errors='surrogateescape')


def _is_git_repo(path: Path, *, view=False) -> bool:
    result = _command(['git', '-C', str(path), 'rev-parse', '--show-toplevel'],
                      timeout=20, view=view)
    return result.returncode == 0


def _git_repositories(root: Path):
    root = Path(root).resolve()
    results = []
    excluded = {Path(value) for value in _workspace_excludes(root)}
    if _is_git_repo(root):
        results.append((Path('.'), root))
    for current, dirs, files in os.walk(root, followlinks=False):
        here = Path(current)
        rel_here = here.relative_to(root)
        is_excluded = any(rel_here == item or item in rel_here.parents for item in excluded)
        if not is_excluded and ('.git' in dirs or '.git' in files):
            if here != root and _is_git_repo(here):
                try:
                    top = Path(_git(here, 'rev-parse', '--show-toplevel').strip()).resolve()
                    if top == here.resolve():
                        results.append((here.relative_to(root), here))
                except (RuntimeError, ValueError):
                    pass
        dirs[:] = [name for name in dirs if name != '.git' and name not in {'node_modules', '.cache'}
                   and not any((rel_here / name) == item or item in (rel_here / name).parents
                               for item in excluded)]
    return list(dict.fromkeys(results))


def _object_dir(repo: Path) -> Path:
    value = _git(repo, 'rev-parse', '--path-format=absolute', '--git-path', 'objects').strip()
    return Path(value).resolve()


def _prepare_repo(target: Path, source: Path, *, refresh=False, view=False):
    objects = Path(_git(target, 'rev-parse', '--path-format=absolute', '--git-path', 'objects',
                        view=view).strip())
    alternate = objects / 'info' / 'alternates'
    source_objects = _object_dir(source)
    if view:
        code = ('import pathlib,sys; p=pathlib.Path(sys.argv[1]); '
                'p.parent.mkdir(parents=True,exist_ok=True); p.write_text(sys.argv[2])')
        _command([sys.executable, '-c', code, str(alternate), str(source_objects) + '\n'],
                 view=True, check=True)
    else:
        alternate.parent.mkdir(parents=True, exist_ok=True)
        alternate.write_text(str(source_objects) + '\n')
    _git(target, 'config', 'core.checkStat', 'minimal', view=view)
    _git(target, 'config', 'core.trustctime', 'false', view=view)
    if refresh:
        _git(target, 'status', '--porcelain', timeout=300, view=view)


def _base_for_agent(state):
    value = _read_json(_base_state_path(state['repoKey']), {}) or {}
    if value.get('version') != state.get('baseVersion'):
        # Keep agent's exact version in the state and locate it by version name.
        base = _base_dir(state['repoKey']) / 'versions' / str(state.get('baseVersion'))
        return base / 'base.asif'
    return Path(value['image'])


def _repo_state_list(root: Path, mount_repo: Path):
    items = []
    for rel, source in _git_repositories(root):
        target = mount_repo / rel
        if _is_git_repo(target, view=True):
            items.append({'path': str(rel), 'startCommit': _git(target, 'rev-parse', 'HEAD', view=True).strip(),
                          'snapshotCommit': None, 'branch': None})
    return items


def create_workspace(repo_root, agent_id, *, start_commit=None) -> dict[str, Any]:
    root = _repo_root(repo_root)
    agent_id = _safe_id(agent_id)
    key = _repo_key(root)
    state_path = _agent_state_path(agent_id)
    mount = _mount_path(agent_id)
    agent_dir = _agent_dir(agent_id)
    with _file_lock(agent_dir / '.workspace.lock'):
        state = _read_json(state_path, {}) or {}
        if state and state.get('repoKey') != key:
            raise ValueError('Agent id is already reserved for another repository')
        if state.get('state') == 'ready':
            already_ready = True
        else:
            already_ready = False
        if already_ready:
            # Release the reservation lock before ensure_mounted takes it.
            pass
        else:
            base = _read_json(_base_state_path(key), {}) or {}
            if base.get('state') != 'ready':
                raise RuntimeError('Image workspace base is not ready')
            if not state:
                state = {
                    'repoRoot': str(root), 'repoKey': key, 'agentId': agent_id,
                    'baseVersion': base['version'], 'baseImage': base['image'],
                    'startCommit': start_commit, 'snapshotCommit': None,
                    'branch': 'codex-agent/' + agent_id, 'image': None,
                    'mount': str(mount), 'state': 'creating', 'repositories': [],
                    'createdAt': time.time(),
                }
                _write_json(state_path, state)
            image = _get_backend().clone_workspace(Path(state['baseImage']), agent_dir)
            state['image'] = str(image)
            _write_json(state_path, state)
            _get_backend().mount_workspace(image, mount, base_image=Path(state['baseImage']))
            mount_repo = mount / 'repo'
            token = base.get('token')
            excludes = _workspace_excludes(root)
            state['token'] = _get_backend().sync_delta(root, mount_repo, token, excludes=excludes)
            _write_json(state_path, state)
            repos = _repo_state_list(root, mount_repo)
            for item in repos:
                rel = Path(item['path'])
                target = mount_repo / rel
                source = root / rel
                _prepare_repo(target, source, view=True)
                start = start_commit if rel == Path('.') and start_commit else _git(target, 'rev-parse', 'HEAD', view=True).strip()
                item['startCommit'] = start
                item['branch'] = 'codex-agent/' + agent_id
                verify = _command(['git', '-C', str(target), 'show-ref', '--verify', '--quiet',
                                   'refs/heads/' + item['branch']], view=True, timeout=30)
                if verify.returncode == 0:
                    _git(target, 'checkout', item['branch'], view=True)
                    if _git(target, 'rev-parse', 'HEAD', view=True).strip() != start:
                        message = _git(target, 'show', '-s', '--format=%s', 'HEAD', view=True).strip()
                        parents = _git(target, 'rev-list', '--parents', '-n', '1', 'HEAD', view=True).split()
                        if message == 'studio snapshot' and len(parents) == 2 and parents[1] == start:
                            item['snapshotCommit'] = parents[0]
                else:
                    _git(target, 'checkout', '-b', item['branch'], start, view=True)
                if start_commit and rel == Path('.'):
                    _git(target, 'reset', '--hard', start_commit, view=True)
                _git(target, 'add', '-A', view=True)
                staged = _command(['git', '-C', str(target), 'diff', '--cached', '--quiet', 'HEAD', '--'],
                                  view=True, timeout=60)
                if staged.returncode == 1:
                    _git(target, '-c', 'user.name=Codex Studio', '-c', 'user.email=studio@localhost',
                         'commit', '-m', 'studio snapshot', timeout=300, view=True)
                    item['snapshotCommit'] = _git(target, 'rev-parse', 'HEAD', view=True).strip()
            state.update({'repositories': repos, 'snapshotCommit': next(
                (x['snapshotCommit'] for x in repos if x['path'] == '.'), None),
                'startCommit': start_commit or next((x['startCommit'] for x in repos if x['path'] == '.'), None),
                'mount': str(mount), 'repoPath': str(mount_repo), 'state': 'ready',
                'mounted': True})
            _write_json(state_path, state)
    if already_ready:
        return ensure_mounted(agent_id)
    return {'mount': str(mount), 'repoPath': str(mount / 'repo'), 'branch': state['branch'],
            'startCommit': state['startCommit'], 'snapshotCommit': state['snapshotCommit']}


def ensure_mounted(agent_id) -> dict[str, Any]:
    agent_id = _safe_id(agent_id)
    state = _read_json(_agent_state_path(agent_id), {}) or {}
    if not state or not state.get('image'):
        raise ValueError('Unknown image workspace')
    mount = Path(state.get('mount') or _mount_path(agent_id))
    with _file_lock(_agent_dir(agent_id) / '.workspace.lock'):
        mounted = _get_backend().mount_workspace(Path(state['image']), mount,
                                                   base_image=Path(state.get('baseImage') or _base_for_agent(state)))
        state['state'] = 'ready'
        state['mounted'] = True
        state['repoPath'] = str(mount / 'repo')
        _write_json(_agent_state_path(agent_id), state)
    return {'mount': str(mount), 'repoPath': str(mount / 'repo'), 'branch': state.get('branch'),
            'startCommit': state.get('startCommit'), 'snapshotCommit': state.get('snapshotCommit')}


def _fetch_and_replay(user_repo: Path, agent_repo: Path, branch: str, snapshot: str | None, agent_id: str):
    raw_ref = f'refs/studio/agents/{agent_id}/raw'
    result_ref = f'refs/heads/codex-agent/{agent_id}'
    _git(user_repo, 'fetch', str(agent_repo), f'{branch}:{raw_ref}', timeout=300, view=True)
    original_head = _git(user_repo, 'rev-parse', 'HEAD').strip()
    current = original_head
    raw_tip = _git(user_repo, 'rev-parse', raw_ref).strip()
    if snapshot:
        commits = _git(user_repo, 'rev-list', '--reverse', f'{snapshot}..{raw_tip}').split()
    else:
        commits = _git(user_repo, 'rev-list', '--reverse', f'{original_head}..{raw_tip}').split()
    for commit in commits:
        parent = _git(user_repo, 'rev-parse', f'{commit}^').strip()
        tree_result = subprocess.run(['git', '-C', str(user_repo), 'merge-tree', '--write-tree',
                                      '--merge-base', parent, current, commit],
                                     capture_output=True, text=True, timeout=300)
        if tree_result.returncode:
            return {'state': 'conflict', 'conflict': tree_result.stdout[-5000:],
                    'rawRef': raw_ref, 'branch': result_ref}
        tree = tree_result.stdout.splitlines()[0].strip()
        message = _git(user_repo, 'show', '-s', '--format=%B', commit).encode()
        new_commit = _git(user_repo, 'commit-tree', tree, '-p', current, input=message).strip()
        current = new_commit
    if current != original_head:
        update = subprocess.run(['git', '-C', str(user_repo), 'update-ref', result_ref,
                                 current, _git(user_repo, 'rev-parse', result_ref, check=False).strip() or '0' * 40],
                                capture_output=True, text=True)
        if update.returncode:
            # First update has a zero old value; retry as create only if still absent.
            check = subprocess.run(['git', '-C', str(user_repo), 'show-ref', '--verify', '--quiet', result_ref])
            if check.returncode:
                _git(user_repo, 'update-ref', result_ref, current)
            else:
                raise RuntimeError(update.stderr.strip())
    return {'state': 'collected', 'branch': result_ref, 'rawRef': raw_ref,
            'commit': current, 'commits': len(commits)}


def collect(agent_id) -> dict[str, Any]:
    agent_id = _safe_id(agent_id)
    state = _read_json(_agent_state_path(agent_id), {}) or {}
    if not state:
        raise ValueError('Unknown image workspace')
    mount = Path(state.get('mount') or _mount_path(agent_id))
    ensure_mounted(agent_id)
    with _file_lock(_agent_dir(agent_id) / '.workspace.lock'):
        result = {'state': 'collected', 'repositories': []}
        for item in sorted(state.get('repositories', []), key=lambda row: len(Path(row['path']).parts), reverse=True):
            rel = Path(item['path'])
            user_repo = Path(state['repoRoot']) / rel
            agent_repo = mount / 'repo' / rel
            if not _is_git_repo(user_repo) or not _is_git_repo(agent_repo, view=True):
                continue
            nested = _fetch_and_replay(user_repo, agent_repo, item['branch'],
                                       item.get('snapshotCommit'), agent_id)
            result['repositories'].append({'path': item['path'], **nested})
            if nested['state'] == 'conflict':
                return {'state': 'conflict', 'conflict': nested.get('conflict'),
                        'rawRef': nested['rawRef'], 'repositories': result['repositories']}
        result['branch'] = 'codex-agent/' + agent_id
        return result


def remove_workspace(agent_id, *, force=False) -> dict[str, Any]:
    agent_id = _safe_id(agent_id)
    agent_dir = _agent_dir(agent_id)
    state_path = _agent_state_path(agent_id)
    state = _read_json(state_path, {}) or {}
    if not state:
        return {'freedBytes': 0, 'state': 'removed'}
    with _file_lock(agent_dir / '.workspace.lock'):
        mount = Path(state.get('mount') or _mount_path(agent_id))
        before = workspace_bytes(agent_id)
        _get_backend().unmount_workspace(mount, force=force)
        _get_backend().remove_layer(agent_dir)
        cleanup = ('import pathlib,shutil,sys; '
                   'shutil.rmtree(pathlib.Path(sys.argv[1]),ignore_errors=True)')
        _command([sys.executable, '-c', cleanup, str(mount)], view=True, timeout=60)
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


def base_bytes(repo_root) -> int:
    state = _read_json(_base_state_path(_repo_key(_repo_root(repo_root))), {}) or {}
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
