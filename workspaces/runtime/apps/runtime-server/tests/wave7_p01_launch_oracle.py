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
import codex_runtime
import codex_workspace_images

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

def capture_direct(command, **kwargs):
    print(json.dumps(dict(command=command, env=kwargs["env"], cwd=kwargs.get("cwd"))))
    raise Captured()

with patch.dict(os.environ, request["environment"], clear=True):
    try:
        if request.get("mode") == "direct":
            with patch("subprocess.Popen", capture_direct):
                AppServer(root / "native", lambda *_: None, lambda *_: None, lambda *_: None,
                          home=request["home"], isolated=request["isolated"],
                          executable=request["executable"])
        elif request.get("mode") == "direct-wrapped":
            prefix = request["providerPrefix"]
            with patch.object(codex_workspace_images, "exec_prefix", return_value=prefix), \
                 patch.object(codex_runtime.shutil, "which", return_value=prefix[0]), \
                 patch("subprocess.Popen", capture_direct):
                AppServer(root / "native", lambda *_: None, lambda *_: None, lambda *_: None,
                          home=request["home"], isolated=request["isolated"],
                          executable=request["executable"])
        else:
            with patch("codex_process_supervisor.attach", capture):
                AppServer(root / "native", lambda *_: None, lambda *_: None, lambda *_: None,
                          home=request["home"], isolated=request["isolated"],
                          executable=request["executable"], supervisor_handle="account:default",
                          supervisor_root=root)
    except Captured:
        pass
