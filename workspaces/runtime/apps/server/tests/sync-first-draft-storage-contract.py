"""Keep the Python storage schema assertions separate from HTTP contracts."""

import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from server_test_waits import HANG_GUARD_SECONDS

from codex_layout import REPOSITORY_ROOT, SERVER_TESTS_ROOT
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()


root = Path(tempfile.mkdtemp(prefix="sync-first-draft-storage-", dir=os.environ["TMPDIR"]))
state = root / "state"
state.mkdir()
env = os.environ.copy()
for key, leaf in (
        ("CODEX_AGENTS_STATE_DIR", state), ("CODEX_HOME", root / "codex-home"),
        ("XDG_CACHE_HOME", root / "cache"), ("XDG_STATE_HOME", root / "xdg-state"),
        ("HOME", root / "home")):
    Path(leaf).mkdir(exist_ok=True)
    env[key] = str(leaf)
env.update({
    "HTTPS_PROXY": "http://127.0.0.1:9", "HTTP_PROXY": "http://127.0.0.1:9",
    "ALL_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1,localhost",
})
process = subprocess.Popen(
    [sys.executable, "-B", str(SERVER_TESTS_ROOT / "fixtures/sync-first-draft-fixture.py"), str(state)],
    cwd=REPOSITORY_ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
)
try:
    ready = json.loads(process.stdout.readline())
    origin = f"http://127.0.0.1:{ready['port']}"
    database = state / "canvas.sqlite3"
    with sqlite3.connect(database) as db:
        workspace_id = db.execute("SELECT id FROM sync_identity").fetchone()[0]
        assert db.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents'"
        ).fetchone()
    row = {"newDocumentState": {
        "id": "phone:storage:storage", "seq": 0, "_deleted": False,
        "payload": json.dumps({"text": "storage setup", "session": "storage", "device": "phone"}),
    }}
    request = urllib.request.Request(
        origin + "/api/sync/drafts", data=json.dumps({"rows": [row]}).encode(),
        headers={"Content-Type": "application/json", "Origin": origin,
                 "X-Canvas-Token": ready["token"], "X-Canvas-Workspace": workspace_id},
    )
    with urllib.request.urlopen(request, timeout=HANG_GUARD_SECONDS) as response:
        assert response.status == 200, response.read()
        triggers = db.execute(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger' "
            "AND name LIKE 'sync_transcript_revision_%'"
        ).fetchone()[0]
    assert triggers == 6, (ready, triggers)
    print("first-draft storage schema contract passed")
finally:
    process.terminate()
    process.wait(timeout=15)
    import shutil
    shutil.rmtree(root)
