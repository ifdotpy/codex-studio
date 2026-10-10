"""Host import contracts use the actual caller with a guest fixture."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from contextlib import nullcontext
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
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
        if method == "share.name":
            return {"projectId": params["projectId"], "name": params["name"]}
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
        saved = json.loads((self.root / "layr-projects/app/project.json").read_text())
        self.assertEqual(saved["source"], str(Path("/missing-mac-folder").resolve()))
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

    def test_failed_finder_mount_keeps_the_import_and_stays_visible(self):
        # macOS blocks a background Python without Local Network permission ("No route to host").
        self.mount.stop()
        self.addCleanup(self.mount.start)
        refused = LinuxVMError("The authenticated SMB mount failed: server connection failed")
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive), \
                patch.object(share, "mount_project", side_effect=refused):
            result = self.client.ensure_layr_project("app", self.root, request_id="first")
        self.assertEqual(result["stateId"], "import-state")
        self.assertEqual(result["mount"]["state"], "failed")
        self.assertIn("server connection failed", result["mount"]["error"])
        self.assertTrue((self.root / "layr-projects/app/project.json").exists())
        status = share.mount_status(self.root)
        self.assertEqual(status["state"], "failed")
        self.assertEqual(status["projects"][0]["projectId"], "app")
        with patch.object(share, "mount_project", return_value={"state": "mounted", "readOnly": True}):
            self.client.ensure_layr_project("app", self.root)
        self.assertEqual(share.mount_status(self.root), {"state": "unmounted", "projects": []})

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

    def test_mac_mount_types_the_password_on_a_controlling_terminal(self):
        # mount_smbfs reads the password only from /dev/tty, never from a pipe.
        with tempfile.TemporaryDirectory() as name:
            fake = Path(name) / "mount_smbfs"
            seen = Path(name) / "seen"
            fake.write_text("#!/bin/bash\nIFS= read -r -s -p 'Password for host: ' value </dev/tty || exit 3\n"
                            f"printf '%s|%s' \"$value\" \"$*\" > {seen}\n")
            fake.chmod(0o700)
            result = share._mount_smbfs("//studio-view@192.168.64.2/project", Path(name) / "mnt",
                                        "a" * 64, executable=str(fake))
            self.assertEqual(result.returncode, 0, result.stderr)
            value, arguments = seen.read_text().split("|", 1)
            self.assertEqual(value, "a" * 64)
            self.assertNotIn("a" * 64, arguments)
            self.assertNotIn("a" * 64, result.stdout + result.stderr)
            failing = Path(name) / "failing"
            failing.write_text("#!/bin/bash\nIFS= read -r -s -p 'Password: ' value </dev/tty\n"
                               "echo \"mount_smbfs: server rejected $value\" >&2; exit 77\n")
            failing.chmod(0o700)
            project = {"projectId": "p1", "share": {"state": "ready"}}
            original = share._mount_smbfs
            self.mount.stop()
            self.addCleanup(self.mount.start)
            with patch.object(Path, "home", return_value=Path(name)), patch.object(share, "_mount_entries", return_value=[]), \
                    patch.object(self.client, "status", return_value=self.bridge(45000)), \
                    patch.object(share, "_mount_smbfs", lambda source, destination, password:
                                 original(source, destination, password, executable=str(failing))):
                with self.assertRaisesRegex(LinuxVMError, r"SMB mount failed: mount_smbfs: server rejected \*\*\*$"):
                    share.mount_project(self.client, project, "a" * 64, "p1")
            # A reused session mounts without a prompt; its exit code still decides.
            quiet = Path(name) / "quiet"
            quiet.write_text("#!/bin/bash\nexit 0\n")
            quiet.chmod(0o700)
            self.assertEqual(share._mount_smbfs("//x@127.0.0.1:1/p", Path(name) / "mnt", "a" * 64,
                                                executable=str(quiet)).returncode, 0)

    @staticmethod
    def bridge(port, state="listening"):
        return {"state": "running", "share": {"state": state, "port": port, "error": None}}

    def mount_case(self, entries, status, hint="app", previous=None):
        """mount_project with a fake mount table and helper status; returns (result, umounts, mounts)."""
        home = self.root / "home"
        home.mkdir(exist_ok=True)
        # import_project holds the project folder lock, which creates this folder.
        (self.root / "layr-projects/app").mkdir(parents=True, exist_ok=True)
        if previous:
            (self.root / "layr-projects/app/mount.json").write_text(json.dumps(previous))
        project = {"projectId": "app", "share": {"state": "ready"}}
        calls = {"umount": [], "mount": []}
        table = [dict({"path": str(home / "Studio/app")}, **row) for row in entries]

        def run(argv, **kwargs):
            calls["umount"].append(argv)
            table[:] = [row for row in table if row["path"] != argv[-1]]
            return subprocess.CompletedProcess(argv, 0, "", "")

        def mount_smbfs(source, destination, password):
            calls["mount"].append(source)
            table.append({"path": str(destination), "source": source, "options": "smbfs, nodev, nosuid, read-only"})
            return subprocess.CompletedProcess([], 0, "", "")

        self.mount.stop()
        try:
            with patch.object(Path, "home", return_value=home), \
                    patch.object(share, "_mount_entries", side_effect=lambda: list(table)), \
                    patch.object(self.client, "status", return_value=status), \
                    patch.object(share.subprocess, "run", side_effect=run), \
                    patch.object(share, "_mount_smbfs", side_effect=mount_smbfs):
                return share.mount_project(self.client, project, "password", hint), calls
        finally:
            self.mount.start()

    def test_mac_mount_goes_through_the_loopback_bridge(self):
        result, calls = self.mount_case([], self.bridge(45000))
        self.assertEqual(result["state"], "mounted")
        self.assertEqual(calls["mount"], ["//studio-view@127.0.0.1:45000/app"])
        saved = json.loads((self.root / "layr-projects/app/mount.json").read_text())
        self.assertEqual(saved["port"], 45000)
        # The same mount again is accepted without a new mount.
        entry = {"source": "//studio-view@127.0.0.1:45000/app", "options": "smbfs, read-only"}
        result, calls = self.mount_case([entry], self.bridge(45000))
        self.assertEqual((result["state"], calls["mount"], calls["umount"]), ("mounted", [], []))

    def test_mac_mount_moves_from_the_hash_named_folder_to_the_project_name(self):
        home = self.root / "home"
        old = home / "Studio" / ("e" * 64)
        old.mkdir(parents=True)
        stale = {"path": str(old), "source": "//studio-view@127.0.0.1:45000/app", "options": "smbfs, read-only"}
        result, calls = self.mount_case([stale], self.bridge(45000), hint="my-project",
                                        previous={"projectId": "app", "path": str(old), "port": 45000})
        self.assertEqual(result["path"], str(home / "Studio/my-project"))
        self.assertEqual(calls["umount"], [["/sbin/umount", str(old)]])
        self.assertFalse(old.exists())
        self.assertEqual(calls["mount"], ["//studio-view@127.0.0.1:45000/my-project"])
        saved = json.loads((self.root / "layr-projects/app/mount.json").read_text())
        self.assertEqual((saved["name"], saved["path"]), ("my-project", str(home / "Studio/my-project")))

    def test_share_hint_is_the_ascii_project_folder_name(self):
        self.assertEqual(share.share_hint("/Users/me/Projects/layr-live-check"), "layr-live-check")
        self.assertEqual(share.share_hint("/Users/me/My App (v2)"), "My-App-v2")
        self.assertEqual(share.share_hint("/Users/me/Проект"), "project")
        self.assertEqual(share.share_hint("/Users/me/global"), "project")
        self.assertEqual(len(share.share_hint("/x/" + "a" * 90)), 40)

    def test_mac_mount_needs_a_listening_bridge(self):
        with self.assertRaisesRegex(LinuxVMError, "share bridge is unavailable: port taken"):
            self.mount_case([], {"state": "running", "share": {"state": "unavailable", "port": None, "error": "port taken"}})

    def test_mac_mount_replaces_only_its_own_older_read_only_share(self):
        stale = {"source": "//studio-view@192.168.64.15/app", "options": "smbfs, read-only"}
        result, calls = self.mount_case([stale], self.bridge(45001))
        self.assertEqual(calls["umount"][0][0], "/sbin/umount")
        self.assertEqual(calls["mount"], ["//studio-view@127.0.0.1:45001/app"])
        for foreign in ({"source": "//studio-view@127.0.0.1:45000/other", "options": "smbfs, read-only"},
                        {"source": "//someone@127.0.0.1:45000/app", "options": "smbfs, read-only"},
                        {"source": "//studio-view@127.0.0.1:45000/app", "options": "smbfs, local"}):
            with self.subTest(foreign=foreign), self.assertRaisesRegex(LinuxVMError, "different filesystem"):
                self.mount_case([foreign], self.bridge(45000))


if __name__ == "__main__":
    unittest.main()
