"""Background, read-only measurements of Studio worker worktrees."""

import json
import os
import sqlite3
import threading
import time
from pathlib import Path


DEFAULT_LIMIT_BYTES = 100 * 1024 ** 3
_scanners = {}
_scanners_lock = threading.Lock()


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


def _allocated_bytes(root, *, excluded=(), pause=time.sleep):
    """Count allocated blocks without following symlinks or holding a runtime lock."""
    total = root.lstat().st_blocks * 512
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
                entry_stat = entry.stat(follow_symlinks=False)
                identity = (entry_stat.st_dev, entry_stat.st_ino)
                if entry_stat.st_nlink <= 1 or identity not in hardlinks:
                    total += entry_stat.st_blocks * 512
                if entry_stat.st_nlink > 1 and not entry.is_dir(follow_symlinks=False):
                    hardlinks.add(identity)
                if entry.is_dir(follow_symlinks=False):
                    pending.append(entry_path)
                count += 1
                if count % 500 == 0:
                    pause(.01)
    return total


class WorktreeDiskScanner:
    def __init__(self, state_root, *, clock=time.time, pause=time.sleep):
        self.db_path = Path(state_root) / 'canvas.sqlite3'
        self.clock = clock
        self.pause = pause
        self.lock = threading.Lock()
        self.sizes = {}
        self.scanning = False
        self.error = None
        self.started = False

    def start(self):
        with self.lock:
            if self.started:
                return
            self.started = True
        threading.Thread(target=self._run, name='studio-worktree-disk', daemon=True).start()

    def _workers(self):
        db = sqlite3.connect(self.db_path.absolute().as_uri() + '?mode=ro', uri=True, timeout=3)
        try:
            return {agent['id']: _worker_root(agent)
                    for (record,) in db.execute('SELECT record FROM runtime_agents')
                    if (agent := json.loads(record)) and agent.get('worktree') and not agent.get('isLead')}
        finally:
            db.close()

    def scan_once(self):
        workers = self._workers()
        with self.lock:
            self.scanning = True
            self.error = None
            self.sizes = {key: value for key, value in self.sizes.items() if key in workers}
        for key, path in workers.items():
            if path is None:
                with self.lock:
                    self.sizes[key] = {'state': 'missing'}
                continue
            with self.lock:
                cached = self.sizes.get(key)
            if (cached and cached.get('state') == 'ready' and cached.get('path') == str(path)
                    and self.clock() - cached['scannedAt'] < 300):
                continue
            try:
                nested = {other for other in workers.values()
                          if other is not None and other != path and other.is_relative_to(path)}
                size = _allocated_bytes(path, excluded=nested, pause=self.pause)
            except OSError as error:
                with self.lock:
                    self.sizes[key] = {'state': 'unavailable', 'error': str(error)[:160], 'path': str(path)}
            else:
                with self.lock:
                    self.sizes[key] = {'state': 'ready', 'bytes': size,
                                       'scannedAt': self.clock(), 'path': str(path)}
        with self.lock:
            self.scanning = False

    def _run(self):
        while True:
            try:
                self.scan_once()
            except (OSError, sqlite3.Error, ValueError) as error:
                with self.lock:
                    self.error = str(error)[:160]
                    self.scanning = False
            self.pause(60)

    def snapshot(self):
        self.start()
        with self.lock:
            sizes = {key: {field: value for field, value in row.items() if field != 'path'}
                     for key, row in self.sizes.items()}
            scanning, error = self.scanning, self.error
        total = sum(row.get('bytes', 0) for row in sizes.values() if row['state'] == 'ready')
        limit = disk_limit_bytes()
        return {'workers': sizes, 'totalBytes': total, 'limitBytes': limit,
                'warning': bool(limit and total >= limit), 'scanning': scanning, 'error': error,
                'measure': 'allocated blocks', 'cloneSharedExtentsMayOverlap': True}


def scanner(state_root):
    key = str(Path(state_root).absolute())
    with _scanners_lock:
        if key not in _scanners:
            _scanners[key] = WorktreeDiskScanner(key)
        return _scanners[key]


def management_view(runtime, agents):
    """Add cached sizes and a scoped total to an authorized worker list."""
    disk = scanner(runtime.root).snapshot() if hasattr(runtime, 'root') else {
        'workers': {}, 'totalBytes': 0, 'limitBytes': disk_limit_bytes(),
        'warning': False, 'scanning': False, 'error': None,
    }
    workers = disk['workers']
    by_agent = {a['id']: workers.get(a['id'], {'state': 'unmeasured'})
                for a in agents}
    known = [row['bytes'] for row in by_agent.values() if row.get('state') == 'ready']
    return by_agent, {
        'totalBytes': sum(known), 'unmeasured': sum(row.get('state') != 'ready'
                                                   for row in by_agent.values()),
        'allWorkersBytes': disk['totalBytes'], 'limitBytes': disk['limitBytes'],
        'warning': disk['warning'], 'scanning': disk['scanning'],
        'measure': 'allocated blocks', 'cloneSharedExtentsMayOverlap': True,
    }
