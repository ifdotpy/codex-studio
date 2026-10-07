#!/usr/bin/env python3
"""Disk scans group lexical roots without changing priority or exclusion scope."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import statistics
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
source = Path(os.environ.get('STUDIO_DISK_GROUPS_SOURCE',
    Path(__file__).resolve().parents[1] / 'scripts' / 'codex_worktree_disk.py'))
spec = importlib.util.spec_from_file_location('disk_groups_fixture', source)
disk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(disk)


class CountPath(type(Path())):
    comparisons = 0

    def is_relative_to(self, other):
        type(self).comparisons += 1
        return super().is_relative_to(other)


class DiskGroups(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='studio-disk-groups-')
        self.root = Path(self.tmp.name)
        self.now = 10.0
        self.scanner = disk.WorktreeDiskScanner(self.root / 'state', clock=lambda: self.now,
                                               pause=lambda _seconds: None)

    def tearDown(self):
        self.tmp.cleanup()

    def scan(self, workers, *, priority=(), measure=None, signature=None):
        with patch.object(self.scanner, '_workers', return_value=workers), \
                patch.object(disk, '_change_signature', side_effect=signature or (lambda _path: (1, 2, 3, 4, 5))) as stat, \
                patch.object(disk, '_measure_worktree', side_effect=measure or (lambda *_args, **_kwargs: (100, 'private on APFS'))) as walk, \
                patch.object(disk, '_publish_worktree_disk'):
            order = self.scanner.scan_once(priority)
        return order, stat, walk

    def test_aliases_share_one_signature_and_sample_with_independent_rows(self):
        path = self.root / 'worktree'
        workers = {'first': path, 'second': path, 'third': path}
        order, stat, walk = self.scan(workers, priority=('third',))
        self.assertEqual(order, ['third', 'first', 'second'])
        self.assertEqual(stat.call_count, 1)
        self.assertEqual(walk.call_count, 1)
        self.assertEqual(set(self.scanner.sizes), set(workers))
        self.assertEqual(list(self.scanner.sizes.values()),
                         [{'state': 'ready', 'bytes': 100, 'scannedAt': 10.0,
                           'measure': 'private on APFS'}] * 3)
        self.scanner.sizes['first']['bytes'] = 999
        self.assertEqual(self.scanner.sizes['second']['bytes'], 100)
        self.assertEqual(self.scanner.cache[str(path)]['bytes'], 100)
        _, stat, walk = self.scan(workers)
        self.assertEqual(stat.call_count, 1)
        self.assertEqual(walk.call_count, 0)
        self.assertEqual(self.scanner.sizes['first']['bytes'], 100)
        self.assertNotIn('excluded', self.scanner.sizes['first'])

    def test_lexical_descendants_match_the_original_scope_without_pairwise_search(self):
        roots = [CountPath('/scope/parent'), CountPath('/scope/parent/child'),
                 CountPath('/scope/parent/child/deep'), CountPath('/scope/parent-other'),
                 CountPath('relative'), CountPath('relative/nested'), CountPath('relative/../escape')]
        expected = {root: {other for other in roots if other != root and other.is_relative_to(root)}
                    for root in roots}
        CountPath.comparisons = 0
        actual = {}

        def measure(root, *, excluded, pause):
            self.assertTrue(self.scanner.lock.acquire(blocking=False))
            self.scanner.lock.release()
            actual[root] = set(excluded)
            return 100, 'private on APFS'

        self.scan({str(index): root for index, root in enumerate(roots)}, measure=measure)
        self.assertEqual(actual, expected)
        self.assertEqual(CountPath.comparisons, 0)

    def test_dynamic_priorities_keep_exact_agent_order_including_sampled_aliases(self):
        paths = {key: self.root / key for key in ('first', 'b', 'c', 'd')}
        workers = {**paths, 'missing': None, 'c-alias': paths['c']}

        def measure(root, **_kwargs):
            if root == paths['c']:
                with self.scanner.lock:
                    self.scanner.priority.update({'b', 'd', 'c-alias', 'unknown', 'c'})
            return 100, 'allocated blocks'

        order, _, walk = self.scan(workers, priority=('c', 'missing'), measure=measure)
        self.assertEqual(order, ['c', 'b', 'd', 'missing', 'c-alias', 'first'])
        self.assertEqual([call.args[0] for call in walk.call_args_list],
                         [paths[key] for key in ('c', 'b', 'd', 'first')])
        self.assertEqual(self.scanner.sizes['missing'], {'state': 'missing'})
        self.assertEqual(self.scanner.sizes['c-alias'], self.scanner.sizes['c'])

    def test_added_and_removed_nested_worker_changes_parent_cache_without_root_change(self):
        parent = self.root / 'parent'
        child = parent / 'nested'
        child.mkdir(parents=True)
        (parent / 'own').write_bytes(b'p' * 8192)
        (child / 'child').write_bytes(b'c' * 16384)
        (parent / 'hardlink').hardlink_to(parent / 'own')
        outside = self.root / 'outside'
        outside.write_bytes(b'x' * 65536)
        (parent / 'symlink').symlink_to(outside)
        signature = disk._change_signature(parent)
        whole = disk._allocated_bytes(parent, pause=lambda _: None)
        separate = disk._allocated_bytes(parent, excluded={child}, pause=lambda _: None)
        child_bytes = disk._allocated_bytes(child, pause=lambda _: None)

        def scan(workers):
            with patch.object(self.scanner, '_workers', return_value=workers), \
                    patch.object(disk, '_apfs_private_bytes', return_value=None), \
                    patch.object(disk, '_measure_worktree', wraps=disk._measure_worktree) as walk, \
                    patch.object(disk, '_publish_worktree_disk'):
                self.scanner.scan_once()
            return walk

        scan({'parent': parent})
        self.assertEqual(self.scanner.sizes['parent']['bytes'], whole)
        walk = scan({'parent': parent, 'child': child})
        self.assertEqual(disk._change_signature(parent), signature)
        self.assertEqual(walk.call_count, 2)
        self.assertEqual(self.scanner.sizes['parent']['bytes'], separate)
        self.assertEqual(self.scanner.sizes['child']['bytes'], child_bytes)
        self.assertEqual(separate + child_bytes, whole)
        walk = scan({'parent': parent})
        self.assertEqual(walk.call_count, 1)
        self.assertEqual(disk._change_signature(parent), signature)
        self.assertEqual(self.scanner.sizes['parent']['bytes'], whole)
        self.assertNotIn('child', self.scanner.sizes)

    def test_first_sample_time_survives_alias_delay_and_next_pass_checks_ttl_and_identity(self):
        first, other = self.root / 'first', self.root / 'other'
        generation = {first: 1, other: 1}
        workers = {'first': first, 'other': other, 'late-alias': first}

        def measure(path, **_kwargs):
            if path == other and self.now < 1000.0:
                self.now = 1000.0
                generation[first] = 2
            return generation[path] * 100, 'private on APFS'

        signature = lambda path: (1, generation[path], 3, 4, 5)
        order, stat, walk = self.scan(workers, measure=measure, signature=signature)
        self.assertEqual(order, list(workers))
        self.assertEqual(stat.call_count, 2)
        self.assertEqual(walk.call_count, 2)
        self.assertEqual(self.scanner.sizes['late-alias'], self.scanner.sizes['first'])
        self.assertEqual(self.scanner.sizes['first']['bytes'], 100)
        self.assertEqual(self.scanner.sizes['first']['scannedAt'], 10.0)
        _, stat, walk = self.scan(workers, measure=measure, signature=signature)
        self.assertEqual(stat.call_count, 2)
        self.assertEqual(walk.call_count, 1)
        self.assertEqual(self.scanner.sizes['first']['bytes'], 200)
        self.assertEqual(self.scanner.sizes['first']['scannedAt'], 1000.0)
        self.now += disk.CACHE_TTL
        _, _, walk = self.scan(workers, measure=measure, signature=signature)
        self.assertEqual(walk.call_count, 2)

    def test_unavailable_aliases_and_legacy_cache_do_not_hide_missing_topology(self):
        path = self.root / 'gone'
        workers = {'first': path, 'alias': path}
        self.scanner.cache[str(path)] = {'state': 'ready', 'bytes': 999, 'scannedAt': self.now,
                                        'measure': 'private on APFS', 'signature': (1, 2, 3, 4, 5), 'path': str(path)}
        _, _, walk = self.scan(workers)
        self.assertEqual(walk.call_count, 1)
        self.assertEqual(self.scanner.sizes['first']['bytes'], 100)
        _, stat, walk = self.scan(workers, signature=lambda _: (_ for _ in ()).throw(FileNotFoundError('private missing root')))
        self.assertEqual(stat.call_count, 1)
        self.assertEqual(walk.call_count, 0)
        self.assertEqual(self.scanner.sizes['first']['state'], 'unavailable')
        self.assertEqual(self.scanner.sizes['alias'], self.scanner.sizes['first'])
        self.scan({'first': None})
        self.assertEqual(self.scanner.sizes, {'first': {'state': 'missing'}})
        self.assertEqual(self.scanner.cache, {})


def benchmark():
    workers = {}
    for index in range(512):
        path = Path('/private/tmp/studio-disk-metadata') / ('worker-' + str(index))
        workers['worker-' + str(index)] = path
        workers['alias-' + str(index)] = path
    priority = tuple('alias-' + str(index) for index in range(0, 512, 4))
    cpus, signatures, measurements, digest = [], [], [], None
    for _ in range(5):
        scanner = disk.WorktreeDiskScanner('/private/tmp/studio-disk-metadata-state',
                                          clock=lambda: 10.0, pause=lambda _: None)
        with patch.object(scanner, '_workers', return_value=workers), \
                patch.object(disk, '_change_signature', return_value=(1, 2, 3, 4, 5)) as stat, \
                patch.object(disk, '_measure_worktree', return_value=(100, 'private on APFS')) as walk, \
                patch.object(disk, '_publish_worktree_disk'):
            started = time.thread_time()
            order = scanner.scan_once(priority)
            cpus.append((time.thread_time() - started) * 1000)
        signatures.append(stat.call_count)
        measurements.append(walk.call_count)
        current = hashlib.sha256(json.dumps({'order': order, 'sizes': scanner.sizes},
                                           sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        if digest is not None:
            assert current == digest
        digest = current
    print(json.dumps({'sourceSHA': hashlib.sha256(source.read_bytes()).hexdigest(),
                      'agents': len(workers), 'distinctRoots': 512, 'runs': len(cpus),
                      'cpuMs': [round(value, 3) for value in cpus],
                      'medianCpuMs': round(statistics.median(cpus), 3),
                      'signatureCalls': signatures, 'measurementCalls': measurements,
                      'resultSHA': digest}), flush=True)


if __name__ == '__main__':
    if sys.argv[1:] == ['--benchmark']:
        benchmark()
    else:
        unittest.main()
