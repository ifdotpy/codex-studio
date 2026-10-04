#!/usr/bin/env python3
"""Terminal output and lifecycle changes publish committed typed resources."""

import asyncio
import base64
import codecs
from concurrent.futures import Future
from contextlib import contextmanager
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse
from fastapi.testclient import TestClient

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_terminals import HISTORY_LIMIT, TerminalManager
from codex_canvas import Canvas
from studio_api.context import ApiContext
from studio_api.io.router import create_router
from studio_api.sync.resources.hub import (
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import (
    ResourceChangeEvent,
    ResourceRef,
    TerminalResource,
    TerminalsResource,
)


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
            output_lock_acquired = self.owned["output_lock"].acquire(blocking=False)
            self.assertTrue(output_lock_acquired, "output publication ran while its terminal lock was held")
            if output_lock_acquired:
                self.owned["output_lock"].release()
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
        first = base64.b64encode(b"\xe2").decode()
        continuation = base64.b64encode(b"\x82\xac").decode()
        self.manager.output_delta(self.key, self.owned, first, 7)
        self.assertEqual(self.owned["decoder"].getstate()[0], b"\xe2")
        with patch.object(self.manager, "_publish") as publish:
            self.manager.notification({
                "method": "process/outputDelta",
                "params": {"processHandle": self.key, "deltaBase64": first},
                "_studioSupervisorSequence": 7,
            })
            self.assertEqual(self.owned["decoder"].getstate()[0], b"\xe2")
            self.manager.output_delta(self.key, self.owned, continuation, 8)
        self.assertEqual(self.manager.output(self.key)["text"], "€")
        publish.assert_called_once()

    def test_output_and_exit_race_preserves_output_and_publishes_final_state(self):
        decode_started = threading.Event()
        continue_decode = threading.Event()
        output_thread_done = threading.Event()
        exit_thread_started = threading.Event()
        events = []

        class BlockingDecoder:
            def __init__(self):
                self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

            def decode(self, value, final=False):
                if value:
                    decode_started.set()
                    if not continue_decode.wait(2):
                        raise TimeoutError("Output decode was not released")
                return self.decoder.decode(value, final=final)

            def getstate(self):
                return self.decoder.getstate()

            def setstate(self, state):
                self.decoder.setstate(state)

        self.owned["decoder"] = BlockingDecoder()

        def publish(*resources):
            output = self.client.get(
                "/api/terminals/output", params={"id": self.key}
            ).json()
            events.append((output, resources))

        def output_delta():
            self.manager.output_delta(
                self.key, self.owned, base64.b64encode(b"parallel").decode(), 1
            )
            output_thread_done.set()

        def finish():
            exit_thread_started.set()
            self.manager.finish(self.key, 0)

        with patch.object(self.manager, "_publish", side_effect=publish):
            output_thread = threading.Thread(target=output_delta)
            exit_thread = threading.Thread(target=finish)
            output_thread.start()
            self.assertTrue(decode_started.wait(2))
            exit_thread.start()
            self.assertTrue(exit_thread_started.wait(2))
            continue_decode.set()
            output_thread.join(2)
            exit_thread.join(2)

        self.assertTrue(output_thread_done.is_set())
        self.assertFalse(output_thread.is_alive())
        self.assertFalse(exit_thread.is_alive())
        final = self.client.get("/api/terminals/output", params={"id": self.key}).json()
        self.assertEqual(final["text"], "parallel")
        self.assertEqual(final["status"], "exited")
        self.assertEqual(len(events), 2)
        self.assertTrue(all(isinstance(resources[0].root, TerminalResource) for _, resources in events))

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


class TerminalHubIntegration(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="terminal-hub-")
        self.canvas = Canvas(Path(self.temp.name))
        self.context = ApiContext(self.canvas, token="terminal-hub-test")
        self.context.initialize()
        # Match server startup: the context registers the hub before any
        # terminal manager can publish mutations.
        self.manager = TerminalManager(self.canvas.root)

    def tearDown(self):
        hub = getattr(self.context, "_resource_hub", None)
        if hub is not None:
            unregister_resource_hub(self.canvas.root, hub)
            hub.close()
        self.manager.close()
        self.temp.cleanup()

    async def test_startup_registration_delivers_actual_terminal_producer_event(self):
        hub = self.context.resource_hub()
        self.assertIsInstance(hub, ResourceHub)
        terminal_id = "integrated-terminal"
        with self.manager.db() as db:
            db.execute(
                "INSERT INTO user_terminals VALUES (?,?,?,?)",
                (terminal_id, json.dumps({
                    "id": terminal_id,
                    "title": "Terminal",
                    "status": "running",
                    "created": 1,
                }), "", 0),
            )

        terminal_ref = ResourceRef(TerminalResource(kind="terminal", terminalId=terminal_id))
        terminals_ref = ResourceRef(TerminalsResource(kind="terminals"))
        subscription = hub.subscribe(
            [terminal_ref, terminals_ref], loop=asyncio.get_running_loop()
        )
        try:
            self.assertEqual(subscription.initial.reason, "initial")
            self.manager.append(terminal_id, "arrived")
            event = await subscription.next_event(timeout=1)
            self.assertIsInstance(event, ResourceChangeEvent)
            assert event is not None
            self.assertEqual(event.reason, "change")
            self.assertEqual(event.resources, [terminal_ref])
            self.assertEqual(self.manager.output(terminal_id)["text"], "arrived")

            self.manager.action("close", {"id": terminal_id})
            lifecycle = await subscription.next_event(timeout=1)
            self.assertIsInstance(lifecycle, ResourceChangeEvent)
            assert lifecycle is not None
            self.assertEqual(
                {resource.root.kind for resource in lifecycle.resources},
                {"terminal", "terminals"},
            )
            self.assertEqual(self.manager.listing()["items"], [])
        finally:
            subscription.close()

    async def test_terminal_producer_is_safe_before_any_hub_is_registered(self):
        hub = self.context.resource_hub()
        unregister_resource_hub(self.canvas.root, hub)
        terminal_id = "pre-hub-terminal"
        with self.manager.db() as db:
            db.execute(
                "INSERT INTO user_terminals VALUES (?,?,?,?)",
                (terminal_id, json.dumps({
                    "id": terminal_id,
                    "title": "Terminal",
                    "status": "running",
                    "created": 1,
                }), "", 0),
            )

        self.manager.append(terminal_id, "committed without a hub")
        self.assertEqual(self.manager.output(terminal_id)["text"], "committed without a hub")
        register_resource_hub(self.canvas.root, hub)

    async def test_publisher_failure_during_create_does_not_resubmit_spawn(self):
        class Runtime:
            lock = threading.RLock()

            @contextmanager
            def db(self):
                yield object()

            def checked_actor(self, _db, _agent):
                return {"id": "agent", "cwd": str(Path(self_dir).resolve())}

        class Server:
            closed = False
            transport_error = None
            proc = Mock()
            proc.poll.return_value = None

            def __init__(self):
                self.spawn_count = 0

            def submit(self, method, _params, *, operation_id):
                self.assert_spawn(method, operation_id)
                self.spawn_count += 1
                return (method, operation_id, Future())

            @staticmethod
            def assert_spawn(method, operation_id):
                if method != "process/spawn" or not operation_id.startswith("terminal-spawn:"):
                    raise AssertionError("unexpected native submission")

            def on_result(self, _submitted, _callback):
                pass

            def wait(self, _submitted, timeout):
                if timeout != 10:
                    raise AssertionError("unexpected spawn wait")

            def close(self):
                pass

            @staticmethod
            def join_callbacks():
                return True

        hub = self.context.resource_hub()
        server = Server()
        self_dir = self.temp.name
        with patch.object(self.manager, "connect", return_value=server):
            self.manager.connection = object()
            self.manager.server = server
            with patch.object(hub, "publish_many", side_effect=RuntimeError("test publisher failure")):
                with self.assertLogs("studio_api.sync.resources.hub", level="ERROR"):
                    created = self.manager.create(Runtime(), {"id": "create-once", "agent": "agent"})

        self.assertEqual(created["status"], "running")
        self.assertEqual(server.spawn_count, 1)
        self.assertEqual(self.manager.listing()["items"][0]["id"], created["id"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
