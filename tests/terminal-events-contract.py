#!/usr/bin/env python3
"""Terminal output and lifecycle changes publish committed typed resources."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
import codecs
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse
from fastapi.testclient import TestClient

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_terminals import HISTORY_LIMIT, TerminalManager
from studio_api.io.router import create_router
from studio_api.sync.resources.models import TerminalResource, TerminalsResource


class TerminalContext:
    def __init__(self, manager: TerminalManager) -> None:
        self.manager = manager
        self.runtime = None

    def terminals(self) -> TerminalManager:
        return self.manager

    def send(self, _request: Request, value: object, **_kwargs: object) -> JSONResponse:
        return JSONResponse(value)


class TerminalResourceEvents(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="terminal-events-")
        self.manager = TerminalManager(self.temp.name)
        self.key = "terminal-event-test"
        with self.manager.db() as db:
            db.execute(
                "INSERT INTO user_terminals VALUES (?,?,?,?)",
                (self.key, json.dumps({
                    "id": self.key, "title": "Terminal", "status": "running", "created": 1,
                }), "", 0),
            )
        self.owned = {
            "decoder": codecs.getincrementaldecoder("utf-8")("replace"),
            "output_lock": threading.RLock(),
            "pid_path": Path(self.temp.name) / "missing-terminal.pid",
            "server": None,
        }
        self.manager.processes[self.key] = self.owned
        app = FastAPI()
        app.include_router(create_router(TerminalContext(self.manager)))
        self.client = TestClient(app)

    def tearDown(self):
        self.client.close()
        self.manager.close()
        self.temp.cleanup()

    def test_output_notification_publishes_only_after_visible_commit_and_without_terminal_lock(self):
        published = threading.Event()
        release = threading.Event()
        observed = []

        def publish(*resources):
            acquired = self.manager.lock.acquire(blocking=False)
            self.assertTrue(acquired, "resource publication ran while the terminal lock was held")
            if acquired:
                self.manager.lock.release()
            observed.append(self.client.get(
                "/api/terminals/output", params={"id": self.key, "offset": "0"}
            ).json())
            observed.append(resources)
            published.set()
            self.assertTrue(release.wait(2))

        with patch.object(self.manager, "_publish", side_effect=publish):
            worker = threading.Thread(target=self.manager.notification, args=({
                "method": "process/outputDelta",
                "params": {"processHandle": self.key,
                           "deltaBase64": base64.b64encode("hello🙂".encode()).decode()},
            },))
            worker.start()
            self.assertTrue(published.wait(2), "terminal output did not publish an event")
            self.assertEqual(observed[0]["text"], "hello🙂")
            self.assertEqual(observed[0]["offset"], len("hello🙂"))
            self.assertEqual(len(observed[1]), 1)
            self.assertIsInstance(observed[1][0].root, TerminalResource)
            self.assertEqual(observed[1][0].root.terminalId, self.key)
            release.set()
            worker.join(2)
            self.assertFalse(worker.is_alive())

    def test_close_publishes_list_and_targeted_terminal_resources_after_commit(self):
        events = []

        def publish(*resources):
            self.assertTrue(self.manager.lock.acquire(blocking=False))
            self.manager.lock.release()
            events.append((self.client.get("/api/terminals").json(), resources))

        with patch.object(self.manager, "_publish", side_effect=publish):
            result = self.manager.action("close", {"id": self.key})

        self.assertEqual(result["status"], "closed")
        self.assertEqual(events[0][0]["items"], [])
        roots = [resource.root for resource in events[0][1]]
        self.assertTrue(any(isinstance(resource, TerminalsResource) for resource in roots))
        terminal = next(resource for resource in roots if isinstance(resource, TerminalResource))
        self.assertEqual(terminal.terminalId, self.key)

    def test_rename_publishes_list_after_updated_title_is_readable(self):
        observed = []

        def publish(*resources):
            observed.append(self.client.get("/api/terminals").json())
            observed.append(resources)

        with patch.object(self.manager, "_publish", side_effect=publish):
            self.manager.action("rename", {"id": self.key, "title": "Build logs"})

        self.assertEqual(observed[0]["items"][0]["title"], "Build logs")
        self.assertTrue(any(isinstance(resource.root, TerminalsResource) for resource in observed[1]))

    def test_duplicate_supervisor_sequence_does_not_publish_again(self):
        with self.manager.db() as db:
            db.execute("INSERT INTO user_terminal_event_cursor VALUES (?,?)", (self.key, 7))
        with patch.object(self.manager, "_publish") as publish:
            self.manager.notification({
                "method": "process/outputDelta",
                "params": {"processHandle": self.key,
                           "deltaBase64": base64.b64encode(b"duplicate").decode()},
                "_studioSupervisorSequence": 7,
            })
        publish.assert_not_called()
        self.assertEqual(self.manager.output(self.key)["text"], "")

    def test_live_view_truncation_keeps_cursor_replay_and_publishes_terminal(self):
        with self.manager.db() as db:
            db.execute(
                "UPDATE user_terminals SET output=?,offset=0 WHERE id=?",
                ("x" * HISTORY_LIMIT, self.key),
            )
        observed = []

        def publish(*resources):
            observed.append(self.client.get(
                "/api/terminals/output", params={"id": self.key, "offset": "0"}
            ).json())
            observed.append(resources)

        with patch.object(self.manager, "_publish", side_effect=publish):
            self.manager.append(self.key, "tail")

        self.assertTrue(observed[0]["truncated"])
        self.assertEqual(observed[0]["offset"], HISTORY_LIMIT + 4)
        self.assertTrue(observed[0]["text"].endswith("tail"))
        self.assertEqual(len(observed[1]), 1)
        self.assertIsInstance(observed[1][0].root, TerminalResource)


if __name__ == "__main__":
    unittest.main(verbosity=2)
