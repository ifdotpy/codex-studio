#!/usr/bin/env python3
"""Browser skill setup preserves account identity without reconnecting held locks."""

from pathlib import Path
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_browser import configure_browser, GUIDANCE


class SkillServer:
    def __init__(self):
        self.calls = []

    def call(self, method, params, timeout):
        self.calls.append((method, params, timeout))
        return {}


class BrowserLockContract(unittest.TestCase):
    def setUp(self):
        self.default, self.selected, self.replacement = SkillServer(), SkillServer(), SkillServer()
        self.runtime = SimpleNamespace(
            accounts=SimpleNamespace(home=lambda key: Path("/accounts") / key, base_home=Path("/shared")),
            servers={"default": self.default, "second": self.selected},
            offline_accounts=set(), start_lock=threading.Lock(),
        )
        self.connect_calls = []

        def connect(key):
            self.connect_calls.append(key)
            with self.runtime.start_lock:
                self.runtime.servers[key] = self.replacement
                return self.replacement

        self.runtime.connect = connect
        self.params = {"config": {"existing": True}, "developerInstructions": "existing"}
        self.root = Path("/shared/chrome/skills")
        self.status = self.enterContext(patch("codex_browser.browser_status", return_value=({"browser": True}, None)))
        self.enterContext(patch("codex_browser.skill_root", return_value=self.root))

    def configure(self):
        return configure_browser(self.runtime, {"accountKey": "second"}, self.params)

    def test_catalog_refresh_held_lock_uses_exact_connected_server(self):
        errors = []
        completed = threading.Event()

        def run():
            try:
                self.configure()
            except BaseException as error:
                errors.append(error)
            finally:
                completed.set()

        self.runtime.start_lock.acquire()
        worker = threading.Thread(target=run, daemon=True)
        worker.start()
        try:
            self.assertTrue(completed.wait(1), "Browser configuration reentered the held start_lock")
            self.assertEqual(errors, [])
            self.assertEqual(self.connect_calls, [])
            self.assertEqual(self.selected.calls, [
                ("skills/extraRoots/set", {"extraRoots": [str(self.root)]}, 20)])
            self.assertEqual(self.default.calls, [])
            self.assertEqual(self.replacement.calls, [])
            self.assertEqual(self.params["config"], {"existing": True, "browser": True})
            self.assertEqual(self.params["developerInstructions"], "existing\n" + GUIDANCE)
        finally:
            self.runtime.start_lock.release()
            worker.join(2)

    def test_missing_account_server_connects_that_account(self):
        del self.runtime.servers["second"]
        self.configure()
        self.assertEqual(self.connect_calls, ["second"])
        self.assertEqual(len(self.replacement.calls), 1)
        self.assertEqual(self.default.calls, [])
        self.assertEqual(self.selected.calls, [])

    def test_offline_server_is_not_reused(self):
        self.runtime.offline_accounts.add("second")
        self.configure()
        self.assertEqual(self.connect_calls, ["second"])
        self.assertEqual(len(self.replacement.calls), 1)
        self.assertEqual(self.selected.calls, [])

    def test_disabled_browser_clears_only_selected_server_without_connect(self):
        self.configure()
        self.selected.calls.clear()
        self.params = {"config": {"existing": True}, "developerInstructions": "existing"}
        self.status.return_value = ({}, "Disabled")
        with self.runtime.start_lock:
            self.configure()
        self.assertEqual(self.connect_calls, [])
        self.assertEqual(self.selected.calls, [("skills/extraRoots/set", {"extraRoots": []}, 20)])
        self.assertEqual(self.default.calls, [])
        self.assertEqual(self.params, {"config": {"existing": True}, "developerInstructions": "existing"})


if __name__ == "__main__":
    unittest.main()
