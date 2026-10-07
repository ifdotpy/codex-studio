#!/usr/bin/env python3
"""Disk measurements keep their bytes while the background scan has a CPU budget."""
import ast
import ctypes
import importlib.util
import inspect
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import textwrap
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
source = Path(os.environ.get('STUDIO_DISK_CPU_SOURCE',
    Path(__file__).resolve().parents[1] / 'scripts' / 'codex_worktree_disk.py'))
spec = importlib.util.spec_from_file_location('disk_cpu_fixture', source)
disk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(disk)


class Clock:
    def __init__(self):
        self.wall = self.cpu = 0.0
        self.pauses = []

    def work(self):
        self.wall += .0001
        self.cpu += .0001

    def pause(self, seconds):
        self.pauses.append(seconds)
        self.wall += seconds


def legacy_function(function):
    """Remove only the new pacing calls to retain the old active code object."""
    tree = ast.parse(textwrap.dedent(inspect.getsource(function)))

    class WithoutBudget(ast.NodeTransformer):
        def visit_Expr(self, node):
            if isinstance(node.value, ast.Call) and getattr(node.value.func, 'id', '') == '_scan_pause':
                return None
            return self.generic_visit(node)

        def visit_If(self, node):
            if any(isinstance(child, ast.Name) and child.id == '_scan_pause'
                   for child in ast.walk(node.test)):
                node.test = node.test.values[0]
            node = self.generic_visit(node)
            return node if node.body else None

    tree = ast.fix_missing_locations(WithoutBudget().visit(tree))
    namespace = {}
    exec(compile(tree, function.__code__.co_filename, 'exec'), function.__globals__, namespace)
    return namespace[function.__name__]


class DiskCpuContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.worktree = self.root / 'repo' / '.worktrees' / 'codex-agents' / 'worker'
        self.worktree.mkdir(parents=True)
        self.paths = [self.worktree]
        for index in range(8):
            directory = self.worktree / str(index)
            directory.mkdir()
            self.paths.append(directory)
            for entry in range(128):
                file = directory / str(entry)
                file.write_bytes(b'x' * 512)
                self.paths.append(file)
        (self.worktree / 'hardlink').hardlink_to(self.paths[-1])
        target = self.root / 'outside'
        target.mkdir()
        (target / 'keep').write_bytes(b'y' * 8192)
        link = self.worktree / 'symlink'
        link.symlink_to(target, target_is_directory=True)
        self.paths.append(link)
        self.child = self.worktree / 'excluded'
        self.child.mkdir()
        (self.child / 'child-file').write_bytes(b'z' * 4096)

    def worker(self, operation):
        result = {}

        def run():
            try:
                result['value'] = operation()
            except BaseException as error:
                result['error'] = error

        thread = threading.Thread(target=run, name='studio-worktree-disk')
        thread.start()
        thread.join(3)
        self.assertFalse(thread.is_alive(), 'The private scan must finish')
        if 'error' in result:
            raise result['error']
        return result['value']

    def clock_patches(self, clock):
        return (patch.object(disk.time, 'monotonic', lambda: clock.wall),
                patch.object(disk.time, 'thread_time', lambda: clock.cpu),
                patch.object(disk, '_scan_cpu_state', threading.local(), create=True))

    def test_allocated_walk_keeps_every_byte_and_limits_background_cpu(self):
        clock = Clock()
        visited = []

        def allocated(path, info):
            clock.work()
            visited.append(path)
            return info.st_blocks * 512

        wall, cpu, state = self.clock_patches(clock)
        with wall, cpu, state:
            measured = self.worker(lambda: disk._tree_bytes(
                self.worktree, allocated, excluded={self.child}, pause=clock.pause))
        expected = sum(path.lstat().st_blocks * 512 for path in self.paths)
        self.assertEqual(measured, expected)
        identity = lambda path: (path.lstat().st_dev, path.lstat().st_ino)
        self.assertEqual({identity(path) for path in visited},
                         {identity(path) for path in self.paths})
        self.assertEqual(len(visited), len(self.paths))
        self.assertLessEqual(clock.cpu / clock.wall, .17,
                             'The scan must use at most the CPU budget plus one batch')

    def test_old_active_apfs_walker_uses_the_next_updated_entry(self):
        clock = Clock()
        old_tree = legacy_function(disk._tree_bytes)
        old_apfs = legacy_function(disk._apfs_private_bytes)
        updated_code = disk._apfs_private_bytes.__code__
        entered, release = threading.Event(), threading.Event()
        calls = []

        def native_api(path, _attrs, output, _size, _options):
            clock.work()
            calls.append(Path(os.fsdecode(path)))
            if len(calls) == 20:
                entered.set()
                if not release.wait(2):
                    raise TimeoutError('The private update gate did not open')
            raw = (12).to_bytes(4, sys.byteorder) + (13).to_bytes(8, sys.byteorder)
            ctypes.memmove(output, raw, len(raw))
            return 0

        result = {}

        def run():
            try:
                result['bytes'] = old_tree(self.worktree,
                    lambda path, _info: disk._apfs_private_bytes(path),
                    excluded={self.child}, pause=clock.pause)
            except BaseException as error:
                result['error'] = error

        wall, cpu, state = self.clock_patches(clock)
        with wall, cpu, state, patch.object(disk.sys, 'platform', 'darwin'), \
                patch.object(disk.time, 'sleep', clock.pause), \
                patch.object(disk, '_getattrlist_api', return_value=native_api), \
                patch.object(disk, '_apfs_private_bytes', old_apfs):
            thread = threading.Thread(target=run, name='studio-worktree-disk')
            thread.start()
            try:
                self.assertTrue(entered.wait(2))
                old_apfs.__code__ = updated_code
            finally:
                release.set()
                thread.join(3)
            self.assertFalse(thread.is_alive())
        if 'error' in result:
            raise result['error']
        self.assertEqual(result['bytes'], 13 * len(self.paths))
        self.assertEqual(len(calls), len(self.paths))
        self.assertLessEqual(clock.cpu / clock.wall, .18,
                             'An old active frame must use the new per-entry budget')

    def test_legacy_timer_skips_a_fresh_full_scan_but_priority_and_expiry_work(self):
        state = self.root / 'state'
        state.mkdir()
        db = sqlite3.connect(state / 'canvas.sqlite3')
        try:
            db.execute('CREATE TABLE runtime_agents (id TEXT, record TEXT)')
            db.commit()
        finally:
            db.close()
        now = [0.0]
        scanner = disk.WorktreeDiskScanner(state, clock=lambda: now[0], pause=lambda _: None)
        workers = {'worker': self.worktree}
        child = self.root / 'new-worker'
        child.mkdir()
        with patch.object(scanner, '_workers', side_effect=lambda: dict(workers)) as read, \
                patch.object(disk, '_measure_worktree', return_value=(123, 'allocated blocks')) as measure, \
                patch.object(disk, '_publish_worktree_disk', create=True):
            self.worker(scanner.scan_once)
            if hasattr(scanner, 'last_scan_at'):
                del scanner.last_scan_at  # Simulate a completed old active frame.
            now[0] = 60
            (self.worktree / 'new-entry').touch()
            self.assertEqual(self.worker(scanner.scan_once), [])
            self.assertEqual(read.call_count, 1)
            self.assertEqual(measure.call_count, 1)
            workers['new-worker'] = child
            self.worker(lambda: scanner.scan_once(('new-worker',)))
            self.assertEqual(scanner.sizes['new-worker']['bytes'], 123)
            workers['worker'] = None
            self.worker(lambda: scanner.scan_once(('worker',)))
            self.assertEqual(scanner.sizes['worker']['state'], 'missing')
            self.assertNotIn(str(self.worktree), scanner.cache)
            now[0] += disk.CACHE_TTL
            self.worker(scanner.scan_once)
            self.assertEqual(read.call_count, 4)
            self.assertEqual(measure.call_count, 4)


if __name__ == '__main__':
    unittest.main()
