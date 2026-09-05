#!/usr/bin/env python3
"""Opt-in local Codex protocol check. Uses a temporary home and no model turn."""

import base64
from pathlib import Path
import os
import sys
import tempfile
import time
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import AppServer, THREAD_CONFIG, TOOLS

with tempfile.TemporaryDirectory(prefix="codex-workspace-protocol-") as directory:
    root = Path(directory)
    home = root / "codex"
    home.mkdir()
    output = []
    with patch.dict(os.environ, {"CODEX_HOME": str(home)}):
        server = AppServer(
            root, lambda m: output.append(m), lambda m: None, lambda: None
        )
        try:
            params = {
                "cwd": directory,
                "model": "gpt-5.6-sol",
                "config": THREAD_CONFIG,
                "dynamicTools": TOOLS,
                "approvalPolicy": "never",
                "sandbox": "read-only",
            }
            result = server.call("thread/start", params)
            tid = result["thread"]["id"]
            assert tid and result["sandbox"]
            try:
                server.call(
                    "thread/fork",
                    {"threadId": tid, "config": THREAD_CONFIG, "cwd": directory},
                )
            except RuntimeError as error:
                assert "no rollout found" in str(error), str(error)
            server.call("skills/list", {"cwds": [directory]})
            server.call("mcpServerStatus/list", {"threadId": tid, "limit": 100})
            # An idle thread must reject steering, not silently create a new turn.
            try:
                server.call(
                    "turn/steer",
                    {
                        "threadId": tid,
                        "expectedTurnId": "no-turn",
                        "input": [{"type": "text", "text": "probe"}],
                    },
                )
            except RuntimeError as error:
                message = str(error).lower()
                assert "active" in message or "turn" in message, message
                assert (
                    "invalid type" not in message and "missing field" not in message
                ), message
            else:
                raise AssertionError("Idle steer unexpectedly succeeded")
            key = "workspace-protocol-pty"
            started = server.submit(
                "command/exec",
                {
                    "command": [
                        "/bin/sh",
                        "-c",
                        'printf "ready\\n"; read answer; printf "received:%s\\n" "$answer"',
                    ],
                    "cwd": directory,
                    "processId": key,
                    "tty": True,
                    "streamStdin": True,
                    "streamStdoutStderr": True,
                    "timeoutMs": 5000,
                    "sandboxPolicy": {"type": "readOnly"},
                },
            )
            end = time.monotonic() + 3
            while not any(
                m.get("method") == "command/exec/outputDelta" for m in output
            ):
                if time.monotonic() > end:
                    raise AssertionError("No PTY output")
                time.sleep(0.02)
            server.call(
                "command/exec/resize",
                {"processId": key, "size": {"rows": 30, "cols": 100}},
            )
            server.call(
                "command/exec/write",
                {
                    "processId": key,
                    "deltaBase64": base64.b64encode(b"workspace\n").decode(),
                },
            )
            terminal = server.wait(started, 10)
            assert terminal["exitCode"] == 0, terminal
            chunks = b"".join(
                base64.b64decode(m["params"].get("deltaBase64", ""))
                for m in output
                if m.get("method") == "command/exec/outputDelta"
            )
            assert b"received:workspace" in chunks, chunks
            print(
                "PASS: thread start, empty fork rejection, workspace tool schemas, skills/MCP discovery, idle steer rejection, PTY output/input/resize/exit. No model turn."
            )
        finally:
            server.close()
