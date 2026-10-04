"""Inventory production backend sources shared by identity and live updates."""
from pathlib import Path, PurePosixPath


_EXCLUDED_DIRS = {
    "tests", "benchmarks", "__pycache__", "vendor", "venv",
}
_EXCLUDED_TEST_PACKAGE_PATHS = {
    "studio_api/verification",
    "studio_api/schema_tests",
}
_NODE_CRYPTO_HELPER = "codex_federation_crypto.mjs"


def _safe_relative(path: Path, root: Path) -> str:
    relative = path.relative_to(root).as_posix()
    pure = PurePosixPath(relative)
    if (not relative or "\\" in relative or pure.is_absolute()
            or any(part in {"", ".", ".."} for part in pure.parts)):
        raise ValueError("Unsafe backend source path: " + relative)
    return relative


def source_files(scripts: str | Path) -> tuple[tuple[str, Path], ...]:
    """Return (stable relative path, Path) pairs for the production source tree.

    Top-level Python files and codex-canvas retain their historical identity.
    Nested sources are included only inside regular Python package directories.
    """
    root = Path(scripts).resolve(strict=True)
    if not root.is_dir():
        raise ValueError("The backend scripts directory is missing")
    found: dict[str, Path] = {}

    def add(path: Path) -> None:
        relative = _safe_relative(path, root)
        if path.suffix == ".py" and path.name.startswith("test_"):
            return
        if path.is_symlink() or not path.is_file():
            raise ValueError("Backend source must be a local regular file: " + relative)
        found[relative] = path

    def package(directory: Path) -> None:
        if directory.is_symlink() or not directory.is_dir():
            relative = _safe_relative(directory, root)
            raise ValueError("Backend package must be a local directory: " + relative)
        children = sorted(directory.iterdir(), key=lambda item: item.name)
        for child in children:
            if child.name in _EXCLUDED_DIRS:
                continue
            if _safe_relative(child, root) in _EXCLUDED_TEST_PACKAGE_PATHS:
                continue
            if child.is_symlink():
                if child.suffix == ".py" or child.name == _NODE_CRYPTO_HELPER or child.is_dir():
                    raise ValueError("Backend source must not be a symlink: " + _safe_relative(child, root))
                continue
            if child.is_file() and child.suffix == ".py":
                add(child)
            elif child.is_dir():
                init = child / "__init__.py"
                if init.exists() or init.is_symlink():
                    if init.is_symlink() or not init.is_file():
                        relative = _safe_relative(init, root)
                        raise ValueError("Backend package initializer must be a regular file: " + relative)
                    package(child)

    for path in sorted(root.iterdir(), key=lambda item: item.name):
        if path.name in _EXCLUDED_DIRS:
            continue
        if path.is_symlink():
            if path.suffix == ".py" or path.name == _NODE_CRYPTO_HELPER or path.is_dir():
                raise ValueError("Backend source must not be a symlink: " + _safe_relative(path, root))
            continue
        if path.is_file() and (path.suffix == ".py" or path.name in {"codex-canvas", _NODE_CRYPTO_HELPER}):
            add(path)
        elif path.is_dir():
            init = path / "__init__.py"
            if init.exists() or init.is_symlink():
                if init.is_symlink() or not init.is_file():
                    relative = _safe_relative(init, root)
                    raise ValueError("Backend package initializer must be a regular file: " + relative)
                package(path)
    return tuple((name, found[name]) for name in sorted(found))
