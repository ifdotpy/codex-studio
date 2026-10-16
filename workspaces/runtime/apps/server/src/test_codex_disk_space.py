"""The low disk warning: thresholds, the desktop status, and its publication."""
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

import codex_disk_space as disk


def usage(free: int) -> SimpleNamespace:
    return SimpleNamespace(total=free * 4, used=free * 3, free=free)


class DiskSpace(unittest.TestCase):
    def test_levels_follow_the_named_thresholds(self) -> None:
        self.assertIsNone(disk.level(disk.WARNING_FREE_BYTES))
        self.assertEqual(disk.level(disk.WARNING_FREE_BYTES - 1), 'warning')
        self.assertEqual(disk.level(disk.CRITICAL_FREE_BYTES), 'warning')
        self.assertEqual(disk.level(disk.CRITICAL_FREE_BYTES - 1), 'critical')

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


if __name__ == '__main__':
    unittest.main()
