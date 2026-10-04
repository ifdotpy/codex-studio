"""Exercise both state snapshot variants through the isolated UI fixture."""

from __future__ import annotations

import json
import os
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from urllib.request import Request, urlopen

from studio_api.sync.models import SnapshotAgentDto, SnapshotChatGroupDto, StateSnapshot


class StateFixtureResponseTests(unittest.TestCase):
    process: subprocess.Popen[str] | None = None
    temporary: tempfile.TemporaryDirectory[str] | None = None
    port: int

    @classmethod
    def setUpClass(cls) -> None:
        repository = Path(__file__).resolve().parents[3]
        cache = Path(os.environ.get("TMPDIR", "/tmp"))
        cls.temporary = tempfile.TemporaryDirectory(prefix="sync-state-fixture-", dir=cache)
        state_directory = str(Path(cls.temporary.name) / "state")
        environment = os.environ.copy()
        environment["PYTHONPATH"] = os.pathsep.join((str(repository / "scripts"), str(repository / "tests")))
        environment["TMPDIR"] = str(cache)
        cls.process = subprocess.Popen(
            [sys.executable, "-B", str(repository / "tests" / "simple-ui-fixture.py"), state_directory, "0"],
            cwd=repository,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        output = cls.process.stdout
        if output is None:
            cls._stop_fixture()
            raise RuntimeError("The isolated state fixture did not expose startup output")
        lines: queue.Queue[str] = queue.Queue()

        def collect_output() -> None:
            for line in output:
                lines.put(line)

        threading.Thread(target=collect_output, name="state-fixture-output", daemon=True).start()
        observed: list[str] = []
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if cls.process.poll() is not None and lines.empty():
                break
            try:
                line = lines.get(timeout=0.2).strip()
            except queue.Empty:
                continue
            observed.append(line)
            if line.isdecimal():
                cls.port = int(line)
                return
        cls._stop_fixture()
        raise RuntimeError("The isolated state fixture failed to start: " + " | ".join(observed[-8:]))

    @classmethod
    def tearDownClass(cls) -> None:
        cls._stop_fixture()

    @classmethod
    def _stop_fixture(cls) -> None:
        if cls.process is not None:
            if cls.process.poll() is None:
                cls.process.send_signal(signal.SIGINT)
                try:
                    cls.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    cls.process.kill()
                    cls.process.wait(timeout=5)
            if cls.process.stdout is not None:
                cls.process.stdout.close()
            cls.process = None
        if cls.temporary is not None:
            cls.temporary.cleanup()
            cls.temporary = None

    def test_full_and_chat_state_responses_validate_actual_runtime(self) -> None:
        base = f"http://127.0.0.1:{self.port}"
        with urlopen(f"{base}/api/state", timeout=20) as response:
            initial = StateSnapshot.model_validate_json(response.read())
        self.assertGreaterEqual(len(initial.threads), 2)
        chat_id = "00000000-0000-4000-8000-000000000001"
        create_chat = Request(
            f"{base}/api/chats",
            data=json.dumps({
                "id": chat_id,
                "name": "Snapshot fixture chat",
                "members": [initial.threads[0].id, initial.threads[1].id],
            }).encode(),
            headers={"Content-Type": "application/json", "X-Canvas-Token": initial.token},
            method="POST",
        )
        with urlopen(create_chat, timeout=20) as response:
            self.assertEqual(response.status, 200)

        snapshots: dict[str, StateSnapshot] = {}
        for name, path in (("full", "/api/state"), ("chat", "/api/state?view=chat")):
            with self.subTest(view=name), urlopen(f"{base}{path}", timeout=20) as response:
                self.assertEqual(response.status, 200)
                snapshot = StateSnapshot.model_validate_json(response.read())
                self.assertIsNotNone(snapshot.runtime)
                self.assertTrue(snapshot.threads)
                self.assertEqual([chat.id for chat in snapshot.chats], [chat_id])
                self.assertTrue(snapshot.nodes)
                self.assertTrue(any(isinstance(node, SnapshotAgentDto) for node in snapshot.nodes))
                self.assertTrue(any(isinstance(node, SnapshotChatGroupDto) for node in snapshot.nodes))
                snapshots[name] = snapshot
        full_runtime = snapshots["full"].runtime
        chat_runtime = snapshots["chat"].runtime
        assert full_runtime is not None
        assert chat_runtime is not None
        self.assertIsNotNone(full_runtime.work)
        self.assertIsNone(chat_runtime.work)


if __name__ == "__main__":
    unittest.main()
