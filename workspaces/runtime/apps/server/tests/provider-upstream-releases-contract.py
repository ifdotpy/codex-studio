#!/usr/bin/env python3
"""Upstream provider release checks are cached, conditional, and offline in tests."""
# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import json
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

from codex_provider_update_notice import compare_versions
from codex_provider_upstream_releases import (
    CHECK_INTERVAL_SECONDS,
    PACKAGES,
    PREFERENCE_KEY,
    UpstreamReleaseChecker,
    _updates_enabled,
)
from codex_sync_entities import ensure_tables
from studio_api.sync.preferences import PreferenceField, PreferencePushRequest, merge_preferences


class FakeRegistry:
    def __init__(self, versions=None, etags=None):
        self.versions = versions or {}
        self.etags = etags or {}
        self.calls = []
        self.responses = {}

    def __call__(self, url, etag, timeout):
        provider = next(
            name for name, (_package, package_url) in PACKAGES.items() if package_url == url
        )
        self.calls.append((provider, etag, timeout))
        response = self.responses.get(provider)
        if response:
            return response
        body = json.dumps({"dist-tags": {"latest": self.versions.get(provider, "2.0.0")}}).encode()
        return 200, {"ETag": self.etags.get(provider, f'"{provider}-etag"')}, body


class ProviderUpstreamReleaseContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "release-cache.json"
        self.now = [1_800_000_000.0]
        network_guard = patch(
            "codex_provider_upstream_releases.open_request",
            side_effect=AssertionError("test attempted a real network request"),
        )
        network_guard.start()
        self.addCleanup(network_guard.stop)

    def checker(self, request):
        return UpstreamReleaseChecker(self.path, request=request, clock=lambda: self.now[0])

    def test_latest_tag_comparison_handles_both_provider_packages(self):
        for provider in PACKAGES:
            registry = FakeRegistry({provider: "2.0.0"})
            checker = self.checker(registry)
            latest = checker.check()[provider]
            self.assertEqual(latest, "2.0.0", provider)
            self.assertEqual(
                compare_versions("1.9.0", latest, source="upstream").model_dump(by_alias=True),
                {"availableVersion": "2.0.0", "source": "upstream"},
            )
            self.assertIsNone(compare_versions("2.0.0", latest, source="upstream"))
            self.assertIsNone(compare_versions("2.1.0", latest, source="upstream"))
            self.assertIsNone(compare_versions(None, latest, source="upstream"))
            self.assertIsNone(compare_versions("not a version", latest, source="upstream"))

    def test_prerelease_ordering_for_both_packages(self):
        for provider in PACKAGES:
            registry = FakeRegistry({provider: "2.0.0-beta.2"})
            latest = self.checker(registry).check()[provider]
            self.assertIsNotNone(
                compare_versions("2.0.0-beta.1", latest, source="upstream")
            )
            self.assertIsNone(compare_versions("2.0.0", latest, source="upstream"))

    def test_unparsable_latest_is_not_cached_or_noticed_for_both_packages(self):
        for provider in PACKAGES:
            registry = FakeRegistry({name: "latest-unknown" for name in PACKAGES})
            self.assertNotIn(provider, self.checker(registry).check())
            cached = json.loads(self.path.read_text())
            package, _url = PACKAGES[provider]
            self.assertNotIn("version", cached["packages"][package])

    def test_second_check_within_a_day_uses_cache_without_a_request(self):
        registry = FakeRegistry({"claude": "2.2.0", "codex": "0.162.2"})
        self.assertEqual(set(self.checker(registry).check()), set(PACKAGES))
        self.assertEqual(len(registry.calls), 2)
        offline = self.checker(lambda *_args: self.fail("registry request was repeated"))
        self.now[0] += CHECK_INTERVAL_SECONDS - 1
        self.assertEqual(offline.check(), {"claude": "2.2.0", "codex": "0.162.2"})

    def test_etag_is_conditional_and_http_304_refreshes_the_cached_version(self):
        registry = FakeRegistry({"claude": "2.2.0", "codex": "0.162.2"})
        first = self.checker(registry)
        first.check()
        self.now[0] += CHECK_INTERVAL_SECONDS
        registry.responses = {
            provider: (304, {"ETag": f'"{provider}-etag"'}, b"")
            for provider in PACKAGES
        }
        self.assertEqual(first.check(), {"claude": "2.2.0", "codex": "0.162.2"})
        self.assertEqual([call[1] for call in registry.calls[-2:]], [
            '"claude-etag"', '"codex-etag"',
        ])

    def test_failure_and_timeout_preserve_cache_and_emit_nothing(self):
        from codex_provider_versions import _diagnostic

        registry = FakeRegistry({"claude": "2.2.0", "codex": "0.162.2"})
        checker = self.checker(registry)
        checker.check()
        before = json.loads(self.path.read_text())
        self.now[0] += CHECK_INTERVAL_SECONDS

        def timeout(*_args):
            raise TimeoutError("fixture timeout")

        failed = self.checker(timeout)
        with patch("codex_provider_upstream_releases.open_request", side_effect=AssertionError("real network used")):
            unavailable = failed.check()
        self.assertEqual(unavailable, {})
        self.assertEqual(
            _diagnostic("work", "codex", {
                "runningVersion": "0.162.0",
                "installedVersion": "0.162.0",
                "upstreamVersion": unavailable.get("codex"),
            })["updateNotices"],
            [],
        )
        after = json.loads(self.path.read_text())
        for provider, (package, _url) in PACKAGES.items():
            self.assertEqual(after["packages"][package]["version"], before["packages"][package]["version"])
            self.assertEqual(after["packages"][package]["etag"], before["packages"][package]["etag"])
            self.assertEqual(after["packages"][package]["fetchedAt"], before["packages"][package]["fetchedAt"])
            self.assertEqual(after["packages"][package]["lastAttemptAt"], self.now[0])

    def test_http_error_preserves_cache_and_emits_nothing(self):
        registry = FakeRegistry({"claude": "2.2.0", "codex": "0.162.2"})
        checker = self.checker(registry)
        checker.check()
        before = json.loads(self.path.read_text())
        self.now[0] += CHECK_INTERVAL_SECONDS
        registry.responses = {provider: (503, {}, b"unavailable") for provider in PACKAGES}
        self.assertEqual(checker.check(), {})
        after = json.loads(self.path.read_text())
        for package, _url in PACKAGES.values():
            self.assertEqual(after["packages"][package]["version"], before["packages"][package]["version"])
            self.assertEqual(after["packages"][package]["etag"], before["packages"][package]["etag"])

    def test_opt_out_does_not_request_registry(self):
        request = lambda *_args: self.fail("registry request made while opted out")
        with patch("codex_provider_upstream_releases.open_request", side_effect=AssertionError("real network used")):
            self.assertEqual(self.checker(request).check(enabled=False), {})

    def test_registry_request_contract_has_no_identifying_headers_or_auth(self):
        import codex_provider_upstream_releases as upstream

        captured = []

        class Response:
            status = 200
            headers = {"ETag": '"fixture"'}

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return None

            def read(self):
                return b'{"dist-tags":{"latest":"2.0.0"}}'

        def open_request(request, *, timeout):
            captured.append((request, timeout))
            return Response()

        with patch.object(upstream, "open_request", open_request):
            status, headers, body = upstream._registry_request(PACKAGES["claude"][1], None, 5)
        self.assertEqual(status, 200)
        self.assertEqual(headers["ETag"], '"fixture"')
        self.assertIn(b"latest", body)
        request, timeout = captured[0]
        self.assertEqual(timeout, 5)
        self.assertEqual(request.get_header("Accept"), "application/vnd.npm.install-v1+json")
        self.assertNotIn("Authorization", request.headers)
        self.assertNotIn("Cookie", request.headers)
        self.assertNotIn("X-Canvas-Token", request.headers)

    def test_preference_defaults_on_and_opt_out_is_read_from_existing_user_preferences(self):
        with sqlite3.connect(":memory:") as db:
            ensure_tables(db)
            class Runtime:
                @contextmanager
                def read_db(self):
                    yield db

            runtime = Runtime()
            self.assertTrue(_updates_enabled(runtime))
            body = PreferencePushRequest(scope="user", fields={
                PREFERENCE_KEY: PreferenceField(value=False, timestamp=1, writer="test"),
            })
            merge_preferences(db, body)
            self.assertFalse(_updates_enabled(runtime))


if __name__ == "__main__":
    unittest.main()
