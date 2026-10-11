"""Command link installation and source-independent imports. No model requests."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import importlib.util
from pathlib import Path
import os
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
spec = importlib.util.spec_from_file_location(
    "install_cli", SERVER_SOURCE_ROOT / "install-cli.py"
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class InstallContract(unittest.TestCase):
    def test_links_execute_outside_checkout_and_repeat_safely(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "bin"
            with patch.object(module, "operations_extension", return_value=(ROOT / "native", "native.so")):
                first = module.install(ROOT, target)
                self.assertEqual(first, module.install(ROOT, target))
            self.assertFalse((target / "codex-board").exists())
            for name in [
                "codex-canvas",
                "codex-graph",
                "codex-chat",
                "codex-control",
                "codex-supervisor",
                "codex-daemon",
            ]:
                result = subprocess.run(
                    [str(target / name), "--help"],
                    cwd=root,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
            env = {
                **os.environ,
                "CODEX_AGENTS_STATE_DIR": str(root / "state"),
            }
            for name, args in [("luna", ["ls", "--all"])]:
                result = subprocess.run(
                    [str(target / name), *args],
                    cwd=root,
                    env=env,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_conflict_preflight_preserves_every_existing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            foreign = target / "luna"
            foreign.write_text("unrelated")
            with patch.object(module, "operations_extension", return_value=(ROOT / "native", "native.so")):
                with self.assertRaises(ValueError):
                    module.install(ROOT, target)
            self.assertEqual(list(target.iterdir()), [foreign])
            self.assertEqual(foreign.read_text(), "unrelated")

    def test_replaces_dangling_same_checkout_links_and_refuses_unrelated_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "bin"
            target.mkdir()
            old_link = target / "luna"
            old_link.symlink_to(ROOT / "scripts/luna")
            source_root = ROOT / "workspaces/runtime/apps/server/src"
            valid_link = target / "codex-daemon"
            valid_link.symlink_to(source_root / "codex-daemon")
            unrelated_file = target / "codex-chat"
            unrelated_file.write_text("unrelated executable")
            unrelated_link = target / "codex-control"
            unrelated_target = root / "unrelated-target"
            unrelated_target.write_text("unrelated link target")
            unrelated_link.symlink_to(unrelated_target)
            with patch.object(module, "operations_extension", return_value=(ROOT / "native", "native.so")):
                with self.assertRaises(ValueError):
                    module.install(ROOT, target)
            self.assertTrue(old_link.is_symlink())
            self.assertEqual(valid_link.resolve(), source_root / "codex-daemon")
            self.assertEqual(unrelated_file.read_text(), "unrelated executable")
            self.assertEqual(unrelated_link.resolve(), unrelated_target)

            unrelated_file.unlink()
            unrelated_link.unlink()
            with patch.object(module, "operations_extension", return_value=(ROOT / "native", "native.so")):
                module.install(ROOT, target)
            self.assertEqual(old_link.resolve(), SERVER_SOURCE_ROOT / "luna")
            self.assertEqual(valid_link.resolve(), source_root / "codex-daemon")

            (target / "codex-chat").unlink()
            (target / "codex-chat").symlink_to(unrelated_target)
            with patch.object(module, "operations_extension", return_value=(ROOT / "native", "native.so")):
                with self.assertRaises(ValueError):
                    module.install(ROOT, target)


if __name__ == "__main__":
    unittest.main()
