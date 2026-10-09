"""Live-update hashes include implementation but exclude colocated tests."""
from __future__ import annotations
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import sys
import tempfile
from pathlib import Path
import unittest

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

from codex_source_inventory import source_files


def write_source(root: Path, relative: str) -> None:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("# fixture\n", encoding="utf-8")


class SourceInventoryContract(unittest.TestCase):
    def test_nested_implementation_is_hashed_and_tests_are_excluded(self) -> None:
        with tempfile.TemporaryDirectory(prefix="source-inventory-contract-") as directory:
            scripts = Path(directory) / "scripts"
            sources = (
                "codex-canvas",
                "studio_api/__init__.py",
                "studio_api/domain/__init__.py",
                "studio_api/domain/models.py",
                "studio_api/domain/router.py",
                "studio_api/domain/test_router.py",
                "studio_api/verification/__init__.py",
                "studio_api/verification/test_routes.py",
                "studio_api/schema_tests/__init__.py",
                "studio_api/schema_tests/test_models.py",
                "other/__init__.py",
                "other/verification/__init__.py",
                "other/verification/runtime.py",
                "other/schema_tests/__init__.py",
                "other/schema_tests/runtime.py",
            )
            for relative in sources:
                write_source(scripts, relative)

            names = {name for name, _path in source_files(scripts)}

        self.assertIn("codex-canvas", names)
        self.assertIn("studio_api/domain/models.py", names)
        self.assertIn("studio_api/domain/router.py", names)
        self.assertIn("other/verification/runtime.py", names)
        self.assertIn("other/schema_tests/runtime.py", names)
        self.assertNotIn("studio_api/domain/test_router.py", names)
        self.assertFalse(any(name.startswith("studio_api/verification/") for name in names))
        self.assertFalse(any(name.startswith("studio_api/schema_tests/") for name in names))


if __name__ == "__main__":
    unittest.main(verbosity=2)
