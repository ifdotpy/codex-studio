"""Shared paths, identity and bounded reads for the Codex agent CLIs."""
from __future__ import annotations

import glob
import json
import os
from pathlib import Path

ACTIVE = {"starting", "running", "waiting", "capacity-retry"}

def outside_claude(path: Path) -> Path:
    resolved = path.expanduser().resolve()
    if ".claude" in resolved.parts:
        raise RuntimeError(f"Codex agent data must be outside .claude: {resolved}")
    return resolved

def state_dir(legacy_key: str | None = None) -> Path:
    configured = os.environ.get("CODEX_AGENTS_STATE_DIR")
    if not configured and legacy_key:
        configured = os.environ.get(legacy_key)
    if configured:
        return outside_claude(Path(configured))
    base = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local" / "state")
    return outside_claude(base / "codex-agents")

def board_dir(root: Path | None = None) -> Path:
    configured = os.environ.get("CODEX_BOARD_STATE_DIR")
    return outside_claude(Path(configured) if configured else (root or state_dir()) / "board")

def codex_home() -> Path:
    return outside_claude(Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex"))

def process_is_alive(pid: int) -> bool:
    if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except PermissionError:
        return True
    except OSError:
        return False

def effective_status(thread: dict) -> str:
    status = thread.get("turnStatus", "unknown")
    if status in ACTIVE and not process_is_alive(thread.get("launcherPid")):
        return "abandoned"
    return status

def read_threads(root: Path, wave: str | None = None) -> list[dict]:
    paths = [root / f"codex-swarm-status.{wave}.json"] if wave else sorted(root.glob("codex-swarm-status.*.json"))
    threads = []
    for path in paths:
        try:
            records = json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            continue
        except (OSError, json.JSONDecodeError) as error:
            raise RuntimeError(f"cannot read {path}: {error}") from error
        if not isinstance(records, list) or not all(
            isinstance(row, dict) and isinstance(row.get("name"), str)
            and isinstance(row.get("threadId"), str) for row in records
        ):
            raise RuntimeError(f"invalid status file: {path}")
        file_wave = path.name.removeprefix("codex-swarm-status.").removesuffix(".json")
        for row in records:
            if row.get("wave", file_wave) != file_wave:
                raise RuntimeError(f"wave identity mismatch: {path}")
            threads.append({**row, "wave": file_wave})
    return threads

def final_answer(thread_id: str, limit: int) -> str:
    pattern = str(codex_home() / "sessions" / "*" / "*" / "*" / f"*{thread_id}*.jsonl")
    matches = glob.glob(pattern)
    if not matches:
        return "(no rollout file)"
    rollout = max(matches, key=os.path.getmtime)
    final = ""
    fallback = ""
    with open(rollout, encoding="utf-8") as handle:
        for line in handle:
            if '"role":"assistant"' not in line and '"role": "assistant"' not in line:
                continue
            try:
                payload = json.loads(line).get("payload", {})
            except json.JSONDecodeError:
                continue
            content = payload.get("content") or []
            chunk = "".join(
                item.get("text", "")
                for item in content
                if item.get("type") in ("output_text", "text")
            )
            if not chunk.strip():
                continue
            fallback = chunk
            if payload.get("phase") == "final_answer":
                final = chunk
    text = (final or fallback).strip()
    if not text:
        return "(no final answer yet)"
    if len(text) <= limit:
        return text
    return text[:limit] + f"\n[truncated, {len(text)} chars total]"
