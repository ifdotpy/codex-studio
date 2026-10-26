"""The low disk warning and the disk-full state: thresholds, status, and errors."""
import errno
import os
from pathlib import Path
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

import codex_disk_space as disk
from codex_native_errors import NativeRpcError, error_message
from codex_private_paths import write_atomic
from codex_shared_values import (DISK_FULL_ENTER_FREE_BYTES, DISK_FULL_EXIT_FREE_BYTES,
                                 DISK_FULL_RECHECK_SECONDS)


def usage(free: int) -> SimpleNamespace:
    return SimpleNamespace(total=free * 4, used=free * 3, free=free)


class DiskSpace(unittest.TestCase):
    def test_levels_follow_the_named_thresholds(self) -> None:
        self.assertIsNone(disk.level(disk.WARNING_FREE_BYTES))
        self.assertEqual(disk.level(disk.WARNING_FREE_BYTES - 1), 'warning')
        self.assertEqual(disk.level(disk.CRITICAL_FREE_BYTES), 'warning')
        self.assertEqual(disk.level(disk.CRITICAL_FREE_BYTES - 1), 'critical')
        self.assertEqual(disk.level(DISK_FULL_ENTER_FREE_BYTES), 'critical')
        self.assertEqual(disk.level(DISK_FULL_ENTER_FREE_BYTES - 1), 'full')
        self.assertEqual(disk.level(DISK_FULL_EXIT_FREE_BYTES, full=True), 'full')

    def test_status_reports_free_space_and_a_message(self) -> None:
        with patch('codex_disk_space.shutil.disk_usage', return_value=usage(3 * disk.GIB)):
            status = disk.status(Path('/state'))
        self.assertEqual((status['level'], status['freeBytes']), ('critical', 3 * disk.GIB))
        assert status['message'] is not None
        self.assertIn('3.0 GB free', status['message'])
        with patch('codex_disk_space.shutil.disk_usage', return_value=usage(100 * disk.GIB)):
            self.assertEqual(disk.status(Path('/state')),
                             {'level': None, 'freeBytes': 100 * disk.GIB, 'message': None})
        with patch('codex_disk_space.shutil.disk_usage', side_effect=OSError('gone')):
            self.assertEqual(disk.status(Path('/state')),
                             {'level': None, 'freeBytes': None, 'message': None})

    def test_first_tick_records_a_low_level_without_a_publish(self) -> None:
        runtime = SimpleNamespace(root='/state', _publish_desktop_resource=Mock())
        with patch('codex_disk_space.shutil.disk_usage', side_effect=lambda _path: usage(2 * disk.GIB)):
            disk.tick(runtime, now=1000)  # type: ignore[arg-type]
            disk.tick(runtime, now=1000 + disk.CHECK_SECONDS)  # type: ignore[arg-type]
        runtime._publish_desktop_resource.assert_not_called()

    def test_tick_publishes_only_a_changed_level_once_per_interval(self) -> None:
        runtime = SimpleNamespace(root='/state', _publish_desktop_resource=Mock())
        free = [100 * disk.GIB]
        with patch('codex_disk_space.shutil.disk_usage', side_effect=lambda _path: usage(free[0])):
            disk.tick(runtime, now=1000)  # type: ignore[arg-type]
            free[0] = 10 * disk.GIB
            disk.tick(runtime, now=1000 + disk.CHECK_SECONDS - 1)  # type: ignore[arg-type]
            self.assertEqual(runtime._publish_desktop_resource.call_count, 0)
            disk.tick(runtime, now=1000 + disk.CHECK_SECONDS)  # type: ignore[arg-type]
            self.assertEqual(runtime._publish_desktop_resource.call_count, 1)
            disk.tick(runtime, now=1000 + 2 * disk.CHECK_SECONDS)  # type: ignore[arg-type]
            self.assertEqual(runtime._publish_desktop_resource.call_count, 1)
            free[0] = 2 * disk.GIB
            disk.tick(runtime, now=1000 + 3 * disk.CHECK_SECONDS)  # type: ignore[arg-type]
            self.assertEqual(runtime._publish_desktop_resource.call_count, 2)
            free[0] = 100 * disk.MIB
            disk.tick(runtime, now=1000 + 4 * disk.CHECK_SECONDS)  # type: ignore[arg-type]
            self.assertEqual(runtime._publish_desktop_resource.call_count, 3,
                             'entering the disk-full state publishes the notice')

    def test_disk_full_errors_are_recognized_in_every_form(self) -> None:
        incident = ("ENOSPC: no space left on device, rename '/state/account-servers/claude-profile/"
                    "sessions/55f43293.meta.json.1.tmp' -> '/state/sessions/55f43293.meta.json'")
        rejection = NativeRpcError({'code': -32000, 'message': 'Disk is full. Claude did not receive this input.',
                                    'data': {'turnStartOutcome': 'not_applied', 'diskFull': True}})
        for error in (
            OSError(errno.ENOSPC, 'No space left on device'),
            {'message': incident},
            {'message': 'Codex failed', 'codexErrorInfo': 'diskFull'},
            {'message': 'Write failed', 'data': {'diskFull': True}},
            'sqlite3.OperationalError: database or disk is full',
            'No space left on device (os error 28)',
            sqlite3.OperationalError('database or disk is full'),
            rejection,
        ):
            with self.subTest(error=error):
                self.assertTrue(disk.is_disk_full_error(error))
        wrapped = RuntimeError('save failed')
        wrapped.__cause__ = OSError(errno.ENOSPC, 'full')
        self.assertTrue(disk.is_disk_full_error(wrapped))
        ordinary: tuple[object, ...] = ({'message': 'Model is at capacity', 'codexErrorInfo': 'serverOverloaded'},
                                        OSError(errno.EIO, 'I/O error'), 'permission denied', None, {})
        for value in ordinary:
            with self.subTest(error=value):
                self.assertFalse(disk.is_disk_full_error(value))
        self.assertEqual(error_message({'message': incident}), disk.ERROR_MESSAGE)

    def test_full_state_uses_hysteresis_and_observed_errors(self) -> None:
        runtime = SimpleNamespace(root='/state')
        free = [DISK_FULL_ENTER_FREE_BYTES - 1]
        with patch('codex_disk_space.shutil.disk_usage', side_effect=lambda _path: usage(free[0])):
            self.assertEqual(disk.full_free_bytes(runtime, now=100), DISK_FULL_ENTER_FREE_BYTES - 1)
            free[0] = DISK_FULL_EXIT_FREE_BYTES - 1
            self.assertEqual(disk.full_free_bytes(runtime, now=102), DISK_FULL_EXIT_FREE_BYTES - 1,
                             'the full state ends only at the exit threshold')
            free[0] = DISK_FULL_EXIT_FREE_BYTES
            self.assertIsNone(disk.full_free_bytes(runtime, now=104))
            free[0] = DISK_FULL_ENTER_FREE_BYTES
            self.assertIsNone(disk.full_free_bytes(runtime, now=106),
                              'no full state at or above the entry threshold')
            disk.observe_full(runtime, now=108)
            self.assertEqual(disk.full_free_bytes(runtime, now=108), DISK_FULL_ENTER_FREE_BYTES,
                             'an observed ENOSPC holds starts even when the sample shows free space')
            free[0] = 100 * disk.GIB
            self.assertIsNotNone(disk.full_free_bytes(runtime, now=108 + DISK_FULL_RECHECK_SECONDS - 1))
            self.assertIsNone(disk.full_free_bytes(runtime, now=108 + DISK_FULL_RECHECK_SECONDS))
        with patch('codex_disk_space.shutil.disk_usage', side_effect=OSError('gone')):
            self.assertIsNone(disk.full_free_bytes(SimpleNamespace(root='/state'), now=1),
                              'an unreadable volume does not block work')

    def test_full_status_names_the_free_space_and_the_resume_threshold(self) -> None:
        runtime = SimpleNamespace(root='/state')
        with patch('codex_disk_space.shutil.disk_usage', return_value=usage(130 * disk.MIB)):
            status = disk.status(Path('/state'), runtime, now=10)
        self.assertEqual((status['level'], status['freeBytes']), ('full', 130 * disk.MIB))
        self.assertEqual(status['message'], 'Disk is full: 130 MiB free. Studio starts no new turns and '
                         'resumes held agents when 2 GiB is free. Free disk space now.')
        self.assertEqual(disk.held_reason(130 * disk.MIB),
                         'Disk is full: 130 MiB free. Studio resumes this agent when free space returns.')

    def test_atomic_write_failure_keeps_the_previous_file_and_no_temporary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'state.json'
            write_atomic(target, '{"good": true}')
            with patch('codex_private_paths.os.replace', side_effect=OSError(errno.ENOSPC, 'No space left')):
                with self.assertRaises(OSError) as raised:
                    write_atomic(target, '{"good": false}')
            self.assertEqual(raised.exception.errno, errno.ENOSPC)
            self.assertEqual(target.read_text(), '{"good": true}')
            self.assertEqual(sorted(os.listdir(directory)), ['state.json'])

            real_fdopen = os.fdopen

            def partial(descriptor: int, *args: object, **kwargs: object) -> object:
                stream = real_fdopen(descriptor, *args, **kwargs)  # type: ignore[call-overload]

                class Full:
                    def __enter__(self) -> "Full":
                        return self

                    def __exit__(self, *_exc: object) -> None:
                        stream.close()

                    def write(self, data: bytes) -> None:
                        stream.write(data[:4])
                        raise OSError(errno.ENOSPC, 'No space left on device')

                return Full()

            with patch('codex_private_paths.os.fdopen', side_effect=partial):
                with self.assertRaises(OSError):
                    write_atomic(target, '{"good": false}', durable=True)
            self.assertEqual(target.read_text(), '{"good": true}')
            self.assertEqual(sorted(os.listdir(directory)), ['state.json'])
            self.assertEqual(os.stat(target).st_mode & 0o777, 0o600)


if __name__ == '__main__':
    unittest.main()
