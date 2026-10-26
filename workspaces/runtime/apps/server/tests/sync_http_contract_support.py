"""Preparation for Rust sync HTTP contracts using the real Python SyncStore."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
from typing import Any


def prepare_sync_store_for_rust_contract(
    backend: Any, *, seed_python_rows: bool = False
) -> None:
    """Restart Rust with store-bootstrap state, optionally seeded by Python writers."""
    if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
        return
    backend.stop()
    if seed_python_rows:
        python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
        source = Path(__file__).resolve().parents[1] / "src"
        prepare_sync_state = (
            "import os, sys; "
            f"sys.path.insert(0, {str(source)!r}); "
            "from pathlib import Path; "
            "from codex_canvas import Canvas; "
            "from codex_sync import SyncStore; "
            "canvas = Canvas(Path(os.environ['CODEX_AGENTS_STATE_DIR'])); "
            "store = SyncStore(canvas.connect, canvas.transcript, canvas=canvas); "
        )
        prepare_sync_state += """
import json
store.push_drafts([{"newDocumentState": {
    "id": "python-device:python-session",
    "payload": json.dumps({
        "device": "python-device", "session": "python-session", "text": "written by Python"
    }, ensure_ascii=False, separators=(",", ":")),
}}])
from studio_api.sync.preferences import PreferencePushRequest, merge_preferences
request = PreferencePushRequest(scope="user", fields={
    '["codex-studio-preferences-v1","typography"]': {
        "value": "custom", "timestamp": 2, "writer": "python-seed"
    }
})
with canvas.connect() as db:
    merge_preferences(db, request)
"""
        subprocess.run(
            [python, "-B", "-c", prepare_sync_state],
            cwd=backend.cwd,
            env=backend._environment(),
            check=True,
            timeout=30,
        )
    backend.start()
    backend.ready_samples.append(backend.ready_seconds or 0)
