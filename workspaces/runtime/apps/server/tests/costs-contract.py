#!/usr/bin/env python3
"""Free tests of the local scanner bridge, no user logs or model requests."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import copy
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_costs import (
    CLAUDE_COST_HISTORY_DAYS,
    CLAUDE_COST_REFRESH_INTERVAL_SECONDS,
    COST_REPORT_HISTORY_DAYS,
    COST_REFRESH_INTERVAL_SECONDS,
    COST_SCAN_OUTPUT_LIMIT_BYTES,
    COST_SCAN_TIMEOUT_SECONDS,
    COST_SCANNER_BUILD_TIMEOUT_SECONDS,
    CostReader,
    normalize,
)
from server_test_waits import wait_for_condition


def report(day=None):
    day = day or time.strftime("%Y-%m-%d")
    return [
        {
            "provider": "codex",
            "source": "local",
            "currencyCode": "USD",
            "sessionCostUSD": 12.5,
            "last30DaysCostUSD": 100.25,
            "sessionTokens": 1000,
            "last30DaysTokens": 4000,
            "historyCoverageIsEstablished": True,
            "coverage": {"priced": 2, "unpriced": 0},
            "daily": [
                {
                    "date": day,
                    "modelsUsed": ["known"],
                    "modelBreakdowns": [
                        {"modelName": "known", "cost": 12.5, "totalTokens": 1000}
                    ],
                }
            ],
        }
    ]


def wait(reader):
    wait_for_condition(lambda: not reader.busy, 'cost reader completion')
    return reader.snapshot()


class CostsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.payload = self.root / "payload.json"
        self.payload.write_text(json.dumps(report()))
        self.cli = self.root / "cost-scanner"
        self.cli.write_text(
            f'#!{sys.executable}\nimport pathlib,sys,time\np=pathlib.Path({str(self.root)!r})\nassert sys.argv[1:]==[]\nwith (p/"calls").open("a") as f: f.write("call\\n")\nif (p/"delay").exists(): time.sleep(3)\nprint((p/"payload.json").read_text())\n'
        )
        self.cli.chmod(0o700)
        self.reader = CostReader(self.root, command=lambda: [str(self.cli)])

    def tearDown(self):
        self.reader.close()
        self.tmp.cleanup()

    def test_rust_scanner_bounds_match_named_python_owners(self):
        rust_source = (
            REPOSITORY_ROOT
            / "workspaces/runtime/packages/runtime-services/src/usage.rs"
        ).read_text()
        swift_source = (
            SERVER_SOURCE_ROOT / "cost-scanner/main.swift"
        ).read_text()
        build_source = (
            SERVER_SOURCE_ROOT / "cost-scanner/build.py"
        ).read_text()
        bounds = {
            "COST_REFRESH_SECONDS": (
                COST_REFRESH_INTERVAL_SECONDS,
                "120.0",
                120,
            ),
            "CLAUDE_COST_REFRESH_SECONDS": (
                CLAUDE_COST_REFRESH_INTERVAL_SECONDS,
                "300.0",
                300,
            ),
            "CLAUDE_COST_HISTORY_DAYS": (
                CLAUDE_COST_HISTORY_DAYS,
                "30",
                30,
            ),
            "COST_REPORT_HISTORY_DAYS": (
                COST_REPORT_HISTORY_DAYS,
                "30",
                30,
            ),
            "COST_SCAN_TIMEOUT_SECONDS": (
                COST_SCAN_TIMEOUT_SECONDS,
                "Duration::from_secs(900)",
                900,
            ),
            "COST_SCANNER_BUILD_TIMEOUT_SECONDS": (
                COST_SCANNER_BUILD_TIMEOUT_SECONDS,
                "Duration::from_secs(120)",
                120,
            ),
            "COST_SCAN_OUTPUT_LIMIT_BYTES": (
                COST_SCAN_OUTPUT_LIMIT_BYTES,
                "32 * 1024 * 1024",
                32 * 1024 * 1024,
            ),
            "COST_SCANNER_BUILD_STDERR_LIMIT_BYTES": (
                None,
                "64 * 1024",
                64 * 1024,
            ),
            "COST_COMMAND_TERMINATION_GRACE_SECONDS": (
                5,
                "Duration::from_secs(5)",
                5,
            ),
            "COST_COMMAND_TERMINATION_HEADROOM_SECONDS": (
                None,
                "Duration::from_secs(2)",
                2,
            ),
            "COST_SCAN_LOCK_RETRY_INTERVAL_MILLISECONDS": (
                None,
                "50",
                50,
            ),
            "COST_COMMAND_POLL_INTERVAL_MILLISECONDS": (
                None,
                "10",
                10,
            ),
        }
        for name, (python_value, rust_value, expected) in bounds.items():
            match = re.search(
                rf"const\s+{name}\s*:[^=]+?=\s*([^;]+);", rust_source
            )
            with self.subTest(name=name):
                self.assertIsNotNone(match, f"Rust constant {name} is missing")
                if python_value is not None:
                    self.assertEqual(python_value, expected)
                self.assertEqual(match.group(1).strip(), rust_value)
        swift_days = re.search(
            r"private\s+let\s+costReportHistoryDays\s*=\s*(\d+)", swift_source
        )
        self.assertIsNotNone(swift_days, "Swift scanner history bound is missing")
        self.assertEqual(int(swift_days.group(1)), COST_REPORT_HISTORY_DAYS)
        for name, expected in {
            "PROCESS_TERMINATION_GRACE_SECONDS": 5,
            "SCANNER_COMPILER_TIMEOUT_SECONDS": 110,
        }.items():
            match = re.search(rf"^{name}\s*=\s*(\d+)\s*$", build_source, re.M)
            with self.subTest(build_bound=name):
                self.assertIsNotNone(match, f"{name} must own its named seconds bound")
                self.assertEqual(int(match.group(1)), expected)
        compiler_timeout = re.search(
            r"^SCANNER_COMPILER_TIMEOUT_SECONDS\s*=\s*(\d+)\s*$",
            build_source,
            re.M,
        )
        self.assertLess(
            int(compiler_timeout.group(1)),
            COST_SCANNER_BUILD_TIMEOUT_SECONDS,
            "The scanner compiler timeout must remain inside the outer build timeout",
        )

    @unittest.skipUnless(
        os.name == "posix", "the cost scanner helper requires POSIX signals"
    )
    def test_build_helper_forwards_shutdown_to_the_swiftc_session(self):
        build_script = SERVER_SOURCE_ROOT / "cost-scanner/build.py"
        compiler_pid_path = self.root / "compiler.pid"
        signal_received_path = self.root / "compiler-signal-received"
        fake_compiler = self.root / "fake-swiftc"
        fake_compiler.write_text(
            f"#!{sys.executable}\n"
            "import os,pathlib,signal,sys,time\n"
            "marker=pathlib.Path(os.environ['FAKE_COMPILER_PID'])\n"
            "temporary=marker.with_suffix('.tmp')\n"
            "temporary.write_text(str(os.getpid()))\n"
            "os.replace(temporary,marker)\n"
            "def received_signal(_signum,_frame):\n"
            " signal_marker=pathlib.Path(os.environ['FAKE_COMPILER_SIGNAL'])\n"
            " temporary=signal_marker.with_suffix('.tmp')\n"
            " temporary.write_text('received')\n"
            " os.replace(temporary,signal_marker)\n"
            " raise SystemExit(0)\n"
            "signal.signal(signal.SIGTERM,received_signal)\n"
            "time.sleep(60)\n"
        )
        fake_compiler.chmod(0o700)
        output = self.root / "scanner"
        helper_code = (
            "import importlib.util,pathlib,sys; "
            "spec=importlib.util.spec_from_file_location('cost_scanner_build',sys.argv[1]); "
            "module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module); "
            "module.compile_binary(pathlib.Path(sys.argv[2]),[],sys.argv[3])"
        )
        helper = subprocess.Popen(
            [
                sys.executable,
                "-c",
                helper_code,
                str(build_script),
                str(output),
                str(fake_compiler),
            ],
            env={
                **os.environ,
                "FAKE_COMPILER_PID": str(compiler_pid_path),
                "FAKE_COMPILER_SIGNAL": str(signal_received_path),
            },
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        compiler_pid = None
        compiler_reaped = False
        try:
            wait_for_condition(
                lambda: compiler_pid_path.read_text().strip()
                if compiler_pid_path.is_file()
                and compiler_pid_path.read_text().strip().isdigit()
                else None,
                "fake swiftc PID publication",
            )
            compiler_pid = int(compiler_pid_path.read_text())
            helper.send_signal(signal.SIGTERM)
            self.assertEqual(helper.wait(timeout=10), 128 + signal.SIGTERM)
            helper.communicate()

            def compiler_is_gone():
                try:
                    os.kill(compiler_pid, 0)
                except ProcessLookupError:
                    return True
                return False

            wait_for_condition(compiler_is_gone, "swiftc process reaping")
            compiler_reaped = True
            self.assertTrue(
                signal_received_path.is_file(),
                "SIGTERM must reach the detached compiler before the timeout kill",
            )
            self.assertFalse(output.exists())
        finally:
            if helper.poll() is None:
                helper.kill()
                helper.communicate()
            if compiler_pid is not None and not compiler_reaped:
                try:
                    os.kill(compiler_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_estimate_is_not_bill_and_does_not_add_token_categories(self):
        row = report()
        row[0]["totals"] = {
            "inputTokens": 100000,
            "outputTokens": 200,
            "cacheReadTokens": 90000,
        }
        value = normalize(row)
        self.assertEqual(value["todayUSD"], 12.5)
        self.assertEqual(value["last30DaysUSD"], 100.25)
        self.assertIsNone(value["billedUSD"])
        self.assertEqual(value["coverage"], "reported")

    def test_unknown_models_and_zeros_are_not_free(self):
        row = report()
        row[0]["sessionCostUSD"] = row[0]["last30DaysCostUSD"] = 0
        row[0]["daily"][0]["modelBreakdowns"][0]["cost"] = 0
        value = normalize(row)
        self.assertIsNone(value["todayUSD"])
        self.assertEqual(value["coverage"], "partial")
        self.assertEqual(value["unknownModels"], ["known"])

    def test_missing_coverage_is_rejected_and_incomplete_history_is_explicit(self):
        for missing in ("coverage", "historyCoverageIsEstablished"):
            row = report()
            del row[0][missing]
            with self.assertRaisesRegex(ValueError, "invalid coverage"):
                normalize(row)
        row = report()
        row[0]["historyCoverageIsEstablished"] = False
        self.assertEqual(normalize(row)["coverage"], "partial")

    def test_scanner_command_is_required_and_cannot_fall_back(self):
        with self.assertRaisesRegex(ValueError, "command is required"):
            CostReader(self.root, command=None)
        self.reader.command = lambda: []
        with patch("subprocess.Popen") as start:
            self.reader._refresh()
        start.assert_not_called()
        self.assertIn("command is invalid", self.reader.state["error"])

    def test_refresh_notifies_after_state_is_persisted_and_unlocked(self):
        observations = []
        reader = None

        def on_change():
            acquired = reader.lock.acquire(blocking=False)
            observations.append((acquired, reader.path.exists(), reader.busy))
            if acquired:
                reader.lock.release()

        reader = CostReader(self.root / "published", command=lambda: [str(self.cli)], on_change=on_change)
        try:
            reader._refresh()
            self.assertEqual(observations, [(True, True, False)])
            self.assertIsNone(reader.state["error"])
        finally:
            reader.close()

    def test_missing_invalid_and_cross_provider_data(self):
        for value in (
            None,
            {},
            [{"provider": "claude"}],
            [{"provider": "codex", "source": "web"}],
        ):
            with self.assertRaises(ValueError):
                normalize(value)
        row = report()
        row[0]["sessionCostUSD"] = float("nan")
        self.assertIsNone(normalize(row)["todayUSD"])
        row[0]["last30DaysCostUSD"] = True
        self.assertIsNone(normalize(row)["last30DaysUSD"])

    def test_concurrent_reads_reuse_scan_and_durable_snapshot(self):
        (self.root / "delay").touch()
        start = time.monotonic()
        for _ in range(50):
            self.reader.snapshot()
        self.assertLess(time.monotonic() - start, 0.5)
        value = wait(self.reader)
        self.assertEqual((self.root / "calls").read_text().splitlines(), ["call"])
        self.assertEqual(value["data"]["todayUSD"], 12.5)
        other = CostReader(self.root, command=lambda: [str(self.cli)])
        self.assertEqual(other.snapshot()["data"], value["data"])
        self.assertFalse(other.busy)
        self.assertEqual((self.root / "local-costs.json").stat().st_mode & 0o777, 0o600)
        other.close()

    def test_changed_report_updates_after_one_request_interval(self):
        self.reader.snapshot()
        first = wait(self.reader)
        changed = report()
        changed[0]["sessionCostUSD"] = 13.75
        changed[0]["last30DaysCostUSD"] = 101.5
        changed[0]["daily"][0]["modelBreakdowns"][0]["cost"] = 13.75
        changed[0]["updatedAt"] = first["data"]["sourceUpdatedAt"]
        self.payload.write_text(json.dumps(changed))
        self.reader.state["checkedAt"] = 1
        self.reader.attempt = 0
        self.reader.snapshot()
        refreshed = wait(self.reader)
        self.assertEqual(refreshed["data"]["todayUSD"], 13.75)
        self.assertEqual(refreshed["data"]["last30DaysUSD"], 101.5)
        self.assertEqual((self.root / "calls").read_text().splitlines(), ["call", "call"])

    def test_source_timestamp_only_change_does_not_claim_new_report(self):
        self.reader.snapshot()
        first = wait(self.reader)
        changed = report()
        changed[0]["updatedAt"] = "2099-01-01T00:00:00Z"
        self.payload.write_text(json.dumps(changed))
        self.reader.state["checkedAt"] = 1
        self.reader.attempt = 0
        self.reader.snapshot()
        refreshed = wait(self.reader)
        self.assertEqual(refreshed["at"], first["at"])
        self.assertEqual(
            refreshed["data"]["sourceUpdatedAt"], first["data"]["sourceUpdatedAt"]
        )

    def test_repeated_report_does_not_accumulate_and_error_retains_previous(self):
        self.reader.snapshot()
        value = wait(self.reader)
        self.reader.state["checkedAt"] = 1
        self.reader.attempt = 0
        self.reader.snapshot()
        repeated = wait(self.reader)
        self.assertEqual(repeated["data"], value["data"])
        self.payload.write_text("bad json")
        reported_at = repeated["at"]
        self.reader.state["checkedAt"] = 1
        self.reader.attempt = 0
        self.reader.snapshot()
        failed = wait(self.reader)
        self.assertEqual(failed["data"], value["data"])
        self.assertEqual(failed["at"], reported_at)
        self.assertEqual(
            failed["data"]["sourceUpdatedAt"], value["data"]["sourceUpdatedAt"]
        )
        self.assertTrue(failed["error"])
        self.assertTrue(failed["stale"])

    def test_day_rollover_does_not_label_yesterday_as_today(self):
        day = time.strftime("%Y-%m-%d")
        midnight = time.mktime(time.strptime(day, "%Y-%m-%d"))
        clock = [midnight + 12 * 3600]
        self.payload.write_text(json.dumps(report(day=day)))
        reader = CostReader(self.root, command=lambda: [str(self.cli)], clock=lambda: clock[0])
        try:
            reader.snapshot()
            first = wait(reader)
            self.assertEqual(first["data"]["todayUSD"], 12.5)
            clock[0] = midnight + 36 * 3600
            reader.state["checkedAt"] = 1
            reader.attempt = 0
            before = reader.snapshot()
            self.assertIsNone(before["data"]["todayUSD"])
            self.assertEqual(before["data"]["last30DaysUSD"], 100.25)
            self.assertTrue(before["stale"])
            after = wait(reader)
            self.assertIsNone(after["data"]["todayUSD"])
            self.assertEqual(after["data"]["last30DaysUSD"], 100.25)
        finally:
            reader.close()

    def test_default_bound_lets_a_large_first_scan_finish(self):
        # A 25 GB profile needed 213 s for its first scan; the old 90 s bound killed it every time.
        self.assertGreaterEqual(self.reader.timeout, 600)

    def test_timeout_ends_only_owned_scanner(self):
        (self.root / "delay").touch()
        self.reader.timeout = 0.05
        self.reader.snapshot()
        value = wait(self.reader)
        self.assertIn("timed out", value["error"])
        self.assertIsNone(value["data"])
        self.assertIsNone(self.reader.process)

    def test_scope_change_does_not_reuse_totals(self):
        self.reader.snapshot()
        wait(self.reader)
        with patch.dict(os.environ, {"CODEX_HOME": str(self.root / "another-home")}):
            other = CostReader(self.root, command=lambda: [str(self.cli)])
            self.assertIsNone(other.state["data"])
            other.close()


if __name__ == "__main__":
    unittest.main()
