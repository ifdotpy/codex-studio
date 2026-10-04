from __future__ import annotations

import hashlib
import tempfile
from pathlib import Path
import unittest
from unittest.mock import patch

from codex_python import managed_python, requirements_digest, resolve_python


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


if __name__ == "__main__":
    unittest.main()
