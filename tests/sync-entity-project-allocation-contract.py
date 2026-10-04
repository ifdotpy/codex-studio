"""Keep agent projection filtering, nested ownership, and truncation stable."""
from pathlib import Path
import sys
from typing import cast

root = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(root / "scripts"))

from codex_sync_entities import project


source = {
    "id": "worker-1",
    "name": "Worker",
    "status": "running",
    "tail": "t" * 3000,
    "prompt": "p" * 5000,
    "lastAnswer": "a" * 5000,
    "lastCompletedTurn": "turn-1",
    "activity": {
        "phase": "tool",
        "at": 1.0,
        "tools": [
            {"id": f"tool-{index}", "type": "function", "name": f"Tool {index}"}
            for index in range(205)
        ],
        "private": "not renderer-owned",
    },
    "nativeStatus": {
        "phase": "auth",
        "error": "e" * 5000,
        "message": "m" * 5000,
        "turnId": "turn-1",
        "private": "not renderer-owned",
    },
    "nativeSafetyRetry": {
        "id": "retry-1",
        "stage": "running",
        "error": "r" * 5000,
        "private": "not renderer-owned",
    },
    "nativeRelease": {
        "phase": "released",
        "resetPending": True,
        "private": "not renderer-owned",
    },
    "overview": {
        "task": "q" * 14000,
        "taskTruncated": True,
        "result": "z" * 14000,
        "resultTruncated": True,
        "resultTurnId": "turn-1",
        "private": "not renderer-owned",
    },
    "privateData": {"secret": "must not cross the renderer boundary"},
}

projected = project("agent", source)
assert isinstance(projected, dict)
assert projected["tail"] == "t" * 2000
overview = cast(dict[str, object], projected["overview"])
native_status = cast(dict[str, object], projected["nativeStatus"])
safety_retry = cast(dict[str, object], projected["nativeSafetyRetry"])
activity = cast(dict[str, object], projected["activity"])
tools = cast(list[dict[str, object]], activity["tools"])
assert overview["task"] == "q" * 12000
assert overview["result"] == "z" * 12000
assert overview["taskTruncated"] is True
assert overview["resultTruncated"] is True
assert native_status["error"] == "e" * 2000
assert native_status["message"] == "m" * 2000
assert safety_retry["error"] == "r" * 2000
assert projected["nativeRelease"] == {"phase": "released", "resetPending": True}
assert len(tools) == 200
assert "private" not in activity
assert "private" not in native_status
assert "private" not in safety_retry
assert "private" not in overview
assert "privateData" not in projected

# Projection values are renderer-owned copies, not aliases into runtime state.
assert activity is not source["activity"]
assert tools is not source["activity"]["tools"]
assert tools[0] is not source["activity"]["tools"][0]
tools[0]["name"] = "changed in renderer DTO"
assert source["activity"]["tools"][0]["name"] == "Tool 0"
