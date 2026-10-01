#!/usr/bin/env python3
"""Advisory CLI version checks stay nonblocking and account scoped."""
from pathlib import Path
import sys
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_provider_versions import ProviderVersionMonitor, parse_version, version_warning


class Accounts:
    def get(self, key):
        return {"provider": "claude" if key == "claude" else "codex"}


class Runtime:
    def __init__(self):
        self.lock = threading.RLock()
        self.servers = {"claude": object(), "codex": object()}
        self.accounts = Accounts()
        self.changed = threading.Event()


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

    def test_version_detection_runs_off_thread_and_returns_scoped_advisories(self):
        runtime = Runtime()
        started = threading.Event()
        release = threading.Event()

        def read(provider, _server, _account):
            started.set()
            release.wait(2)
            return "0.153.3" if provider == "codex" else "2.1.277"

        monitor = ProviderVersionMonitor(read_version=read)
        before = time.monotonic()
        monitor.tick(runtime)
        self.assertLess(time.monotonic() - before, 0.5)
        self.assertTrue(started.wait(1))
        release.set()
        monitor.worker.join(2)
        self.assertFalse(monitor.worker.is_alive())
        self.assertTrue(runtime.changed.is_set())
        warnings = monitor.status()["warnings"]
        self.assertEqual({item["accountKey"] for item in warnings}, {"claude", "codex"})
        self.assertTrue(all(item["id"].startswith("provider-version:") for item in warnings))

    def test_failed_or_current_detection_does_not_warn_or_block_use(self):
        runtime = Runtime()
        monitor = ProviderVersionMonitor(
            read_version=lambda provider, _server, _account:
            "codex-cli 0.153.4" if provider == "codex" else None,
        )
        monitor.tick(runtime)
        monitor.worker.join(2)
        self.assertEqual(monitor.status()["warnings"], [])

    def test_unreadable_version_does_not_hide_previous_advisory(self):
        runtime = Runtime()
        monitor = ProviderVersionMonitor(
            read_version=lambda provider, _server, _account:
            "0.153.3" if provider == "codex" else "2.1.277",
        )
        monitor.tick(runtime)
        monitor.worker.join(2)
        runtime.servers = {"codex": runtime.servers["codex"]}
        monitor.read_version = lambda _provider, _server, _account: None
        monitor.tick(runtime)
        monitor.worker.join(2)
        self.assertEqual([item["accountKey"] for item in monitor.status()["warnings"]], ["codex"])


if __name__ == "__main__":
    unittest.main()
