#!/usr/bin/env python3
"""The opt-in live checks use a throwaway state folder, never the state of the running Studio."""
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from codex_state import state_dir  # noqa: E402


def load_runtime_live():
    spec = importlib.util.spec_from_file_location("runtime_live_under_test", TESTS / "runtime-live.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class LiveIsolation(unittest.TestCase):
    def setUp(self):
        self.temp = Path(self.enterContext(tempfile.TemporaryDirectory(prefix="live-isolation-")))
        self.addCleanup(shutil.rmtree, self.temp, ignore_errors=True)
        # A stand-in for the default state of the running Studio, which Rust owns.
        live = self.temp / "xdg-state" / "codex-agents"
        live.mkdir(parents=True)
        (live / "rust-state-owner.json").write_text(
            json.dumps({"runtime": "rust", "formatVersion": 1, "backendBuild": "live"}), encoding="utf-8")
        environment = {name: value for name, value in os.environ.items() if name != "CODEX_AGENTS_STATE_DIR"}
        environment.update(XDG_STATE_HOME=str(self.temp / "xdg-state"), CODEX_AGENTS_SUPERVISOR_MODE="1")
        patcher = patch.dict(os.environ, environment, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def keep_out_of_live_state(self, root):
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        self.addCleanup(shutil.rmtree, os.environ["CODEX_WORKSPACE_STORE"], ignore_errors=True)
        self.assertEqual(state_dir(), root)
        self.assertEqual(sorted(root.iterdir()), [])
        self.assertTrue(root.name.startswith("codex-runtime-live-"))
        self.assertEqual(root.parent, Path(tempfile.gettempdir()).resolve())
        self.assertNotIn("CODEX_AGENTS_SUPERVISOR_MODE", os.environ)
        store = Path(os.environ["CODEX_WORKSPACE_STORE"]).resolve()
        self.assertEqual(store.parent, Path(tempfile.gettempdir()).resolve())

    def test_an_unset_state_folder_resolves_the_fenced_live_state(self):
        # The failure that runtime-live.py had: its helpers resolved the default state.
        with self.assertRaisesRegex(RuntimeError, "fenced from Rust-owned state"):
            state_dir()

    def test_runtime_live_uses_its_own_state_when_the_setting_is_unset(self):
        module = load_runtime_live()
        self.keep_out_of_live_state(module.state_root())

    def test_runtime_live_replaces_an_inherited_state_folder(self):
        os.environ["CODEX_AGENTS_STATE_DIR"] = str(self.temp / "xdg-state" / "codex-agents")
        module = load_runtime_live()
        self.keep_out_of_live_state(module.state_root())

    def test_both_live_checks_use_the_shared_isolation(self):
        for name in ("runtime-live.py", "sign-in-live.py"):
            source = (TESTS / name).read_text(encoding="utf-8")
            with self.subTest(check=name):
                self.assertIn("isolate_live_environment()", source)
                self.assertIn("live_state_root(", source)
                self.assertLess(source.index("isolate_live_environment()"), source.index("from codex_runtime"))


if __name__ == "__main__":
    unittest.main()
