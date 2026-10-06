"""Background, read-only measurements of Studio worker worktrees."""

import ctypes
from collections.abc import Iterable
import json
import os
import sqlite3
import sys
import threading
import time
from pathlib import Path


DEFAULT_LIMIT_BYTES = 100 * 1024 ** 3
CACHE_TTL = 300
SQLITE_TIMEOUT = .5
SQLITE_QUERY_DEADLINE = 2
ATTR_CMNEXT_PRIVATESIZE = 0x00000008
FSOPT_NOFOLLOW = 0x00000001
FSOPT_ATTR_CMN_EXTENDED = 0x00000020
_scanners = {}
_scanners_lock = threading.Lock()
_apfs_api_lock = threading.Lock()
_apfs_api = None


def _publish_worktree_disk(state_dir: str | Path, agent_ids: Iterable[str]) -> None:
    if not agent_ids:
        return
    from studio_api.sync.resources.hub import publish_resources
    from studio_api.sync.resources.models import ResourceRef, WorktreeDiskResource

    resources = tuple(
        ResourceRef(WorktreeDiskResource(kind='worktree-disk', agentId=agent_id))
        for agent_id in sorted(set(agent_ids))
    )
    publish_resources(state_dir, *resources)


class _AttrList(ctypes.Structure):
    _fields_ = [('bitmapcount', ctypes.c_uint16), ('reserved', ctypes.c_uint16),
                ('commonattr', ctypes.c_uint32), ('volattr', ctypes.c_uint32),
                ('dirattr', ctypes.c_uint32), ('fileattr', ctypes.c_uint32),
                ('forkattr', ctypes.c_uint32)]


def _getattrlist_api():
    global _apfs_api
    if _apfs_api is not None:
        return _apfs_api
    with _apfs_api_lock:
        if _apfs_api is None:
            libc = ctypes.CDLL(None, use_errno=True)
            function = libc.getattrlist
            function.argtypes = [ctypes.c_char_p, ctypes.POINTER(_AttrList),
                                 ctypes.c_void_p, ctypes.c_size_t, ctypes.c_ulong]
            function.restype = ctypes.c_int
            _apfs_api = function
        return _apfs_api


def disk_limit_bytes():
    value = os.environ.get('CODEX_WORKTREE_DISK_LIMIT_BYTES')
    if value is None:
        return DEFAULT_LIMIT_BYTES
    try:
        result = int(value)
    except ValueError:
        return DEFAULT_LIMIT_BYTES
    return max(0, result)


def _worker_root(agent):
    if not agent.get('worktree') or agent.get('isLead'):
        return None
    key = agent['id']
    cwd = Path(agent.get('cwd') or '')
    for path in (cwd, *cwd.parents):
        if (path.name == key and path.parent.name == 'codex-agents'
                and path.parent.parent.name == '.worktrees'):
            return path if path.is_dir() and not path.is_symlink() else None
    for path in (cwd, *cwd.parents):
        candidate = path / '.worktrees' / 'codex-agents' / key
        if candidate.is_dir() and not candidate.is_symlink():
            return candidate
    return None


def _apfs_private_bytes(path):
    """Return ATTR_CMNEXT_PRIVATESIZE, or None when the volume lacks it."""
    if sys.platform != 'darwin':
        return None
    try:
        getattrlist = _getattrlist_api()
        attrs = _AttrList(5, 0, 0, 0, 0, 0, ATTR_CMNEXT_PRIVATESIZE)
        output = ctypes.create_string_buffer(16)
        options = FSOPT_ATTR_CMN_EXTENDED | FSOPT_NOFOLLOW
        if getattrlist(os.fsencode(path), ctypes.byref(attrs), output,
                       ctypes.sizeof(output), options) != 0:
            return None
        length = int.from_bytes(output.raw[:4], byteorder=sys.byteorder)
        if length < 12 or length > ctypes.sizeof(output):
            return None
        return int.from_bytes(output.raw[4:12], byteorder=sys.byteorder)
    except (AttributeError, ctypes.ArgumentError, OSError, TypeError, ValueError):
        return None


def _tree_bytes(root, value_for, *, excluded=(), pause=time.sleep):
    root_info = root.lstat()
    root_value = value_for(root, root_info)
    if root_value is None:
        return None
    total = root_value
    pending = [root]
    count = 0
    hardlinks = set()
    while pending:
        path = pending.pop()
        with os.scandir(path) as entries:
            for entry in entries:
                entry_path = Path(entry.path)
                if entry_path in excluded:
                    continue
                info = entry.stat(follow_symlinks=False)
                is_dir = entry.is_dir(follow_symlinks=False)
                identity = (info.st_dev, info.st_ino)
                if info.st_nlink <= 1 or identity not in hardlinks:
                    value = value_for(entry_path, info)
                    if value is None:
                        return None
                    total += value
                if info.st_nlink > 1 and not is_dir:
                    hardlinks.add(identity)
                if is_dir:
                    pending.append(entry_path)
                count += 1
                if count % 500 == 0:
                    pause(.01)
    return total


def _allocated_bytes(root, *, excluded=(), pause=time.sleep):
    """Count allocated blocks without following symlinks or holding a runtime lock."""
    return _tree_bytes(root, lambda _path, info: info.st_blocks * 512,
                       excluded=excluded, pause=pause)


def _measure_worktree(root, *, excluded=(), pause=time.sleep):
    private = _apfs_private_bytes(root)
    if private is not None:
        result = _tree_bytes(root, lambda path, _info: _apfs_private_bytes(path),
                             excluded=excluded, pause=pause)
        if result is not None:
            return result, 'private on APFS'
    return _allocated_bytes(root, excluded=excluded, pause=pause), 'allocated blocks'


def _change_signature(path):
    info = path.stat()
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


class WorktreeDiskScanner:
    def __init__(self, state_root, *, clock=time.time, pause=time.sleep):
        self.state_dir = Path(state_root)
        self.db_path = Path(state_root) / 'canvas.sqlite3'
        self.clock = clock
        self.pause = pause
        self.lock = threading.Lock()
        self.sizes = {}
        self.cache = {}  # Canonical worktree path -> measured row and root signature.
        self.scanning = False
        self.last_scan_at = None
        self.error = None
        self.started = False
        self.priority = set()
        self.priority_requested = {}
        self.wake = threading.Event()

    def start(self):
        with self.lock:
            if self.started:
                return
            self.started = True
        threading.Thread(target=self._run, name='studio-worktree-disk', daemon=True).start()

    def request(self, worker_ids=()):
        with self.lock:
            now = self.clock()
            requested = set(worker_ids)
            new_priority = {key for key in requested
                            if now - self.priority_requested.get(key, float('-inf')) >= CACHE_TTL}
            self.priority.update(new_priority)
            for key in new_priority:
                self.priority_requested[key] = now
            if new_priority:
                self.error = None
            first_start = not self.started
            full_scan_due = (
                not requested
                and self.started
                and (self.last_scan_at is None or now - self.last_scan_at >= CACHE_TTL)
            )
        self.start()
        if first_start or new_priority or full_scan_due:
            self.wake.set()

    def _take_priority(self):
        with self.lock:
            priority, self.priority = self.priority, set()
        return priority

    def _workers(self):
        db = sqlite3.connect(self.db_path.absolute().as_uri() + '?mode=ro', uri=True,
                             timeout=SQLITE_TIMEOUT)
        try:
            db.execute(f'PRAGMA busy_timeout={int(SQLITE_TIMEOUT * 1000)}')
            db.execute('PRAGMA query_only=ON')
            deadline = time.monotonic() + SQLITE_QUERY_DEADLINE
            db.set_progress_handler(lambda: int(time.monotonic() >= deadline), 1000)
            workers = {}
            for (record,) in db.execute('SELECT record FROM runtime_agents'):
                if time.monotonic() >= deadline:
                    raise TimeoutError('Worker database read exceeded its deadline')
                agent = json.loads(record)
                if agent and agent.get('worktree') and not agent.get('isLead'):
                    workers[agent['id']] = _worker_root(agent)
            return workers
        finally:
            db.close()

    def scan_once(self, priority_ids=()):
        workers = self._workers()
        ids = list(workers)
        priorities = set(priority_ids)
        with self.lock:
            self.scanning = True
            self.error = None
            previous_sizes = {key: dict(row) for key, row in self.sizes.items()}
        paths = {key: str(path.absolute()) if path is not None else None
                 for key, path in workers.items()}
        live_paths = {path for path in paths.values() if path is not None}
        with self.lock:
            self.cache = {path: row for path, row in self.cache.items() if path in live_paths}
            self.sizes = {key: row for key, row in self.sizes.items() if key in workers}
            self.sizes.update({key: {'state': 'missing'} for key, path in paths.items()
                               if path is None})
        todo = set(ids)
        order = []
        while todo:
            priorities.update(self._take_priority())
            selected = next((key for key in ids if key in todo and key in priorities), None)
            if selected is None:
                selected = next(key for key in ids if key in todo)
            todo.remove(selected)
            order.append(selected)
            path = workers[selected]
            if path is None:
                continue
            path_key = paths[selected]
            try:
                signature = _change_signature(path)
                with self.lock:
                    cached = self.cache.get(path_key)
                if (cached and cached.get('signature') == signature
                        and self.clock() - cached['scannedAt'] < CACHE_TTL):
                    row = {key: value for key, value in cached.items()
                           if key not in ('signature', 'path')}
                else:
                    nested = {other for other in workers.values()
                              if other is not None and other != path and other.is_relative_to(path)}
                    byte_count, measure = _measure_worktree(path, excluded=nested, pause=self.pause)
                    row = {'state': 'ready', 'bytes': byte_count,
                           'scannedAt': self.clock(), 'measure': measure}
                    with self.lock:
                        self.cache[path_key] = {**row, 'signature': signature, 'path': path_key}
            except OSError as error:
                row = {'state': 'unavailable', 'error': str(error)[:160],
                       'measure': 'allocated blocks'}
            with self.lock:
                for agent_id, agent_path in paths.items():
                    if agent_path == path_key:
                        self.sizes[agent_id] = dict(row)
        with self.lock:
            self.scanning = False
            self.last_scan_at = self.clock()
            changed_ids = {
                agent_id
                for agent_id in previous_sizes.keys() | self.sizes.keys()
                if previous_sizes.get(agent_id) != self.sizes.get(agent_id)
            }
        _publish_worktree_disk(self.state_dir, changed_ids)
        return order

    def _run(self):
        while True:
            self.wake.wait()
            self.wake.clear()
            try:
                self.scan_once(self._take_priority())
            except (OSError, sqlite3.Error, ValueError) as error:
                with self.lock:
                    self.error = str(error)[:160]
                    self.scanning = False
                    self.last_scan_at = self.clock()
                    affected = set(self.sizes)
                _publish_worktree_disk(self.state_dir, affected)

    def snapshot(self, priority_ids=()):
        self.request(priority_ids)
        with self.lock:
            sizes = {key: dict(row) for key, row in self.sizes.items()}
            scanning, error = self.scanning, self.error
        images = {}
        image_repos = set()
        try:
            db = sqlite3.connect(self.db_path.absolute().as_uri() + '?mode=ro', uri=True,
                                 timeout=SQLITE_TIMEOUT)
            try:
                db.execute('PRAGMA query_only=ON')
                for (record,) in db.execute('SELECT record FROM runtime_agents'):
                    agent = json.loads(record)
                    if agent and agent.get('imageWorkspaceRepo'):
                        image_repos.add(agent['imageWorkspaceRepo'])
                    if agent and agent.get('imageWorkspaceBaseRepo'):
                        image_repos.add(agent['imageWorkspaceBaseRepo'])
                    if (agent and agent.get('imageWorkspace')
                            and agent.get('imageWorkspacePhase') not in {'removed', 'fallback'}):
                        images[agent['id']] = agent.get('imageWorkspaceRepo')
            finally:
                db.close()
            from codex_workspace_images import base_bytes, workspace_bytes
            for agent_id, repo in images.items():
                try:
                    sizes[agent_id] = {'state': 'ready', 'bytes': workspace_bytes(agent_id),
                                       'scannedAt': self.clock(), 'measure': 'private workspace bytes'}
                except (OSError, RuntimeError, ValueError) as error:
                    sizes[agent_id] = {'state': 'unavailable', 'error': str(error)[:160],
                                       'measure': 'private workspace bytes'}
            bases = {}
            for repo in sorted(image_repos):
                try:
                    bases[repo] = {'state': 'ready', 'bytes': base_bytes(repo),
                                   'scannedAt': self.clock(), 'measure': 'private base bytes'}
                except (OSError, RuntimeError, ValueError) as error:
                    bases[repo] = {'state': 'unavailable', 'error': str(error)[:160],
                                   'measure': 'private base bytes'}
        except (ImportError, OSError, sqlite3.Error, RuntimeError, ValueError) as image_error:
            bases = {}
            if images:
                error = error or str(image_error)[:160]
        total = sum(row.get('bytes', 0) for row in sizes.values() if row['state'] == 'ready')
        base_total = sum(row.get('bytes', 0) for row in bases.values() if row['state'] == 'ready')
        measures = {row.get('measure', 'allocated blocks') for row in sizes.values()
                    if row['state'] == 'ready'}
        measure = next(iter(measures)) if len(measures) == 1 else (
            'mixed measures' if measures else 'unmeasured')
        limit = disk_limit_bytes()
        return {'workers': sizes, 'totalBytes': total, 'baseBytes': base_total,
                'storageBytes': total + base_total, 'bases': bases, 'limitBytes': limit,
                'warning': bool(limit and total + base_total >= limit), 'scanning': scanning, 'error': error,
                'measure': measure}


def scanner(state_root):
    key = str(Path(state_root).absolute())
    with _scanners_lock:
        if key not in _scanners:
            _scanners[key] = WorktreeDiskScanner(key)
        return _scanners[key]


def management_view(runtime, agents, *, db=None):
    """Add cached sizes and a scoped total to an authorized worker list."""
    disk = scanner(runtime.root).snapshot() if hasattr(runtime, 'root') else {
        'workers': {}, 'totalBytes': 0, 'limitBytes': disk_limit_bytes(),
        'warning': False, 'scanning': False, 'error': None,
        'measure': 'allocated blocks',
    }
    workers = disk['workers']
    by_agent = {a['id']: workers.get(a['id'], {'state': 'unmeasured'})
                for a in agents}
    repos = {a.get('imageWorkspaceRepo') for a in agents
             if a.get('imageWorkspace') and a.get('imageWorkspaceRepo')}
    for root_id in {a.get('rootId') for a in agents if a.get('rootId')}:
        try:
            lead = runtime.agent(root_id, db) if db is not None else runtime.agent(root_id)
        except ValueError:
            continue
        if lead.get('imageWorkspaceBaseRepo'):
            repos.add(lead['imageWorkspaceBaseRepo'])
    bases = {repo: disk.get('bases', {}).get(repo, {'state': 'unmeasured'}) for repo in repos}
    known_rows = [row for row in by_agent.values() if row.get('state') == 'ready']
    known = [row['bytes'] for row in known_rows]
    measures = {row.get('measure', 'allocated blocks') for row in known_rows}
    measure = next(iter(measures)) if len(measures) == 1 else (
        'mixed measures' if measures else 'unmeasured')
    return by_agent, {
        'totalBytes': sum(known), 'unmeasured': sum(row.get('state') != 'ready'
                                                   for row in by_agent.values()),
        'allWorkersBytes': disk['totalBytes'], 'limitBytes': disk['limitBytes'],
        'baseBytes': sum(row.get('bytes', 0) for row in bases.values()
                         if row.get('state') == 'ready'),
        'bases': bases, 'warning': disk['warning'], 'scanning': disk['scanning'],
        'measure': measure,
    }
