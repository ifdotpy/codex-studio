"""Private folder copies for image workspaces."""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import threading
import time
from contextlib import contextmanager
from pathlib import Path
from collections.abc import Callable, Iterable, Iterator
from typing import Callable, NotRequired, Protocol, TypedDict

from codex_records import (ImageWorkspaceCollectResultRecord, ImageWorkspaceMount,
                           ImageWorkspaceRemovalResultRecord, JsonObject, JsonValue)


class WorkspaceBaseStaging(TypedDict):
    root: NotRequired[Path]
    token: str | int | None
    versionPath: Path
    image: NotRequired[Path]
    mount: NotRequired[Path]
    version: NotRequired[str]
    refresh: NotRequired[bool]


class WorkspaceMount(TypedDict):
    mount: str
    layer: NotRequired[str]
    baseImage: NotRequired[str]
    pid: NotRequired[int]


class WorkspaceDelta(TypedDict):
    token: str | int | None
    changedPaths: list[str]
    historyLost: bool
    scanPaths: NotRequired[list[str]]
    refreshBase: NotRequired[bool]


class WorkspaceBackend(Protocol):
    def supported(self, root: Path) -> tuple[bool, str]: ...
    def current_event_id(self, root: Path) -> Any: ...
    def open_base_staging(self, root: Path, key: str, version: str) -> dict[str, Any]: ...
    def copy_base_tree(self, root: Path, destination: Path, *, excludes: tuple[str, ...],
                       check=None) -> None: ...
    def seal_base(self, staging: dict[str, Any]) -> dict[str, Any]: ...
    def clone_workspace(self, image: Path, agent_dir: Path) -> Path: ...
    def mount_workspace(self, layer: Path, mount: Path, *, base_image: Path | None = None) -> dict[str, Any]: ...
    def sync_delta(self, root: Path, target: Path, token: Any, *, excludes: tuple[str, ...]) -> Any: ...
    def unmount_workspace(self, mount: Path, *, force: bool = False) -> dict[str, Any]: ...
    def remove_layer(self, agent_dir: Path) -> None: ...
    def remove_base_version(self, path: Path) -> None: ...
    def private_bytes(self, path: Path) -> int: ...
    def exec_prefix(self) -> list[str]: ...


_STORE_OVERRIDE = 'CODEX_WORKSPACE_STORE'
_DEFAULT_STORE = Path.home() / '.local' / 'state' / 'codex-agents' / 'workspaces'
_BASE_MIN_FREE_ENV = 'CODEX_WORKSPACE_MIN_FREE_BYTES'
_AGENT_MIN_FREE_ENV = 'CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES'
_DEFAULT_BASE_MIN_FREE_BYTES = 20 * 1024**3
_DEFAULT_AGENT_MIN_FREE_BYTES = 5 * 1024**3
_backend_instance: WorkspaceBackend | None = None
_backend_lock = threading.Lock()
_build_lock = threading.Lock()
_build_threads: dict[str, threading.Thread] = {}
_build_callbacks: dict[str, list] = {}


def _backend() -> WorkspaceBackend:
    if sys.platform == 'darwin':
        from codex_workspace_macos import Backend
    else:
        raise RuntimeError('Image workspaces require macOS ASIF. Use a Git worktree on this server')
    return Backend()


def _get_backend() -> WorkspaceBackend:
    global _backend_instance
    if _backend_instance is None:
        with _backend_lock:
            if _backend_instance is None:
                _backend_instance = _backend()
    return _backend_instance


def _store() -> Path:
    return Path(os.environ.get(_STORE_OVERRIDE) or _DEFAULT_STORE).expanduser().resolve()


def _free_bytes(path: Path) -> int:
    path = Path(path).expanduser()
    while not path.exists():
        parent = path.parent
        if parent == path:
            raise OSError(f'Cannot find an existing path for disk-space check: {path}')
        path = parent
    return shutil.disk_usage(path).free


def _minimum_free_bytes(variable: str, default: int) -> int:
    value = os.environ.get(variable)
    if value is None:
        return default
    try:
        result = int(value)
    except ValueError as exc:
        raise ValueError(f'{variable} must be a non-negative integer') from exc
    if result < 0:
        raise ValueError(f'{variable} must be a non-negative integer')
    return result


def _free_space_error(free: int, floor: int, *, purpose='image base') -> str:
    return (f'Not enough free disk space for the {purpose}: '
            f'{free} free, {floor} required (bytes)')


def _require_base_space(staging=None) -> None:
    floor = _minimum_free_bytes(_BASE_MIN_FREE_ENV, _DEFAULT_BASE_MIN_FREE_BYTES)
    free = _free_bytes(_store())
    if free < floor:
        raise RuntimeError(_free_space_error(free, floor))
    if staging and staging.get('mount'):
        mount = Path(staging['mount'])
        try:
            mount_device = mount.stat().st_dev
            parent_device = mount.parent.stat().st_dev
        except OSError as exc:
            raise RuntimeError(f'Image base staging mount is unavailable: {mount}: {exc}') from exc
        if mount_device == parent_device:
            raise RuntimeError(f'Image base staging mount disappeared: {mount}')


def _record_base_space_failure(root: Path, key: str, error: str) -> dict[str, Any]:
    base_dir = _base_dir(key)
    with _file_lock(base_dir / '.build.lock'):
        current = _read_json(_base_state_path(key), {}) or {}
        failed = {**current, 'schema': 2, 'changeDetector': 'git-v1',
                  'state': 'failed', 'repoRoot': str(root), 'repoKey': key,
                  'error': error, 'failedAt': time.time()}
        _write_json(_base_state_path(key), failed)
    return {key: failed.get(key) for key in ('state', 'version', 'error')}


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


def _git_repositories(root: Path) -> list[tuple[str, Path]]:
    """Find Git work trees below root without following links or entering .git."""
    root = Path(root).resolve()
    excluded = _workspace_excludes(root)
    found = []
    for current, directories, _files in os.walk(root, followlinks=False):
        directory = Path(current)
        git_path = directory / '.git'
        directories[:] = [name for name in directories
                          if name != '.git' and not (directory / name).is_symlink()
                          and not any((directory / name).relative_to(root).as_posix() == value
                                      or (directory / name).relative_to(root).as_posix().startswith(
                                          value + '/') for value in excluded)]
        if not git_path.is_symlink() and (git_path.is_dir() or git_path.is_file()):
            probe = subprocess.run(['git', '-C', str(directory), 'rev-parse', '--show-toplevel'],
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False)
            if probe.returncode == 0 and Path(os.fsdecode(probe.stdout).strip()).resolve() == directory.resolve():
                relative = directory.relative_to(root).as_posix()
                found.append(('.' if relative == '.' else relative, directory))
    return found


def _git(repo: Path, *arguments: str, input_data: bytes | None = None,
         accepted=(0,), timeout=1800, prefix=(), readonly=False) -> bytes:
    env = os.environ.copy()
    if readonly:
        env['GIT_OPTIONAL_LOCKS'] = '0'
    result = subprocess.run([*prefix, 'git', '-C', str(repo), *arguments], input=input_data,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            timeout=timeout, check=False, env=env)
    if result.returncode not in accepted:
        detail = result.stderr.decode(errors='replace')[-2000:]
        raise RuntimeError(f"git {' '.join(arguments)} failed in {repo}: {detail}")
    return result.stdout


def _git_index_path(repo: Path, *, require_inside=False, inside_root: Path | None = None,
                    prefix=()) -> Path:
    value = os.fsdecode(_git(repo, 'rev-parse', '--git-path', 'index', prefix=prefix)).strip()
    index = Path(value)
    if not index.is_absolute():
        index = repo / index
    index = index.resolve()
    allowed_root = inside_root if inside_root is not None else repo
    if require_inside and not index.is_relative_to(allowed_root.resolve()):
        raise RuntimeError(f'Git index is outside the image copy: {index}')
    return index


def _index_fingerprint(repo: Path) -> str | None:
    try:
        value = _git_index_path(repo).stat()
    except FileNotFoundError:
        return None
    # Git writes index.lock and atomically renames it over index. Stat data gives
    # an O(1) change check and avoids reading a large index on every workspace start.
    identity = (value.st_dev, value.st_ino, value.st_size,
                value.st_mtime_ns, value.st_ctime_ns)
    return hashlib.sha256(json.dumps(identity).encode()).hexdigest()


def _index_entries(repo: Path, *, paths=None, prefix=()) -> dict[bytes, tuple[tuple[bytes, bytes, bytes], ...]]:
    if paths is not None and not paths:
        return {}
    arguments = ['--literal-pathspecs', 'ls-files', '--stage', '-z']
    if paths:
        arguments.extend(['--', *sorted(os.fsdecode(path) for path in paths)])
    raw = _git(repo, *arguments, prefix=prefix)
    entries: dict[bytes, list[tuple[bytes, bytes, bytes]]] = {}
    for record in raw.split(b'\0'):
        if not record:
            continue
        metadata, path = record.split(b'\t', 1)
        mode, oid, stage = metadata.split(b' ', 2)
        entries.setdefault(path, []).append((mode, oid, stage))
    return {path: tuple(sorted(values)) for path, values in entries.items()}


def _index_state(repo: Path):
    """Read staged entries from a stable index without refreshing or writing it."""
    for _attempt in range(3):
        before = _index_fingerprint(repo)
        entries = _index_entries(repo)
        after = _index_fingerprint(repo)
        if before == after:
            return after, entries
    raise RuntimeError(f'Git index changed while it was read: {repo}')


def _git_head(repo: Path) -> str | None:
    result = subprocess.run(['git', '-C', str(repo), 'rev-parse', 'HEAD'],
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                            check=False, env={**os.environ, 'GIT_OPTIONAL_LOCKS': '0'})
    return os.fsdecode(result.stdout).strip() if result.returncode == 0 else None


def _git_dirty_paths(repo: Path) -> set[str]:
    raw = _git(repo, 'status', '--porcelain=v1', '-z', '--untracked-files=all', readonly=True)
    records = raw.split(b'\0')
    paths = set()
    index = 0
    while index < len(records):
        record = records[index]
        index += 1
        if not record:
            continue
        paths.add(os.fsdecode(record[3:]))
        if record[:2].strip() in {b'R', b'C'} and index < len(records):
            paths.add(os.fsdecode(records[index]))
            index += 1
    return paths


def _repo_snapshots(root: Path, known_repositories=None) -> list[dict[str, Any]]:
    fast_discovery = (known_repositories is not None
                      and any(item.get('path') == '.' for item in known_repositories))
    if not fast_discovery:
        repositories = _git_repositories(root)
    else:
        repositories = []
        excluded = _workspace_excludes(root)
        known_paths = set()
        dirty_paths = {}

        def add_repository(relative, repo):
            if relative in known_paths:
                return True
            current = repo
            while current != root:
                if current.is_symlink() or not current.is_relative_to(root):
                    return False
                current = current.parent
            git_path = repo / '.git'
            if git_path.is_symlink() or not (git_path.is_dir() or git_path.is_file()):
                return False
            result = subprocess.run(['git', '-C', str(repo), 'rev-parse', '--show-toplevel'],
                                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                    check=False)
            if result.returncode or Path(os.fsdecode(result.stdout).strip()).resolve() != repo.resolve():
                return False
            if relative not in known_paths:
                known_paths.add(relative)
                repositories.append((relative, repo))
            return True

        add_repository('.', root)
        for item in known_repositories:
            relative = item.get('path')
            if not relative or relative == '.':
                continue
            add_repository(relative, root / relative)

        visited = set()
        cursor = 0
        while cursor < len(repositories):
            relative, repo = repositories[cursor]
            cursor += 1
            dirty = _git_dirty_paths(repo)
            dirty_paths[relative] = dirty
            for name in dirty:
                path = repo / name
                if not path.is_dir():
                    path = path.parent
                while path != repo and path.is_relative_to(repo):
                    candidate = path.relative_to(root).as_posix()
                    if candidate not in visited:
                        visited.add(candidate)
                        if not any(candidate == value or candidate.startswith(value + '/')
                                   for value in excluded):
                            add_repository(candidate, path)
                    path = path.parent

    records = []
    for relative, repo in repositories:
        fingerprint = _index_fingerprint(repo)
        records.append({'path': relative, 'head': _git_head(repo),
                        'dirtyPaths': sorted(dirty_paths[relative] if fast_discovery
                                             else _git_dirty_paths(repo)),
                        'indexFingerprint': fingerprint,
                        'gitMetadataFingerprint': _git_metadata_fingerprint(repo)})
    return records


def _git_metadata_fingerprint(repo: Path) -> str:
    """Fingerprint small, user-visible Git metadata without walking object storage."""
    refs = _git(repo, 'for-each-ref', '--format=%(refname)%00%(objectname)', readonly=True)
    git_dir = Path(os.fsdecode(_git(repo, 'rev-parse', '--absolute-git-dir',
                                    readonly=True)).strip())
    common_dir_text = os.fsdecode(_git(repo, 'rev-parse', '--git-common-dir', readonly=True)).strip()
    common_dir = Path(common_dir_text)
    if not common_dir.is_absolute():
        common_dir = repo / common_dir
    git_dirs = sorted({path.resolve() for path in (git_dir, common_dir)})
    state = []
    for path in git_dirs:
        relative = str(path)
        state.extend(_metadata_tree_fingerprint(path, relative))
        alternates = path / 'objects' / 'info' / 'alternates'
        try:
            state.append((str(alternates), _metadata_file_fingerprint(alternates)))
        except FileNotFoundError:
            state.append((str(alternates), None))
    return hashlib.sha256(refs + json.dumps(state, separators=(',', ':')).encode()).hexdigest()


def _metadata_file_fingerprint(path: Path, value=None):
    value = value or path.lstat()
    if stat.S_ISDIR(value.st_mode):
        return ('directory', value.st_mode)
    if path.is_symlink():
        content = os.fsencode(os.readlink(path))
    elif path.is_file():
        content = path.read_bytes()
    else:
        content = b''
    return (value.st_mode, value.st_size, value.st_mtime_ns, value.st_ctime_ns,
            hashlib.sha256(content).hexdigest())


def _metadata_tree_fingerprint(root: Path, label: str):
    if not root.exists():
        return [(label, None)]
    result = []
    for current, directories, files in os.walk(root, followlinks=False):
        directory = Path(current)
        relative = directory.relative_to(root).as_posix()
        result.append((f'{label}/{relative}', _metadata_file_fingerprint(directory)))
        kept_directories = []
        for name in sorted(directories):
            path = directory / name
            if path.is_symlink():
                result.append((f'{label}/{path.relative_to(root).as_posix()}',
                               _metadata_file_fingerprint(path)))
            elif name != 'objects':
                kept_directories.append(name)
        directories[:] = kept_directories
        for name in sorted(files):
            if name in {'index', 'objects'}:
                continue
            path = directory / name
            result.append((f'{label}/{path.relative_to(root).as_posix()}',
                           _metadata_file_fingerprint(path)))
    return result


def _repositories_for_records(root: Path, records: list[dict[str, Any]]):
    return [(record['path'], root if record['path'] == '.' else root / record['path'])
            for record in records]


def _target_prefix(backend=None):
    return tuple((backend or _get_backend()).exec_prefix())


def _detected_paths(root: Path, baseline: dict[str, Any], current_repositories=None
                    ) -> tuple[set[str], list[dict[str, Any]]]:
    """Read Git state only from the source, and return paths changed since the baseline."""
    baseline_repos = {item['path']: item for item in baseline.get('repositories', [])}
    current_repos = current_repositories if current_repositories is not None else _repo_snapshots(root)
    changed: set[str] = set()
    current_by_path = {item['path']: item for item in current_repos}
    for relative, repo in _repositories_for_records(root, current_repos):
        before = baseline_repos.get(relative, {})
        prefix = '' if relative == '.' else relative + '/'
        current_head = current_by_path[relative].get('head')
        if before.get('head') and current_head and before['head'] != current_head:
            output = _git(repo, 'diff', '--no-renames', '--name-only', '-z',
                          before['head'], current_head,
                          readonly=True)
            changed.update(prefix + os.fsdecode(path) for path in output.split(b'\0') if path)
        elif not before.get('head') and current_head:
            output = _git(repo, 'ls-files', '-z', readonly=True)
            changed.update(prefix + os.fsdecode(path) for path in output.split(b'\0') if path)
        elif before.get('head') and not current_head:
            output = _git(repo, 'ls-files', '-z', readonly=True)
            changed.update(prefix + os.fsdecode(path) for path in output.split(b'\0') if path)
        changed.update(prefix + path for path in before.get('dirtyPaths', []))
        changed.update(prefix + path for path in current_by_path[relative].get('dirtyPaths', []))
    return changed, current_repos


def _copy_exact_paths(source_root: Path, target_root: Path, paths: set[str], backend) -> None:
    safe_paths = []
    for value in sorted(paths):
        path = Path(value)
        if path.is_absolute() or '..' in path.parts or value in {'', '.'}:
            if value in {'', '.'}:
                continue
            raise RuntimeError(f'Unsafe workspace delta path: {value!r}')
        if any(part == '.git' for part in path.parts):
            continue
        safe_paths.append(value)
    if not safe_paths:
        return
    script = '''import json, os, pathlib, shutil, stat, sys
src, dst = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
selected = set()
for name in json.loads(sys.stdin.read()):
    parent = src
    for component in pathlib.Path(name).parts[:-1]:
        parent = parent / component
        try:
            mode = parent.lstat().st_mode
        except FileNotFoundError:
            break
        if not stat.S_ISDIR(mode):
            name = parent.relative_to(src).as_posix()
            break
    selected.add(name)
for name in sorted(selected):
    source, target = src / name, dst / name
    parent = src
    for component in pathlib.Path(name).parts[:-1]:
        parent = parent / component
        try:
            mode = parent.lstat().st_mode
        except FileNotFoundError:
            break
        if not stat.S_ISDIR(mode):
            raise RuntimeError("source parent changed during workspace copy")
    if not target.parent.resolve().is_relative_to(dst.resolve()):
        raise RuntimeError("target path escapes workspace root")
    try:
        mode = source.lstat().st_mode
    except FileNotFoundError:
        mode = None
    if mode is None:
        if target.is_dir() and not target.is_symlink():
            shutil.rmtree(target)
        else:
            target.unlink(missing_ok=True)
        parent = target.parent
        while parent != dst and parent.is_dir() and not any(parent.iterdir()):
            parent.rmdir()
            parent = parent.parent
        continue
    target.parent.mkdir(parents=True, exist_ok=True)
    if stat.S_ISLNK(mode):
        if target.is_dir() and not target.is_symlink():
            shutil.rmtree(target)
        else:
            target.unlink(missing_ok=True)
        target.symlink_to(os.readlink(source))
    elif stat.S_ISREG(mode):
        if target.is_dir() and not target.is_symlink():
            shutil.rmtree(target)
        elif target.is_symlink():
            target.unlink()
        shutil.copy2(source, target)
    elif stat.S_ISDIR(mode) and not target.is_dir():
        target.unlink(missing_ok=True)
        target.mkdir()
'''
    subprocess.run([*_target_prefix(backend), sys.executable, '-c', script,
                    str(source_root), str(target_root)],
                   input=json.dumps(safe_paths).encode(), stdout=subprocess.PIPE,
                   stderr=subprocess.PIPE, check=True)


def _sync_git_directories(source_root: Path, target_root: Path, backend, *, repositories=None,
                          sync_metadata=True) -> None:
    prefix = _target_prefix(backend)
    repositories = _git_repositories(source_root) if repositories is None else repositories
    git_dirs = []
    for relative, source_repo in repositories:
        source_git = Path(os.fsdecode(_git(source_repo, 'rev-parse', '--absolute-git-dir',
                                           readonly=True)).strip()).resolve()
        git_dirs.append((relative, source_repo, source_git))
    for relative, source_repo, source_git in git_dirs:
        target_repo = target_root if relative == '.' else target_root / relative
        target_repo.mkdir(parents=True, exist_ok=True)
        if not source_git.is_relative_to(source_root.resolve()):
            raise RuntimeError(f'Git metadata is outside the source copy: {source_git}')
        target_git = target_root / source_git.relative_to(source_root.resolve())
        target_git.parent.mkdir(parents=True, exist_ok=True)
        source_entry = source_repo / '.git'
        target_entry = target_repo / '.git'
        if source_entry.is_dir():
            if target_entry.is_symlink() or target_entry.is_file():
                target_entry.unlink()
            target_entry.mkdir(exist_ok=True)
        elif source_entry.is_file():
            if not source_entry.read_text().startswith('gitdir: '):
                raise RuntimeError(f'Invalid Git directory pointer: {source_entry}')
            pointer = os.path.relpath(target_git, target_repo)
            target_entry.write_text(f'gitdir: {pointer}\n')
        if not target_git.resolve().is_relative_to(target_root.resolve()):
            raise RuntimeError(f'Git metadata is outside the image copy: {target_git}')
        if sync_metadata:
            args = [*prefix, 'rsync', '-a', '--delete', '--exclude=/index']
            for _nested, _repo, nested_git in git_dirs:
                try:
                    nested_relative = nested_git.relative_to(source_git)
                except ValueError:
                    continue
                if nested_relative.parts:
                    args.append(f'--exclude=/{nested_relative.as_posix()}/index')
            args.extend([str(source_git) + '/', str(target_git) + '/'])
            subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        target_top = Path(os.fsdecode(_git(target_repo, 'rev-parse', '--show-toplevel',
                                           prefix=prefix)).strip()).resolve()
        if target_top != target_repo.resolve():
            raise RuntimeError(f'Git repository is missing from the image copy: {relative}')
        resolved_target_git = Path(os.fsdecode(_git(
            target_repo, 'rev-parse', '--absolute-git-dir', prefix=prefix)).strip()).resolve()
        if resolved_target_git != target_git.resolve():
            raise RuntimeError(f'Git metadata path changed in the image copy: {relative}')


def _sync_detected(root: Path, target: Path, baseline: dict[str, Any], excludes, backend,
                   current_repositories=None, repair_pointers=False):
    paths, current_repositories = _detected_paths(root, baseline, current_repositories)
    excluded = [Path(value) for value in excludes]
    paths = {value for value in paths
             if not any(Path(value) == item or item in Path(value).parents for item in excluded)}
    known = {item['path'] for item in baseline.get('repositories', [])}
    known.update(item['path'] for item in current_repositories)
    if '.' in known:
        _copy_exact_paths(root, target, paths, backend)
    else:
        # A non-Git folder has no cheap reliable change log. Copy its current tree once.
        args = [*_target_prefix(backend), 'rsync', '-a', '--delete']
        for value in excludes:
            suffix = '' if value.endswith('/.git/index') or value == '.git/index' else '/***'
            args.append(f'--exclude=/{value}{suffix}')
        args.extend([str(root) + '/', str(target) + '/'])
        subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
    baseline_repos = {item['path']: item for item in baseline.get('repositories', [])}
    needs_git_sync = any(
        baseline_repos.get(item['path'], {}).get('gitMetadataFingerprint')
        != item.get('gitMetadataFingerprint') for item in current_repositories)
    if needs_git_sync or repair_pointers:
        _sync_git_directories(root, target, backend,
                              repositories=_repositories_for_records(root, current_repositories),
                              sync_metadata=needs_git_sync)
    return {'changedPaths': sorted(paths), 'repositories': current_repositories}


def _copy_index_entries(source: Path, target: Path, source_entries=None, *, paths=None,
                        image_root=None, prefix=()) -> set[str]:
    _git_index_path(target, require_inside=True, inside_root=image_root, prefix=prefix)
    source_entries = source_entries if source_entries is not None else _index_entries(source, paths=paths)
    target_entries = _index_entries(target, paths=paths, prefix=prefix)
    changed = {path for path in source_entries.keys() | target_entries.keys()
               if source_entries.get(path) != target_entries.get(path)}
    if not changed:
        return set()
    for entries in source_entries.items():
        path, values = entries
        if path not in changed:
            continue
        for _mode, oid, _stage in values:
            present = subprocess.run([*prefix, 'git', '-C', str(target), 'cat-file', '-e',
                                       os.fsdecode(oid)], stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL, check=False).returncode == 0
            if present:
                continue
            kind = _git(source, 'cat-file', '-t', os.fsdecode(oid), readonly=True).strip()
            body = _git(source, 'cat-file', os.fsdecode(kind), os.fsdecode(oid), readonly=True)
            copied = _git(target, 'hash-object', '-w', '-t', os.fsdecode(kind), '--stdin',
                          input_data=body, prefix=prefix).strip()
            if copied != oid:
                raise RuntimeError(f'Git object changed while copying index entry {os.fsdecode(path)!r}')
    records = []
    for path in sorted(changed):
        for _mode, oid, stage in target_entries.get(path, ()):
            records.append(b'0 ' + oid + b' ' + stage + b'\t' + path + b'\0')
    for path in sorted(changed):
        for mode, oid, stage in source_entries.get(path, ()):
            records.append(mode + b' ' + oid + b' ' + stage + b'\t' + path + b'\0')
    _git(target, 'update-index', '-z', '--index-info', input_data=b''.join(records), prefix=prefix)
    return {os.fsdecode(path) for path in changed}


def _refresh_index(repo: Path, paths=None, *, image_root=None, prefix=()):
    _git_index_path(repo, require_inside=True, inside_root=image_root, prefix=prefix)
    arguments = ['--literal-pathspecs', 'update-index', '--refresh', '-q']
    if paths:
        arguments.extend(['--', *sorted(paths)])
    # Return code 1 means at least one work tree file differs from its index.
    _git(repo, *arguments, accepted=(0, 1), prefix=prefix)


def _index_excludes(root: Path, repositories=None) -> tuple[str, ...]:
    root = Path(root).resolve()
    values = []
    repositories = _git_repositories(root) if repositories is None else repositories
    for relative, _repo in repositories:
        prefix = '' if relative == '.' else relative + '/'
        values.append(prefix + '.git/index')
    return tuple(values)


def _delta_excludes(root: Path, excludes, repositories=None) -> tuple[str, ...]:
    return tuple(sorted(set(excludes) | set(_index_excludes(root, repositories))))


def _repo_index_metadata(source_root: Path, copy_root: Path, previous=None, *, refresh_all=True,
                         backend=None, repository_records=None, candidate_paths=None):
    previous = previous or {}
    repository_records = (_repo_snapshots(source_root) if repository_records is None
                          else repository_records)
    candidates = {os.fsencode(path) for path in (candidate_paths or ())}
    records = []
    changed_paths = {}
    worktree_paths = set()
    fingerprints = {}
    repo_paths = [item['path'] for item in repository_records]
    for relative, source in _repositories_for_records(source_root, repository_records):
        target = copy_root if relative == '.' else copy_root / relative
        snapshot = next(item for item in repository_records if item['path'] == relative)
        fingerprint = snapshot.get('indexFingerprint')
        prior = previous.get(relative)
        changed = set()
        if prior is None or prior.get('indexFingerprint') != fingerprint:
            prefix = b'' if relative == '.' else os.fsencode(relative + '/')
            repo_candidates = None if not candidates else {
                path[len(prefix):] for path in candidates
                if (not prefix or path.startswith(prefix)) and
                (not prefix or path[len(prefix):])}
            if repo_candidates is not None:
                child_prefixes = [os.fsencode((path if relative == '.' else
                                               path[len(relative) + 1:]) + '/')
                                  for path in repo_paths if path != '.' and path.startswith(
                                      '' if relative == '.' else relative + '/') and path != relative]
                if child_prefixes:
                    repo_candidates = {path for path in repo_candidates
                                       if not any(path.startswith(child) for child in child_prefixes)}
            if repo_candidates is not None and not repo_candidates:
                fingerprints[relative] = fingerprint
                continue
            source_entries = _index_entries(source, paths=repo_candidates)
            if _index_fingerprint(source) != fingerprint:
                fingerprint, source_entries = _index_state(source)
                repo_candidates = None
            changed = _copy_index_entries(source, target, source_entries, paths=repo_candidates,
                                          image_root=copy_root, prefix=_target_prefix(backend))
        fingerprints[relative] = fingerprint
        if changed:
            changed_paths[relative] = changed
            prefix = '' if relative == '.' else relative + '/'
            worktree_paths.update(prefix + path for path in changed)
    if worktree_paths:
        _copy_exact_paths(source_root, copy_root, worktree_paths, backend or _get_backend())
    for relative, source in _repositories_for_records(source_root, repository_records):
        target = copy_root if relative == '.' else copy_root / relative
        if refresh_all:
            _refresh_index(target, image_root=copy_root, prefix=_target_prefix(backend))
        records.append({'path': relative, 'indexFingerprint': fingerprints[relative]})
    return records, changed_paths


def _refresh_changed_paths(root: Path, target_root: Path, changed_paths, index_changes, backend=None,
                           repository_records=None):
    paths = set(changed_paths or ())
    # A path whose index entry just changed already has the correct staged content.
    # Let the first status refresh its copied stat data instead of parsing the full index here.
    paths.difference_update(index_changes or ())
    repository_records = _repo_snapshots(root) if repository_records is None else repository_records
    for relative, _source in _repositories_for_records(root, repository_records):
        repo_paths = set()
        prefix = '' if relative == '.' else relative + '/'
        for path in paths:
            if path in {'.', ''}:
                repo_paths.add('.')
            elif not prefix:
                repo_paths.add(path)
            elif path.startswith(prefix):
                repo_paths.add(path[len(prefix):])
        repo_paths = {path for path in repo_paths
                      if path not in {'', '.git'} and not path.startswith('.git/')}
        if repo_paths:
            target = target_root if relative == '.' else target_root / relative
            index_args = {'image_root': target_root, 'prefix': _target_prefix(backend)}
            git_args = {'prefix': _target_prefix(backend)}
            if '.' in repo_paths:
                _refresh_index(target, **index_args)
                continue
            tracked = _git(target, 'ls-files', '-z', '--',
                           *[f':(literal){path}' for path in sorted(repo_paths)], **git_args)
            tracked_paths = {os.fsdecode(path) for path in tracked.split(b'\0') if path}
            if tracked_paths:
                source = root if relative == '.' else root / relative
                unstaged = _git(source, '--literal-pathspecs', 'diff-files', '--name-only', '-z',
                                '--', *sorted(tracked_paths), readonly=True)
                unstaged_paths = {os.fsdecode(path) for path in unstaged.split(b'\0') if path}
                tracked_paths.difference_update(unstaged_paths)
            if tracked_paths:
                _refresh_index(target, tracked_paths, **index_args)


def _safe_id(value) -> str:
    value = str(value)
    if not value or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-' for c in value):
        raise ValueError('Invalid workspace identifier')
    return value


def _read_json(path: Path, default: JsonObject | None = None) -> JsonObject | None:
    try:
        return json.loads(path.read_text())
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return default


def _write_json(path: Path, value: JsonObject) -> None:
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
def _file_lock(path: Path, *, blocking: bool = True) -> Iterator[None]:
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
    if not state or state.get('schema') != 2 or state.get('changeDetector') != 'git-v1':
        return {'state': 'missing', 'version': None, 'error': None}
    if state.get('state') == 'building' and state.get('builderPid') != os.getpid():
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
    space_failure = None
    with _build_lock:
        thread = _build_threads.get(key)
        if thread is not None and thread.is_alive():
            if on_done:
                _build_callbacks.setdefault(key, []).append(on_done)
            return {'state': 'building', 'version': status.get('version'), 'error': None}
        try:
            _require_base_space()
        except (OSError, RuntimeError, ValueError) as exc:
            space_failure = _record_base_space_failure(folder, key, str(exc))
    if space_failure:
        if on_done:
            _call_callback(on_done, space_failure)
        return space_failure
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


def _check_base_refresh(root: Path, key: str, expected_version: str) -> None:
    try:
        state = _read_json(_base_state_path(key), {}) or {}
        if (state.get('state') == 'ready' and state.get('version') == expected_version
                and _get_backend().base_needs_refresh(root, state)):  # type: ignore[attr-defined]  # typed-narrowing: the preceding feature probe guards this optional backend hook
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
            if (prior.get('schema') == 2 and prior.get('changeDetector') == 'git-v1'
                    and prior.get('state') == 'ready'
                    and Path(prior.get('image', '')).exists()
                    and (refresh_from is None or prior.get('version') != refresh_from)):
                result = prior
            else:
                version = f'v-{time.time_ns()}'
                _require_base_space()
                if refresh_from is None:
                    _write_json(_base_state_path(key), {
                        'schema': 2, 'state': 'building', 'repoRoot': str(root), 'repoKey': key,
                        'changeDetector': 'git-v1', 'version': version,
                        'error': None, 'startedAt': time.time(),
                        'builderPid': os.getpid(),
                    })
                else:
                    prior['refreshingVersion'] = version
                    _write_json(_base_state_path(key), prior)
                backend = _get_backend()
                staging = backend.open_base_staging(root, key, version)
                _require_base_space(staging)
                excludes = _workspace_excludes(root)
                baseline = {'repositories': _repo_snapshots(root)}
                delta_excludes = _delta_excludes(
                    root, excludes, _repositories_for_records(root, baseline['repositories']))
                token = staging.get('token')
                check_space = lambda: _require_base_space(staging)
                backend.copy_base_tree(root, Path(staging['root']), excludes=excludes,
                                       check=check_space)
                check_space()
                delta = _sync_detected(root, Path(staging['root']), baseline,
                                       delta_excludes, backend, repair_pointers=True)
                index_records, _index_changes = _repo_index_metadata(
                    root, Path(staging['root']), refresh_all=True, backend=backend,
                    repository_records=delta['repositories'],
                    candidate_paths=delta['changedPaths'])
                fingerprints = {item['path']: item['indexFingerprint'] for item in index_records}
                repositories = [dict(item, indexFingerprint=fingerprints.get(item['path']))
                                for item in delta['repositories']]
                sealed = backend.seal_base(staging)
                result = {
                    'schema': 2, 'state': 'ready', 'repoRoot': str(root), 'repoKey': key,
                    'changeDetector': 'git-v1',
                    'version': version, 'error': None,
                    'image': str(sealed['image']),
                    'versionPath': str(sealed['versionPath']),
                    'token': token,
                    'excludes': list(delta_excludes),
                    'repositories': repositories,
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
    return value if (value.get('schema') == 2 and value.get('changeDetector') == 'git-v1'
                     and value.get('version') == version) else None


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
            try:
                free = _free_bytes(_store())
                floor = _minimum_free_bytes(_AGENT_MIN_FREE_ENV,
                                            _DEFAULT_AGENT_MIN_FREE_BYTES)
            except (OSError, ValueError) as exc:
                raise RuntimeError(f'Cannot verify free disk space for the image workspace: {exc}') from exc
            if free < floor:
                raise RuntimeError(_free_space_error(
                    free, floor, purpose='image workspace'))
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
            current_repositories = _repo_snapshots(folder, base.get('repositories', []))
            excludes = _delta_excludes(
                folder, tuple(base.get('excludes') or _workspace_excludes(folder)),
                _repositories_for_records(folder, current_repositories))
            backend = _get_backend()
            delta = _sync_detected(folder, workspace_root, base, excludes, backend,
                                   current_repositories=current_repositories)
            base_repositories = {item.get('path'): item for item in base.get('repositories', [])
                                 if isinstance(item, dict)}
            repositories, index_changes = _repo_index_metadata(
                folder, workspace_root, base_repositories, refresh_all=False, backend=backend,
                repository_records=delta['repositories'], candidate_paths=delta['changedPaths'])
            changed_index_paths = set()
            for relative, paths in index_changes.items():
                prefix = '' if relative == '.' else relative + '/'
                changed_index_paths.update(prefix + path for path in paths)
            _refresh_changed_paths(folder, workspace_root,
                                   delta.get('changedPaths'), changed_index_paths, backend,
                                   repository_records=delta['repositories'])
            state.update({'path': str(workspace_root), 'mount': str(mount),
                          'state': 'ready', 'mounted': True,
                          'repositories': repositories,
                          'deltaFallbackReason': None})
            _write_json(state_path, state)
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
        unmount = _get_backend().unmount_workspace(mount, force=True)
        state['state'] = 'archived'
        state['mounted'] = False
        _write_json(_agent_state_path(agent_id), state)
    return {'freedBytes': 0, 'state': 'archived', 'unmount': unmount}


def remove_workspace(agent_id, *, force=False) -> dict[str, Any]:
    del force
    agent_id = _safe_id(agent_id)
    agent_dir = _agent_dir(agent_id)
    state = _read_json(_agent_state_path(agent_id), {}) or {}
    if not state:
        return {'freedBytes': 0, 'state': 'removed'}
    with _file_lock(agent_dir / '.workspace.lock'):
        mount = Path(state.get('mount') or _mount_path(agent_id))
        unmount = _get_backend().unmount_workspace(mount, force=True)
        _get_backend().remove_layer(agent_dir)
        shutil.rmtree(mount, ignore_errors=True)
        if state.get('repoKey'):
            _prune_base_versions(state['repoKey'])
    return {'freedBytes': None, 'state': 'removed', 'unmount': unmount}


def list_workspaces() -> list[JsonObject]:
    results = []
    for path in (_store() / 'agents').glob('*/agent.json'):
        value = _read_json(path, {})
        if value:
            results.append(value)
    return results


def exec_prefix() -> list[str]:
    return _get_backend().exec_prefix()
