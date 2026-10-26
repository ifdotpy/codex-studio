#!/usr/bin/env python3
"""Measure current source text. Do not create a runtime or access agent state."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(root / "scripts"))
from codex_runtime import Runtime, INSTRUCTIONS
from codex_progress import progress_context


def sizes(text):
    return {"utf8_bytes": len(text.encode()), "characters": len(text)}


result = {
    "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
    "unit": "UTF-8 bytes and characters, not model tokens",
    "path_policy": "source paths are current checkout; progress path uses synthetic state and UUID",
    "common_instructions": sizes(INSTRUCTIONS),
    "roles": {},
}
for name, lead in [("lead", True), ("worker", False)]:
    role = Runtime.role_guidance({"isLead": lead})
    tools = Runtime.tool_definitions({"isLead": lead})
    progress = progress_context("/example/studio-state", "00000000-0000-0000-0000-000000000000")
    compact = lambda value: json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    result["roles"][name] = {
        "role_guidance": sizes(role), "progress_guidance": sizes(progress),
        "tool_count": len(tools), "tool_catalog": sizes(compact(tools)),
        "tools": [{"name": tool["name"], **sizes(compact(tool))} for tool in tools],
        "first_turn_repeated_role_and_progress_bytes": len((role + progress).encode()),
    }
paths = ["scripts/codex_runtime.py", "scripts/codex_efficiency.py", "scripts/codex_progress.py",
         "scripts/codex_progress_layout.py", ".agents/skills/codex-orchestrator/SKILL.md",
         ".agents/skills/codex-subagent/SKILL.md"]
result["source_hashes"] = {path: hashlib.sha256((root / path).read_bytes()).hexdigest() for path in paths}
print(json.dumps(result, indent=2))
