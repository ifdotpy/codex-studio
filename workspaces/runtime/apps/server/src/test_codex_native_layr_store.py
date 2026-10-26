"""Native store retries must never replace an image or adopt another volume."""
import json
from pathlib import Path
import plistlib
import tempfile
import unittest
from typing import Any, Iterator
from unittest.mock import patch

import codex_native_layr_store as store


class NativeStoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()

    def test_unknown_create_is_not_repeated(self) -> None:
        with patch.object(store, "run", side_effect=TimeoutError("lost response")) as run:
            with self.assertRaises(TimeoutError):
                store.ensure_macos(self.root)
            with self.assertRaisesRegex(RuntimeError, "unknown outcome"):
                store.ensure_macos(self.root)
        self.assertEqual(run.call_count, 1)

    def test_unrecorded_image_is_not_replaced(self) -> None:
        (self.root / "history.sparsebundle").mkdir()
        with patch.object(store, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "without its creation receipt"):
                store.ensure_macos(self.root)
        run.assert_not_called()

    def test_other_image_at_mount_is_refused(self) -> None:
        mount = self.root / "volume"
        info = {"images": [{"image-path": str(self.root / "other.sparsebundle"),
                            "system-entities": [{"mount-point": str(mount)}]}]}
        with patch.object(store, "run", return_value=plistlib.dumps(info)):
            with self.assertRaisesRegex(RuntimeError, "another image"):
                store.mounted_image(self.root / "history.sparsebundle", mount)

    def test_ready_volume_identity_is_checked(self) -> None:
        image, mount = self.root / "history.sparsebundle", self.root / "volume"
        image.mkdir()
        (self.root / "store.json").write_text(json.dumps({
            "image": str(image), "mount": str(mount), "phase": "ready", "volume": "original"}))
        info = {"FilesystemType": "apfs", "FilesystemName": "Case-sensitive APFS",
                "GlobalPermissionsEnabled": True, "VolumeUUID": "replacement"}
        with patch.object(store, "mounted_image", return_value=True), \
                patch.object(store, "run", return_value=plistlib.dumps(info)):
            with self.assertRaisesRegex(RuntimeError, "volume identity changed"):
                store.ensure_macos(self.root)


if __name__ == "__main__":
    unittest.main()
