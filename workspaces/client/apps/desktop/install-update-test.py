"""install_update.py on a temporary Applications folder with fake codesign, osascript and backend."""
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
REPOSITORY_ROOT = HERE.parents[3]
sys.path.insert(0, str(REPOSITORY_ROOT / "workspaces/runtime/apps/server/src"))
spec = importlib.util.spec_from_file_location("install_update", HERE / "install_update.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
import codex_peer_updates as updates  # noqa: E402

OLD = {"version": 1, "builtAt": 1791900000.5, "commit": "a" * 40, "commitTime": 1791899000, "branch": "main"}
NEW = {"version": 1, "builtAt": 1791980000.25, "commit": "b" * 40, "commitTime": 1791979000, "branch": "main"}


def make_app(app, build, certificate="Studio Local Signing"):
    (app / "Contents/Resources").mkdir(parents=True)
    (app / updates.BUILD_STAMP).write_text(json.dumps(build))
    (app / "Contents/CERTIFICATE").write_text(certificate)
    return app


class FakeMac:
    """The commands install_update.py runs, on fixture folders only."""

    def __init__(self):
        self.window = True
        self.commands = []

    def run(self, argv, timeout):
        self.commands.append(argv)
        tool, args = Path(argv[0]).name, argv[1:]
        out, code = "", 0
        if tool == "ditto":
            shutil.copytree(args[0], args[1], symlinks=True)
        elif tool == "codesign" and args[0] == "--verify":
            code = 1 if (Path(args[-1]) / "Contents/BAD_SIGNATURE").exists() else 0
        elif tool == "codesign":
            certificate = (Path(args[-1]) / "Contents/CERTIFICATE").read_text()
            out = f'designated => identifier "local.codex.agents" and certificate leaf = H"{certificate}"\n'
        elif tool == "osascript" and args[1].endswith("is running"):
            out = "true\n" if self.window else "false\n"
        elif tool == "osascript" and args[1].endswith("to quit"):
            self.window = False
        elif tool == "open":
            self.window = True
        else:
            raise AssertionError(f"Unexpected command {argv}")
        return subprocess.CompletedProcess(argv, code, out, "")

    def ran(self, tool, last=None):
        return [argv for argv in self.commands
                if Path(argv[0]).name == tool and (last is None or argv[-1].endswith(last))]


class InstallUpdate(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="studio-install-update-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name).resolve()
        self.state = root / "state"
        self.applications = root / "Applications"
        self.state.mkdir()
        self.applications.mkdir()
        self.installed = make_app(self.applications / "Codex Studio.app", OLD)
        self.staged = make_app(self.state / "updates" / str(NEW["builtAt"]) / "Codex Studio.app", NEW)
        self.pending = {"id": "update-1", "app": str(self.staged), "build": NEW, "from": "server"}
        updates.write_json(self.state / updates.PENDING_FILE, self.pending)
        self.config = {"port": 4620, "resources": str(REPOSITORY_ROOT), "supervisorEnabled": True}
        self.mac = FakeMac()
        self.stops = []
        environment = patch.dict(os.environ, {"CODEX_LINUX_VM_STATE_DIR": str(self.state / "linux-vm")})
        environment.start()
        self.addCleanup(environment.stop)

    def stop(self, config, state):
        self.stops.append((config["port"], state))
        return "stopped"

    def install(self):
        return installer.install(self.state, self.config, applications=self.applications,
                                 run=self.mac.run, stop=self.stop, sleep=lambda seconds: None,
                                 clock=lambda: 1_800_000_000.0)

    def receipt(self):
        return json.loads((self.state / updates.RECEIPT_FILE).read_text())

    def test_swap_installs_the_build_and_restarts_backend_and_window(self):
        make_app(self.applications / "Codex Studio.previous.app", {**OLD, "builtAt": 1.0})
        code, _ = self.install()
        self.assertEqual(code, installer.INSTALLED)
        self.assertEqual(updates.read_build(self.installed), NEW)
        self.assertEqual(updates.read_build(self.applications / "Codex Studio.previous.app"), OLD)
        self.assertEqual(sorted(p.name for p in self.applications.iterdir()),
                         ["Codex Studio.app", "Codex Studio.previous.app"])
        self.assertEqual(self.stops, [(4620, self.state)])
        self.assertEqual(len(self.mac.ran("osascript", "to quit")), 1)
        self.assertEqual(self.mac.ran("open"), [["/usr/bin/open", "-g", str(self.installed)]])
        receipt = self.receipt()
        self.assertEqual((receipt["outcome"], receipt["id"], receipt["build"], receipt["from"]),
                         ("installed", "update-1", NEW, "server"))
        self.assertEqual(receipt["backend"], "stopped")
        self.assertIn("installedAt", receipt)
        self.assertFalse((self.state / updates.PENDING_FILE).exists())
        self.assertFalse(self.staged.parent.exists())

    def test_closed_window_stays_closed(self):
        self.mac.window = False
        self.assertEqual(self.install()[0], installer.INSTALLED)
        self.assertEqual(self.mac.ran("osascript", "to quit"), [])
        self.assertEqual(self.mac.ran("open"), [])

    def test_waits_without_supervisor_mode(self):
        self.config["supervisorEnabled"] = False
        self.assertEqual(self.install(), (installer.WAITING, "waiting for supervisor mode"))
        self.assertEqual(updates.read_build(self.installed), OLD)
        self.assertTrue((self.state / updates.PENDING_FILE).exists())
        self.assertEqual(self.mac.commands, [])
        self.assertEqual(self.stops, [])

    def test_waits_while_a_vm_upgrade_holds_the_maintenance_lock(self):
        lock = self.state / "linux-vm" / "maintenance.lock"
        lock.parent.mkdir()
        with lock.open("a") as lease:
            fcntl.flock(lease, fcntl.LOCK_EX)
            self.assertEqual(self.install(), (installer.WAITING, "waiting for the VM maintenance lock"))
        self.assertEqual(self.mac.commands, [])
        self.assertEqual(self.install()[0], installer.INSTALLED)

    def test_other_certificate_leaves_the_installed_app_unchanged(self):
        (self.staged / "Contents/CERTIFICATE").write_text("Another Signing")
        code, reason = self.install()
        self.assertEqual((code, reason), (installer.FAILED, "Another certificate signed the build"))
        self.assertEqual(updates.read_build(self.installed), OLD)
        self.assertEqual([p.name for p in self.applications.iterdir()], ["Codex Studio.app"])
        self.assertEqual(self.mac.ran("osascript", "to quit"), [])
        self.assertEqual(self.stops, [])
        self.assertEqual(self.receipt()["outcome"], "failed")
        self.assertFalse((self.state / updates.PENDING_FILE).exists())
        self.assertFalse(self.staged.parent.exists())

    def test_failed_swap_moves_the_previous_app_back(self):
        rename = Path.rename

        def failing(path, target):
            if path.name == "Codex Studio.next.app":
                raise OSError("Operation not permitted")
            return rename(path, target)

        with patch.object(Path, "rename", failing):
            code, reason = self.install()
        self.assertEqual(code, installer.FAILED)
        self.assertTrue(reason.startswith("The swap failed; the previous app is back"))
        self.assertEqual(updates.read_build(self.installed), OLD)
        self.assertEqual([p.name for p in self.applications.iterdir()], ["Codex Studio.app"])
        self.assertEqual(self.stops, [])
        # The window that the install quit opens again.
        self.assertEqual(len(self.mac.ran("open")), 1)
        self.assertTrue(self.mac.window)
        self.assertEqual(self.receipt()["outcome"], "failed")

    def test_copy_that_fails_verification_is_removed(self):
        run = self.mac.run

        def broken_copy(argv, timeout):
            result = run(argv, timeout)
            if Path(argv[0]).name == "ditto":
                (Path(argv[2]) / "Contents/BAD_SIGNATURE").write_text("")
            return result

        code, reason = installer.install(self.state, self.config, applications=self.applications,
                                         run=broken_copy, stop=self.stop, sleep=lambda seconds: None)
        self.assertEqual(code, installer.FAILED)
        self.assertTrue(reason.startswith("The signature check failed"))
        self.assertEqual([p.name for p in self.applications.iterdir()], ["Codex Studio.app"])
        self.assertEqual(updates.read_build(self.installed), OLD)
        self.assertEqual(len(self.mac.ran("open")), 1)

    def test_installed_build_that_is_newer_makes_the_update_obsolete(self):
        (self.installed / updates.BUILD_STAMP).write_text(json.dumps(NEW))
        self.assertEqual(self.install()[0], installer.FAILED)
        self.assertEqual(self.receipt()["outcome"], "obsolete")
        self.assertEqual(self.mac.commands, [])
        self.assertFalse((self.state / updates.PENDING_FILE).exists())

    def test_backend_that_cannot_stop_keeps_the_new_app(self):
        def refuse(config, state):
            raise RuntimeError("The reported backend PID is not the packaged Codex Canvas process.")

        code, _ = installer.install(self.state, self.config, applications=self.applications,
                                    run=self.mac.run, stop=refuse, sleep=lambda seconds: None)
        self.assertEqual(code, installer.INSTALLED)
        self.assertEqual(updates.read_build(self.installed), NEW)
        self.assertTrue(self.receipt()["backend"].startswith("not restarted"))


class RecoveryHook(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="studio-install-hook-")
        self.addCleanup(temporary.cleanup)
        self.state = Path(temporary.name).resolve()
        self.filename = self.state / "background-recovery.json"
        self.config = {"python": sys.executable, "codex": "/usr/bin/true", "resources": str(REPOSITORY_ROOT),
                       "port": 4620, "supervisorEnabled": True}
        self.calls = []

    def run_installer(self, code):
        def run(argv, **options):
            self.calls.append((argv, options))
            return subprocess.CompletedProcess(argv, code)
        return installer.recovery.update_tick(self.filename, self.config, self.state, run=run)

    def test_no_pending_update_runs_nothing(self):
        self.assertIsNone(self.run_installer(0))
        self.assertEqual(self.calls, [])

    def test_pending_update_runs_the_installer_of_this_app(self):
        (self.state / updates.PENDING_FILE).write_text("{}")
        self.assertEqual(self.run_installer(0), "update installed")
        argv, options = self.calls[0]
        self.assertEqual(argv, [sys.executable, "-B", str(HERE / "install_update.py"), "--config", str(self.filename)])
        self.assertEqual(options["env"]["CODEX_AGENTS_SUPERVISOR_MODE"], "1")
        self.assertEqual(self.run_installer(75), "update waiting")
        self.assertEqual(self.run_installer(1), "update failed")


if __name__ == "__main__":
    unittest.main()
