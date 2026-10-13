"""Host import contracts use the actual caller with a guest fixture."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
import hashlib
import io
import json
import os
from pathlib import Path
import socket
import tarfile
import subprocess
import sys
import tempfile
import threading
import unittest
from contextlib import nullcontext
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_linux_vm import Client, LinuxVMError
from server_test_waits import HANG_GUARD_SECONDS
import codex_linux_vm_share as share


class Fixture(Client):
    def __init__(self, root):
        super().__init__(root)
        self.calls = []
        self.exists = False
        self.fail_upload = False
        self.received = 0
        self.chunks = []
        self.seed_failures = 0

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
        if method == "upload.begin":
            return {"state": "receiving", "receivedBytes": self.received}
        if method == "upload.chunk":
            if params["uploadId"].startswith("seed-"):
                self.chunks.append((params["seq"], base64.b64decode(params["data"])))
            return {}
        if method == "project.seed":
            if self.seed_failures:
                self.seed_failures -= 1
                raise LinuxVMError("The layr admin channel is unavailable", uncertain=True, code="outcome_unknown")
            return {"projectId": params["projectId"], "stateId": "import-state", "path": params["path"], "seeded": True}
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

    def test_failed_import_keeps_its_reason_until_an_attempt_succeeds(self):
        self.client.fail_upload = True
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive):
            with self.assertRaises(LinuxVMError):
                self.client.ensure_layr_project("app", self.root, request_id="same")
            report = json.loads((self.root / "layr-projects/app/import-error.json").read_text())
            self.assertEqual((report["projectId"], report["error"]), ("app", "lost"))
            self.assertEqual(share.import_readiness(self.root, "app"), {"state": "failed", "error": "lost"})
            self.client.fail_upload = False
            self.client.ensure_layr_project("app", self.root, request_id="same")
        self.assertFalse((self.root / "layr-projects/app/import-error.json").exists())
        self.assertEqual(share.import_readiness(self.root, "app"), {"state": "ready"})

    def test_failed_check_of_an_existing_project_is_recorded(self):
        def stopped(method, params=None, **kwargs):
            raise LinuxVMError("The VM is not running.")
        with patch.object(self.client, "call", side_effect=stopped):
            with self.assertRaises(LinuxVMError):
                self.client.ensure_layr_project("app", self.root)
        self.assertEqual(share.import_readiness(self.root, "app"),
                         {"state": "failed", "error": "The VM is not running."})

    def test_started_runs_once_the_attempt_holds_the_project_lock(self):
        seen = []
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive):
            self.client.ensure_layr_project(
                "app", self.root, request_id="first",
                started=lambda: seen.append(share.import_readiness(self.root, "app")))
        self.assertEqual(seen, [{"state": "importing"}])
        self.assertEqual(share.import_readiness(self.root, "app"), {"state": "ready"})


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


class ImportReadinessTests(unittest.TestCase):
    """The sidebar state of a layr project, from the host records of import_project only."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.directory = self.root / "layr-projects/app"

    def state(self):
        return share.import_readiness(self.root, "app")

    def hold_lock(self):
        """Hold import.lock from a second open file, as an import in another thread does."""
        entered, release = threading.Event(), threading.Event()

        def hold():
            with share._project_lock(self.directory):
                entered.set()
                release.wait(10)

        thread = threading.Thread(target=hold)
        thread.start()
        self.assertTrue(entered.wait(HANG_GUARD_SECONDS))
        self.addCleanup(thread.join, 10)
        self.addCleanup(release.set)

    def test_no_record_is_not_imported_and_reading_creates_nothing(self):
        self.assertEqual(self.state(), {"state": "not_imported"})
        self.assertFalse((self.root / "layr-projects").exists())
        self.directory.mkdir(parents=True)
        (self.directory / "import.lock").touch()
        self.assertEqual(self.state(), {"state": "not_imported"})

    def test_held_lock_without_a_project_record_is_importing(self):
        self.hold_lock()
        self.assertEqual(self.state(), {"state": "importing"})

    def test_project_record_is_ready_even_while_a_chat_start_holds_the_lock(self):
        self.directory.mkdir(parents=True)
        (self.directory / "project.json").write_text(json.dumps({"projectId": "app", "source": "/mac"}))
        self.assertEqual(self.state(), {"state": "ready"})
        self.hold_lock()
        self.assertEqual(self.state(), {"state": "ready"})

    def test_failure_record_gives_the_reason(self):
        self.directory.mkdir(parents=True)
        (self.directory / "import-error.json").write_text(json.dumps({"projectId": "app", "error": "disk full"}))
        self.assertEqual(self.state(), {"state": "failed", "error": "disk full"})

    def test_receipt_without_result_after_its_attempt_is_failed(self):
        self.directory.mkdir(parents=True)
        receipt = self.directory / ("import-" + "a" * 64 + ".json")
        receipt.write_text(json.dumps({"uploadId": "a" * 64}))
        self.assertEqual(self.state()["state"], "failed")
        self.assertIn("stopped before it finished", self.state()["error"])
        receipt.write_text(json.dumps({"uploadId": "a" * 64, "result": {"projectId": "app"}}))
        self.assertEqual(self.state(), {"state": "not_imported"})

    def test_unreadable_record_is_a_visible_failure(self):
        self.directory.mkdir(parents=True)
        (self.directory / "import-error.json").write_text("{")
        self.assertEqual(self.state()["state"], "failed")
        self.assertIn("unreadable", self.state()["error"])


class SeedTests(unittest.TestCase):
    """A folder marked `regenerable seed` goes to the VM main once, as a warm layer."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.client = Fixture(self.root / "state")
        self.source = self.root / "attar"
        (self.source / "chromium/src/.git").mkdir(parents=True)
        (self.source / "chromium/src/.git/HEAD").write_text("ref: refs/heads/attar/fork\n")
        (self.source / "chromium/src/BUILD.gn").write_bytes(b"group" * 700)
        (self.source / "chromium/src/link").symlink_to("BUILD.gn")
        os.link(self.source / "chromium/src/BUILD.gn", self.source / "chromium/src/same.gn")
        (self.source / ".layrattributes").write_text("node_modules regenerable\nchromium regenerable seed\n")
        self.mount = patch.object(share, "mount_project", return_value={"state": "mounted", "readOnly": True})
        self.mount.start()
        self.addCleanup(self.mount.stop)

    def archive(self, source, path):
        path.write_bytes(b"code only")
        return {"totalBytes": 9, "sha256": hashlib.sha256(b"code only").hexdigest(),
                "mode": "full", "deletePaths": [], "repositories": []}

    def tar(self):
        data = io.BytesIO()
        share._layer_tar(self.source / "chromium", data)
        return data.getvalue()

    def test_only_plain_paths_marked_regenerable_and_seed_are_seeded(self):
        for name in ("a", "b", "c", "d", "e/f"):
            (self.source / name).mkdir(parents=True)
        (self.source / "link").symlink_to(self.source / "chromium")
        (self.source / ".layrattributes").write_text(
            "# comment\nchromium regenerable seed\na seed\nb regenerable seed\nb regenerable -seed\n"
            "c* regenerable seed\n/d/ regenerable seed\ne/f regenerable seed\nlink regenerable seed\n"
            "missing regenerable seed\n../x regenerable seed\n")
        self.assertEqual(share.seed_paths(self.source), ["chromium", "d", "e/f"])
        self.assertEqual(share.seed_paths(self.root / "none"), [])

    def test_the_layer_tar_is_the_same_each_time_and_the_guest_accepts_it(self):
        os.chown(self.source / "chromium/src/BUILD.gn", os.getuid(), os.getgid())
        server = socket.socket(socket.AF_UNIX)
        self.addCleanup(server.close)
        server.bind(str(self.source / "chromium/src/s"))
        first = self.tar()
        self.assertEqual(self.tar(), first)
        with tarfile.open(fileobj=io.BytesIO(first)) as archive:
            members = {member.name: member for member in archive.getmembers()}
        self.assertNotIn("src/s", members)
        self.assertTrue(members["src/same.gn"].isfile())
        self.assertEqual({(m.uid, m.gid, m.uname, m.gname) for m in members.values()}, {(0, 0, "", "")})
        (self.root / "archive.tar").write_bytes(first)
        # The guest's own extraction worker, in its own process as in the VM.
        guest = Path(__file__).resolve().parents[2] / "vm-guest/upload.py"
        subprocess.run([sys.executable, str(guest), str(self.root / "archive.tar"), str(self.root / "expanded"),
                        hashlib.sha256(first).hexdigest()], check=True, capture_output=True,
                       env={**os.environ, "CODEX_WORKSPACE_MIN_FREE_BYTES": "0"})
        self.assertEqual((self.root / "expanded/src/same.gn").read_bytes(), b"group" * 700)
        self.assertEqual(os.readlink(self.root / "expanded/src/link"), "BUILD.gn")

    def test_a_new_import_sends_the_layer_once_in_order(self):
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive), \
                patch.object(share, "CHUNK", 1024):
            self.client.ensure_layr_project("app", self.source, request_id="first")
            self.client.ensure_layr_project("app", self.source, request_id="first")
        expected = self.tar()
        self.assertEqual(b"".join(data for _, data in self.client.chunks), expected)
        self.assertEqual([seq for seq, _ in self.client.chunks], list(range(len(self.client.chunks))))
        begin = next(row[1] for row in self.client.calls if row[0] == "upload.begin" and row[1]["uploadId"].startswith("seed-"))
        self.assertEqual((begin["totalBytes"], begin["sha256"]), (len(expected), hashlib.sha256(expected).hexdigest()))
        seeds = [row for row in self.client.calls if row[0] == "project.seed"]
        self.assertEqual(len(seeds), 1)
        self.assertEqual(seeds[0][1], {"projectId": "app", "path": "chromium", "uploadId": begin["uploadId"]})
        self.assertNotIn("upload.commit", [row[0] for row in self.client.calls if "seed" in str(row[1])])
        record = json.loads((self.root / "state/layr-projects/app/seeds.json").read_text())
        self.assertEqual(record["chromium"]["seeded"], True)

    def test_an_interrupted_upload_continues_after_the_received_bytes(self):
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive), \
                patch.object(share, "CHUNK", 1024):
            self.client.received = 2048
            self.client.ensure_layr_project("app", self.source, request_id="first")
        expected = self.tar()
        self.assertEqual(self.client.chunks[0][0], 2)
        self.assertEqual(b"".join(data for _, data in self.client.chunks), expected[2048:])

    def test_an_existing_vm_project_without_a_plan_reads_no_layer(self):
        self.client.exists = True
        with patch.object(share, "_layer_tar") as tar:
            self.client.ensure_layr_project("app", self.source)
        tar.assert_not_called()
        self.assertNotIn("project.seed", [row[0] for row in self.client.calls])

    def test_a_failed_seed_repeats_alone_on_the_next_ensure(self):
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive), \
                patch.object(share, "seed_layers", side_effect=LinuxVMError("lost", uncertain=True)):
            with self.assertRaises(LinuxVMError):
                self.client.ensure_layr_project("app", self.source, request_id="first")
        self.assertEqual(json.loads((self.root / "state/layr-projects/app/seeds.json").read_text()), {"chromium": None})
        self.client.calls.clear()
        self.client.ensure_layr_project("app", self.source, request_id="first")
        self.assertNotIn("project.import", [row[0] for row in self.client.calls])
        self.assertEqual([row[0] for row in self.client.calls].count("project.seed"), 1)


class SeedRetryTests(SeedTests):
    def test_a_seed_that_failed_uncertainly_repeats_with_new_request_ids(self):
        # The guest answers a request ID with its saved answer, so the same ID would fail forever.
        self.client.seed_failures = 1
        with patch("codex_linux_workspace_sync.archive_source", side_effect=self.archive), \
                patch.object(share, "CHUNK", 1024):
            with self.assertRaises(LinuxVMError):
                self.client.ensure_layr_project("app", self.source, request_id="first")
            record = self.root / "state/layr-projects/app/seeds.json"
            self.assertEqual(json.loads(record.read_text())["chromium"], {"pending": True, "attempts": 1})
            self.client.ensure_layr_project("app", self.source, request_id="first")
        seeds = [row[2] for row in self.client.calls if row[0] == "project.seed"]
        self.assertEqual(len(seeds), 2)
        self.assertNotEqual(seeds[0], seeds[1])
        self.assertTrue(json.loads(record.read_text())["chromium"]["seeded"])


if __name__ == "__main__":
    unittest.main()
