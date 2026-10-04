from __future__ import annotations

import errno
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

from codex_python import (
    _publish_environment,
    managed_python,
    requirements_digest,
    resolve_python,
)


class PythonResolutionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.project = Path(self.temporary.name)
        self.scripts = self.project / "scripts"
        self.scripts.mkdir()
        (self.project / "requirements.txt").write_text("fastapi==1\n")

    def test_managed_environment_path_uses_lock_digest_and_user_cache(self) -> None:
        digest = hashlib.sha256(b"fastapi==1\n").hexdigest()
        path = managed_python(
            self.scripts, {"XDG_CACHE_HOME": str(self.project / "cache")}
        )
        self.assertEqual(
            path,
            self.project / "cache/codex-agents/python" / digest / "bin/python",
        )
        self.assertEqual(requirements_digest(self.scripts), digest)

    def test_explicit_interpreter_has_priority(self) -> None:
        selected = self.project / "selected-python"
        selected.touch()
        with patch("codex_python.interpreter_has_api", return_value=True):
            result = resolve_python(
                self.scripts,
                {
                    "CODEX_AGENTS_PYTHON": str(selected),
                    "XDG_CACHE_HOME": str(self.project / "cache"),
                },
            )
        self.assertEqual(result, selected)

    def test_invalid_explicit_interpreter_fails_closed(self) -> None:
        with patch("codex_python.interpreter_has_api", return_value=False):
            with self.assertRaisesRegex(RuntimeError, "CODEX_AGENTS_PYTHON"):
                resolve_python(
                    self.scripts,
                    {"CODEX_AGENTS_PYTHON": str(self.project / "missing")},
                )

    def test_managed_interpreter_precedes_system_fallback(self) -> None:
        cache = self.project / "cache"
        expected = managed_python(self.scripts, {"XDG_CACHE_HOME": str(cache)})
        expected.parent.mkdir(parents=True)
        expected.touch()
        with (
            patch("codex_python.interpreter_has_api", side_effect=lambda path: path == expected),
            patch("codex_python._candidate_python", return_value=self.project / "system-python"),
        ):
            result = resolve_python(
                self.scripts, {"XDG_CACHE_HOME": str(cache), "PATH": ""}
            )
        self.assertEqual(result, expected)

    def test_cli_reexecs_into_venv_even_when_python_is_a_symlink(self) -> None:
        scripts = self.project / "scripts"
        python = self.project / "venv/bin/python"
        subprocess.run(
            [sys.executable, "-m", "venv", "--without-pip", str(python.parent.parent)],
            check=True,
            timeout=60,
        )
        self.assertTrue(python.is_symlink())
        self.assertEqual(python.resolve(), Path(sys.executable).resolve())
        site_packages = subprocess.run(
            [str(python), "-c", "import sysconfig; print(sysconfig.get_paths()['purelib'])"],
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout.strip()
        dependency_dir = Path(site_packages)
        dependency_dir.mkdir(parents=True, exist_ok=True)
        for module_name in ("fastapi", "httpx", "pydantic", "uvicorn", "watchdog"):
            (dependency_dir / f"{module_name}.py").write_text("READY = True\n")

        shutil.copyfile(Path(__file__).with_name("codex-canvas"), scripts / "codex-canvas")
        (scripts / "codex_startup_memory.py").write_text(
            "def mark(stage: str) -> None:\n    pass\n"
        )
        (scripts / "codex_canvas.py").write_text(
            "import json, sys\n"
            "import fastapi, httpx, pydantic, uvicorn\n"
            "def main() -> None:\n"
            "    print(json.dumps({'prefix': sys.prefix, 'base_prefix': sys.base_prefix, "
            "'fastapi': fastapi.__file__}))\n"
        )
        (self.project / "requirements.txt").write_text("fastapi==1\n")
        environment = os.environ.copy()
        environment["CODEX_AGENTS_PYTHON"] = str(python)
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
        result = subprocess.run(
            [sys.executable, str(scripts / "codex-canvas")],
            check=False,
            capture_output=True,
            text=True,
            timeout=15,
            env=environment,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        report: dict[str, str] = json.loads(result.stdout)
        self.assertEqual(Path(report["prefix"]), python.parent.parent)
        self.assertNotEqual(report["prefix"], report["base_prefix"])
        self.assertEqual(Path(report["fastapi"]).parent, dependency_dir)

    def test_concurrent_venv_winner_is_kept_and_losing_staging_removed(self) -> None:
        for collision_errno in (errno.EEXIST, errno.ENOTEMPTY):
            with self.subTest(errno=collision_errno):
                staging = self.project / f"stage-{collision_errno}"
                environment_dir = self.project / f"installed-{collision_errno}"
                python = environment_dir / "bin/python"
                staging.mkdir()
                python.parent.mkdir(parents=True)
                python.touch()
                with (
                    patch.object(
                        Path,
                        "rename",
                        side_effect=OSError(collision_errno, "destination exists"),
                    ),
                    patch("codex_python.interpreter_has_api", return_value=True),
                ):
                    self.assertEqual(_publish_environment(staging, environment_dir, python), python)
                self.assertFalse(staging.exists())
                self.assertTrue(python.is_file())

    def test_unusable_existing_venv_gives_repair_instruction(self) -> None:
        staging = self.project / "losing-stage"
        environment_dir = self.project / "broken-environment"
        python = environment_dir / "bin/python"
        staging.mkdir()
        python.parent.mkdir(parents=True)
        python.touch()
        with (
            patch.object(
                Path,
                "rename",
                side_effect=OSError(errno.ENOTEMPTY, "destination exists"),
            ),
            patch("codex_python.interpreter_has_api", return_value=False),
        ):
            with self.assertRaisesRegex(RuntimeError, "remove that cache directory"):
                _publish_environment(staging, environment_dir, python)
        self.assertFalse(staging.exists())


if __name__ == "__main__":
    unittest.main()
