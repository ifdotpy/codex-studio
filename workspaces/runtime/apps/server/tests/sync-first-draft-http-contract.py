"""A first protocol write can push drafts before any sync pull."""
from codex_layout import REPOSITORY_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import urllib.request

ROOT = REPOSITORY_ROOT

with tempfile.TemporaryDirectory(prefix="sync-first-draft-") as directory:
    root = Path(directory)
    env = os.environ.copy()
    for key, leaf in (
        ("CODEX_AGENTS_STATE_DIR", "state"),
        ("CODEX_HOME", "codex-home"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "xdg-state"),
        ("HOME", "home"),
    ):
        path = root / leaf
        path.mkdir()
        env[key] = str(path)
    env.update({
        "HTTPS_PROXY": "http://127.0.0.1:9",
        "HTTP_PROXY": "http://127.0.0.1:9",
        "ALL_PROXY": "http://127.0.0.1:9",
        "NO_PROXY": "127.0.0.1,localhost",
    })
    process = subprocess.Popen(
        [sys.executable, "-B", str(SERVER_TESTS_ROOT / "fixtures/sync-first-draft-fixture.py"),
         str(root / "state")],
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        ready = json.loads(process.stdout.readline())
        origin = f"http://127.0.0.1:{ready['port']}"
        database = root / "state" / "canvas.sqlite3"
        with sqlite3.connect(database) as db:
            workspace_id = db.execute("SELECT id FROM sync_identity").fetchone()[0]
            assert db.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_agents'"
            ).fetchone()
        row = {
            "newDocumentState": {
                "id": "phone:first-chat",
                "payload": json.dumps({
                    "device": "phone", "session": "first-chat", "text": "first push",
                }),
            },
        }
        request = urllib.request.Request(
            origin + "/api/sync/drafts",
            data=json.dumps({"rows": [row]}).encode(),
            headers={
                "Content-Type": "application/json",
                "Origin": origin,
                "X-Canvas-Token": ready["token"],
                "X-Canvas-Workspace": workspace_id,
            },
        )
        with urllib.request.urlopen(request, timeout=10) as response:
            assert json.load(response) == []
        with sqlite3.connect(database) as db:
            assert db.execute(
                "SELECT payload FROM sync_documents WHERE scope='drafts' AND id='phone:first-chat'"
            ).fetchone() is not None
            assert db.execute(
                "SELECT count(*) FROM sqlite_master WHERE type='trigger' "
                "AND name LIKE 'sync_transcript_revision_%'"
            ).fetchone()[0] == 6
        print("first draft push HTTP contract passed")
    finally:
        process.terminate()
        process.wait(timeout=15)
