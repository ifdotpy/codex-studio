"""A provider receives a native folder only after durable Layr preparation."""
from contextlib import contextmanager
from pathlib import Path
import threading
import tempfile
import unittest
from typing import Any, Iterator
from unittest.mock import Mock, patch
import json

from codex_native_layr_agents import prepare, command_environment


class Runtime:
    def __init__(self, agents: dict[str, dict[str, Any]]) -> None:
        self.agents = agents
        self.lock = threading.RLock()

    def agent(self, key: str, db: Any = None) -> dict[str, Any]:
        return dict(self.agents[key])

    @contextmanager
    def db(self) -> Iterator["Runtime"]:
        yield self

    def put(self, db: Any, table: str, record: dict[str, Any]) -> None:
        self.agents[record["id"]] = dict(record)


class NativePrepareTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.source = Path(self.temp.name).resolve()

    def test_lead_keeps_original_path(self) -> None:
        native = Mock()
        native.store, native.socket = Path("/store"), None
        native.ensure_project.return_value = {"project": "project", "line": "main", "cwd": str(self.source)}
        native.command.return_value = json.dumps({"path": str(self.source), "line_id": "line-id", "state": "state"})
        runtime = Runtime({"lead": {"id": "lead", "isLead": True, "cwd": str(self.source)}})
        with patch("codex_native_layr_agents.client", return_value=native):
            result = prepare(runtime, runtime.agent("lead"))
        self.assertEqual(result["cwd"], str(self.source))
        self.assertEqual(result["environment"], "host")
        self.assertTrue(result["nativeLayrReady"])
        self.assertFalse(result["worktree"])
        self.assertEqual(command_environment(result)["LAYR_ROOT"], "/store")
        self.assertEqual(command_environment(result)["LAYR_DIRECT"], "1")

    def test_worker_base_survives_unknown_creation(self) -> None:
        native = Mock()
        native.command.return_value = "saved-base"
        native.ensure_worker.side_effect = TimeoutError("lost response")
        runtime = Runtime({"lead": {"id": "lead", "cwd": str(self.source), "nativeLayrReady": True,
                                    "nativeLayrProject": "project"},
                           "worker": {"id": "worker", "parentId": "lead", "cwd": str(self.source)}})
        with patch("codex_native_layr_agents.client", return_value=native):
            for _ in range(2):
                with self.assertRaises(TimeoutError):
                    prepare(runtime, runtime.agent("worker"))
        self.assertEqual(native.command.call_count, 1)
        self.assertEqual(runtime.agent("worker")["layrBaseState"], "saved-base")
        self.assertEqual(native.ensure_worker.call_args.args[-1], "saved-base")
        self.assertNotIn("nativeLayrReady", runtime.agent("worker"))

    def test_replaced_line_does_not_rebind_chat(self) -> None:
        native = Mock()
        native.ensure_project.return_value = {"project": "project", "line": "main", "cwd": str(self.source)}
        native.command.return_value = json.dumps({"path": str(self.source), "line_id": "other", "state": "state"})
        runtime = Runtime({"lead": {"id": "lead", "isLead": True, "cwd": str(self.source), "nativeLayrLineId": "original"}})
        with patch("codex_native_layr_agents.client", return_value=native):
            with self.assertRaisesRegex(RuntimeError, "replaced"):
                prepare(runtime, runtime.agent("lead"))
        self.assertEqual(runtime.agent("lead")["nativeLayrLineId"], "original")

    def test_monitor_uses_chat_environment_after_shell_startup(self) -> None:
        import os
        import subprocess
        from codex_native_layr_agents import managed_command
        agent = {"id": "a", "workspaceMode": "layr-native", "nativeLayrReady": True,
                 "nativeLayrRoot": "/store with spaces/$(false)",
                 "nativeLayrProject": "project", "layrLine": "main"}
        command = managed_command(agent, 'printf "%s|%s|%s" "$LAYR_ROOT" "$LAYR_LINE" "${LAYR_SOCKET-unset}"')
        result = subprocess.run(["/bin/sh", "-c", command], check=True, text=True,
                                capture_output=True, env={**os.environ, "LAYR_SOCKET": "/other", "LAYR_LINE": "other"})
        self.assertEqual(result.stdout, "/store with spaces/$(false)|main|unset")
        self.assertEqual(managed_command({"workspaceMode": "worktree"}, "echo old"), "echo old")


if __name__ == "__main__":
    unittest.main()
