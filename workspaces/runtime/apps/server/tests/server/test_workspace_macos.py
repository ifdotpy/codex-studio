"""Bounded unmount checks for the macOS image backend."""

from __future__ import annotations

import os
import plistlib
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from codex_layout import SERVER_SOURCE_ROOT

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_workspace_images as images  # noqa: E402
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
        self.assertEqual(run.call_args.kwargs["timeout"], 20)
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
        outputs = [subprocess.TimeoutExpired("diskutil", 20),
                   subprocess.TimeoutExpired("lsof", 2), busy, detached, detached]
        with patch.object(macos, "_mounted", side_effect=[True, False]), \
                patch.object(macos, "_device_for_mount", return_value="/dev/disk1"), \
                patch.object(macos, "_run", side_effect=outputs):
            result = backend.unmount_workspace(Path("/workspace/mount"), force=True)
        self.assertTrue(result["lsofTimedOut"])
        self.assertTrue(result["ejectTimedOut"])
        self.assertEqual(result["signaledPids"], 0)


@unittest.skipUnless(sys.platform == "darwin"
                     and os.environ.get("CODEX_WORKSPACE_MACOS_INTEGRATION") == "1",
                     "set CODEX_WORKSPACE_MACOS_INTEGRATION=1 on macOS")
class MacBackendUnmountIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-unmount-integration-")
        self.root = Path(self.temp.name)
        self.store = self.root / "store"
        self.old_store = os.environ.get("CODEX_WORKSPACE_STORE")
        os.environ["CODEX_WORKSPACE_STORE"] = str(self.store)
        self.agent_id = "bounded-unmount-integration"
        self.agent_dir = images._agent_dir(self.agent_id)
        self.image = self.agent_dir / "workspace.asif"
        self.mount = images._mount_path(self.agent_id)
        self.holders: list[subprocess.Popen] = []
        self.backend = macos.Backend()
        self.created = False

    def tearDown(self):
        for process in self.holders:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=2)
        if images._agent_state_path(self.agent_id).exists():
            try:
                images.remove_workspace(self.agent_id)
            except (OSError, RuntimeError, ValueError):
                pass
        elif macos._mounted(self.mount):
            try:
                self.backend.unmount_workspace(self.mount, force=True)
            except (OSError, RuntimeError):
                pass
        if self.created and self.image.exists():
            try:
                self.backend.remove_base_version(self.image)
            except (OSError, RuntimeError):
                pass
        if self.old_store is None:
            os.environ.pop("CODEX_WORKSPACE_STORE", None)
        else:
            os.environ["CODEX_WORKSPACE_STORE"] = self.old_store
        self.temp.cleanup()

    def start_holder(self):
        held_file = (self.mount / "held-open-file").open("rb")
        process = subprocess.Popen(["/bin/sleep", "600"], stdin=held_file,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        held_file.close()
        self.holders.append(process)
        return process

    def assert_bounded(self, action):
        started = time.monotonic()
        result = action()
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, 35, f"workspace operation took {elapsed:.2f}s")
        return result, elapsed

    def test_tiny_image_archive_and_remove_with_open_file_holder(self):
        self.agent_dir.mkdir(parents=True)
        create = subprocess.run(
            ["diskutil", "image", "create", "blank", "--format", "ASIF", "--size", "50m",
             "--volumeName", "StudioTest", "--fs", "APFS", str(self.image)],
            check=True, capture_output=True, timeout=30)
        del create
        self.created = True
        self.backend.mount_workspace(self.image, self.mount)
        (self.mount / "held-open-file").write_bytes(b"tiny workspace fixture")
        images._write_json(images._agent_state_path(self.agent_id), {
            "schema": 2, "agentId": self.agent_id, "repoKey": "integration",
            "image": str(self.image), "baseImage": str(self.image), "mount": str(self.mount),
            "state": "ready", "mounted": True,
        })

        idle, idle_seconds = self.assert_bounded(
            lambda: images.archive_workspace(self.agent_id))
        self.assertEqual(idle["unmount"]["method"], "eject")
        self.assertFalse(macos._mounted(self.mount))

        images.ensure_mounted(self.agent_id)
        archive_holder = self.start_holder()
        archived, archive_seconds = self.assert_bounded(
            lambda: images.archive_workspace(self.agent_id))
        self.assertGreater(archived["unmount"].get("holdersFound", 0), 0)
        self.assertIn(archived["unmount"]["method"], {
            "eject-after-terminating-holders", "force-unmount-after-terminating-holders"})
        self.assertIsNotNone(archive_holder.poll(), "archive left the file holder running")
        self.assertGreater(archived["unmount"].get("signaledPids", 0), 0)

        images.ensure_mounted(self.agent_id)
        remove_holder = self.start_holder()
        removed, remove_seconds = self.assert_bounded(
            lambda: images.remove_workspace(self.agent_id))
        self.assertGreater(removed["unmount"].get("holdersFound", 0), 0)
        self.assertEqual(removed["state"], "removed")
        self.assertIsNotNone(remove_holder.poll(), "remove left the file holder running")
        self.assertGreater(removed["unmount"].get("signaledPids", 0), 0)
        self.assertFalse(self.image.exists())
        info = plistlib.loads(subprocess.run(
            ["hdiutil", "info", "-plist"], capture_output=True, check=True, timeout=5).stdout)
        self.assertFalse(any(Path(image.get("image-path", "")).resolve() == self.image.resolve()
                             for image in info.get("images", [])))
        print(f"idle archive={idle_seconds:.3f}s, busy archive={archive_seconds:.3f}s, "
              f"busy remove={remove_seconds:.3f}s; "
              f"archive detach={archived['unmount']}; remove detach={removed['unmount']}")


if __name__ == "__main__":
    unittest.main()
