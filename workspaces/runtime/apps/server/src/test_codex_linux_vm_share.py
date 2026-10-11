"""The remount of registered project shares after a VM boot (docs/vm-layr.md)."""
from __future__ import annotations

import json
from pathlib import Path
import tempfile
from typing import Any
import unittest
from unittest.mock import patch

import codex_linux_vm_share as share
from codex_linux_vm import LinuxVMError


class Guest:
    """Fails project.ensure with each error of `failures` first, then answers."""

    def __init__(self, state_dir: Path, failures: list[LinuxVMError]):
        self.state_dir, self.failures = state_dir, failures
        self.calls: list[str] = []

    def call(self, method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
        self.calls.append(method)
        if method == 'project.ensure' and self.failures:
            raise self.failures.pop(0)
        return {'projectId': params.get('projectId', ''), 'state': 'ready'}


class Remount(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.state = Path(self.temp.name)
        folder = self.state / 'layr-projects' / 'p'
        folder.mkdir(parents=True)
        (folder / 'mount.json').write_text(json.dumps({'projectId': 'a' * 64, 'path': '/Volumes/p'}))
        self.mounted: list[Any] = []
        self.patches = [patch.object(share, '_credential', lambda client: 'pw'),
                        patch.object(share, '_mount_or_report', lambda *args: self.mounted.append(args)),
                        patch.object(share.time, 'sleep', lambda seconds: None)]
        for item in self.patches:
            item.start()

    def tearDown(self) -> None:
        for item in self.patches:
            item.stop()
        self.temp.cleanup()

    def test_a_remount_waits_while_the_admin_channel_restarts(self) -> None:
        unavailable = LinuxVMError('Linux guest project.ensure: unavailable', code='unavailable')
        guest = Guest(self.state, [unavailable, unavailable])
        share.remount_registered(guest)  # type: ignore[arg-type]
        self.assertEqual(guest.calls.count('project.ensure'), 3)
        self.assertEqual(len(self.mounted), 1)

    def test_an_unknown_outcome_is_not_repeated(self) -> None:
        unknown = LinuxVMError('Linux guest project.ensure: outcome_unknown', code='outcome_unknown', uncertain=True)
        guest = Guest(self.state, [unknown])
        with self.assertRaises(LinuxVMError):
            share.remount_registered(guest)  # type: ignore[arg-type]
        self.assertEqual(guest.calls.count('project.ensure'), 1)
        self.assertFalse(self.mounted)

    def test_the_wait_ends(self) -> None:
        unavailable = LinuxVMError('Linux guest project.ensure: unavailable', code='unavailable')
        guest = Guest(self.state, [unavailable] * 1000)
        clock = iter(range(0, 10_000, 10))
        with patch.object(share.time, 'monotonic', lambda: float(next(clock))):
            with self.assertRaises(LinuxVMError):
                share.remount_registered(guest)  # type: ignore[arg-type]
        self.assertLess(guest.calls.count('project.ensure'), 20)


if __name__ == '__main__':
    unittest.main()
