#!/usr/bin/env python3
"""Auth probes must not hold account or cross-profile cache locks."""

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
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
import uuid

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_accounts
import codex_claude


def metadata(identity="one"):
    return {"status": "ready", "accountId": "claude:" + identity,
            "_credentialIdentity": "claude:" + identity,
            "email": identity, "plan": "max"}


class LockContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.env = patch.dict(os.environ, {"CODEX_HOME": str(self.root / "codex")})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.codex_auth = patch.object(codex_accounts, "auth_metadata", return_value={
            "status": "ready", "accountId": "codex-one",
            "_credentialIdentity": "chatgpt:codex-one"})
        self.codex_auth.start()
        self.addCleanup(self.codex_auth.stop)
        self.store = codex_accounts.AccountStore(self.root / "state")
        self.store.discovered = True
        self.store.data["accounts"]["claude-one"] = {
            "id": "claude-one", "provider": "claude", "home": str(self.root / "claude"),
            "claudeOptions": {"configDir": str(self.root / "claude")},
            "label": "Claude", "source": "Claude Code", **metadata()}
        with codex_claude._lock:
            codex_claude._cache.clear()
        self.threads = []
        self.releases = []
        self.results = {}
        self.errors = []
        self.addCleanup(self.finish_threads)

    def finish_threads(self):
        for event in self.releases:
            event.set()
        for thread in self.threads:
            thread.join(2)
            self.assertFalse(thread.is_alive(), "private fixture thread did not finish")

    def start(self, name, action):
        done = threading.Event()
        def target():
            try:
                self.results[name] = action()
            except BaseException as error:
                self.errors.append(error)
            finally:
                done.set()
        thread = threading.Thread(target=target, name="private-auth-" + name)
        self.threads.append(thread)
        thread.start()
        return done

    def auth_barrier(self):
        entered, release = threading.Event(), threading.Event()
        self.releases.append(release)
        def auth(options=None, force=False):
            entered.set()
            if not release.wait(2):
                raise AssertionError("private auth release missing")
            return metadata()
        return entered, release, auth

    def assert_registry_available(self):
        acquired = self.store.lock.acquire(timeout=.1)
        if acquired:
            self.store.lock.release()
        self.assertTrue(acquired, "Claude auth holds the shared account lock")
        done = self.start("codex", lambda: self.store.get("default"))
        self.assertTrue(done.wait(.2), "unrelated Codex account waits for Claude auth")

    def test_get_list_and_snapshot_release_registry_during_auth(self):
        for name, action in (("get", lambda: self.store.get("claude-one")),
                             ("list", self.store.list), ("snapshot", self.store.snapshot)):
            with self.subTest(caller=name):
                entered, release, auth = self.auth_barrier()
                with patch.object(codex_claude, "auth_metadata", side_effect=auth):
                    done = self.start(name, action)
                    self.assertTrue(entered.wait(.5))
                    try:
                        self.assert_registry_available()
                    finally:
                        release.set()
                        self.assertTrue(done.wait(.5))
                self.assertEqual(self.errors, [])
                self.assertEqual(self.store.data["accounts"]["claude-one"]["accountId"], "claude:one")

    def test_account_actions_do_not_wrap_auth_in_registry_lock(self):
        options = self.store.data["accounts"]["claude-one"]["claudeOptions"]
        actions = (("default", lambda: self.store.default("claude-one")),
                   ("update", lambda: self.store.update_claude("claude-one", options)),
                   ("reconnect", lambda: self.store.reconnect("claude-one")),
                   ("disconnect", lambda: self.store.disconnect("claude-one")),
                   ("delete", lambda: self.store.delete("claude-one", str(uuid.uuid4()))))
        for name, action in actions:
            with self.subTest(caller=name):
                with self.store.lock:
                    self.store.data["defaultAccountKey"] = "default"
                    row = self.store.data["accounts"]["claude-one"]
                    row.pop("deleted", None)
                    row.pop("disconnected", None)
                entered, release, auth = self.auth_barrier()
                with patch.object(codex_claude, "auth_metadata", side_effect=auth):
                    done = self.start(name, action)
                    self.assertTrue(entered.wait(.5))
                    try:
                        self.assert_registry_available()
                    finally:
                        release.set()
                        self.assertTrue(done.wait(.5))
                self.assertEqual(self.errors, [])

    def test_changed_options_recheck_outside_lock_before_applying_auth(self):
        entered, release = threading.Event(), threading.Event()
        self.releases.append(release)
        seen = []
        def auth(options=None, force=False):
            self.assertFalse(self.store.lock._is_owned())
            seen.append(copy.deepcopy(options))
            if len(seen) == 1:
                entered.set()
                self.assertTrue(release.wait(2))
                return metadata("foreign")
            return metadata()
        with patch.object(codex_claude, "auth_metadata", side_effect=auth):
            done = self.start("changed", lambda: self.store.get("claude-one"))
            self.assertTrue(entered.wait(.5))
            try:
                self.assert_registry_available()
                with self.store.lock:
                    self.store.data["accounts"]["claude-one"]["claudeOptions"]["launchArgs"] = "--no-chrome"
            finally:
                release.set()
                self.assertTrue(done.wait(.5))
        self.assertEqual(self.errors, [])
        self.assertEqual(len(seen), 2)
        self.assertNotIn("launchArgs", seen[0])
        self.assertEqual(seen[1]["launchArgs"], "--no-chrome")
        self.assertEqual(self.results["changed"]["status"], "ready")
        self.assertEqual(self.results["changed"]["accountId"], "claude:one")

    def test_snapshot_rechecks_visibility_after_auth(self):
        entered, release, auth = self.auth_barrier()
        with patch.object(codex_claude, "auth_metadata", side_effect=auth):
            done = self.start("snapshot", self.store.snapshot)
            self.assertTrue(entered.wait(.5))
            try:
                self.assert_registry_available()
                with self.store.lock:
                    self.store.data["accounts"]["claude-one"]["deleted"] = True
            finally:
                release.set()
                self.assertTrue(done.wait(.5))
        self.assertEqual(self.errors, [])
        result = self.results["snapshot"]
        self.assertNotIn("claude-one", [row["id"] for row in result["accounts"]])
        self.assertEqual([row["id"] for row in result["archivedAccounts"]], ["claude-one"])

    def test_auth_cache_does_not_block_other_profile_and_coalesces_same_profile(self):
        entered, release = threading.Event(), threading.Event()
        self.releases.append(release)
        calls = []
        def native(executable, env, **_kwargs):
            argv = [executable, "auth", "status", "--json"]
            kwargs = {"env": env, "timeout": 8}
            config = kwargs["env"]["CLAUDE_CONFIG_DIR"]
            calls.append(config)
            if config.endswith("/a"):
                entered.set()
                self.assertTrue(release.wait(2))
            return metadata(config)
        def get(name):
            return codex_claude.auth_metadata({"configDir": str(self.root / name)})
        with patch.object(codex_claude, "installed", return_value="/private/fake-claude"), \
             patch.object(codex_claude, "_auth_status", side_effect=native):
            cached = get("cached")
            first = self.start("a", lambda: get("a"))
            self.assertTrue(entered.wait(.5))
            second = self.start("same", lambda: get("a"))
            other = self.start("b", lambda: get("b"))
            cached_done = self.start("cached", lambda: get("cached"))
            try:
                self.assertTrue(other.wait(.2), "different profile probe waits for global cache lock")
                self.assertTrue(cached_done.wait(.2), "cached profile waits for global cache lock")
                self.assertFalse(second.is_set())
            finally:
                release.set()
                self.assertTrue(first.wait(.5))
                self.assertTrue(second.wait(.5))
                self.assertTrue(other.wait(.5))
                self.assertTrue(cached_done.wait(.5))
        self.assertEqual(self.errors, [])
        self.assertEqual(calls.count(str(self.root / "a")), 1)
        self.assertEqual(self.results["same"], self.results["a"])
        self.assertEqual(self.results["cached"], cached)

    def test_probe_error_and_foreign_identity_preserve_pin_and_exact_delete_receipt(self):
        with patch.object(codex_claude, "auth_metadata", return_value={
                "status": "error", "accountId": None, "_credentialIdentity": None,
                "error": "Cannot read Claude Code sign-in status"}):
            row = self.store.get("claude-one")
            self.assertEqual(row["status"], "error")
            self.assertEqual(row["accountId"], "claude:one")
            with self.assertRaises(ValueError):
                self.store.default("claude-one")
        with patch.object(codex_claude, "auth_metadata", return_value=metadata("foreign")):
            self.assertEqual(self.store.get("claude-one")["status"], "changed")
        self.assertEqual(self.store.data["accounts"]["claude-one"]["_credentialIdentity"], "claude:one")
        request = str(uuid.uuid4())
        with patch.object(codex_claude, "auth_metadata", return_value=metadata()):
            self.store.delete("claude-one", request)
            receipt = copy.deepcopy(self.store.data["deleteReceipts"][request])
            with self.store.lock:
                self.store.data["accounts"]["claude-one"].pop("deleted")
            result = self.store.delete("claude-one", request)
        self.assertEqual(self.store.data["deleteReceipts"][request], receipt)
        self.assertIn("claude-one", [row["id"] for row in result["accounts"]])

    def test_postlogin_force_does_not_reuse_a_newer_passive_probe(self):
        resolving, resolution, reading, release = [threading.Event() for _ in range(4)]
        self.releases.extend((resolution, release))
        modes = []
        def installed(_profile=None):
            if threading.current_thread().name == 'private-auth-login':
                resolving.set()
                self.assertTrue(resolution.wait(2))
            return '/private/fake-claude'
        def native(_executable, _env, *, interactive=False):
            modes.append(interactive)
            if interactive:
                return metadata('after-login')
            reading.set()
            self.assertTrue(release.wait(2))
            return {'status': 'error', 'accountId': None, '_authErrorKind': 'keychain'}
        with patch.object(codex_claude, 'installed', side_effect=installed), \
                patch.object(codex_claude, '_auth_status', side_effect=native):
            login = self.start('login', lambda: codex_claude.auth_metadata(force=True, interactive=True))
            self.assertTrue(resolving.wait(.5))
            passive = self.start('passive', codex_claude.auth_metadata)
            self.assertTrue(reading.wait(.5))
            resolution.set()
            release.set()
            self.assertTrue(login.wait(.5))
            self.assertTrue(passive.wait(.5))
            self.assertEqual(self.results['passive']['_authErrorKind'], 'keychain')
            self.assertEqual(self.results['login'], metadata('after-login'))
            self.assertEqual(codex_claude.auth_metadata(), metadata('after-login'))
        self.assertEqual(modes, [False, True])
        self.assertEqual(self.errors, [])

    def test_force_probe_error_keeps_error_cache_and_can_refresh_again(self):
        calls = []
        def native(executable, env, **_kwargs):
            argv = [executable, "auth", "status", "--json"]
            kwargs = {"env": env, "timeout": 8}
            self.assertEqual(argv[1:], ["auth", "status", "--json"])
            self.assertEqual(kwargs["timeout"], 8)
            calls.append(argv)
            if len(calls) == 2:
                raise subprocess.TimeoutExpired(argv, 8)
            return metadata()
        with patch.object(codex_claude, "installed", return_value="/private/fake-claude"), \
             patch.object(codex_claude, "_auth_status", side_effect=native):
            self.assertEqual(codex_claude.auth_metadata()["status"], "ready")
            failed = codex_claude.auth_metadata(force=True)
            self.assertEqual(failed["status"], "error")
            self.assertEqual(failed["_authErrorKind"], "timeout")
            self.assertEqual(failed["error"], "Cannot read Claude Code sign-in status")
            self.assertEqual(codex_claude.auth_metadata(), failed)
            self.assertEqual(len(calls), 2)
            self.assertEqual(codex_claude.auth_metadata(force=True)["status"], "ready")
            self.assertEqual(len(calls), 3)

    def test_repeated_option_changes_stop_with_error_and_preserve_identity(self):
        calls = []
        def auth(options=None, force=False):
            self.assertFalse(self.store.lock._is_owned())
            calls.append(options)
            with self.store.lock:
                self.store.data["accounts"]["claude-one"]["claudeOptions"]["launchArgs"] = str(len(calls))
            return metadata("foreign")
        with patch.object(codex_claude, "auth_metadata", side_effect=auth):
            row = self.store.get("claude-one")
        self.assertEqual(len(calls), 2)
        self.assertEqual(row["status"], "error")
        self.assertEqual(row["accountId"], "claude:one")
        self.assertEqual(self.store.data["accounts"]["claude-one"]["_credentialIdentity"], "claude:one")

    def test_active_force_probe_blocks_old_ready_cache_and_propagates_outcome(self):
        entered, release = threading.Event(), threading.Event()
        self.releases.append(release)
        calls = []
        def native(executable, env, **_kwargs):
            argv = [executable, "auth", "status", "--json"]
            kwargs = {"env": env, "timeout": 8}
            calls.append(argv)
            if len(calls) == 1:
                return metadata()
            entered.set()
            self.assertTrue(release.wait(2))
            return {'status': 'signedOut', 'accountId': None, 'email': None, 'plan': None}
        with patch.object(codex_claude, "installed", return_value="/private/fake-claude"), \
             patch.object(codex_claude, "_auth_status", side_effect=native):
            self.assertEqual(codex_claude.auth_metadata()["status"], "ready")
            force_done = self.start("forced", lambda: codex_claude.auth_metadata(force=True))
            self.assertTrue(entered.wait(.5))
            normal_done = self.start("normal", codex_claude.auth_metadata)
            try:
                self.assertFalse(normal_done.wait(.05), "old ready cache bypasses a fresh auth check")
            finally:
                release.set()
                self.assertTrue(force_done.wait(.5))
                self.assertTrue(normal_done.wait(.5))
        self.assertEqual(self.errors, [])
        self.assertEqual(self.results["normal"]["status"], "signedOut")
        self.assertEqual(self.results["forced"], self.results["normal"])
        self.assertEqual(len(calls), 2)

    def test_unexpected_probe_failure_does_not_publish_signed_out_to_waiter(self):
        entered, release = threading.Event(), threading.Event()
        self.releases.append(release)
        def native(executable, env, **_kwargs):
            argv = [executable, "auth", "status", "--json"]
            kwargs = {"env": env, "timeout": 8}
            entered.set()
            self.assertTrue(release.wait(2))
            raise RuntimeError("private fixture failure")
        with patch.object(codex_claude, "installed", return_value="/private/fake-claude"), \
             patch.object(codex_claude, "_auth_status", side_effect=native):
            owner_done = self.start("owner", codex_claude.auth_metadata)
            self.assertTrue(entered.wait(.5))
            waiter_done = self.start("waiter", codex_claude.auth_metadata)
            try:
                self.assertFalse(waiter_done.wait(.05))
            finally:
                release.set()
                self.assertTrue(owner_done.wait(.5))
                self.assertTrue(waiter_done.wait(.5))
        self.assertEqual(len(self.errors), 1)
        self.assertIsInstance(self.errors[0], RuntimeError)
        self.assertEqual(self.results["waiter"]["status"], "error")

    def test_login_receipt_cancellation_survives_auth_overlap(self):
        request = str(uuid.uuid4())
        self.store.data["logins"][request] = {
            "requestId": request, "accountKey": "default", "reauthAccountKey": "default",
            "expectedAccountId": "codex-one", "status": "pending", "createdAt": 0,
            "nativeCompleted": True}
        entered, release = threading.Event(), threading.Event()
        self.releases.append(release)
        def auth(home):
            self.assertFalse(self.store.lock._is_owned())
            entered.set()
            self.assertTrue(release.wait(2))
            return {"status": "ready", "accountId": "codex-one",
                    "_credentialIdentity": "chatgpt:codex-one"}
        with patch.object(codex_accounts, "auth_metadata", side_effect=auth):
            done = self.start("receipt", self.store.login_receipts)
            self.assertTrue(entered.wait(.5))
            try:
                acquired = self.store.lock.acquire(timeout=.1)
                self.assertTrue(acquired)
                try:
                    self.store.data["logins"][request]["status"] = "cancelled"
                finally:
                    self.store.lock.release()
            finally:
                release.set()
                self.assertTrue(done.wait(.5))
        self.assertEqual(self.errors, [])
        self.assertEqual(self.results["receipt"][0]["status"], "cancelled")
        self.assertEqual(self.store.data["logins"][request]["requestId"], request)

    def test_force_after_login_cannot_reuse_pre_login_account_proof(self):
        observed, release = threading.Event(), threading.Event()
        self.releases.append(release)
        calls = []
        def native(executable, env, **_kwargs):
            argv = [executable, "auth", "status", "--json"]
            kwargs = {"env": env, "timeout": 8}
            calls.append(argv)
            identity = "one" if len(calls) == 1 else "foreign"
            if len(calls) == 1:
                observed.set()
                self.assertTrue(release.wait(2))
            return metadata(identity)
        profile = self.store.data["accounts"]["claude-one"]
        with patch.object(codex_claude, "installed", return_value="/private/fake-claude"), \
             patch.object(codex_claude, "_auth_status", side_effect=native):
            old = self.start("before-login", lambda: codex_claude.auth_metadata(profile))
            self.assertTrue(observed.wait(.5))
            # LoginManager._run uses force=True after its native login process exits.
            fresh = self.start("after-login", lambda: codex_claude.auth_metadata(profile, force=True))
            try:
                self.assertFalse(fresh.wait(.05))
            finally:
                release.set()
                self.assertTrue(old.wait(.5))
                self.assertTrue(fresh.wait(.5))
            refreshed = self.store.refresh("claude-one")
        self.assertEqual(self.errors, [])
        self.assertEqual(len(calls), 2)
        self.assertEqual(self.results["before-login"]["accountId"], "claude:one")
        self.assertEqual(self.results["after-login"]["accountId"], "claude:foreign")
        self.assertEqual(refreshed["status"], "changed")
        self.assertEqual(refreshed["accountId"], "claude:one")

    def test_auth_error_kind_is_private_and_clears_after_verified_success(self):
        with patch.object(codex_claude, "auth_metadata", return_value={
                "status": "error", "accountId": None, "_authErrorKind": "timeout",
                "error": "Cannot read Claude Code sign-in status"}):
            self.assertNotIn("_authErrorKind", self.store.get("claude-one"))
            self.assertEqual(self.store.data["accounts"]["claude-one"]["_authErrorKind"], "timeout")
        with patch.object(codex_claude, "auth_metadata", return_value=metadata()):
            row = self.store.get("claude-one")
        self.assertEqual(row["status"], "ready")
        self.assertEqual(row["accountId"], "claude:one")
        self.assertNotIn("_authErrorKind", self.store.data["accounts"]["claude-one"])
        with patch.object(codex_claude, "auth_metadata", return_value={
                "status": "error", "accountId": None, "_authErrorKind": "timeout",
                "error": "Cannot read Claude Code sign-in status"}):
            self.store.get("claude-one")
        with patch.object(codex_claude, "auth_metadata", return_value=metadata("foreign")):
            changed = self.store.get("claude-one")
        self.assertEqual(changed["status"], "changed")
        self.assertEqual(changed["accountId"], "claude:one")
        self.assertNotIn("_authErrorKind", self.store.data["accounts"]["claude-one"])

    def test_force_rechecks_even_when_cached_account_stays_the_same(self):
        with patch.object(codex_claude, "installed", return_value="/private/fake-claude"), \
             patch.object(codex_claude, "_auth_status", return_value=metadata()) as native:
            initial = codex_claude.auth_metadata()
            self.assertEqual(codex_claude.auth_metadata(), initial)
            self.assertEqual(native.call_count, 1)
            self.assertEqual(codex_claude.auth_metadata(force=True), initial)
            self.assertEqual(native.call_count, 2)


if __name__ == "__main__":
    unittest.main()
