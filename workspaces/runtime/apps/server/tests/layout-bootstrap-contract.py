#!/usr/bin/env python3
"""Keep top-level server contracts runnable without the suite runner."""

import ast
from pathlib import Path
import unittest


TESTS_ROOT = Path(__file__).resolve().parent
SOURCE_ROOT = TESTS_ROOT.parent / "src"
SOURCE_MODULES = {path.stem for path in SOURCE_ROOT.glob("*.py")}
SOURCE_MODULES.update(
    path.name
    for path in SOURCE_ROOT.iterdir()
    if path.is_dir() and (path / "__init__.py").is_file()
)


def module_imports_source(path: Path) -> tuple[int | None, int | None]:
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    first_source_import = None
    bootstrap_line = None
    for node in tree.body:
        imported = []
        if isinstance(node, ast.Import):
            imported = [alias.name.split(".")[0] for alias in node.names]
        elif isinstance(node, ast.ImportFrom) and node.module:
            imported = [node.module.split(".")[0]]
        if first_source_import is None and set(imported) & (SOURCE_MODULES | {"codex_layout"}):
            first_source_import = node.lineno
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
            if ast.unparse(node.value) == (
                "sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'src'))"
            ):
                bootstrap_line = node.lineno
    return first_source_import, bootstrap_line


class LayoutBootstrapContract(unittest.TestCase):
    def test_every_top_level_source_import_follows_direct_execution_bootstrap(self):
        files = sorted(TESTS_ROOT.glob("*.py"))
        missing = []
        checked = 0
        for path in files:
            source_import, bootstrap = module_imports_source(path)
            if source_import is None:
                continue
            checked += 1
            if bootstrap is None or bootstrap >= source_import:
                missing.append(path.name)

        self.assertEqual(missing, [], f"source imports without an earlier bootstrap: {missing}")
        self.assertGreater(checked, 0, "no server-source imports were checked")


if __name__ == "__main__":
    unittest.main(verbosity=2)
