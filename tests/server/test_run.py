"""Regression tests for server test discovery and isolated subprocess execution."""

import contextlib
import ctypes
import importlib.util
import io
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = ROOT / "tests" / "server" / "run.py"
RUNNER_SPEC = importlib.util.spec_from_file_location("server_suite_runner", RUNNER_PATH)
RUNNER = importlib.util.module_from_spec(RUNNER_SPEC)
RUNNER_SPEC.loader.exec_module(RUNNER)


class ServerSuiteRunner(unittest.TestCase):
    def test_discovers_ast_suites_and_benchmark_suites_but_excludes_helpers(self):
        paths = {path for path, _kind in RUNNER.inventory()}
        categories = dict(RUNNER.inventory())
        self.assertIn("tests/test_isolation_contract.py", paths)
        self.assertIn("tests/worker-lifecycle-scenarios-11-14.py", paths)
        self.assertIn("scripts/benchmarks/message_delivery/test_benchmark.py", paths)
        self.assertIn("scripts/benchmarks/runtime_load/test_runtime_load.py", paths)
        self.assertNotIn("tests/delivery-latency-fixture.py", paths)
        self.assertNotIn("tests/remaining-delivery-latency-fixture.py", paths)
        self.assertIn("tests/portable-smoke.mjs", paths)
        self.assertIn("tests/state-contract-smoke.mjs", paths)
        self.assertIn("tests/swarm-retry-contract.mjs", paths)
        self.assertEqual(categories["tests/workspace-native-turn.py"], "native")
        self.assertEqual(categories["tests/workspace-protocol.py"], "native")
        self.assertEqual(categories["tests/tool-parity.py"], "native")
        self.assertEqual(categories["tests/time-awareness.py"], "safe")
        source_roots = (ROOT / "tests", ROOT / "scripts" / "benchmarks")
        for source_root in source_roots:
            for path in source_root.rglob("*.py"):
                relative = path.relative_to(ROOT).as_posix()
                if (RUNNER.is_unittest_suite(path) and relative not in RUNNER.NON_TESTS
                        and "tests/fixtures/" not in relative):
                    self.assertIn(relative, paths, f"AST unittest suite omitted: {relative}")

    def test_aggregates_failed_child_and_reports_opt_in_skip(self):
        calls = []

        def execute(command, _cwd, _timeout, _environment):
            calls.append(command[-1])
            return (1, None) if command[-1].endswith("failure.py") else (0, None)

        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            runnable, skipped, failures, _elapsed = RUNNER.run_suites(
                [("failure.py", "safe"), ("native.py", "native"), ("success.py", "safe")],
                set(), 1, 1, root=Path("/tmp"), execute=execute)
        self.assertEqual(len(runnable), 2)
        self.assertEqual(skipped, [("native.py", "native")])
        self.assertEqual(failures, [("failure.py", "exit 1")])
        self.assertEqual(len(calls), 2)

        empty, skipped_only, failures, _elapsed = RUNNER.run_suites(
            [("native.py", "native")], set(), 1, 1, root=Path("/tmp"), execute=execute)
        self.assertEqual(empty, [])
        self.assertEqual(skipped_only, [("native.py", "native")])
        self.assertEqual(failures, [])

    def test_timeout_deadlines_must_be_positive_and_finite(self):
        parser = __import__("argparse").ArgumentParser()
        for deadline in (0, -1, float("inf"), float("nan")):
            with (self.subTest(deadline=deadline), contextlib.redirect_stderr(io.StringIO()),
                  self.assertRaises(SystemExit)):
                RUNNER.validate_deadlines(parser, type("Args", (), {
                    "timeout": deadline, "expensive_timeout": 1.0})())

    def test_deadline_kills_owned_process_group(self):
        with tempfile.TemporaryDirectory(prefix="server-runner-timeout-") as temp:
            marker = Path(temp) / "child-output"
            child = ("import pathlib,time\np=pathlib.Path(" + repr(str(marker)) + ")\n"
                     "while True:\n p.open('a').write('x')\n time.sleep(.01)\n")
            parent = ("import subprocess,sys,time; subprocess.Popen([sys.executable,'-c'," +
                      repr(child) + "]); time.sleep(10)")
            returncode, error = RUNNER.run_process(
                [sys.executable, "-c", parent], ROOT, .2, os.environ.copy())
            self.assertIsNone(returncode)
            self.assertIn("timed out", error)
            self.assertTrue(marker.exists())
            size = marker.stat().st_size
            time.sleep(.08)
            self.assertEqual(marker.stat().st_size, size, "grandchild continued after timeout")

    def test_successful_exit_kills_leftover_owned_process_group(self):
        with tempfile.TemporaryDirectory(prefix="server-runner-success-cleanup-") as temp:
            marker = Path(temp) / "child-output"
            child = ("import pathlib,time\np=pathlib.Path(" + repr(str(marker)) + ")\n"
                     "while True:\n p.open('a').write('x')\n time.sleep(.01)\n")
            parent = ("import pathlib,subprocess,sys,time; subprocess.Popen([sys.executable,'-c'," +
                      repr(child) + "]); p=pathlib.Path(" + repr(str(marker)) +
                      "); deadline=time.monotonic()+2\nwhile not p.exists() and time.monotonic()<deadline: time.sleep(.01)")
            returncode, error = RUNNER.run_process(
                [sys.executable, "-c", parent], ROOT, 3, os.environ.copy())
            self.assertEqual(returncode, 0)
            self.assertIsNone(error)
            self.assertTrue(marker.exists())
            size = marker.stat().st_size
            time.sleep(.08)
            self.assertEqual(marker.stat().st_size, size, "grandchild survived successful suite exit")

    def test_posix_fallback_cleans_process_group_after_reaping_leader(self):
        with tempfile.TemporaryDirectory(prefix="server-runner-fallback-cleanup-") as temp:
            marker = Path(temp) / "child-output"
            child = ("import pathlib,time\np=pathlib.Path(" + repr(str(marker)) + ")\n"
                     "while True:\n p.open('a').write('x')\n time.sleep(.01)\n")
            parent = ("import pathlib,subprocess,sys,time; subprocess.Popen([sys.executable,'-c'," +
                      repr(child) + "]); p=pathlib.Path(" + repr(str(marker)) +
                      "); deadline=time.monotonic()+2\nwhile not p.exists() and time.monotonic()<deadline: time.sleep(.01)")
            with mock.patch.object(RUNNER, "_supports_waitid_nowait", return_value=False):
                returncode, error = RUNNER.run_process(
                    [sys.executable, "-c", parent], ROOT, 3, os.environ.copy())
            self.assertEqual(returncode, 0)
            self.assertIsNone(error)
            self.assertTrue(marker.exists())
            size = marker.stat().st_size
            time.sleep(.08)
            self.assertEqual(marker.stat().st_size, size,
                             "fallback suite process group retained its grandchild")

    def test_windows_job_assignment_failure_kills_suspended_suite(self):
        class ApiFunction:
            def __init__(self, callback):
                self.callback = callback

            def __call__(self, *args):
                return self.callback(*args)

        class FakeKernel:
            CreateJobObjectW = ApiFunction(lambda *_args: 123)
            SetInformationJobObject = ApiFunction(lambda *_args: 1)
            AssignProcessToJobObject = ApiFunction(lambda *_args: 0)
            TerminateJobObject = ApiFunction(lambda *_args: 1)
            CloseHandle = ApiFunction(lambda *_args: 1)

        class FakeProcess:
            _handle = 456

            def __init__(self):
                self.killed = False
                self.waited = False

            def kill(self):
                self.killed = True

            def wait(self):
                self.waited = True

        process = FakeProcess()
        with (mock.patch.object(RUNNER, "_windows_kernel32", return_value=FakeKernel()),
              mock.patch.object(RUNNER.ctypes, "WinError", side_effect=OSError, create=True),
              mock.patch.object(RUNNER.ctypes, "get_last_error", return_value=5, create=True)):
            with self.assertRaises(OSError):
                RUNNER._assign_windows_job(process)
        self.assertTrue(process.killed)
        self.assertTrue(process.waited)

    def test_windows_resume_failure_kills_suspended_suite(self):
        class ApiFunction:
            argtypes = None
            restype = None

            def __call__(self, *_args):
                return 1  # NTSTATUS failure

        class FakeNtdll:
            NtResumeProcess = ApiFunction()

        class FakeProcess:
            _handle = 456

            def __init__(self):
                self.killed = False
                self.waited = False

            def kill(self):
                self.killed = True

            def wait(self):
                self.waited = True

        process = FakeProcess()
        with mock.patch.object(RUNNER.ctypes, "WinDLL", return_value=FakeNtdll(), create=True):
            with self.assertRaisesRegex(OSError, "NtResumeProcess"):
                RUNNER._resume_windows_process(process)
        self.assertTrue(process.killed)
        self.assertTrue(process.waited)

    def test_windows_interruption_closes_its_owned_job(self):
        class FakeProcess:
            pid = 123
            returncode = None

            def __init__(self):
                self.wait_calls = 0

            def wait(self, timeout=None):
                self.wait_calls += 1
                if self.wait_calls == 1:
                    raise KeyboardInterrupt()
                self.returncode = -9
                return self.returncode

            def kill(self):
                pass

        process = FakeProcess()
        with (mock.patch.object(RUNNER.os, "name", "nt"),
              mock.patch.object(RUNNER.subprocess, "CREATE_NEW_PROCESS_GROUP", 0x200, create=True),
              mock.patch.object(RUNNER.subprocess, "Popen", return_value=process),
              mock.patch.object(RUNNER, "_assign_windows_job", return_value=456),
              mock.patch.object(RUNNER, "_resume_windows_process"),
              mock.patch.object(RUNNER, "_close_windows_job") as close_job):
            with self.assertRaises(KeyboardInterrupt):
                RUNNER.run_process(["suite.exe"], ROOT, 1, {})
        close_job.assert_called_once_with(456)
        self.assertEqual(process.wait_calls, 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
