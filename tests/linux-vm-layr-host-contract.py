"""Host import contracts use the actual caller with a guest fixture."""
import json
from pathlib import Path
import sys
import tempfile
import unittest
from contextlib import nullcontext
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_linux_vm import Client, LinuxVMError
import codex_linux_vm_share as share


class Fixture(Client):
    def __init__(self, root):
        super().__init__(root)
        self.calls = []
        self.exists = False
        self.fail_upload = False

    def _lock(self, *args):
        return nullcontext()

    def call(self, method, params=None, **kwargs):
        self.calls.append((method, params, kwargs.get("request_id")))
        if method == "project.ensure":
            if not self.exists:
                raise LinuxVMError("missing", code="not_found")
            return {"projectId": params["projectId"], "stateId": "vm-state", "share": {}}
        if method == "project.import":
            self.exists = True
            return {"projectId": params["projectId"], "stateId": "import-state", "share": {}}
        if method == "upload.commit" and self.fail_upload:
            raise LinuxVMError("lost", uncertain=True)
        if method in {"share.configure", "share.status"}:
            return {"state": "ready"}
        return {}


class HostImportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.client = Fixture(self.root)
        self.mount = patch.object(share, "mount_project", return_value={"state": "mounted", "readOnly": True})
        self.mount.start()
        self.addCleanup(self.mount.stop)

    def archive(self, source, path):
        path.write_bytes(b"one source tree")
        import hashlib
        return {"totalBytes": path.stat().st_size, "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "mode": "full", "deletePaths": [], "repositories": []}

    def test_existing_vm_project_never_reads_mac_source(self):
        self.client.exists = True
        with patch("codex_linux_workspace_sync.archive_source") as archive:
            result = self.client.ensure_layr_project("app", "/missing-mac-folder")
        archive.assert_not_called()
        self.assertEqual(result["stateId"], "vm-state")
        self.assertNotIn("project.import", [row[0] for row in self.client.calls])

    def test_initial_import_has_stable_upload_and_operation_identities(self):
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive):
            result = self.client.ensure_layr_project("app", self.root, request_id="first")
            imported = [row for row in self.client.calls if row[0] == "project.import"]
            replay = self.client.ensure_layr_project("app", self.root, request_id="first")
        self.assertEqual(len(imported), 1)
        self.assertEqual(result["stateId"], "import-state")
        self.assertEqual(replay["stateId"], "vm-state")
        identities = [row[2] for row in self.client.calls if row[0].startswith("upload.")]
        self.assertTrue(all(identity.startswith(imported[0][2].split(":")[0]) for identity in identities))
        self.assertEqual(list((self.root / "layr-projects/app").glob("*.tar.gz")), [])

    def test_unknown_upload_retains_archive_and_uses_same_identity_on_retry(self):
        self.client.fail_upload = True
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive) as archive:
            for _ in range(2):
                with self.assertRaises(LinuxVMError) as error:
                    self.client.ensure_layr_project("app", self.root, request_id="same")
                self.assertTrue(error.exception.uncertain)
        archive.assert_called_once()
        uploads = [row for row in self.client.calls if row[0] == "upload.commit"]
        self.assertEqual(uploads[0], uploads[1])
        self.assertEqual(len(list((self.root / "layr-projects/app").glob("*.tar.gz"))), 1)
        self.assertNotIn("project.import", [row[0] for row in self.client.calls])

    def test_changed_source_with_same_import_identity_is_rejected(self):
        self.client.fail_upload = True
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive):
            with self.assertRaises(LinuxVMError):
                self.client.ensure_layr_project("app", self.root, request_id="same")
            with self.assertRaises(LinuxVMError) as error:
                self.client.ensure_layr_project("app", self.root / "other", request_id="same")
        self.assertEqual(error.exception.code, "id_conflict")

    def test_explicit_import_needs_expected_state_and_identity(self):
        with self.assertRaises(ValueError):
            self.client.ensure_layr_project("app", self.root, incremental=True)

    def test_mac_mount_rejects_existing_writable_or_different_share(self):
        home = self.root / "home"
        home.mkdir()
        project = {"projectId": "app", "share": {"state": "ready", "address": "192.168.64.2"}}
        self.mount.stop()
        entries = [{"path": str(home / "Studio/app"), "source": "//studio-view@192.168.64.2/app", "options": "smbfs, local"}]
        with patch.object(Path, "home", return_value=home), patch.object(share, "_mount_entries", return_value=entries):
            with self.assertRaises(LinuxVMError):
                share.mount_project(self.client, project, "password")
        self.mount.start()


if __name__ == "__main__":
    unittest.main()
