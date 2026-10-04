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


def _workspace_excludes(repo_root: Path, repositories=()) -> tuple[str, ...]:
    root = Path(repo_root).resolve()
    values = {'.worktrees'}
    try:
        store_relative = _store().relative_to(root)
    except ValueError:
        pass
    else:
        values.add(store_relative.as_posix())
    for _relative_repo, repo in repositories:
        marker_path = Path(_relative_repo) / '.git'
        values.add(marker_path.as_posix())
        try:
            object_path = _object_dir(repo).relative_to(root)
        except (RuntimeError, ValueError):
            continue
        values.add(object_path.as_posix())
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
                repository_paths = [str(rel) for rel, _repo in source_repos]
                excludes = _workspace_excludes(root, source_repos)
                ref = 'refs/studio/base/' + base_id
                # Pin the current heads before a long tree copy can overlap git gc.
                # Materialize nested repository metadata before root status visits gitlinks.
                for rel, user_repo in sorted(source_repos, key=lambda pair: len(pair[0].parts), reverse=True):
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
                    delta = backend.sync_delta(root, base_repo, token,
                                               excludes=excludes)
                    token = delta.get('token', token) if isinstance(delta, dict) else delta
                    staging['token'] = token
                else:
                    backend.copy_base_tree(root, base_repo, excludes=excludes)
                dirty_paths = {}
                for rel, user_repo in sorted(source_repos, key=lambda pair: len(pair[0].parts), reverse=True):
                    base_repo_path = base_repo / rel
                    status = _prepare_repo(base_repo_path, user_repo, refresh=True)
                    dirty_paths[str(rel)] = _status_paths(status)
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
                    'repositories': repository_paths, 'dirtyPaths': dirty_paths,
                    'excludes': list(excludes),
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
    code = r'''import pathlib,shutil,subprocess,sys
source,target=map(pathlib.Path,sys.argv[1:3]); refresh=sys.argv[3]=="1"
source_marker=source/".git"; target_marker=target/".git"
if source_marker.is_file():
    value=source_marker.read_text().strip()
    if not value.startswith("gitdir:"): raise RuntimeError("invalid source gitdir marker")
    gitdir=(source/value.split(":",1)[1].strip()).resolve()
else: gitdir=source_marker
common=gitdir
common_file=gitdir/"commondir"
if common_file.exists(): common=(gitdir/common_file.read_text().strip()).resolve()
needs_private=(refresh or not target_marker.is_dir() or (target_marker/"commondir").exists()
               or not any(target_marker.iterdir()))
if needs_private:
    if target_marker.is_dir(): shutil.rmtree(target_marker)
    elif target_marker.exists(): target_marker.unlink()
    target_marker.mkdir(parents=True,exist_ok=True)
    def copy_tree(src,dst):
        for item in src.iterdir():
            if item.name in {"objects","worktrees","commondir","gitdir"}: continue
            out=dst/item.name
            if item.is_dir() and not item.is_symlink():
                out.mkdir(exist_ok=True); copy_tree(item,out)
            elif item.is_symlink():
                if out.exists() or out.is_symlink(): out.unlink()
                out.symlink_to(item.resolve())
            else: shutil.copy2(item,out)
    copy_tree(common,target_marker)
    if gitdir != common: copy_tree(gitdir,target_marker)
else:
    source_config=common/"config"
    if source_config.is_file(): shutil.copy2(source_config,target_marker/"config")
if source_marker.is_file():
    for config in (target_marker/"config",target_marker/"config.worktree"):
        if config.is_file():
            subprocess.run(["git","config","--file",str(config),"--unset-all","core.worktree"],
                           capture_output=True,check=False)
objects=target_marker/"objects"; objects.mkdir(parents=True,exist_ok=True)
print(objects)
'''
    if view:
        objects = Path(_command([sys.executable, '-c', code, str(source), str(target),
                                 '1' if refresh else '0'],
                                view=True, check=True).stdout.decode().strip())
    else:
        objects = Path(subprocess.run([sys.executable, '-c', code, str(source), str(target),
                                       '1' if refresh else '0'],
                                      check=True, capture_output=True, text=True,
                                      timeout=60).stdout.strip())
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
        return _git(target, 'status', '--porcelain=v1', '-z', timeout=300, view=view)
    return ''


def _status_paths(output):
    """Read Git's NUL-delimited porcelain status and return changed paths."""
    paths = set()
    fields = output.split('\0')
    index = 0
    while index < len(fields):
        entry = fields[index]
        index += 1
        if len(entry) < 4:
            continue
        paths.add(entry[3:])
        if 'R' in entry[:2] or 'C' in entry[:2]:
            if index < len(fields):
                paths.add(fields[index])
                index += 1
    return sorted(paths)


def _stage_committed_changes(target: Path, old_head: str, new_head: str, *, view=False):
    if old_head == new_head:
        return
    output = _git(target, 'diff', '--no-renames', '--name-only', '-z',
                  old_head, new_head, view=view)
    paths = [value for value in output.split('\0') if value]
    # Reset only committed paths. Keep source working-tree edits for the snapshot.
    for offset in range(0, len(paths), 256):
        pathspecs = [f':(literal){value}' for value in paths[offset:offset + 256]]
        _git(target, 'reset', new_head, '--', *pathspecs, view=view)


def _sync_refs(target: Path, source: Path, agent_id: str, *, view=False):
    format_arg = '--format=%(refname) %(objectname) %(symref)'
    source_refs = _git(source, 'for-each-ref', format_arg).splitlines()
    target_refs = _git(target, 'for-each-ref', format_arg, view=view).splitlines()
    agent_ref = 'refs/heads/codex-agent/' + agent_id

    def parse(lines):
        refs = {}
        for line in lines:
            try:
                name, oid, symref = line.split(' ', 2)
            except ValueError:
                continue
            if symref or name.startswith('refs/studio/') or name == agent_ref:
                continue
            refs[name] = oid
        return refs

    source_map, target_map = parse(source_refs), parse(target_refs)
    commands = ['start']
    commands.extend(f'update {name} {oid}' for name, oid in source_map.items())
    commands.extend(f'delete {name}' for name in target_map if name not in source_map)
    commands.extend(('prepare', 'commit'))
    _git(target, 'update-ref', '--stdin', input=('\n'.join(commands) + '\n').encode(), view=view)


def _base_for_agent(state):
    value = _read_json(_base_state_path(state['repoKey']), {}) or {}
    if value.get('version') != state.get('baseVersion'):
        # Keep agent's exact version in the state and locate it by version name.
        base = _base_dir(state['repoKey']) / 'versions' / str(state.get('baseVersion'))
        return base / 'base.asif'
    return Path(value['image'])


def _repo_state_list(root: Path, mount_repo: Path, known_paths, delta):
    """Resolve repositories known at base time plus those named by delta events."""
    candidates = {Path(value) for value in known_paths}
    for value in delta.get('changedPaths', ()):
        relative = Path(value)
        parts = relative.parts
        for index, part in enumerate(parts):
            if part == '.git':
                candidates.add(Path(*parts[:index]) if index else Path('.'))
                break
        if relative == Path('.'):
            candidates.add(Path('.'))
    for value in delta.get('scanPaths', ()):
        scan_root = Path(value)
        candidates.update(_git_repository_paths_under(scan_root, root / scan_root))
    items = []
    for rel in sorted(candidates, key=lambda path: (len(path.parts), str(path))):
        if any(part in {'.worktrees', 'objects'} for part in rel.parts):
            continue
        target = mount_repo / rel
        source = root / rel
        if (source / '.git').exists():
            _prepare_repo(target, source, view=True)
        if _is_git_repo(target, view=True):
            items.append({'path': str(rel), 'startCommit': _git(target, 'rev-parse', 'HEAD', view=True).strip(),
                          'snapshotCommit': None, 'branch': None})
    return items


def _git_repository_paths_under(relative_root: Path, absolute_root: Path):
    if not absolute_root.is_dir():
        return set()
    found = set()
    for current, dirs, files in os.walk(absolute_root, followlinks=False):
        here = Path(current)
        rel = here.relative_to(absolute_root)
        if '.git' in dirs or '.git' in files:
            repo_rel = relative_root / rel
            if '.worktrees' not in repo_rel.parts:
                found.add(repo_rel)
        dirs[:] = [name for name in dirs if name != '.git' and name not in {'node_modules', '.cache', '.worktrees'}]
    return found


def _snapshot_paths(repo_rel: Path, dirty_paths, delta, nested_repositories=()):
    paths = {Path(value) for value in dirty_paths}
    repo_parts = repo_rel.parts
    nested_repositories = tuple(Path(value) for value in nested_repositories if Path(value) != repo_rel)

    def add_delta_path(value):
        path = Path(value)
        parts = path.parts
        if any(part in {'.worktrees'} for part in parts):
            return
        if repo_rel == Path('.'):
            for nested in nested_repositories:
                if parts[:len(nested.parts)] == nested.parts:
                    paths.add(nested)
                    return
            if '.git' in parts:
                index = parts.index('.git')
                # Nested repository changes must update the parent's gitlink.
                if index:
                    paths.add(Path(*parts[:index]))
                return
            paths.add(path)
            return
        if parts[:len(repo_parts)] == repo_parts:
            local = parts[len(repo_parts):]
            if '.git' in local:
                return
            paths.add(Path(*local) if local else Path('.'))

    for value in delta.get('changedPaths', ()):
        add_delta_path(value)
    for value in delta.get('scanPaths', ()):
        scan = Path(value)
        if any(part == '.worktrees' for part in scan.parts):
            continue
        scan_parts = scan.parts
        if repo_rel == Path('.'):
            add_delta_path(value)
        elif scan_parts[:len(repo_parts)] == repo_parts:
            paths.add(Path(*scan_parts[len(repo_parts):]) if len(scan_parts) > len(repo_parts)
                      else Path('.'))
        elif repo_parts[:len(scan_parts)] == scan_parts:
            paths.add(Path('.'))
    return sorted((path for path in paths if path != Path('.git') and '.git' not in path.parts),
                  key=lambda path: path.as_posix())


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
            excludes = tuple(base.get('excludes') or _workspace_excludes(root))
            delta = _get_backend().sync_delta(root, mount_repo, token, excludes=excludes)
            if not isinstance(delta, dict):
                delta = {'token': delta, 'changedPaths': []}
            state['token'] = delta.get('token', token)
            _write_json(state_path, state)
            repos = _repo_state_list(root, mount_repo, base.get('repositories', ['.']), delta)
            for item in repos:
                rel = Path(item['path'])
                target = mount_repo / rel
                source = root / rel
                copied_head = item['startCommit']
                start = (start_commit if rel == Path('.') and start_commit else
                         _git(source, 'rev-parse', 'HEAD').strip())
                item['startCommit'] = start
                item['branch'] = 'codex-agent/' + agent_id
                verify = _command(['git', '-C', str(target), 'show-ref', '--verify', '--quiet',
                                   'refs/heads/' + item['branch']], view=True, timeout=30)
                prior_snapshot = None
                if verify.returncode == 0:
                    prior_tip = _git(target, 'rev-parse', 'refs/heads/' + item['branch'], view=True).strip()
                    message = _git(target, 'show', '-s', '--format=%s', prior_tip, view=True).strip()
                    parents = _git(target, 'rev-list', '--parents', '-n', '1', prior_tip, view=True).split()
                    if (not start_commit and message == 'studio snapshot' and len(parents) == 2
                            and parents[1] == start):
                        prior_snapshot = prior_tip
                branch_tip = (prior_snapshot if prior_snapshot
                              else start_commit if start_commit and rel == Path('.') else start)
                _sync_refs(target, source, agent_id, view=True)
                if start_commit and rel == Path('.'):
                    _git(target, 'checkout', '-f', '-B', item['branch'], start_commit, view=True)
                else:
                    if not prior_snapshot:
                        _stage_committed_changes(target, copied_head, start, view=True)
                    _git(target, 'update-ref', 'refs/heads/' + item['branch'], branch_tip, view=True)
                    _git(target, 'symbolic-ref', 'HEAD', 'refs/heads/' + item['branch'], view=True)
                if prior_snapshot:
                    item['snapshotCommit'] = prior_snapshot
                nested_repositories = [value['path'] for value in repos]
                paths = _snapshot_paths(rel, base.get('dirtyPaths', {}).get(item['path'], []),
                                        delta, nested_repositories)
                if paths:
                    pathspecs = [f':(literal){path.as_posix()}' for path in paths]
                    _git(target, 'add', '-A', '--', *pathspecs, view=True)
                staged = (_command(['git', '-C', str(target), 'diff', '--cached', '--quiet', 'HEAD', '--',
                                    *(f':(literal){path.as_posix()}' for path in paths)], view=True, timeout=60)
                          if paths else None)
                if staged and staged.returncode == 1:
                    _git(target, '-c', 'user.name=Codex Studio', '-c', 'user.email=studio@localhost',
                         'commit', '-m', 'studio snapshot', timeout=300, view=True)
                    item['snapshotCommit'] = _git(target, 'rev-parse', 'HEAD', view=True).strip()
            state.update({'repositories': repos, 'snapshotCommit': next(
                (x['snapshotCommit'] for x in repos if x['path'] == '.'), None),
                'startCommit': start_commit or next((x['startCommit'] for x in repos if x['path'] == '.'), None),
                'mount': str(mount), 'repoPath': str(mount_repo), 'state': 'ready',
                'mounted': True})
            _write_json(state_path, state)
            if delta.get('refreshBase'):
                _schedule_base_refresh(root, base['version'])
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


def _fetch_and_replay(user_repo: Path, agent_repo: Path, branch: str, snapshot: str | None,
                      start_commit: str | None, agent_id: str):
    raw_ref = f'refs/studio/agents/{agent_id}/raw'
    result_ref = f'refs/heads/codex-agent/{agent_id}'
    _git(user_repo, 'fetch', str(agent_repo), f'{branch}:{raw_ref}', timeout=300, view=True)
    raw_tip = _git(user_repo, 'rev-parse', raw_ref).strip()
    base_commit = (_git(user_repo, 'rev-parse', f'{snapshot}^').strip() if snapshot
                   else _git(user_repo, 'rev-parse', start_commit or 'HEAD').strip())
    current = base_commit
    commits = _git(user_repo, 'rev-list', '--reverse', f'{snapshot or base_commit}..{raw_tip}').split()
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
    if current != base_commit:
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
                                       item.get('snapshotCommit'), item.get('startCommit'), agent_id)
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
