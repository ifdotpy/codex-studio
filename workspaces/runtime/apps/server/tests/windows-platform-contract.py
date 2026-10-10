#!/usr/bin/env python3
"""Windows-only contracts for Studio's server platform layer."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))

from codex_executables import command, tailscale, which
from codex_file_lock import flock, LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN
from codex_private_paths import ensure_private_dir, protect, protect_temp_file
from codex_process_supervisor import process_start_time
from codex_state import cache_dir, codex_home, state_dir
from codex_native_binary import native_candidate


WINDOWS = os.name == "nt"
skip_posix = unittest.skipUnless(WINDOWS, "Windows contract")
if WINDOWS:
    tempfile.tempdir = str(Path.home() / "studio-dev" / "tmp")
    Path(tempfile.tempdir).mkdir(parents=True, exist_ok=True)


@skip_posix
class WindowsPlatformContract(unittest.TestCase):
    def test_state_cache_and_profile_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            local = Path(temporary) / "Local Data Ω"
            with patch.dict(os.environ, {"LOCALAPPDATA": str(local), "CODEX_AGENTS_STATE_DIR": "", "CODEX_HOME": "", "XDG_CACHE_HOME": ""}, clear=False):
                self.assertEqual(state_dir(), local / "CodexStudio" / "state")
                self.assertEqual(cache_dir(), local / "CodexStudio" / "cache")
                self.assertEqual(codex_home(), Path.home() / ".codex")
                override = local / "state with spaces" / "Профиль"
                with patch.dict(os.environ, {"CODEX_AGENTS_STATE_DIR": str(override)}):
                    self.assertEqual(state_dir(), override.resolve())
                profile = local / "Codex Profile Ω"
                cache = local / "cache with spaces"
                with patch.dict(os.environ, {"CODEX_HOME": str(profile), "CODEX_AGENTS_CACHE_DIR": str(cache)}):
                    self.assertEqual(codex_home(), profile.resolve())
                    self.assertEqual(cache_dir(), cache)

    def test_private_acl_for_root_and_atomic_temp_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = ensure_private_dir(Path(temporary) / "state Ω")
            temporary_file = root / "registry.tmp"
            temporary_file.write_text("private", encoding="utf-8")
            protect_temp_file(temporary_file)
            protect(root, directory=True)
            final_file = root / "registry.json"
            temporary_file.replace(final_file)
            identity = subprocess.run(["whoami.exe", "/user", "/fo", "csv", "/nh"],
                                      capture_output=True, text=True, check=True).stdout
            account_name = identity.split(",", 1)[0].strip().strip('"').casefold()
            for target in (root, final_file):
                subprocess.run(["icacls.exe", str(target), "/grant", "*S-1-1-0:F"],
                               capture_output=True, text=True, check=True, timeout=10)
                protect(target, directory=target.is_dir())
                output = subprocess.run(["icacls.exe", str(target)], capture_output=True,
                                        text=True, check=True, timeout=10).stdout.casefold()
                self.assertNotIn("everyone", output)
                self.assertNotIn("\\builtin\\users", output)
                self.assertIn(account_name, output)
                principals = [line.strip().casefold() for line in output.splitlines()
                               if ":(" in line]
                self.assertEqual(len(principals), 1, output)
                self.assertIn(account_name, principals[0], output)

    def test_supervisor_metadata_read_while_lock_is_held(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "supervisor.lock"
            path.write_text('{"pid":123,"owner":"metadata"}', encoding="utf-8")
            worker = """
import sys, time
sys.path.insert(0, sys.argv[2])
from codex_file_lock import flock, LOCK_EX
with open(sys.argv[1], 'r+') as handle:
    flock(handle, LOCK_EX)
    print('locked', flush=True)
    time.sleep(60)
"""
            process = subprocess.Popen([getattr(sys, "_base_executable", sys.executable),
                                        "-c", worker, str(path), str(SERVER_SOURCE_ROOT)],
                                       stdout=subprocess.PIPE, text=True)
            try:
                self.assertEqual(process.stdout.readline().strip(), "locked")
                self.assertIn(b'"owner":"metadata"', path.read_bytes())
            finally:
                process.kill()
                process.wait(timeout=10)
                process.stdout.close()

    def test_windows_npm_shim_layouts_resolve_native_bundle(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            package = base / "node_modules" / "@openai" / "codex"
            binary_dir = package / "vendor" / "x86_64-pc-windows-msvc" / "bin"
            binary_dir.mkdir(parents=True)
            (package / "package.json").write_text('{"name":"@openai/codex"}', encoding="utf-8")
            (package / "bin").mkdir()
            (package / "bin" / "codex.js").write_text("// shim target", encoding="utf-8")
            binary = binary_dir / "codex.exe"
            binary.write_bytes(b"binary")
            root_shim = base / "npm" / "codex.cmd"
            root_shim.parent.mkdir()
            root_shim.write_text('@echo off\r\nnode "%~dp0\\node_modules\\@openai\\codex\\bin\\codex.js" %*\r\n', encoding="utf-8")
            local_bin = base / "project" / "node_modules" / ".bin"
            local_bin.mkdir(parents=True)
            local_shim = local_bin / "codex.cmd"
            local_shim.write_text('@echo off\r\nnode "%~dp0\\..\\@openai\\codex\\bin\\codex.js" %*\r\n', encoding="utf-8")
            for shim in (root_shim, local_shim):
                self.assertEqual(native_candidate(shim), binary)

    def test_lock_contention_and_process_crash_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "lock space Ω"
            path.touch()
            worker = """
import os, sys, time
sys.path.insert(0, sys.argv[2])
from codex_file_lock import flock, LOCK_EX
handle = open(sys.argv[1], 'a+')
flock(handle, LOCK_EX)
print('locked', flush=True)
time.sleep(60)
"""
            process = subprocess.Popen([sys.executable, "-c", worker, str(path), str(SERVER_SOURCE_ROOT)],
                                       stdout=subprocess.PIPE, text=True)
            try:
                self.assertEqual(process.stdout.readline().strip(), "locked")
                with path.open("a+") as handle:
                    with self.assertRaises(BlockingIOError):
                        flock(handle, LOCK_EX | LOCK_NB)
                process.kill()
                process.wait(timeout=10)
                with path.open("a+") as handle:
                    flock(handle, LOCK_EX | LOCK_NB)
                    flock(handle, LOCK_UN)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
                process.stdout.close()

    def test_shared_lock_and_long_unicode_path(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "shared lock Ω"
            path.touch()
            with path.open("a+") as first, path.open("a+") as second:
                flock(first, LOCK_SH | LOCK_NB)
                flock(second, LOCK_SH | LOCK_NB)
                flock(first, LOCK_UN)
                flock(second, LOCK_UN)
            long_path = Path("\\\\?\\" + str(Path(temporary).resolve()))
            for index in range(8):
                long_path /= ("segment Ω " + str(index) + "x" * 25)
            try:
                ensure_private_dir(long_path)
                self.assertTrue(long_path.is_dir())
                self.assertGreater(len(str(long_path)), 260)
            finally:
                for _ in range(8):
                    try:
                        os.rmdir(long_path)
                    except OSError:
                        break
                    long_path = long_path.parent

    def test_pathext_cmd_shim_and_tailscale_exe(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "tools Ω"
            root.mkdir()
            shim = root / "studio-tool.cmd"
            shim.write_text("@echo off\r\necho %1\r\n", encoding="utf-8")
            found = which("studio-tool", environment={"PATH": str(root), "PATHEXT": ".EXE;.CMD"})
            self.assertEqual(Path(found), shim)
            result = subprocess.run(command(found, ["safe-value"]), capture_output=True,
                                    text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("safe-value", result.stdout)
            with self.assertRaises(ValueError):
                command(found, ["%PATH%"])
            tail = root / "tailscale.exe"
            tail.write_bytes(b"fixture")
            with patch.dict(os.environ, {"PATH": str(root), "PATHEXT": ".EXE;.CMD"}):
                self.assertEqual(Path(tailscale()), tail)

    def test_process_identity_and_server_import_smoke(self):
        self.assertIsInstance(process_start_time(os.getpid()), str)
        code = "import sys, codex_runtime, codex_accounts, studio_api.app; " \
               "assert not {'fcntl', 'pwd', 'resource'} & sys.modules.keys(); " \
               "print('server-import-ok')"
        environment = os.environ.copy()
        environment["PYTHONPATH"] = str(SERVER_SOURCE_ROOT)
        result = subprocess.run([sys.executable, "-B", "-c", code], env=environment,
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("server-import-ok", result.stdout)

    def test_runtime_pipe_write_uses_blocking_windows_io(self):
        from codex_runtime import AppServer
        from types import SimpleNamespace

        process = subprocess.Popen(
            [sys.executable, "-c", "import sys; line=sys.stdin.readline(); sys.stdout.write(line); sys.stdout.flush()"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        server = AppServer.__new__(AppServer)
        server.proc = process
        server.closed = False
        server.transport_error = None
        server.supervisor_mode = False
        server.write_lock = threading.RLock()
        server.transcript_capture = SimpleNamespace(record=lambda *_args: None)
        try:
            server.write({"method": "platform-test", "params": {}})
            self.assertEqual(process.stdout.readline().replace(b"\r\n", b"\n"),
                             b'{"method": "platform-test", "params": {}}\n')
            self.assertEqual(process.wait(timeout=10), 0)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)
            process.stdin.close()
            process.stdout.close()
            process.stderr.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
