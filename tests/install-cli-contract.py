"""Command link installation and source-independent imports. No model requests."""

import importlib.util
from pathlib import Path
import os
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "install_cli", ROOT / "scripts/install-cli.py"
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class InstallContract(unittest.TestCase):
    def test_links_execute_outside_checkout_and_repeat_safely(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "bin"
            first = module.install(ROOT, target)
            self.assertEqual(first, module.install(ROOT, target))
            for name in [
                "codex-canvas",
                "codex-graph",
                "codex-chat",
                "codex-control",
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
                "CODEX_BOARD_STATE_DIR": str(root / "board"),
            }
            for name, args in [("codex-board", ["show"]), ("luna", ["ls", "--all"])]:
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
            with self.assertRaises(ValueError):
                module.install(ROOT, target)
            self.assertEqual(list(target.iterdir()), [foreign])
            self.assertEqual(foreign.read_text(), "unrelated")

    def test_only_matching_old_command_links_can_move(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "bin"
            target.mkdir()
            old = root / "old"
            (target / "luna").symlink_to(old / "scripts/luna")
            with self.assertRaises(ValueError):
                module.install(ROOT, target)
            module.install(ROOT, target, old)
            self.assertEqual((target / "luna").resolve(), ROOT / "scripts/luna")
            (target / "luna").unlink()
            (target / "luna").symlink_to(old / "scripts/codex-board")
            with self.assertRaises(ValueError):
                module.install(ROOT, target, old)


if __name__ == "__main__":
    unittest.main()
