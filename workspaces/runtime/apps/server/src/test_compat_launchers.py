from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
from typing import Any
import unittest
from unittest.mock import patch

from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT


def load_installer() -> Any:
    path = SERVER_SOURCE_ROOT / "install-cli.py"
    spec = importlib.util.spec_from_file_location("runtime_install_cli", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("Could not load the CLI installer")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CompatibilityLauncherTests(unittest.TestCase):
    def test_python_shim_resolves_symlink_and_execs_with_original_interpreter(
        self: CompatibilityLauncherTests,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory) / "checkout"
            scripts = project / "scripts"
            source_root = project / "workspaces/runtime/apps/server/src"
            bin_dir = Path(directory) / "bin"
            scripts.mkdir(parents=True)
            source_root.mkdir(parents=True)
            bin_dir.mkdir()

            shim = scripts / "codex-chat"
            shim.write_bytes((REPOSITORY_ROOT / "scripts/codex-chat").read_bytes())
            shim.chmod(0o755)
            source = source_root / "codex-chat"
            source.write_text(
                "import json, os, signal, sys\n"
                "print(json.dumps({'argv': sys.argv[1:], 'exe': sys.executable, 'pid': os.getpid()}))\n"
                "if os.environ.get('SHIM_SIGNAL'): os.kill(os.getpid(), int(os.environ['SHIM_SIGNAL']))\n"
                "if os.environ.get('SHIM_EXIT'): raise SystemExit(int(os.environ['SHIM_EXIT']))\n",
                encoding="utf-8",
            )
            alias = bin_dir / "codex-chat"
            alias.symlink_to(shim)

            process = subprocess.Popen(
                [sys.executable, "-B", str(alias), "--value", "has spaces"],
                cwd=directory,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            stdout, stderr = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 0, stderr)
            report = json.loads(stdout)
            self.assertEqual(report["argv"], ["--value", "has spaces"])
            self.assertEqual(Path(report["exe"]).resolve(), Path(sys.executable).resolve())
            self.assertEqual(report["pid"], process.pid)

            failed = subprocess.run(
                [sys.executable, "-B", str(alias), "exit"],
                capture_output=True,
                text=True,
                check=False,
                timeout=10,
                env={**os.environ, "SHIM_EXIT": "23"},
            )
            self.assertEqual(failed.returncode, 23)

            if os.name != "nt":
                signaled = subprocess.run(
                    [sys.executable, "-B", str(alias), "signal"],
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=10,
                    env={**os.environ, "SHIM_SIGNAL": str(signal.SIGTERM)},
                )
                self.assertEqual(signaled.returncode, -signal.SIGTERM)

    def test_python_shims_cover_installed_commands_and_legacy_paths(
        self: CompatibilityLauncherTests,
    ) -> None:
        shim_names = (
            *load_installer().COMMANDS,
            "codex_canvas.py",
            "codex_process_supervisor.py",
            "codex_python.py",
            "install-cli.py",
            "codex-publish-update",
        )
        for name in shim_names:
            if name.endswith(".mjs"):
                continue
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                project = Path(directory) / "checkout"
                scripts = project / "scripts"
                source_root = project / "workspaces/runtime/apps/server/src"
                scripts.mkdir(parents=True)
                source_root.mkdir(parents=True)
                shim = scripts / name
                shim.write_bytes((REPOSITORY_ROOT / "scripts" / name).read_bytes())
                shim.chmod(0o755)
                source = source_root / name
                source.write_text(
                    "import json, sys\n"
                    "print(json.dumps({'argv': sys.argv[1:], 'source': sys.argv[0]}))\n",
                    encoding="utf-8",
                )
                result = subprocess.run(
                    [sys.executable, "-B", str(shim), "value with spaces"],
                    cwd=directory,
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                report = json.loads(result.stdout)
                self.assertEqual(report["argv"], ["value with spaces"])
                self.assertEqual(Path(report["source"]), source)

    @unittest.skipUnless(shutil.which("node"), "Node.js is not installed")
    def test_node_shim_resolves_symlink_and_forwards_arguments(
        self: CompatibilityLauncherTests,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory) / "checkout"
            scripts = project / "scripts"
            source_root = project / "workspaces/runtime/apps/server/src"
            scripts.mkdir(parents=True)
            source_root.mkdir(parents=True)
            shim = scripts / "codex-swarm.mjs"
            shim.write_bytes(
                (REPOSITORY_ROOT / "scripts/codex-swarm.mjs").read_bytes()
            )
            source = source_root / "codex-swarm.mjs"
            source.write_text(
                "console.log(JSON.stringify({argv: process.argv.slice(2), source: process.argv[1]}));\n",
                encoding="utf-8",
            )
            alias = Path(directory) / "codex-swarm.mjs"
            alias.symlink_to(shim)
            result = subprocess.run(
                [shutil.which("node") or "node", str(alias), "value with spaces"],
                cwd=directory,
                capture_output=True,
                text=True,
                check=False,
                timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertEqual(report["argv"], ["value with spaces"])
            self.assertEqual(Path(report["source"]), source)

class InstallerCompatibilityTests(unittest.TestCase):
    def setUp(self: InstallerCompatibilityTests) -> None:
        self.installer = load_installer()

    def test_installs_new_source_links_into_temporary_bin_dir(
        self: InstallerCompatibilityTests,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "bin"
            environment = {**os.environ, "CODEX_AGENTS_PYTHON": sys.executable}
            with (
                patch.dict(os.environ, environment, clear=True),
                patch.object(self.installer, "resolve_python", return_value=Path(sys.executable)),
                patch.object(sys, "argv", ["install-cli.py", "--bin-dir", str(target)]),
            ):
                self.installer.main()
            for name in self.installer.COMMANDS:
                link = target / name
                self.assertTrue(link.is_symlink(), name)
                self.assertEqual(link.resolve(), SERVER_SOURCE_ROOT / name)

    def test_replaces_only_links_into_old_scripts_tree(
        self: InstallerCompatibilityTests,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "new"
            source_root = project / "workspaces/runtime/apps/server/src"
            source_root.mkdir(parents=True)
            old = root / "old"
            target = root / "bin"
            target.mkdir()
            for name in self.installer.COMMANDS:
                source = source_root / name
                source.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                source.chmod(0o755)
                (target / name).symlink_to(old / "scripts" / name)

            links = self.installer.install(project, target, old)
            self.assertEqual(len(links), len(self.installer.COMMANDS))
            for name in self.installer.COMMANDS:
                self.assertEqual((target / name).resolve(), source_root / name)


if __name__ == "__main__":
    unittest.main()
