"""Throwaway Studio state for the opt-in live checks (runtime-live.py and sign-in-live.py).

A live check uses the real provider sign-ins, so it cannot install the home guard of
test_isolation.py. Its Runtime must still stay away from the running Studio. Without
CODEX_AGENTS_STATE_DIR, the Runtime helpers resolve the default state directory
(codex_state.state_dir: ~/.local/state/codex-agents). That is the live state of the running
Studio, and Python refuses it after the Rust cutover.
"""
import atexit
from contextlib import ExitStack
import ast
import os
from pathlib import Path
import tempfile


_TEMPORARY_STATE = ExitStack()
atexit.register(_TEMPORARY_STATE.close)

def isolate_live_environment() -> None:
    """Remove the supervisor routing names (`SUPERVISOR_ENV` in test_isolation.py, read
    without its home guard) and give the check a private image workspace store. Call it
    before the first Studio import."""
    source = ast.parse(Path(__file__).with_name("test_isolation.py").read_text(encoding="utf-8"))
    names = next(ast.literal_eval(node.value) for node in source.body if isinstance(node, ast.Assign)
                 and any(getattr(target, "id", None) == "SUPERVISOR_ENV" for target in node.targets))
    for name in names:
        os.environ.pop(name, None)
    os.environ["CODEX_WORKSPACE_STORE"] = _TEMPORARY_STATE.enter_context(tempfile.TemporaryDirectory(prefix="live-check-store-"))


def live_state_root(prefix: str) -> Path:
    """A new empty state folder that every state path of the Runtime and its helpers uses.
    It replaces an inherited CODEX_AGENTS_STATE_DIR. The folder is short because a macOS Unix
    socket path has at most 103 bytes."""
    root = Path(_TEMPORARY_STATE.enter_context(tempfile.TemporaryDirectory(prefix=prefix))).resolve()
    os.environ["CODEX_AGENTS_STATE_DIR"] = str(root)
    return root
