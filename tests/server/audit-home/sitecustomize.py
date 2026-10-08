"""Optional audit hook that prevents Python suites touching the real user state."""

from pathlib import Path
import os
import sys


if os.environ.get("CODEX_SERVER_TEST_AUDIT_HOME") == "1":
    real_home = Path(os.environ["CODEX_SERVER_TEST_REAL_HOME"]).resolve()
    protected = tuple(
        (real_home / relative).resolve()
        for relative in (".codex", ".claude", ".local/state/codex-agents")
    )

    def reject_real_home(event, args):
        if event not in {"open", "os.mkdir"} or not args:
            return
        target = args[0]
        if not isinstance(target, (str, bytes, os.PathLike)):
            return
        try:
            path = Path(os.fsdecode(target)).resolve()
        except (OSError, RuntimeError, ValueError):
            return
        for root in protected:
            if path == root or root in path.parents:
                raise PermissionError(
                    f"server test real-home audit blocked {event}: {path}"
                )

    sys.addaudithook(reject_real_home)
