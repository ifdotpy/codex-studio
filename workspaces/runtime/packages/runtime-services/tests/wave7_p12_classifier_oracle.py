"""Small differential oracle for P12's production failure classifier."""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_usage_resume import _auth_error, _limit_error, error_kind

cases = json.load(sys.stdin)
out = []
for error, provider in cases:
    kind = error_kind(error)
    if _limit_error(error):
        cause = "usage_limit" if kind == "usageLimitExceeded" else "rate_limit"
    elif _auth_error(error, provider):
        cause = "auth"
    else:
        cause = None
    out.append(cause)
json.dump(out, sys.stdout)
