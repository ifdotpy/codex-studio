"""install_update.py on a temporary Applications folder with fake codesign, osascript and backend."""
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
REPOSITORY_ROOT = HERE.parents[3]
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location("install_update", HERE / "install_update.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
import desktop_updates as updates  # noqa: E402

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
        # AppKit refuses the Apple Event quit, as on 2026-10-19 with build 75e8c1568.
        self.quit_refused = False
        self.signals = []

    def kill(self, pid, number):
        self.signals.append((pid, number))
        if number == signal.SIGTERM:
            self.window = False

    def run(self, argv, timeout):
        self.commands.append(argv)
        tool, args = Path(argv[0]).name, argv[1:]
        out, err, code = "", "", 0
        if tool == "ditto":
            shutil.copytree(args[0], args[1], symlinks=True)
        elif tool == "codesign" and args[0] == "--verify":
            code = 1 if (Path(args[-1]) / "Contents/BAD_SIGNATURE").exists() else 0
        elif tool == "codesign":
            certificate = (Path(args[-1]) / "Contents/CERTIFICATE").read_text()
            out = f'designated => identifier "local.codex.agents" and certificate leaf = H"{certificate}"\n'
        elif tool == "osascript" and args[1].endswith("is running"):
            out = "true\n" if self.window else "false\n"
        elif tool == "osascript" and args[1].endswith("to quit") and self.quit_refused:
            code = 1
            err = "44:48: execution error: Codex Studio got an error: User canceled. (-128)\n"
        elif tool == "osascript" and args[1].endswith("to quit"):
            self.window = False
        elif tool == "open":
            self.window = True
        else:
            raise AssertionError(f"Unexpected command {argv}")
        return subprocess.CompletedProcess(argv, code, out, err)

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
        self.verified = []
        environment = patch.dict(os.environ, {"CODEX_LINUX_VM_STATE_DIR": str(self.state / "linux-vm")})
        environment.start()
        self.addCleanup(environment.stop)

    def verify(self, config, state):
        # The build at the installed path when the backend is proved.
        self.verified.append(updates.read_build(self.installed))
        return {"pid": 12345, "started": "Sun Oct 18 08:00:00 2026"}

    def stop(self, config, state, backend):
        self.stops.append((config["port"], state, backend["pid"]))
        return "stopped"

    def install(self):
        return installer.install(self.state, self.config, applications=self.applications,
                                 run=self.mac.run, verify=self.verify, stop=self.stop,
                                 sleep=lambda seconds: None, clock=lambda: 1_800_000_000.0)

    def receipt(self):
        return json.loads((self.state / updates.RECEIPT_FILE).read_text())

    STARTED = "Mon Oct 19 03:31:12 2026"

    def write_intent(self, **changes):
        intent = {"version": 1, "desiredOpen": True, "pid": 8864, "startedAt": self.STARTED,
                  "executable": str(self.installed / "Contents/MacOS/studio-native-client"),
                  "profile": "/profile", "closeSignal": "SIGTERM"} | changes
        (self.state / "desktop-recovery.json").write_text(json.dumps(intent))

    def install_with_signal(self):
        def target(state, installed):
            return installer.signal_target(state, installed, started=lambda pid: self.STARTED)
        return installer.install(self.state, self.config, applications=self.applications,
                                 run=self.mac.run, verify=self.verify, stop=self.stop,
                                 sleep=lambda seconds: None, target=target, kill=self.mac.kill)

    def test_window_that_closes_on_sigterm_gets_the_signal_not_the_apple_event_quit(self):
        self.mac.quit_refused = True
        self.write_intent()
        code, _ = self.install_with_signal()
        self.assertEqual(code, installer.INSTALLED)
        self.assertEqual(self.mac.signals, [(8864, signal.SIGTERM)])
        self.assertEqual(self.mac.ran("osascript", "to quit"), [])
        self.assertEqual(updates.read_build(self.installed), NEW)
        self.assertEqual(self.mac.ran("open"), [["/usr/bin/open", "-g", str(self.installed)]])
        self.assertIs(self.receipt()["window"], True)

    def test_window_without_a_proved_close_signal_gets_the_apple_event_quit(self):
        for changes in ({"closeSignal": None}, {"startedAt": "Sun Oct 18 08:00:00 2026"},
                        {"executable": "/tmp/Codex Studio.app/Contents/MacOS/studio-native-client"},
                        {"desiredOpen": False}, {"pid": True}):
            with self.subTest(changes=changes):
                self.write_intent(**changes)
                self.assertIsNone(installer.signal_target(self.state, self.installed,
                                                          started=lambda pid: self.STARTED))
        self.assertEqual(self.install_with_signal()[0], installer.INSTALLED)
        self.assertEqual(self.mac.signals, [])
        self.assertEqual(len(self.mac.ran("osascript", "to quit")), 1)

    def test_refused_apple_event_quit_fails_and_leaves_the_installed_app(self):
        self.mac.quit_refused = True
        code, reason = self.install()
        self.assertEqual(code, installer.FAILED)
        self.assertEqual(reason, "The desktop window did not quit: 44:48: execution error: "
                                 "Codex Studio got an error: User canceled. (-128)")
        self.assertEqual(updates.read_build(self.installed), OLD)
        self.assertEqual(self.stops, [])

    def test_close_signal_matches_the_native_window_owner(self):
        owner = (REPOSITORY_ROOT / "workspaces/client/apps/native/src-tauri/src/desktop_recovery.rs").read_text()
        self.assertIn(f'pub const CLOSE_SIGNAL: &str = "{installer.CLOSE_SIGNAL}";', owner)
        self.assertIn('value["closeSignal"] = CLOSE_SIGNAL.into();', owner)
        self.assertIn(f'"/Applications/Codex Studio.app/{installer.NATIVE_EXECUTABLE}"', owner)
        self.assertIn(f'state.join("{installer.DESKTOP_INTENT}")', owner)

    def test_swap_installs_the_build_and_restarts_backend_and_window(self):
        make_app(self.applications / "Codex Studio.previous.app", {**OLD, "builtAt": 1.0})
        code, _ = self.install()
        self.assertEqual(code, installer.INSTALLED)
        self.assertEqual(updates.read_build(self.installed), NEW)
        self.assertEqual(updates.read_build(self.applications / "Codex Studio.previous.app"), OLD)
        self.assertEqual(sorted(p.name for p in self.applications.iterdir()),
                         ["Codex Studio.app", "Codex Studio.previous.app"])
        # The backend is proved against the app that started it and stopped after the swap.
        self.assertEqual(self.verified, [OLD])
        self.assertEqual(self.stops, [(4620, self.state, 12345)])
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
                                         run=broken_copy, verify=self.verify, stop=self.stop,
                                         sleep=lambda seconds: None)
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
        def refuse(config, state, backend):
            raise RuntimeError("The backend changed during the install; it was not verified.")

        code, _ = installer.install(self.state, self.config, applications=self.applications,
                                    run=self.mac.run, verify=self.verify, stop=refuse,
                                    sleep=lambda seconds: None)
        self.assertEqual(code, installer.INSTALLED)
        self.assertEqual(updates.read_build(self.installed), NEW)
        self.assertTrue(self.receipt()["backend"].startswith("not restarted"))

    def test_unverified_backend_keeps_the_new_app_and_is_not_signalled(self):
        def refuse(config, state):
            raise RuntimeError("The reported backend PID is not the packaged Codex Canvas process.")

        code, _ = installer.install(self.state, self.config, applications=self.applications,
                                    run=self.mac.run, verify=refuse, stop=self.stop,
                                    sleep=lambda seconds: None)
        self.assertEqual(code, installer.INSTALLED)
        self.assertEqual(updates.read_build(self.installed), NEW)
        self.assertEqual(self.stops, [])
        self.assertEqual(self.receipt()["backend"],
                         "not restarted: The reported backend PID is not the packaged Codex Canvas process.")


CHECK_OUTPUT = subprocess.check_output


def make_packaged_app(app, build, executable='/bin/cat'):
    """A native executable fixture at the packaged Rust server path."""
    make_app(app, build)
    binary = app / 'Contents/Resources/studio-runtime-server'
    # The fixture needs the code bytes and executable mode, not protected system-file flags.
    shutil.copyfile(executable, binary)
    shutil.copymode(executable, binary)
    if sys.platform == 'darwin':
        # A copied system executable needs a fixture-local signature before native exec.
        subprocess.run(['/usr/bin/codesign', '--force', '--sign', '-', str(binary)],
                       check=True, capture_output=True, timeout=15)
    return binary


class BackendRestartAfterSwap(unittest.TestCase):
    """Prove the old native executable before the swap, then stop its unchanged PID."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="studio-install-restart-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name).resolve()
        self.state = root / "state"
        self.applications = root / "Applications"
        self.state.mkdir()
        self.applications.mkdir()
        self.installed = self.applications / "Codex Studio.app"
        binary = make_packaged_app(self.installed, OLD)
        staged = self.state / "updates" / str(NEW["builtAt"]) / "Codex Studio.app"
        make_packaged_app(staged, NEW, '/bin/sleep')
        updates.write_json(self.state / updates.PENDING_FILE,
                           {"id": "update-1", "app": str(staged), "build": NEW, "from": "server"})
        self.backend = subprocess.Popen([str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.addCleanup(self.backend.stdin.close)
        self.addCleanup(self.backend.stdout.close)
        self.addCleanup(self.backend.wait, 10)
        self.addCleanup(self.backend.kill)
        threading.Thread(target=self.backend.wait, daemon=True).start()
        self.backend.stdin.write(b'fixture-ready\n')
        self.backend.stdin.flush()
        self.assertEqual(self.backend.stdout.readline(), b'fixture-ready\n', 'native exec is ready')
        self.config = {"port": 4620, "resources": str(binary.parent), "supervisorEnabled": True,
                       "backend": "rust", "runtimeServer": str(binary)}
        self.identity = {"pid": self.backend.pid, "backendKind": "rust",
                         "backendBuild": installer.recovery.file_sha256(binary)}
        environment = patch.dict(os.environ, {"CODEX_LINUX_VM_STATE_DIR": str(self.state / "linux-vm")})
        environment.start()
        self.addCleanup(environment.stop)

    def listeners(self, argv, **options):
        """lsof is not at the same path on every host; the backend owns its listener."""
        if argv[0] == "/usr/sbin/lsof":
            return f"{self.backend.pid}\n"
        return CHECK_OUTPUT(argv, **options)

    def test_native_backend_stops_after_a_swap_that_changes_its_executable(self):
        # HTTP identity and platform-specific lsof are boundary fixtures; native argv,
        # executable hash, PID start-time verification and termination run for real.
        with patch.object(installer.recovery, 'identity', return_value=self.identity), \
                patch.object(installer.recovery.subprocess, "check_output", self.listeners):
            code, _ = installer.install(self.state, self.config, applications=self.applications,
                                        run=FakeMac().run, sleep=lambda seconds: None)
        self.assertEqual(code, installer.INSTALLED)
        self.assertEqual(updates.read_build(self.installed), NEW)
        receipt = json.loads((self.state / updates.RECEIPT_FILE).read_text())
        self.assertEqual(receipt["backend"], "stopped")
        self.assertEqual(self.backend.wait(10), -signal.SIGTERM)


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
