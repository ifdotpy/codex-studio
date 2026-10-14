"""Filesystem oracle over an explicit synthetic Codex provider home."""
import base64
import json
import os
from pathlib import Path
import sys
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))

# The shared guard owns lexical checks. Redirect its private scratch HOME to the
# caller's tmpfs fixture as required by the Wave 9 host-isolation contract.
_temporary_directory = tempfile.TemporaryDirectory
def _tmpfs_temporary_directory(*args, **kwargs):
    if kwargs.get("dir") == "/var/tmp":
        kwargs["dir"] = os.environ["TMPDIR"]
    return _temporary_directory(*args, **kwargs)
tempfile.TemporaryDirectory = _tmpfs_temporary_directory
from oracle_guard import install_guard
install_guard(block_processes=True)

sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
import codex_context_repair as repair

home, source, thread_id, import_id, events_path, terminal_turn, output_path = sys.argv[1:]
events = json.loads(Path(events_path).read_text())
repair.uuid.uuid4 = lambda: uuid.UUID(import_id)
try:
    segments = repair._rollout_segments(Path(home), Path(source), thread_id)
    destination = Path(home) / "sessions/.studio-context-repairs/fixture" / Path(source).name
    destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    report = repair.sanitized_rollout(
        Path(source), destination, thread_id, events, terminal_turn, segments=segments
    )
    copy = Path(report["copyPath"]).read_bytes() if report.get("copyPath") else b""
    Path(output_path).write_bytes(copy)
    print(json.dumps({"refused": False, "report": report, "copy": base64.b64encode(copy).decode()}))
except Exception as exc:
    print(json.dumps({"refused": True, "error": type(exc).__name__}))
