#!/usr/bin/env python3
"""Provider version diagnostics are explicit, nonblocking, and connection scoped."""
from pathlib import Path
import sys
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_provider_versions import (
    ProviderVersionMonitor,
    _claude_version,
    _diagnostic,
    parse_version,
    version_warning,
)


class Accounts:
    def get(self, key):
        return {"provider": "claude" if key == "claude" else "codex"}


class Server:
    def __init__(self, provider="codex", options=None):
        self.provider = provider
        self.provider_options = options or {}


class Runtime:
    def __init__(self):
        self.lock = threading.RLock()
        self.start_lock = threading.RLock()
        self.servers = {"claude": Server("claude"), "codex": Server()}
        self.connection_ids = {"claude": "claude-1", "codex": "codex-1"}
        self.offline_accounts = set()
        self.accounts = Accounts()
        self.changed = threading.Event()


class SnapshotLock:
    """Reentrant runtime lock that exposes the publisher's acquisition point."""
    def __init__(self, publisher_waiting):
        self.lock = threading.RLock()
        self.publisher_waiting = publisher_waiting

    def __enter__(self):
        if threading.current_thread().name == "provider-publisher":
            self.publisher_waiting.set()
        self.lock.acquire()
        return self

    def __exit__(self, *_args):
        self.lock.release()


class ProviderVersionContract(unittest.TestCase):
    def test_baseline_means_lowest_version_recorded_in_repo_evidence(self):
        self.assertIsNone(version_warning("codex", "codex-cli 0.153.4"))
        self.assertIsNone(version_warning("claude", "Claude Code 2.1.278"))
        self.assertIn("lowest recorded tested version", version_warning("codex", "0.153.3")["message"])
        self.assertIn("continue at your own risk", version_warning("claude", "2.1.277")["message"])

    def test_semver_and_unknown_versions(self):
        self.assertLess(parse_version("0.153.4-alpha.2"), parse_version("0.153.4"))
        self.assertIsNone(parse_version("version unavailable"))
        self.assertIsNone(version_warning("codex", "version unavailable"))
        self.assertIsNone(version_warning("future-provider", "0.1.0"))

    def test_claude_inspects_launched_options_and_current_profile_separately(self):
        server = Server("claude", {"binaryPath": "/launched/claude"})
        account = {"provider": "claude", "claudeOptions": {"binaryPath": "/profile/claude"}}
        with patch("codex_provider_versions._read_claude_executable",
                   side_effect=[("Claude Code 2.1.277", None), ("Claude Code 2.2.0", None)]) as read:
            versions = _claude_version(server, account)
        self.assertEqual(read.call_args_list[0].args[0], {
            "claudeOptions": {"binaryPath": "/launched/claude"},
        })
        self.assertIs(read.call_args_list[1].args[0], account)
        self.assertIsNone(versions["runningVersion"])
        self.assertEqual(versions["installedVersion"], "Claude Code 2.1.277")
        self.assertEqual(versions["configuredVersion"], "Claude Code 2.2.0")
        diagnostic = _diagnostic("work", "claude", versions)
        self.assertEqual(diagnostic["status"], "outdated")
        self.assertIn("exact version already running may be different", diagnostic["message"])

    def test_unknown_and_failed_running_versions_are_visible(self):
        unknown = _diagnostic("work", "claude", {
            "runningVersion": None,
            "installedVersion": "Claude Code 2.2.0",
            "configuredVersion": "Claude Code 2.2.0",
            "runningNote": "The exact version already running in Claude sessions is unknown.",
        })
        self.assertEqual(unknown["status"], "unknown")
        self.assertIsNone(unknown["runningVersion"])
        self.assertIn("running in Claude sessions is unknown", unknown["message"])
        failed = _diagnostic("work", "claude", {"error": "Could not read version"})
        self.assertEqual(failed["status"], "error")
        self.assertEqual(failed["error"], "Could not read version")
        undetermined = _diagnostic("work", "claude", {})
        self.assertEqual(undetermined["status"], "unknown")
        self.assertIn("Could not determine", undetermined["message"])

    def test_reader_failure_is_reported_in_monitor_status(self):
        runtime = Runtime()

        def fail(*_args):
            raise OSError("read failed")

        monitor = ProviderVersionMonitor(read_version=fail)
        monitor.tick(runtime)
        monitor.worker.join(2)
        entries = monitor.status()["providers"]
        self.assertEqual({entry["status"] for entry in entries}, {"error"})
        self.assertTrue(all(entry["error"] for entry in entries))

    def test_missing_reader_status_is_reported_as_error(self):
        runtime = Runtime()
        monitor = ProviderVersionMonitor(read_version=lambda *_args: None)
        monitor.tick(runtime)
        monitor.worker.join(2)
        entries = monitor.status()["providers"]
        self.assertEqual({entry["status"] for entry in entries}, {"error"})
        self.assertTrue(all(entry["error"] for entry in entries))

    def test_snapshot_can_read_status_while_publisher_waits_for_runtime_lock(self):
        runtime = Runtime()
        runtime.servers = {}
        runtime.connection_ids = {}
        publisher_waiting = threading.Event()
        runtime.lock = SnapshotLock(publisher_waiting)
        monitor = ProviderVersionMonitor()
        monitor.signature = ()
        monitor.next_check = time.monotonic() + 60
        runtime.provider_version_monitor = monitor
        publisher = threading.Thread(
            name="provider-publisher",
            target=monitor._publish,
            args=(runtime, (), monitor.generation, []),
        )

        with runtime.lock:
            publisher.start()
            self.assertTrue(publisher_waiting.wait(1))
            monitor_lock_was_available = monitor.lock.acquire(blocking=False)
            if monitor_lock_was_available:
                monitor.lock.release()
            # This mirrors Runtime.snapshot: runtime.lock is held while status
            # calls tick() and then reads the monitor's status. Avoid blocking
            # in a broken lock order so a regression fails instead of hanging.
            if monitor_lock_was_available:
                monitor.tick(runtime)
                status = monitor.status()
            else:
                status = None

        publisher.join(2)
        self.assertFalse(publisher.is_alive())
        self.assertTrue(monitor_lock_was_available)
        self.assertEqual(status["providers"], [])
        self.assertTrue(runtime.changed.is_set())

    def test_version_detection_runs_off_thread_and_returns_scoped_advisories(self):
        runtime = Runtime()
        started = threading.Event()
        release = threading.Event()

        def read(provider, _server, _account):
            started.set()
            release.wait(2)
            return {"runningVersion": "0.153.3", "installedVersion": "0.153.3"} \
                if provider == "codex" else {
                    "runningVersion": None,
                    "installedVersion": "Claude Code 2.1.277",
                    "runningNote": "Running version unknown.",
                }

        monitor = ProviderVersionMonitor(read_version=read)
        before = time.monotonic()
        monitor.tick(runtime)
        self.assertLess(time.monotonic() - before, 0.5)
        self.assertTrue(started.wait(1))
        release.set()
        monitor.worker.join(2)
        self.assertFalse(monitor.worker.is_alive())
        self.assertTrue(runtime.changed.is_set())
        status = monitor.status()
        self.assertEqual({row["accountKey"] for row in status["providers"]}, {"claude", "codex"})
        self.assertEqual({row["accountKey"] for row in status["warnings"]}, {"claude", "codex"})

    def test_connection_replacement_discards_stale_result_and_warning(self):
        runtime = Runtime()
        old = Server()
        runtime.servers = {"default": old}
        runtime.connection_ids = {"default": "old-connection"}
        started = threading.Event()
        release = threading.Event()

        def read(_provider, server, _account):
            if server is old:
                started.set()
                release.wait(2)
                return {"runningVersion": "0.153.3", "installedVersion": "0.153.3"}
            return {"runningVersion": "0.153.4", "installedVersion": "0.153.4"}

        monitor = ProviderVersionMonitor(read_version=read)
        monitor.tick(runtime)
        old_worker = monitor.worker
        self.assertTrue(started.wait(1))
        with runtime.start_lock, runtime.lock:
            runtime.servers["default"] = Server()
            runtime.connection_ids["default"] = "new-connection"
        monitor.tick(runtime)
        self.assertEqual(monitor.status()["warnings"], [])
        self.assertEqual(monitor.status()["providers"][0]["status"], "checking")
        release.set()
        old_worker.join(2)
        self.assertEqual(monitor.status()["warnings"], [])
        monitor.tick(runtime)
        monitor.worker.join(2)
        self.assertEqual(monitor.status()["warnings"], [])
        self.assertEqual(monitor.status()["providers"][0]["runningVersion"], "0.153.4")

    def test_unrecognized_provider_has_explicit_unknown_status(self):
        runtime = Runtime()
        runtime.servers = {"future": Server("future-provider")}
        runtime.connection_ids = {"future": "future-1"}
        monitor = ProviderVersionMonitor(read_version=lambda *_: None)
        monitor.tick(runtime)
        monitor.worker.join(2)
        entry = monitor.status()["providers"][0]
        self.assertEqual(entry["status"], "unknown")
        self.assertEqual(entry["provider"], "future-provider")
        self.assertIsNone(entry["baseline"])
        self.assertIn("No evidence-backed", entry["message"])


if __name__ == "__main__":
    unittest.main()
