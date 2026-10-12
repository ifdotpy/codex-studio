#!/usr/bin/env python3
"""Provider version diagnostics are explicit, nonblocking, and connection scoped."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from server_test_waits import HANG_GUARD_SECONDS
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from pathlib import Path
import sys
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_provider_versions import (
    ProviderVersionMonitor,
    _claude_version,
    _diagnostic,
    parse_version,
    version_warning,
)
from codex_provider_update_notice import compare_versions, update_warnings


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
    """Reentrant runtime lock that exposes selected workers' wait points."""
    def __init__(self, waiting):
        self.lock = threading.RLock()
        self.waiting = waiting

    def __enter__(self):
        event = self.waiting.get(threading.current_thread().name)
        if event:
            event.set()
        self.lock.acquire()
        return self

    def __exit__(self, *_args):
        self.lock.release()


class ProviderVersionContract(unittest.TestCase):
    def test_update_notice_requires_a_newer_readable_installed_candidate(self):
        cases = (
            ("newer", "2.0.0", "2.1.0", True),
            ("equal", "2.1.0", "2.1.0", False),
            ("older", "2.2.0", "2.1.0", False),
            ("unparsable", "unknown", "2.2.0", False),
            ("missing installed", None, "2.2.0", False),
            ("missing", "2.1.0", None, False),
        )
        for case, installed, available, expected in cases:
            with self.subTest(case=case):
                notice = compare_versions(installed, available)
                self.assertEqual(notice is not None, expected)
                if notice:
                    self.assertEqual(notice.availableVersion, available)
                    self.assertEqual(notice.source, "installed")

    def test_upstream_comparison_uses_source_discriminator(self):
        notice = compare_versions("0.153.4", "0.155.0", source="upstream")
        self.assertEqual(
            notice.model_dump(by_alias=True),
            {"availableVersion": "0.155.0", "source": "upstream"},
        )

    def test_upstream_notice_requires_a_readable_current_version_for_both_providers(self):
        for provider in ("claude", "codex"):
            for installed in (None, "unknown"):
                diagnostic = _diagnostic("work", provider, {
                    "installedVersion": installed,
                    "upstreamVersion": "2.2.0",
                })
                self.assertEqual(diagnostic["updateNotices"], [])

    def test_upstream_notice_names_the_current_version_for_both_providers(self):
        labels = {"claude": "Claude Code", "codex": "Codex CLI"}
        for provider, version in (("claude", "2.1.295"), ("codex", "0.162.0")):
            diagnostic = _diagnostic("work", provider, {
                "installedVersion": version,
                "upstreamVersion": "9.9.9",
                "at": 1,
            })
            warning = update_warnings([diagnostic], labels)[0]
            self.assertIn("version currently in use", warning["message"])
            self.assertIn(f"Current: {version}", warning["message"])
            self.assertIn("Available: 9.9.9", warning["message"])

    def test_unavailable_codex_candidate_produces_no_notice(self):
        from codex_provider_versions import _codex_version

        with patch("codex_native_runtime._version", return_value="1.0.0"):
            versions = _codex_version("codex", Server(), {}, Runtime())
        self.assertIsNone(versions["availableVersion"])
        self.assertEqual(_diagnostic("work", "codex", versions)["updateNotices"], [])

    def test_selected_codex_candidate_produces_notice(self):
        from codex_provider_versions import _codex_version

        class Updates:
            def selected(self):
                return {"version": "0.154.0"}

        runtime = Runtime()
        runtime.native_runtime_updates = Updates()
        with patch("codex_native_runtime._version", return_value="0.153.4"):
            versions = _codex_version("codex", Server(), {}, runtime)
        self.assertEqual(
            _diagnostic("work", "codex", versions)["updateNotices"],
            [{"availableVersion": "0.154.0", "source": "installed"}],
        )

    def test_monitor_production_reader_path_reports_codex_version_and_notice(self):
        class Updates:
            def selected(self):
                return {"version": "0.154.0"}

        runtime = Runtime()
        runtime.native_runtime_updates = Updates()
        monitor = ProviderVersionMonitor()
        with (
            patch("codex_native_runtime._version", return_value="0.153.4"),
            patch(
                "codex_provider_versions._read_claude_executable",
                return_value=("Claude Code 2.1.278", None),
            ),
        ):
            monitor.tick(runtime)
            monitor.worker.join(HANG_GUARD_SECONDS)

        providers = {row["provider"]: row for row in monitor.status()["providers"]}
        self.assertEqual(providers["codex"]["runningVersion"], "0.153.4")
        self.assertEqual(
            providers["codex"]["updateNotices"],
            [{"availableVersion": "0.154.0", "source": "installed"}],
        )
        self.assertEqual(
            providers["claude"]["installedVersion"], "Claude Code 2.1.278"
        )
        self.assertNotEqual(providers["codex"]["status"], "error")
        self.assertNotEqual(providers["claude"]["status"], "error")

    def test_monitor_does_not_swallow_programming_errors_from_reader(self):
        runtime = Runtime()

        def fail(*_args):
            raise TypeError("reader programming error")

        connected = [
            (key, server, runtime.connection_ids.get(key))
            for key, server in runtime.servers.items()
        ]
        signature = tuple(
            (key, id(server), connection_id)
            for key, server, connection_id in connected
        )
        monitor = ProviderVersionMonitor(read_version=fail)
        with self.assertRaisesRegex(TypeError, "reader programming error"):
            monitor._check(runtime, connected, signature, monitor.generation)

    def test_unreadable_selected_codex_candidate_produces_no_notice(self):
        from codex_provider_versions import _codex_version

        class Updates:
            def selected(self):
                raise OSError("candidate store unavailable")

        runtime = Runtime()
        runtime.native_runtime_updates = Updates()
        with patch("codex_native_runtime._version", return_value="0.153.4"):
            versions = _codex_version("codex", Server(), {}, runtime)
        self.assertEqual(_diagnostic("work", "codex", versions)["updateNotices"], [])

    def test_update_notice_is_projected_as_an_informational_runtime_notice(self):
        monitor = ProviderVersionMonitor()
        monitor.providers = [{
            "id": "provider-version:work",
            "accountKey": "work",
            "provider": "codex",
            "status": "current",
            "runningVersion": "0.153.4",
            "installedVersion": "0.153.4",
            "configuredVersion": None,
            "updateNotices": [{"availableVersion": "0.154.0", "source": "installed"}],
            "baseline": "0.153.4",
            "message": None,
            "at": 1.0,
        }]
        notice = monitor.status()["warnings"][0]
        self.assertEqual(notice["level"], "info")
        self.assertIn("selected for runtime updates", notice["message"])
        self.assertIn("still using 0.153.4", notice["message"])

    def test_installed_and_upstream_notices_are_independent(self):
        diagnostic = _diagnostic("work", "codex", {
            "runningVersion": "0.153.4",
            "installedVersion": "0.153.4",
            "availableVersion": "0.154.0",
            "upstreamVersion": "0.155.0",
        })
        self.assertEqual(diagnostic["updateNotices"], [
            {"availableVersion": "0.154.0", "source": "installed"},
            {"availableVersion": "0.155.0", "source": "upstream"},
        ])
        monitor = ProviderVersionMonitor()
        monitor.providers = [diagnostic]
        notices = monitor.status()["warnings"]
        self.assertEqual(len(notices), 2)
        self.assertIn("selected for runtime updates", notices[0]["message"])
        self.assertIn("available upstream", notices[1]["message"])

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
            versions = _claude_version("claude", server, account, Runtime())
        self.assertEqual(read.call_args_list[0].args[0], {
            "claudeOptions": {"binaryPath": "/launched/claude"},
        })
        self.assertIs(read.call_args_list[1].args[0], account)
        self.assertIsNone(versions["runningVersion"])
        self.assertEqual(versions["installedVersion"], "Claude Code 2.1.277")
        self.assertEqual(versions["configuredVersion"], "Claude Code 2.2.0")
        diagnostic = _diagnostic("work", "claude", versions)
        self.assertEqual(diagnostic["status"], "outdated")
        self.assertEqual(
            diagnostic["updateNotices"],
            [{"availableVersion": "Claude Code 2.2.0", "source": "installed"}],
        )
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
        runtime.lock = SnapshotLock({"provider-publisher": publisher_waiting})
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
            self.assertTrue(publisher_waiting.wait(HANG_GUARD_SECONDS))
            monitor_lock_was_available = monitor.lock.acquire(blocking=False)
            if monitor_lock_was_available:
                monitor.lock.release()
            # runtime.lock is held while the monitor ticks and the test reads
            # its status. Avoid blocking
            # in a broken lock order so a regression fails instead of hanging.
            if monitor_lock_was_available:
                monitor.tick(runtime)
                status = monitor.status()
            else:
                status = None

        publisher.join(HANG_GUARD_SECONDS)
        self.assertFalse(publisher.is_alive())
        self.assertTrue(monitor_lock_was_available)
        self.assertEqual(status["providers"], [])
        self.assertTrue(runtime.changed.is_set())

    def test_send_connect_lock_order_defers_validation_and_publication_then_retries(self):
        runtime = Runtime()
        old_server, new_server = Server(), Server()
        runtime.servers = {"default": old_server}
        runtime.connection_ids = {"default": "old-connection"}
        validator_waiting = threading.Event()
        publisher_waiting = threading.Event()
        runtime.lock = SnapshotLock({
            "provider-validator": validator_waiting,
            "provider-publisher": publisher_waiting,
        })
        old_signature = (("default", id(old_server), "old-connection"),)
        monitor = ProviderVersionMonitor(read_version=lambda *_args: {
            "runningVersion": "0.153.4", "installedVersion": "0.153.4",
        })
        monitor.signature = old_signature
        monitor.connections = {"default": (old_server, "old-connection")}
        monitor.providers = [{
            "id": "provider-version:default", "accountKey": "default", "provider": "codex",
            "status": "outdated", "runningVersion": "0.153.3", "installedVersion": "0.153.3",
            "configuredVersion": None, "baseline": "0.153.4", "error": None,
            "message": "Old connection warning", "at": time.time(),
        }]
        runtime.provider_version_monitor = monitor
        validation = []
        publication = []
        validator = threading.Thread(
            name="provider-validator",
            target=lambda: validation.append(monitor._is_current(runtime, old_signature, monitor.generation)),
        )
        publisher = threading.Thread(
            name="provider-publisher",
            target=lambda: publication.append(monitor._publish(
                runtime, old_signature, monitor.generation, monitor.providers,
            )),
        )

        # Runtime.send owns runtime.lock and then calls connect(), which takes
        # start_lock. Both provider workers must wait for runtime.lock without
        # holding start_lock, so connect remains free to finish its change.
        with runtime.lock:
            validator.start()
            publisher.start()
            self.assertTrue(validator_waiting.wait(HANG_GUARD_SECONDS))
            self.assertTrue(publisher_waiting.wait(HANG_GUARD_SECONDS))
            self.assertTrue(runtime.start_lock.acquire(blocking=False))
            try:
                runtime.servers["default"] = new_server
                runtime.connection_ids["default"] = "new-connection"
            finally:
                runtime.start_lock.release()

        validator.join(HANG_GUARD_SECONDS)
        publisher.join(HANG_GUARD_SECONDS)
        self.assertFalse(validator.is_alive())
        self.assertFalse(publisher.is_alive())
        self.assertEqual(validation, [False])
        self.assertEqual(publication, [False])
        self.assertEqual(monitor.next_check, 0.0)

        monitor.tick(runtime)
        self.assertEqual(monitor.status()["warnings"], [])
        monitor.worker.join(HANG_GUARD_SECONDS)
        self.assertFalse(monitor.worker.is_alive())
        status = monitor.status()
        self.assertEqual(status["providers"][0]["status"], "current")
        self.assertEqual(status["providers"][0]["runningVersion"], "0.153.4")

    def test_version_detection_runs_off_thread_and_returns_scoped_advisories(self):
        runtime = Runtime()
        started = threading.Event()
        release = threading.Event()

        def read(provider, _server, _account, _runtime):
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
        self.assertTrue(started.wait(HANG_GUARD_SECONDS))
        release.set()
        monitor.worker.join(HANG_GUARD_SECONDS)
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

        def read(_provider, server, _account, _runtime):
            if server is old:
                started.set()
                release.wait(2)
                return {"runningVersion": "0.153.3", "installedVersion": "0.153.3"}
            return {"runningVersion": "0.153.4", "installedVersion": "0.153.4"}

        monitor = ProviderVersionMonitor(read_version=read)
        monitor.tick(runtime)
        old_worker = monitor.worker
        self.assertTrue(started.wait(HANG_GUARD_SECONDS))
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
