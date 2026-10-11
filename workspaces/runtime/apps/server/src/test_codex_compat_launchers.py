from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
import unittest
from typing import Any
from unittest.mock import patch

from codex_diagnostics import _kind
from codex_layout import SERVER_SOURCE_ROOT


def load_installer() -> Any:
    path = SERVER_SOURCE_ROOT / "install-cli.py"
    spec = importlib.util.spec_from_file_location("runtime_install_cli", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("Could not load the CLI installer")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CompatibilityLauncherTests(unittest.TestCase):
    def test_bridge_process_classification_uses_relocated_path(self) -> None:
        self.assertEqual(
            _kind("node /checkout/workspaces/providers/apps/claude-bridge/bridge.mjs"),
            "claude_bridge",
        )
        self.assertNotEqual(
            _kind("node /checkout/scripts/claude_bridge/bridge.mjs"),
            "claude_bridge",
        )

    def test_packaged_layout_keeps_desktop_resource_paths(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "Resources/workspace"
            scripts = workspace / "scripts"
            scripts.mkdir(parents=True)
            packaged_layout = scripts / "codex_layout.py"
            packaged_layout.write_bytes((SERVER_SOURCE_ROOT / "codex_layout.py").read_bytes())
            spec = importlib.util.spec_from_file_location("packaged_codex_layout", packaged_layout)
            if spec is None or spec.loader is None:
                self.fail("Could not load packaged codex_layout")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            self.assertEqual(module.REPOSITORY_ROOT, workspace)
            self.assertEqual(module.CLAUDE_BRIDGE_ROOT, scripts / "claude_bridge")
            self.assertEqual(module.WEB_ROOT, workspace / "web")
            self.assertEqual(module.PROMPTS_ROOT, workspace / "prompts")
            self.assertEqual(module.VM_GUEST_ROOT, workspace / "vm/guest")


class InstallerCompatibilityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.installer = load_installer()

    def test_replaces_only_dangling_old_links_and_keeps_new_links(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            source_root = project / "workspaces/runtime/apps/server/src"
            source_root.mkdir(parents=True)
            target = root / "bin"
            target.mkdir()
            names = self.installer.COMMANDS
            for name in names:
                source = source_root / name
                source.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                source.chmod(0o755)

            old_name, valid_name, file_name, link_name = names[:4]
            old_link = target / old_name
            old_link.symlink_to(project / "scripts" / old_name)
            valid_link = target / valid_name
            valid_link.symlink_to(source_root / valid_name)
            unrelated_file = target / file_name
            unrelated_file.write_text("unrelated executable\n", encoding="utf-8")
            unrelated_file.chmod(0o755)
            unrelated_target = root / "elsewhere" / link_name
            unrelated_target.parent.mkdir()
            unrelated_target.write_text("unrelated target\n", encoding="utf-8")
            unrelated_link = target / link_name
            unrelated_link.symlink_to(unrelated_target)

            with (
                patch.object(
                    self.installer,
                    "operations_extension",
                    return_value=(root / "native", "native.so"),
                ),
                patch.object(self.installer, "diagnostics_binary", return_value=None),
            ):
                with self.assertRaisesRegex(ValueError, "existing executable"):
                    self.installer.install(project, target)
                self.assertTrue(old_link.is_symlink())
                self.assertEqual(valid_link.resolve(), source_root / valid_name)
                self.assertEqual(unrelated_file.read_text(encoding="utf-8"), "unrelated executable\n")
                self.assertEqual(unrelated_link.resolve(), unrelated_target)

                unrelated_file.unlink()
                unrelated_link.unlink()
                links = self.installer.install(project, target)

            self.assertEqual(len(links), len(names))
            self.assertEqual(old_link.resolve(), source_root / old_name)
            self.assertEqual(valid_link.resolve(), source_root / valid_name)
            self.assertTrue(all((target / name).is_symlink() for name in names))

    def test_refuses_a_live_link_into_old_scripts_tree(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            source_root = project / "workspaces/runtime/apps/server/src"
            source_root.mkdir(parents=True)
            old_root = project / "scripts"
            old_root.mkdir()
            target = root / "bin"
            target.mkdir()
            for name in self.installer.COMMANDS:
                source = source_root / name
                source.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                source.chmod(0o755)
            first = self.installer.COMMANDS[0]
            old_source = old_root / first
            old_source.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            (target / first).symlink_to(old_source)

            with (
                patch.object(
                    self.installer,
                    "operations_extension",
                    return_value=(root / "native", "native.so"),
                ),
                patch.object(self.installer, "diagnostics_binary", return_value=None),
                self.assertRaisesRegex(ValueError, "unrelated command link"),
            ):
                self.installer.install(project, target)


if __name__ == "__main__":
    unittest.main()
