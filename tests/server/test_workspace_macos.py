"""Bounded unmount checks for the macOS image backend."""

from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
import codex_workspace_macos as macos  # noqa: E402


class MacBackendUnmountTests(unittest.TestCase):
    def test_eject_succeeds_without_scanning_holders(self):
        backend = macos.Backend()
        eject = subprocess.CompletedProcess([], 0, b"", b"")
        with patch.object(macos, "_mounted", side_effect=[True, False]), \
                patch.object(macos, "_device_for_mount", return_value="/dev/disk1"), \
                patch.object(macos, "_run", return_value=eject) as run:
            result = backend.unmount_workspace(Path("/workspace/mount"), force=True)
        self.assertEqual(result, {"method": "eject", "terminatedPids": 0})
        self.assertEqual(run.call_args.kwargs["timeout"], 4)
        self.assertNotIn("lsof", run.call_args.args[0])

    def test_busy_eject_bounds_lsof_then_force_unmounts(self):
        backend = macos.Backend()
        busy = subprocess.CompletedProcess([], 1, b"", b"busy")
        found = subprocess.CompletedProcess([], 0, b"", b"")
        outputs = [busy, found, busy, found, found]
        with patch.object(macos, "_mounted", side_effect=[True, False]), \
                patch.object(macos, "_device_for_mount", return_value="/dev/disk1"), \
                patch.object(macos, "_run", side_effect=outputs) as run:
            result = backend.unmount_workspace(Path("/workspace/mount"), force=True)
        self.assertEqual(result["method"], "force-unmount-after-terminating-holders")
        self.assertEqual(run.call_args_list[1].kwargs["timeout"], 2)
        self.assertEqual(run.call_args_list[-2].args[0][1:4], ["unmount", "force", "/workspace/mount"])
        self.assertEqual(run.call_args_list[-2].kwargs["timeout"], 4)

    def test_lsof_timeout_still_reaches_force_unmount(self):
        backend = macos.Backend()
        busy = subprocess.CompletedProcess([], 1, b"", b"busy")
        detached = subprocess.CompletedProcess([], 0, b"", b"")
        outputs = [subprocess.TimeoutExpired("diskutil", 4),
                   subprocess.TimeoutExpired("lsof", 2), busy, detached, detached]
        with patch.object(macos, "_mounted", side_effect=[True, False]), \
                patch.object(macos, "_device_for_mount", return_value="/dev/disk1"), \
                patch.object(macos, "_run", side_effect=outputs):
            result = backend.unmount_workspace(Path("/workspace/mount"), force=True)
        self.assertTrue(result["lsofTimedOut"])
        self.assertTrue(result["ejectTimedOut"])
        self.assertEqual(result["terminatedPids"], 0)


if __name__ == "__main__":
    unittest.main()
