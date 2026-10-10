"""Isolate supervisor routing and image storage before a Studio test imports code."""
import atexit
import os
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


def isolate_api_schema_cache():
    """Keep runtime schema cache writes inside this test server process."""
    cache_root = tempfile.mkdtemp(prefix="studio-test-api-cache-")
    os.environ["XDG_CACHE_HOME"] = cache_root
    atexit.register(shutil.rmtree, cache_root, ignore_errors=True)


isolate_supervisor_environment()
