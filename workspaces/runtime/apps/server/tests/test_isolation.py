"""Isolate supervisor routing and image storage before a Studio test imports code."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).absolute().parent))
from runtime_home_guard import install_home_guard
install_home_guard()

import atexit
import os
import re
import shutil
import tempfile

SUPERVISOR_ENV = (
    "CODEX_AGENTS_SUPERVISOR_MODE",
    "CODEX_AGENTS_STATE_DIR",
    "CODEX_AGENTS_SUPERVISOR_FALLBACK",
)

TEST_WORKSPACE_STORE_ENV = "CODEX_AGENTS_TEST_WORKSPACE_STORE"
TEST_WORKSPACE_STORE = os.environ.get(TEST_WORKSPACE_STORE_ENV)
if TEST_WORKSPACE_STORE:
    candidate = tempfile.gettempdir()
    is_isolated = (
        os.path.dirname(os.path.abspath(TEST_WORKSPACE_STORE)) == os.path.abspath(candidate)
        and os.path.basename(TEST_WORKSPACE_STORE).startswith("studio-test-workspaces-")
        and os.path.isdir(TEST_WORKSPACE_STORE)
    )
else:
    is_isolated = False
if not is_isolated:
    TEST_WORKSPACE_STORE = tempfile.mkdtemp(prefix="studio-test-workspaces-")
    os.environ[TEST_WORKSPACE_STORE_ENV] = TEST_WORKSPACE_STORE
    atexit.register(shutil.rmtree, TEST_WORKSPACE_STORE, ignore_errors=True)
os.environ["CODEX_WORKSPACE_STORE"] = TEST_WORKSPACE_STORE
# Test stores hold tiny images. The production free-space floors would make
# every test fail on a nearly full disk, so tests opt in to them explicitly.
os.environ.setdefault("CODEX_WORKSPACE_MIN_FREE_BYTES", "0")
os.environ.setdefault("CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES", "0")


def isolate_supervisor_environment():
    for name in SUPERVISOR_ENV:
        os.environ.pop(name, None)


def minimal_runtime_environment(home: str | Path) -> dict[str, str]:
    """Use the same complete environment as HTTP backends and seed processes."""
    from runtime_test_environment import runtime_environment
    return runtime_environment(Path(home))


def isolate_api_schema_cache():
    """Keep runtime schema cache writes inside this test server process."""
    cache_root = tempfile.mkdtemp(prefix="studio-test-api-cache-")
    atexit.register(shutil.rmtree, cache_root, ignore_errors=True)
    os.environ["XDG_CACHE_HOME"] = cache_root
    seed_api_schema_cache_for_fixture(Path(cache_root))


def generated_api_schema_hash() -> str:
    """Read the generated renderer identity used by isolated server fixtures."""
    from codex_layout import WEB_ROOT

    generated = (WEB_ROOT / "src/generated/apiSchema.ts").read_text(encoding="utf-8")
    match = re.search(r'API_SCHEMA_HASH\s*=\s*"([0-9a-f]{64})"', generated)
    if match is None:
        raise RuntimeError("generated API schema hash is missing")
    return match.group(1)


def seed_api_schema_cache_for_fixture(cache_home: Path) -> str:
    """Prime only an explicit disposable fixture cache from generated identity."""
    from studio_api.context import (
        api_schema_cache_key,
        read_cached_or_compute_api_schema_hash,
    )

    temporary_root = Path(tempfile.gettempdir()).resolve()
    resolved_cache_home = cache_home.resolve()
    if resolved_cache_home == temporary_root or not resolved_cache_home.is_relative_to(
        temporary_root
    ):
        raise RuntimeError(
            "fixture API schema cache must stay below the temporary directory"
        )

    previous = os.environ.get("XDG_CACHE_HOME")
    os.environ["XDG_CACHE_HOME"] = str(resolved_cache_home)
    try:
        return read_cached_or_compute_api_schema_hash(
            api_schema_cache_key(), generated_api_schema_hash,
        )
    finally:
        if previous is None:
            os.environ.pop("XDG_CACHE_HOME", None)
        else:
            os.environ["XDG_CACHE_HOME"] = previous


isolate_supervisor_environment()
