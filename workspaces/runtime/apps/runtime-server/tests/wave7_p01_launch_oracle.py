"""Capture the real Python AppServer launch before any provider is started."""
import json
import os
import pathlib
import sys
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "workspaces/runtime/packages/runtime-services/tests/turn_lifecycle"))
from oracle_guard import install_guard
_GUARD = install_guard()
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_runtime import AppServer
from codex_process_supervisor import Journal, Supervisor, native_launch_environment

request = json.load(sys.stdin)
root = pathlib.Path(request["root"])
(root / "native").mkdir(exist_ok=True)
Journal(root)

class Captured(Exception):
    pass

def capture(root, handle, command, env, cwd=None, **_kwargs):
    env = native_launch_environment(root, handle, command, env, cwd)
    print(json.dumps(dict(command=command, env=env, cwd=cwd,
                         signature=Supervisor.signature(command, env, cwd))))
    raise Captured()

with patch.dict(os.environ, request["environment"], clear=True), patch("codex_process_supervisor.attach", capture):
    try:
        AppServer(root / "native", lambda *_: None, lambda *_: None, lambda *_: None,
                  home=request["home"], isolated=request["isolated"],
                  executable=request["executable"], supervisor_handle="account:default",
                  supervisor_root=root)
    except Captured:
        pass
