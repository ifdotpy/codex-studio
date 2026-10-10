"""Resolve and prepare the dependency-equipped interpreter for Studio's API."""

from __future__ import annotations

import errno
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
from typing import Mapping, Sequence

from codex_cache_paths import cache_dir
from codex_layout import REPOSITORY_ROOT, SERVER_APP_ROOT, SERVER_SOURCE_ROOT

API_IMPORT_CHECK = "import fastapi, httpx, pydantic, uvicorn, watchdog"
MINIMUM_PYTHON = (3, 11)


def cache_root(environment: Mapping[str, str] | None = None) -> Path:
    """Return the platform user cache root without using application state."""
    return cache_dir(environment, platform.system())


def requirements_file(scripts: Path, development: bool = False) -> Path:
    """Return the API lock at the repository or packaged workspace root."""
    name = "requirements-dev.txt" if development else "requirements.txt"
    scripts = scripts.resolve()
    root = REPOSITORY_ROOT if scripts == SERVER_SOURCE_ROOT.resolve() else scripts.parent
    return root / name


def requirements_digest(scripts: Path, development: bool = False) -> str:
    """Hash the pinned API lock to give each dependency set its own venv."""
    content = requirements_file(scripts, development).read_bytes()
    if development:
        content += requirements_file(scripts).read_bytes()
    return hashlib.sha256(content).hexdigest()


def managed_python(
    scripts: Path,
    environment: Mapping[str, str] | None = None,
    development: bool = False,
) -> Path:
    """Resolve the prepared venv path for this API lock."""
    env_python = "Scripts/python.exe" if os.name == "nt" else "bin/python"
    return (
        cache_root(environment)
        / "codex-agents"
        / ("python-dev" if development else "python")
        / requirements_digest(scripts, development)
        / env_python
    )


def interpreter_version_ok(python: Path) -> bool:
    """Check the minimum Python version without importing application packages."""
    try:
        version = subprocess.run(
            [str(python), "-c", "import sys; print('%s.%s' % sys.version_info[:2])"],
            check=True,
            capture_output=True,
            text=True,
            timeout=5,
        )
        major, minor = (int(part) for part in version.stdout.strip().split("."))
        return (major, minor) >= MINIMUM_PYTHON
    except (OSError, subprocess.SubprocessError, ValueError):
        return False


def interpreter_has_api(python: Path) -> bool:
    """Check interpreter version and required API imports without side effects."""
    if not interpreter_version_ok(python):
        return False
    try:
        subprocess.run(
            [str(python), "-c", API_IMPORT_CHECK],
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return True


def _publish_environment(
    staging: Path, environment_dir: Path, python: Path
) -> Path:
    """Publish a completed venv without replacing a concurrent winner."""
    try:
        staging.rename(environment_dir)
    except OSError as error:
        if error.errno not in (errno.EEXIST, errno.ENOTEMPTY):
            raise
        if python.is_file() and interpreter_has_api(python):
            shutil.rmtree(staging, ignore_errors=True)
            return python
        shutil.rmtree(staging, ignore_errors=True)
        raise RuntimeError(
            f"The managed Python environment at {environment_dir} exists "
            "but is unusable. After confirming no app process uses it, "
            "remove that cache directory and rerun `python3 "
            "workspaces/runtime/apps/server/src/install-cli.py`."
        ) from error
    return python


def _candidate_python(value: str) -> Path | None:
    candidate = Path(value).expanduser()
    if not candidate.is_absolute():
        found = shutil.which(value)
        if found is None:
            return None
        candidate = Path(found)
    return candidate


def resolve_python(
    scripts: Path, environment: Mapping[str, str] | None = None
) -> Path:
    """Pick explicit, managed, then pre-equipped system Python in that order."""
    values = os.environ if environment is None else environment
    explicit = values.get("CODEX_AGENTS_PYTHON")
    if explicit:
        candidate = _candidate_python(explicit)
        if candidate is None or not interpreter_has_api(candidate):
            raise RuntimeError(
                "CODEX_AGENTS_PYTHON must name Python 3.11+ with the API "
                "dependencies installed from requirements.txt."
            )
        return candidate

    prepared = managed_python(scripts, values)
    if prepared.is_file() and interpreter_has_api(prepared):
        return prepared

    candidates: list[Path] = []
    for name in ("python3", sys.executable):
        found = _candidate_python(name)
        if found is not None and found not in candidates:
            candidates.append(found)
    for candidate in candidates:
        if interpreter_has_api(candidate):
            return candidate
    raise RuntimeError(
        "No Python 3.11+ interpreter has FastAPI, Pydantic, Uvicorn, and HTTPX. "
        "Run `python3 workspaces/runtime/apps/server/src/install-cli.py` to prepare the managed "
        "environment, or set CODEX_AGENTS_PYTHON to an equipped interpreter."
    )


def prepare_environment(
    project: Path, python: Path | None = None, development: bool = False
) -> Path:
    """Create and populate the digest-keyed venv during explicit setup."""
    project = project.resolve()
    scripts = (
        SERVER_SOURCE_ROOT
        if project in {REPOSITORY_ROOT.resolve(), SERVER_APP_ROOT.resolve()}
        else project / "scripts"
    )
    lockfile = requirements_file(scripts, development)
    digest = requirements_digest(scripts, development)
    target = managed_python(scripts, development=development)
    if target.is_file() and interpreter_has_api(target):
        return target

    base_python = Path(sys.executable) if python is None else python
    if not interpreter_version_ok(base_python):
        raise RuntimeError(f"Setup interpreter {base_python} must be Python 3.11 or later.")

    environment_dir = target.parent.parent
    environment_dir.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(
        tempfile.mkdtemp(prefix=f".{digest}.", dir=environment_dir.parent)
    )
    shutil.rmtree(staging)
    try:
        subprocess.run(
            [str(base_python), "-m", "venv", str(staging)],
            check=True,
            timeout=120,
        )
        environment_python = staging / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        subprocess.run(
            [
                str(environment_python),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "-r",
                str(lockfile),
            ],
            check=True,
            timeout=600,
        )
        if not interpreter_has_api(environment_python):
            raise RuntimeError("The prepared Python environment failed validation.")
        return _publish_environment(staging, environment_dir, target)
    except (OSError, subprocess.SubprocessError, RuntimeError):
        shutil.rmtree(staging, ignore_errors=True)
        raise
    return target


def main(arguments: Sequence[str] | None = None) -> int:
    """CLI for installer preparation and launcher resolution."""
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scripts", type=Path, default=SERVER_SOURCE_ROOT)
    parser.add_argument("--prepare", action="store_true")
    parser.add_argument("--dev", action="store_true")
    parser.add_argument("--mypy", action="store_true")
    parser.add_argument("--exec", dest="command", nargs=argparse.REMAINDER)
    options = parser.parse_args(arguments)
    try:
        scripts = options.scripts.resolve()
        if options.prepare:
            python = prepare_environment(
                scripts.parent, development=options.dev
            )
        elif options.mypy:
            python = managed_python(scripts, development=True)
            if not python.is_file() or not interpreter_has_api(python):
                raise RuntimeError(
                    "The API typing environment is missing. Run `python3 "
                    "workspaces/runtime/apps/server/src/install-cli.py --dev` during setup."
                )
        else:
            python = resolve_python(scripts)
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        parser.exit(1, f"codex Python: {error}\n")
    if options.mypy:
        project = REPOSITORY_ROOT
        return subprocess.run(
            [
                str(python),
                "-m",
                "mypy",
                "--config-file",
                str(project / "mypy.ini"),
                str(scripts),
            ],
            check=False,
        ).returncode
    if options.command is not None:
        command = options.command
        if command[:1] == ["--"]:
            command = command[1:]
        if not command:
            parser.error("--exec requires a command")
        command_environment = os.environ.copy()
        existing_path = command_environment.get("PYTHONPATH")
        command_environment["PYTHONPATH"] = os.pathsep.join(
            part
            for part in (str(scripts), existing_path)
            if part
        )
        return subprocess.run(
            [str(python), *command], env=command_environment, check=False
        ).returncode
    print(python)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
