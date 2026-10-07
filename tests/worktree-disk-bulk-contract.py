#!/usr/bin/env python3
"""APFS bulk measurements preserve bytes and reject incomplete native records."""
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

source = Path(os.environ.get('STUDIO_DISK_BULK_SOURCE',
    Path(__file__).resolve().parents[1] / 'scripts' / 'codex_worktree_disk.py'))
spec = importlib.util.spec_from_file_location('disk_bulk_fixture', source)
disk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(disk)

COMMON = 0xa204000b


def record(name, device=1, inode=2, kind=1, links=1, private=4096,
           flags=0, mount=0, returned=None, error=0):
    name = os.fsencode(name) + b'\0'
    length = (68 + len(name) + 7) & ~7
    masks = returned or (COMMON, 0, 4 if kind == 2 else 0, 0 if kind == 2 else 1, 8)
    return (struct.pack('=I5IIiIIIIQIq', length, *masks, error, 40, len(name),
                        device, kind, flags, inode, mount if kind == 2 else links, private)
            + name + bytes(length - 68 - len(name)))


class FakeBulk:
    def __init__(self, paths, *, alter=None):
        self.paths = {(path.lstat().st_dev, path.lstat().st_ino): path for path in paths}
        self.alter = alter
        self.read = set()
        self.calls = 0

    def __call__(self, fd, attrs, output, size, options):
        self.calls += 1
        requested = ctypes.cast(attrs, ctypes.POINTER(disk._AttrList)).contents
        assert (requested.commonattr, requested.dirattr, requested.fileattr,
                requested.forkattr, options) == (COMMON, 4, 1, 8, 0x28)
        info = os.fstat(fd)
        key = (info.st_dev, info.st_ino)
        if key in self.read:
            return 0
        self.read.add(key)
        path = self.paths[key]
        rows = []
        for child in path.iterdir():
            info = child.lstat()
            kind = 2 if stat.S_ISDIR(info.st_mode) else 5 if stat.S_ISLNK(info.st_mode) else 1
            row = record(child.name, info.st_dev, info.st_ino, kind, info.st_nlink,
                         info.st_blocks * 512)
            rows.append(self.alter(child, row) if self.alter else row)
        raw = b''.join(rows)
        assert len(raw) <= size
        ctypes.memmove(output, raw, len(raw))
        return len(rows)


class BulkContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'root'
        self.root.mkdir()
        self.child = self.root / 'dir'
        self.child.mkdir()
        (self.child / 'nested').write_bytes(b'a' * 4096)
        self.file = self.root / 'file'
        self.file.write_bytes(b'b' * 8192)
        (self.root / 'hardlink').hardlink_to(self.file)
        (self.root / 'symlink').symlink_to(self.child, target_is_directory=True)
        self.excluded = self.root / 'excluded'
        self.excluded.mkdir()
        (self.excluded / 'uncounted').write_bytes(b'c' * 4096)

    def measure_fake(self, native, private=None):
        private = private or (lambda path: path.lstat().st_blocks * 512)
        with patch.object(disk, '_getattrlistbulk_api', return_value=native), \
                patch.object(disk, '_apfs_private_bytes', side_effect=private) as exact:
            result = disk._measure_worktree(self.root, excluded={self.excluded}, pause=lambda _: None)
        return result, exact.call_count

    def test_bulk_counts_hardlinks_symlinks_and_excluded_subtrees_once(self):
        native = FakeBulk([self.root, self.child, self.excluded])
        result, calls = self.measure_fake(native)
        expected = disk._allocated_bytes(self.root, excluded={self.excluded}, pause=lambda _: None)
        self.assertEqual(result, (expected, 'private on APFS'))
        self.assertEqual(calls, 1)
        self.assertEqual(native.calls, 4)
        excluded_info = self.excluded.lstat()
        self.assertNotIn((excluded_info.st_dev, excluded_info.st_ino), native.read)

    def test_parser_requires_every_supported_attribute_and_exact_groups(self):
        for index in range(5):
            masks = [COMMON, 0, 0, 1, 8]
            masks[index] = 0 if masks[index] else 1
            with self.subTest(group=index), self.assertRaises(ValueError):
                list(disk._parse_bulk_entries(record('file', returned=tuple(masks)), 1))
        for bit in (1, 2, 8, 0x40000, 0x2000000, 0x20000000, 0x80000000):
            masks = (COMMON & ~bit, 0, 0, 1, 8)
            with self.subTest(common=bit), self.assertRaises(ValueError):
                list(disk._parse_bulk_entries(record('file', returned=masks), 1))

    def test_parser_rejects_native_errors_sizes_types_and_links(self):
        for changes in ({'error': 5}, {'private': -1}, {'kind': 0}, {'kind': 9}, {'links': 0}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                list(disk._parse_bulk_entries(record('file', **changes), 1))
        for count in (-1, 0, 2, True):
            with self.subTest(count=count), self.assertRaises(ValueError):
                list(disk._parse_bulk_entries(record('file'), count))

    def test_parser_rejects_record_and_name_buffer_escapes(self):
        original = record('file')
        cases = [original[:67], original[:-1]]
        for offset, format_, value in ((0, '=I', 64), (0, '=I', 73),
                                     (0, '=I', 65536), (28, '=i', -1),
                                     (28, '=i', 65536), (32, '=I', 65536)):
            raw = bytearray(original)
            struct.pack_into(format_, raw, offset, value)
            cases.append(bytes(raw))
        cases.extend(record(name) for name in ('', '.', '..', '../file', 'a\0b'))
        raw = bytearray(original)
        raw[72] = ord('x')
        cases.append(bytes(raw))
        for raw in cases:
            with self.subTest(raw=raw.hex()), self.assertRaises(ValueError):
                list(disk._parse_bulk_entries(raw, 1))

    def test_parser_reads_aligned_batches_and_non_utf8_names(self):
        raw = record('unicode-λ') + record(b'raw-\xff', kind=2, inode=3)
        rows = list(disk._parse_bulk_entries(raw, 2))
        self.assertEqual([os.fsencode(row[0]) for row in rows], [os.fsencode('unicode-λ'), b'raw-\xff'])
        self.assertEqual(rows[1][3:6], (2, 0, 4096))

    def test_omitted_private_attribute_uses_exact_path_fallback(self):
        def omit(_path, row):
            raw = bytearray(row)
            struct.pack_into('=I', raw, 20, 0)
            struct.pack_into('=q', raw, 60, 0)
            return bytes(raw)
        result, calls = self.measure_fake(FakeBulk([self.root, self.child], alter=omit))
        expected = disk._allocated_bytes(self.root, excluded={self.excluded}, pause=lambda _: None)
        self.assertEqual(result, (expected, 'private on APFS'))
        self.assertGreater(calls, 1, 'Missing PRIVATE cannot report zero bytes')

    def test_mount_and_firmlink_metadata_use_exact_path_fallback(self):
        for flags, mount in ((0x800000, 0), (0, 1), (0, 2)):
            def ambiguous(path, row):
                if path != self.child:
                    return row
                raw = bytearray(row)
                struct.pack_into('=I', raw, 44, flags)
                struct.pack_into('=I', raw, 56, mount)
                struct.pack_into('=q', raw, 60, 999999)
                return bytes(raw)
            with self.subTest(flags=flags, mount=mount):
                result, calls = self.measure_fake(FakeBulk([self.root, self.child], alter=ambiguous))
                expected = disk._allocated_bytes(self.root, excluded={self.excluded}, pause=lambda _: None)
                self.assertEqual(result, (expected, 'private on APFS'))
                self.assertGreater(calls, 1)

    def test_directory_identity_change_rejects_bulk_before_descent(self):
        def changed(path, row):
            if path != self.child:
                return row
            raw = bytearray(row)
            struct.pack_into('=Q', raw, 48, path.lstat().st_ino + 1)
            return bytes(raw)
        native = FakeBulk([self.root, self.child], alter=changed)
        result, calls = self.measure_fake(native)
        expected = disk._allocated_bytes(self.root, excluded={self.excluded}, pause=lambda _: None)
        self.assertEqual(result, (expected, 'private on APFS'))
        self.assertGreater(calls, 1)
        self.assertEqual(native.calls, 1)

    def test_opened_directory_identity_and_symlink_are_fenced(self):
        info = self.root.lstat()
        native = FakeBulk([self.root])
        with patch.object(disk, '_getattrlistbulk_api', return_value=native):
            with self.assertRaises(ValueError):
                list(disk._apfs_bulk_entries(self.root, (info.st_dev, info.st_ino + 1)))
            with self.assertRaises(OSError):
                list(disk._apfs_bulk_entries(self.root / 'symlink', (info.st_dev, info.st_ino)))
        self.assertEqual(native.calls, 0)

    def test_unavailable_bulk_and_private_use_existing_allocated_fallback(self):
        def missing(_path):
            return None if _path in (self.file, self.root / 'hardlink') else _path.lstat().st_blocks * 512
        with patch.object(disk, '_getattrlistbulk_api', side_effect=AttributeError('unavailable')):
            result, calls = self.measure_fake_unavailable(missing)
        expected = disk._allocated_bytes(self.root, excluded={self.excluded}, pause=lambda _: None)
        self.assertEqual(result, (expected, 'allocated blocks'))
        self.assertGreater(calls, 1)

    def measure_fake_unavailable(self, private):
        with patch.object(disk, '_apfs_private_bytes', side_effect=private) as exact:
            result = disk._measure_worktree(self.root, excluded={self.excluded}, pause=lambda _: None)
        return result, exact.call_count

    def test_bulk_failure_and_early_return_close_directory_descriptor(self):
        opened, closed = [], []
        real_open, real_close = os.open, os.close
        def open_(*args):
            fd = real_open(*args)
            opened.append(fd)
            return fd
        def close_(fd):
            closed.append(fd)
            real_close(fd)
        def failure(*_args):
            ctypes.set_errno(34)
            return -1
        with patch.object(disk, '_getattrlistbulk_api', return_value=failure), \
                patch.object(disk.os, 'open', side_effect=open_), \
                patch.object(disk.os, 'close', side_effect=close_):
            result = disk._apfs_bulk_bytes(self.root, 0, pause=lambda _: None)
        self.assertIsNone(result)
        self.assertEqual(opened, closed)
        opened.clear()
        closed.clear()
        def mount(path, row):
            if path != self.child:
                return row
            raw = bytearray(row)
            struct.pack_into('=I', raw, 56, 1)
            return bytes(raw)
        native = FakeBulk([self.root], alter=mount)
        with patch.object(disk, '_getattrlistbulk_api', return_value=native), \
                patch.object(disk.os, 'open', side_effect=open_), \
                patch.object(disk.os, 'close', side_effect=close_):
            result = disk._apfs_bulk_bytes(self.root, 0, pause=lambda _: None)
        self.assertIsNone(result)
        self.assertEqual(opened, closed)

    def test_bulk_native_batches_keep_the_existing_background_cpu_budget(self):
        for index in range(80):
            (self.root / str(index)).write_bytes(b'x')
        native = FakeBulk([self.root, self.child])
        clock = {'wall': 0.0, 'cpu': 0.0, 'pauses': []}
        def measured(*args):
            clock['cpu'] += .01
            clock['wall'] += .01
            return native(*args)
        def pause(seconds):
            clock['pauses'].append(seconds)
            clock['wall'] += seconds
        result = {}
        def run():
            result['bytes'] = disk._apfs_bulk_bytes(
                self.root, self.root.lstat().st_blocks * 512,
                excluded={self.excluded}, pause=pause)
        with patch.object(disk, '_getattrlistbulk_api', return_value=measured), \
                patch.object(disk.time, 'monotonic', side_effect=lambda: clock['wall']), \
                patch.object(disk.time, 'thread_time', side_effect=lambda: clock['cpu']), \
                patch.object(disk, '_scan_cpu_state', threading.local()):
            thread = threading.Thread(target=run, name='studio-worktree-disk')
            thread.start()
            thread.join(2)
            self.assertFalse(thread.is_alive())
        expected = disk._allocated_bytes(self.root, excluded={self.excluded}, pause=lambda _: None)
        self.assertEqual(result['bytes'], expected)
        self.assertTrue(clock['pauses'])
        self.assertLessEqual(clock['cpu'] / clock['wall'], .16)


@unittest.skipUnless(sys.platform == 'darwin', 'Darwin bulk ABI fixture')
class NativeBulkContracts(unittest.TestCase):
    def test_real_apfs_batches_preserve_clone_sparse_hardlink_and_nested_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / 'root'
            root.mkdir()
            if disk._apfs_private_bytes(root) is None:
                self.skipTest('The private fixture volume lacks APFS PRIVATE')
            for branch in range(8):
                folder = root / str(branch)
                folder.mkdir()
                for entry in range(256):
                    # The first directory needs more than one native buffer.
                    name = str(entry).zfill(3) + 'x' * 237 if branch == 0 else str(entry)
                    (folder / name).write_bytes(b'x' * 512)
            source = root / 'clone-source'
            source.write_bytes(os.urandom(65536))
            clone = root / 'clone'
            subprocess.run(['cp', '-c', str(source), str(clone)], check=True,
                           capture_output=True, timeout=3)
            (root / 'hardlink').hardlink_to(source)
            (root / 'symlink').symlink_to(root / '0', target_is_directory=True)
            with (root / 'sparse').open('wb') as output:
                output.truncate(16 * 1024 * 1024)
            (root / 'unicode-λ').write_bytes(b'z' * 4096)
            excluded = root / 'excluded'
            excluded.mkdir()
            (excluded / 'uncounted').write_bytes(os.urandom(8192))
            native = disk._getattrlist_api()
            bulk = ctypes.CDLL(None, use_errno=True).getattrlistbulk
            bulk.argtypes = [ctypes.c_int, ctypes.POINTER(disk._AttrList), ctypes.c_void_p,
                             ctypes.c_size_t, ctypes.c_uint64]
            bulk.restype = ctypes.c_int
            calls = {'exact': 0, 'bulk': 0}
            def exact(*args):
                calls['exact'] += 1
                return native(*args)
            def batched(*args):
                calls['bulk'] += 1
                return bulk(*args)
            with patch.object(disk, '_getattrlist_api', return_value=exact):
                cpu = time.thread_time()
                expected = disk._tree_bytes(root, lambda path, _info: disk._apfs_private_bytes(path),
                                            excluded={excluded}, pause=lambda _: None)
                baseline_cpu = time.thread_time() - cpu
                baseline_calls = calls['exact']
                calls['exact'] = 0
                with patch.object(disk, '_getattrlistbulk_api', return_value=batched, create=True):
                    cpu = time.thread_time()
                    measured = disk._measure_worktree(root, excluded={excluded}, pause=lambda _: None)
                    candidate_cpu = time.thread_time() - cpu
            self.assertEqual(measured, (expected, 'private on APFS'))
            self.assertLess(calls['bulk'] + calls['exact'], baseline_calls // 10)
            self.assertGreater(calls['bulk'], 0)
            self.assertEqual(calls['exact'], 1)
            with clone.open('r+b') as output:
                output.write(b'changed')
            changed = disk._measure_worktree(root, excluded={excluded}, pause=lambda _: None)
            exact_changed = disk._tree_bytes(root, lambda path, _info: disk._apfs_private_bytes(path),
                                             excluded={excluded}, pause=lambda _: None)
            self.assertEqual(changed, (exact_changed, 'private on APFS'))
            print(json.dumps({'baselineCalls': baseline_calls, **calls, 'bytes': expected,
                              'baselineCpuMs': round(baseline_cpu * 1000, 3),
                              'candidateCpuMs': round(candidate_cpu * 1000, 3)}))


if __name__ == '__main__':
    unittest.main()
