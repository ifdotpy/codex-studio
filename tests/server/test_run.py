"""Regression tests for server test discovery and isolated subprocess execution."""

import contextlib
import ctypes
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import io
import json
import multiprocessing
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = ROOT / "tests" / "server" / "run.py"
RUNNER_SPEC = importlib.util.spec_from_file_location("server_suite_runner", RUNNER_PATH)
RUNNER = importlib.util.module_from_spec(RUNNER_SPEC)
RUNNER_SPEC.loader.exec_module(RUNNER)


def _save_profile_in_process(profile_path, profile, start_gate):
    RUNNER.PROFILE_PATH = Path(profile_path)
    RUNNER.TEST_TMP_ROOT = Path(profile_path).parent
    start_gate.wait(timeout=5)
    RUNNER._save_profile(profile)


class ServerSuiteRunner(unittest.TestCase):
    def test_discovers_ast_suites_and_excludes_helpers(self):
        paths = {path for path, _kind in RUNNER.inventory()}
        categories = dict(RUNNER.inventory())
        self.assertIn("tests/test_isolation_contract.py", paths)
        self.assertIn("tests/worker-lifecycle-scenarios-11-14.py", paths)
        self.assertIn("scripts/test_codex_api_client.py", paths)
        self.assertIn("scripts/test_codex_token_rate_events.py", paths)
        self.assertIn("scripts/test_codex_resource_producers.py", paths)
        self.assertIn("tests/portable-smoke.mjs", paths)
        self.assertIn("tests/state-contract-smoke.mjs", paths)
        self.assertIn("tests/swarm-retry-contract.mjs", paths)
        self.assertIn("scripts/sync/benchmarks/message_delivery/test_benchmark.py", paths)
        self.assertIn("scripts/studio_api/benchmarks/runtime_load/test_runtime_load.py", paths)
        self.assertIn("scripts/studio_api/benchmarks/runtime_load/test_http_outcomes.mjs", paths)
        self.assertEqual(categories["scripts/sync/benchmarks/message_delivery/test_benchmark.py"], "expensive")
        self.assertEqual(categories["scripts/studio_api/benchmarks/runtime_load/test_runtime_load.py"], "expensive")
        self.assertEqual(categories["scripts/studio_api/benchmarks/runtime_load/test_http_outcomes.mjs"], "expensive")
        self.assertNotIn("tests/sync-live-patch-http-contract.py", paths)
        self.assertIn("tests/sync-live-patch-http-contract.py", RUNNER.NON_TESTS)
        self.assertEqual(categories["tests/workspace-native-turn.py"], "native")
        self.assertEqual(categories["tests/workspace-protocol.py"], "native")
        self.assertEqual(categories["tests/tool-parity.py"], "native")
        self.assertEqual(categories["tests/linux-vm-auth-native.py"], "vm")
        self.assertEqual(categories["tests/linux-vm-studio-native.py"], "vm")
        self.assertEqual(categories["tests/linux-vm-runtime-contract.py"], "safe")
        self.assertIn("vm", RUNNER.OPT_IN)
        self.assertEqual(categories["tests/time-awareness.py"], "safe")
        source_roots = (
            ROOT / "tests",
            ROOT / "scripts" / "sync" / "benchmarks" / "message_delivery",
            ROOT / "scripts" / "studio_api" / "benchmarks" / "runtime_load",
        )
        for source_root in source_roots:
            for path in source_root.rglob("*.py"):
                relative = path.relative_to(ROOT).as_posix()
                if (RUNNER.is_unittest_suite(path) and relative not in RUNNER.NON_TESTS
                        and "tests/fixtures/" not in relative):
                    self.assertIn(relative, paths, f"AST unittest suite omitted: {relative}")

    def test_discovers_colocated_fastapi_component_tests_recursively(self):
        component_root = RUNNER.STUDIO_API_COMPONENT_ROOT
        paths = dict(RUNNER.inventory())
        if not component_root.is_dir():
            self.skipTest("FastAPI component sources have not arrived in this worktree")
        expected = {
            path.relative_to(ROOT).as_posix()
            for path in component_root.rglob("test_*.py")
            if (path.is_file() and not path.is_symlink()
                and path.relative_to(ROOT).as_posix() not in RUNNER.EXPENSIVE_SUITES)
        }
        self.assertTrue(expected, "FastAPI package has no colocated component tests")
        self.assertTrue(expected.issubset(paths))
        self.assertTrue(all(paths[path] == "component" for path in expected))

    def test_runs_fastapi_component_suites_as_package_modules(self):
        commands = []

        def execute(command, _cwd, _timeout, _environment):
            commands.append(command)
            return 0, None

        with contextlib.redirect_stdout(io.StringIO()):
            RUNNER.run_suites(
                [("scripts/studio_api/agents/test_router.py", "component")],
                set(), 1, 1, root=ROOT, execute=execute, workers=1,
            )

        self.assertEqual(len(commands), 1)
        self.assertEqual(
            commands[0][-4:],
            ["-B", "-m", "unittest", "studio_api.agents.test_router"],
        )

    def test_aggregates_failed_child_and_reports_opt_in_skip(self):
        calls = []

        def execute(command, _cwd, _timeout, _environment):
            calls.append(command[-1])
            return (1, None) if command[-1].endswith("failure.py") else (0, None)

        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            runnable, skipped, failures, _elapsed = RUNNER.run_suites(
                [("failure.py", "safe"), ("native.py", "native"), ("success.py", "safe")],
                set(), 1, 1, root=ROOT, execute=execute, workers=1)
        self.assertEqual(len(runnable), 2)
        self.assertEqual(skipped, [("native.py", "native")])
        self.assertEqual(failures, [("failure.py", "exit 1")])
        self.assertEqual(len(calls), 2)

        empty, skipped_only, failures, _elapsed = RUNNER.run_suites(
            [("native.py", "native")], set(), 1, 1, root=ROOT, execute=execute, workers=1)
        self.assertEqual(empty, [])
        self.assertEqual(skipped_only, [("native.py", "native")])
        self.assertEqual(failures, [])

    def test_parallel_suites_receive_distinct_short_runtime_directories(self):
        barrier = threading.Barrier(2)
        lock = threading.Lock()
        active = 0
        peak_active = 0
        environments = []

        def execute(_command, _cwd, _timeout, environment):
            nonlocal active, peak_active
            with lock:
                environments.append(environment)
                active += 1
                peak_active = max(peak_active, active)
            barrier.wait(timeout=10)
            with lock:
                active -= 1
            return 0, None

        with (contextlib.redirect_stdout(io.StringIO()),
              mock.patch.object(RUNNER, "_save_profile")):
            runnable, skipped, failures, _elapsed = RUNNER.run_suites(
                [("first.py", "safe"), ("second.py", "safe")],
                set(), 1, 1, execute=execute, workers=2,
            )

        self.assertEqual(len(runnable), 2)
        self.assertEqual(skipped, [])
        self.assertEqual(failures, [])
        self.assertEqual(peak_active, 2)
        self.assertEqual(len({environment["TMPDIR"] for environment in environments}), 2)
        for variable in ("HOME", "USERPROFILE", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME",
                         "CODEX_HOME", "CLAUDE_CONFIG_DIR", "CODEX_WORKSPACE_STORE"):
            self.assertEqual(len({environment[variable] for environment in environments}), 2)
        for environment in environments:
            self.assertEqual(Path(environment["CODEX_WORKSPACE_STORE"]).parent,
                             Path(environment["TMPDIR"]))
            self.assertEqual(Path(environment["XDG_CACHE_HOME"]).parent,
                             Path(environment["TMPDIR"]))
            self.assertEqual(Path(environment["HOME"]).parent, Path(environment["TMPDIR"]))
            self.assertEqual(environment["USERPROFILE"], environment["HOME"])

    def test_measured_longest_suites_run_first(self):
        executed = []

        def execute(command, _cwd, _timeout, _environment):
            executed.append(Path(command[-1]).name)
            return 0, None

        profile = {
            "maxSuiteRssBytes": 100,
            "suiteSeconds": {"fast.py": 1.0, "slow.py": 9.0},
        }
        with (contextlib.redirect_stdout(io.StringIO()),
              mock.patch.object(RUNNER, "_load_profile", return_value=profile),
              mock.patch.object(RUNNER, "_save_profile")):
            runnable, skipped, failures, _elapsed = RUNNER.run_suites(
                [("fast.py", "safe"), ("slow.py", "safe")], set(), 1, 1,
                execute=execute, workers=1,
            )

        self.assertEqual(runnable, [("fast.py", "safe"), ("slow.py", "safe")])
        self.assertEqual(skipped, [])
        self.assertEqual(failures, [])
        self.assertEqual(executed, ["slow.py", "fast.py"])

    def test_concurrent_profile_writers_merge_with_unique_atomic_temporaries(self):
        try:
            context = multiprocessing.get_context("fork")
        except ValueError:  # pragma: no cover - Windows
            self.skipTest("Concurrent profile writer test requires process-shared fork events")
        with tempfile.TemporaryDirectory(prefix="server-profile-writers-") as directory:
            profile_path = Path(directory) / "runner-profile.json"
            start_gate = context.Event()

            profiles = (
                {"suiteSeconds": {"first.py": 3.0}, "maxSuiteRssBytes": 100,
                 "maxSuiteScratchBytes": 200},
                {"suiteSeconds": {"second.py": 4.0}, "maxSuiteRssBytes": 300,
                 "maxSuiteScratchBytes": 150},
            )
            writers = [context.Process(target=_save_profile_in_process,
                                       args=(str(profile_path), profile, start_gate))
                       for profile in profiles]
            for writer in writers:
                writer.start()
            start_gate.set()
            for writer in writers:
                writer.join(timeout=5)
                if writer.is_alive():
                    writer.terminate()
                    writer.join(timeout=2)
                self.assertEqual(writer.exitcode, 0)
            saved = json.loads(profile_path.read_text(encoding="utf-8"))
            self.assertEqual(saved["suiteSeconds"], {"first.py": 3.0, "second.py": 4.0})
            self.assertEqual(saved["maxSuiteRssBytes"], 300)
            self.assertEqual(saved["maxSuiteScratchBytes"], 200)
            self.assertEqual(list(Path(directory).glob(".runner-profile.json.*.tmp")), [])

    def test_concurrent_tmpfs_roots_are_unique_and_cleanup_is_owned(self):
        with tempfile.TemporaryDirectory(prefix="server-runner-root-race-") as temp:
            mount = Path(temp)
            with (mock.patch.object(RUNNER, "MAX_SHORT_TMP_ROOT_BYTES", 200),
                  mock.patch.object(RUNNER, "_tmpfs_mounts", side_effect=lambda: iter([mount])),
                  mock.patch.object(RUNNER, "_probe_scratch_capacity", return_value=True),
                  mock.patch.object(RUNNER.shutil, "disk_usage",
                                    return_value=type("Usage", (), {"free": 4096})())):
                with ThreadPoolExecutor(max_workers=2) as pool:
                    roots = list(pool.map(lambda _: RUNNER._memory_scratch_root(1024, 512), range(2)))
            self.assertTrue(all(roots))
            self.assertNotEqual(roots[0], roots[1])
            self.assertTrue(all(root.is_dir() for root in roots))
            RUNNER.shutil.rmtree(roots[0])
            self.assertFalse(roots[0].exists())
            self.assertTrue(roots[1].is_dir())
            RUNNER.shutil.rmtree(roots[1])

    def test_automatic_worker_count_tracks_cpu_affinity_and_measured_memory(self):
        profile = {
            "maxSuiteRssBytes": 1024**3,
            "suiteSeconds": {"slow.py": 721},
        }
        entries = [("slow.py", "safe")] * 80
        with (mock.patch.object(RUNNER, "available_memory_bytes", return_value=12 * 1024**3),
              mock.patch.object(RUNNER, "available_cpu_count", return_value=16),
              mock.patch.object(RUNNER, "sample_runnable_other_process_count", return_value=0)):
            unconstrained = RUNNER.automatic_worker_count(entries, profile)
        with (mock.patch.object(RUNNER, "available_memory_bytes", return_value=12 * 1024**3),
              mock.patch.object(RUNNER, "available_cpu_count", return_value=2),
              mock.patch.object(RUNNER, "sample_runnable_other_process_count", return_value=0)):
            restricted = RUNNER.automatic_worker_count(entries, profile)
        self.assertEqual(unconstrained, 6)
        self.assertEqual(restricted, 2)

        with (mock.patch.object(RUNNER, "available_memory_bytes", return_value=12 * 1024**3),
              mock.patch.object(RUNNER, "available_cpu_count", return_value=16),
              mock.patch.object(RUNNER, "sample_runnable_other_process_count", return_value=0)):
            suite_limited = RUNNER.automatic_worker_count(entries[:3], profile)
        self.assertEqual(suite_limited, 3)
        with (mock.patch.object(RUNNER, "available_memory_bytes", return_value=12 * 1024**3),
              mock.patch.object(RUNNER, "available_cpu_count", return_value=16),
              mock.patch.object(RUNNER, "sample_runnable_other_process_count", return_value=0)):
            suite_plan = RUNNER.worker_plan(entries[:3], profile)
            override_plan = RUNNER.worker_plan(entries, profile, override=4)
        self.assertEqual(suite_plan["limitingBound"], "suites")
        self.assertEqual(override_plan["workers"], 4)
        self.assertEqual(override_plan["limitingBound"], "override")

        # At 6 GiB available, half is 3 GiB but the 4 GiB reserve leaves only
        # 2 GiB allocatable. This verifies the reserve, not just the half cap.
        with (mock.patch.object(RUNNER, "available_memory_bytes", return_value=6 * 1024**3),
              mock.patch.object(RUNNER, "available_cpu_count", return_value=16),
              mock.patch.object(RUNNER, "sample_runnable_other_process_count", return_value=0)):
            low_memory_plan = RUNNER.worker_plan(entries, profile)
        self.assertEqual(low_memory_plan["memoryBudgetBytes"], 2 * 1024**3)
        self.assertEqual(low_memory_plan["memoryWorkerSlots"], 2)
        self.assertEqual(low_memory_plan["limitingBound"], "memory")

        with (mock.patch.object(RUNNER, "available_memory_bytes", return_value=0),
              mock.patch.object(RUNNER, "available_cpu_count", return_value=16),
              mock.patch.object(RUNNER, "sample_runnable_other_process_count", return_value=0)):
            memory_limited = RUNNER.automatic_worker_count(entries, profile)
        self.assertEqual(memory_limited, 1)

    def test_show_jobs_subtracts_mocked_runnable_load(self):
        entries = [(f"suite-{index}.py", "safe") for index in range(40)]
        plans = []
        for other_runnable in (0, 24, 30):
            output = io.StringIO()
            with (mock.patch.object(RUNNER.sys, "argv", ["run.py", "--show-jobs",
                                                            "--load-sample-seconds", "0"]),
                  mock.patch.object(RUNNER, "inventory", return_value=entries),
                  mock.patch.object(RUNNER, "_load_profile", return_value={"maxSuiteRssBytes": 100}),
                  mock.patch.object(RUNNER, "available_cpu_count", return_value=32),
                  mock.patch.object(RUNNER, "available_memory_bytes", return_value=64 * 1024**3),
                  mock.patch.object(RUNNER, "sample_runnable_other_process_count", return_value=other_runnable),
                  contextlib.redirect_stdout(output)):
                self.assertEqual(RUNNER.main(), 0)
            plan_line = next(line for line in output.getvalue().splitlines()
                             if line.startswith("Worker plan: "))
            plans.append(json.loads(plan_line.removeprefix("Worker plan: ")))
        self.assertEqual(plans[0]["workers"], 32)
        self.assertEqual(plans[0]["otherRunnableProcesses"], 0)
        self.assertEqual(plans[0]["limitingBound"], "cpu-median")
        self.assertEqual(plans[1]["workers"], 8)
        self.assertEqual(plans[1]["otherRunnableProcesses"], 24)
        self.assertEqual(plans[1]["limitingBound"], "cpu-median")
        self.assertEqual(plans[2]["workers"], 8)
        self.assertEqual(plans[2]["limitingBound"], "cpu-floor")

    def test_runnable_load_sampler_uses_five_second_median_and_excludes_runner(self):
        samples = iter([1, 3, 5, 8, 4, 2, 7, 3, 1, 4, 6])
        now = [0.0]

        def sleep(seconds):
            now[0] += seconds

        with mock.patch.object(RUNNER.Path, "is_file", return_value=True):
            self.assertEqual(RUNNER.sample_runnable_other_process_count(
                window_seconds=5,
                sample=lambda: next(samples),
                clock=lambda: now[0],
                sleep=sleep,
            ), 3)
        self.assertEqual(now[0], 5.0)

    def test_show_jobs_load_sample_window_can_come_from_environment(self):
        entries = [("only.py", "safe")]
        output = io.StringIO()
        with (mock.patch.object(RUNNER.sys, "argv", ["run.py", "--show-jobs"]),
              mock.patch.dict(os.environ, {"CODEX_SERVER_TEST_LOAD_SAMPLE_SECONDS": "0"}),
              mock.patch.object(RUNNER, "inventory", return_value=entries),
              mock.patch.object(RUNNER, "_load_profile", return_value={"maxSuiteRssBytes": 100}),
              mock.patch.object(RUNNER, "available_cpu_count", return_value=8),
              mock.patch.object(RUNNER, "available_memory_bytes", return_value=16 * 1024**3),
              mock.patch.object(RUNNER, "sample_runnable_other_process_count", return_value=0) as sample,
              contextlib.redirect_stdout(output)):
            self.assertEqual(RUNNER.main(), 0)
        sample.assert_called_once_with(window_seconds=0.0)
        self.assertIn("median of 0-second sampled", output.getvalue())

    def test_tmpfs_scratch_root_requires_capacity_for_all_workers_and_cleans_up(self):
        with tempfile.TemporaryDirectory(prefix="server-runner-tmpfs-") as temp:
            mount = Path(temp)
            with (mock.patch.object(RUNNER, "MAX_SHORT_TMP_ROOT_BYTES", 200),
                  mock.patch.object(RUNNER, "_tmpfs_mounts", return_value=iter([mount])),
                  mock.patch.object(RUNNER, "_probe_scratch_capacity", return_value=True),
                  mock.patch.object(RUNNER.shutil, "disk_usage",
                                    return_value=type("Usage", (), {"free": 599})())):
                self.assertIsNone(RUNNER._memory_scratch_root(600, 100))

            with (mock.patch.object(RUNNER, "MAX_SHORT_TMP_ROOT_BYTES", 200),
                  mock.patch.object(RUNNER, "_tmpfs_mounts", return_value=iter([mount])),
                  mock.patch.object(RUNNER, "_probe_scratch_capacity", return_value=True) as probe,
                  mock.patch.object(RUNNER.shutil, "disk_usage",
                                    return_value=type("Usage", (), {"free": 600})())):
                owned = RUNNER._memory_scratch_root(600, 100)
            self.assertIsNotNone(owned)
            self.assertEqual(owned.parent, mount)
            self.assertLessEqual(len(os.fsencode(owned)), 200)
            probe.assert_called_once_with(owned, RUNNER.SCRATCH_PROBE_MIN_BYTES)
            RUNNER.shutil.rmtree(owned)
            self.assertFalse(owned.exists())

    def test_scratch_capacity_probe_fsyncs_and_removes_its_file(self):
        with tempfile.TemporaryDirectory(prefix="server-runner-capacity-") as temp:
            root = Path(temp)
            with mock.patch.object(RUNNER.os, "fsync", wraps=os.fsync) as fsync:
                self.assertTrue(RUNNER._probe_scratch_capacity(root, 1024))
            fsync.assert_called_once()
            self.assertEqual(list(root.iterdir()), [])

    def test_tmpfs_mount_discovery_ignores_read_only_and_long_mounts(self):
        mountinfo = (
            "21 1 0:1 / /dev/shm rw,nosuid - tmpfs tmpfs rw,size=1g\n"
            "22 1 0:5 / /tmp rw,nosuid - tmpfs tmpfs rw,size=1g\n"
            "23 1 0:2 / /mnt/readonly ro,nosuid - tmpfs tmpfs ro,size=1g\n"
            "24 1 0:3 / /" + "x" * 60 + " rw - tmpfs tmpfs rw,size=1g\n"
            "25 1 0:4 / /home rw - ext4 /dev/sda rw\n"
        )
        with (mock.patch.object(RUNNER.sys, "platform", "linux"),
              mock.patch.object(Path, "read_text", return_value=mountinfo)):
            self.assertEqual(list(RUNNER._tmpfs_mounts()), [Path("/tmp"), Path("/dev/shm")])

    def test_available_cpu_count_observes_cgroup_quota(self):
        original_read_text = Path.read_text
        original_exists = Path.exists

        def read_text(path, *args, **kwargs):
            if path == Path("/proc/self/cgroup"):
                return "0::/test/container"
            if path == Path("/sys/fs/cgroup/cpu.max"):
                return "200000 100000"
            if path in (Path("/sys/fs/cgroup/cpu/cpu.cfs_quota_us"),
                        Path("/sys/fs/cgroup/cpu/cpu.cfs_period_us")):
                raise OSError("not a cgroup v1 host")
            return original_read_text(path, *args, **kwargs)

        def exists(path):
            if path == Path("/sys/fs/cgroup/cpu.max"):
                return True
            return original_exists(path)

        with (mock.patch.object(RUNNER.os, "process_cpu_count", return_value=16, create=True),
              mock.patch.object(Path, "read_text", read_text),
              mock.patch.object(Path, "exists", exists)):
            self.assertEqual(RUNNER.available_cpu_count(), 2)

    def test_cgroup_parent_limits_apply_when_stricter_than_child(self):
        contents = {
            Path("/proc/self/cgroup"): "0::/test/container",
            Path("/sys/fs/cgroup/test/container/cpu.max"): "400000 100000",
            Path("/sys/fs/cgroup/test/cpu.max"): "200000 100000",
            Path("/sys/fs/cgroup/cpu.max"): "max 100000",
            Path("/sys/fs/cgroup/test/container/memory.max"): "1000",
            Path("/sys/fs/cgroup/test/container/memory.current"): "100",
            Path("/sys/fs/cgroup/test/memory.max"): "600",
            Path("/sys/fs/cgroup/test/memory.current"): "100",
            Path("/sys/fs/cgroup/memory.max"): "max",
            Path("/proc/meminfo"): "MemAvailable: 1000000 kB\n",
        }

        def read_text(path, *args, **kwargs):
            if path in contents:
                return contents[path]
            raise OSError(f"unexpected read: {path}")

        def exists(path):
            return path in contents

        with (mock.patch.object(RUNNER.os, "process_cpu_count", return_value=16, create=True),
              mock.patch.object(Path, "read_text", read_text),
              mock.patch.object(Path, "exists", exists)):
            self.assertEqual(RUNNER.available_cpu_count(), 2)
            self.assertEqual(RUNNER.available_memory_bytes(), 500)

    def test_timeout_deadlines_must_be_positive_and_finite(self):
        parser = __import__("argparse").ArgumentParser()
        for deadline in (0, -1, float("inf"), float("nan")):
            with (self.subTest(deadline=deadline), contextlib.redirect_stderr(io.StringIO()),
                  self.assertRaises(SystemExit)):
                RUNNER.validate_deadlines(parser, type("Args", (), {
                    "timeout": deadline, "expensive_timeout": 1.0})())

    def test_fallback_bounds_large_finite_select_timeout(self):
        class FakeProcess:
            pid = 321

            def wait(self, timeout=None):
                return -9

            def kill(self):
                pass

        intervals = []

        def ready_now(_readers, _writers, _errors, interval):
            intervals.append(interval)
            return ([42], [], [])

        with (mock.patch.object(RUNNER.subprocess, "Popen", return_value=FakeProcess()),
              mock.patch.object(RUNNER.os, "pipe", return_value=(42, 43)),
              mock.patch.object(RUNNER.os, "read", return_value=b"0"),
              mock.patch.object(RUNNER.os, "close"),
              mock.patch.object(RUNNER.select, "select", side_effect=ready_now)):
            result = RUNNER._run_process_with_group_supervisor(["suite.py"], ROOT, 1e100, {})
        self.assertEqual(result, (0, None))
        self.assertEqual(intervals, [1.0])

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

    def test_run_process_reports_scratch_exhaustion_as_infrastructure_error(self):
        command = [sys.executable, "-c",
                   "import sys; print('OSError: [Errno 122] Disk quota exceeded'); sys.exit(1)"]
        with contextlib.redirect_stdout(io.StringIO()):
            returncode, error = RUNNER.run_process(command, ROOT, 5, os.environ.copy())
        self.assertEqual(returncode, 1)
        self.assertEqual(error, "scratch exhausted (suite output reported ENOSPC or EDQUOT)")

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
            kill_record = Path(temp) / "group-kill.json"
            sitecustomize = Path(temp) / "sitecustomize.py"
            sitecustomize.write_text(
                "import json, os\n"
                "if os.environ.get('CODEX_SERVER_GROUP_KILL_HOOK'):\n"
                "    _killpg = os.killpg\n"
                "    def _record_kill(group, sig):\n"
                f"        open({str(kill_record)!r}, 'w').write(json.dumps({{'pid': os.getpid(), 'pgid': os.getpgid(0), 'target': group}}))\n"
                "        return _killpg(group, sig)\n"
                "    os.killpg = _record_kill\n",
                encoding="utf-8")
            child = ("import pathlib,time\np=pathlib.Path(" + repr(str(marker)) + ")\n"
                     "while True:\n p.open('a').write('x')\n time.sleep(.01)\n")
            parent = ("import pathlib,subprocess,sys,time; subprocess.Popen([sys.executable,'-c'," +
                      repr(child) + "]); p=pathlib.Path(" + repr(str(marker)) +
                      "); deadline=time.monotonic()+2\nwhile not p.exists() and time.monotonic()<deadline: time.sleep(.01)")
            environment = os.environ.copy()
            environment["CODEX_SERVER_GROUP_KILL_HOOK"] = str(kill_record)
            environment["PYTHONPATH"] = os.pathsep.join(
                (temp, environment.get("PYTHONPATH", "")))
            with mock.patch.object(RUNNER, "_supports_waitid_nowait", return_value=False):
                returncode, error = RUNNER.run_process(
                    [sys.executable, "-c", parent], ROOT, 3, environment)
            self.assertEqual(returncode, 0)
            self.assertIsNone(error)
            self.assertTrue(marker.exists())
            size = marker.stat().st_size
            time.sleep(.08)
            self.assertEqual(marker.stat().st_size, size,
                             "fallback suite process group retained its grandchild")
            killed_by = json.loads(kill_record.read_text(encoding="utf-8"))
            self.assertEqual(killed_by["pid"], killed_by["pgid"])
            self.assertEqual(killed_by["target"], killed_by["pgid"])
            self.assertNotEqual(killed_by["pid"], os.getpid())

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

    def test_windows_termination_failure_still_closes_job_once(self):
        class ApiFunction:
            def __init__(self, result):
                self.result = result
                self.calls = 0

            def __call__(self, *_args):
                self.calls += 1
                return self.result

        class FakeKernel:
            TerminateJobObject = ApiFunction(0)
            CloseHandle = ApiFunction(1)

        kernel = FakeKernel()
        with mock.patch.object(RUNNER, "_windows_kernel32", return_value=kernel):
            RUNNER._close_windows_job(456)
        self.assertEqual(kernel.TerminateJobObject.calls, 1)
        self.assertEqual(kernel.CloseHandle.calls, 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
