#!/usr/bin/env python3
"""Voice mutations publish only after their transaction and caller locks exit."""

import asyncio
from contextlib import contextmanager
import sqlite3
import tempfile
import threading
import unittest
from pathlib import Path

from codex_voice import VoiceStore
from studio_api.sync.resources.hub import ResourceHub, register_resource_hub, unregister_resource_hub
from studio_api.sync.resources.models import ResourceRef, VoiceResource


class _VoiceRuntime:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.lock = threading.RLock()
        self.actors = {"lead": {"isLead": True, "epoch": 1, "autoWake": True}}
        self.active_transactions = 0
        self.fail_changed_transaction = False
        with self.db() as db:
            db.execute("CREATE TABLE runtime_events (id TEXT PRIMARY KEY)")

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.root / "voice.sqlite3")
        db.row_factory = sqlite3.Row
        self.active_transactions += 1
        try:
            yield db
            if self.fail_changed_transaction and db.total_changes:
                self.fail_changed_transaction = False
                raise RuntimeError("simulated voice transaction failure")
            db.commit()
        except BaseException:
            db.rollback()
            raise
        finally:
            self.active_transactions -= 1
            db.close()

    def agent(self, agent_id: str) -> dict[str, object]:
        return self.actors[agent_id]

    def put(self, db, table: str, record: dict[str, object]) -> None:
        return None

    def connection_current(self, account: str, connection: str) -> bool:
        return account == "default" and connection == "connection-a"


class VoiceResourcePublicationTests(unittest.IsolatedAsyncioTestCase):
    def test_cancel_waits_for_reserved_start_dispatch_then_stops(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            runtime = _VoiceRuntime(Path(directory))
            voice = VoiceStore(runtime)
            with runtime.db() as db:
                db.execute(
                    "INSERT INTO voice_sessions(id,agent,created,state) VALUES(?,?,?,?)",
                    ("session-race", "lead", 1.0, "connecting"),
                )
            context = {
                "agent": "lead", "thread": "thread-a", "account": "default",
                "connection": "connection-a", "server": object(), "cancel": False,
                "submitted": False, "start_dispatched": False,
                "dispatch_lock": threading.Lock(), "stopping": False,
            }
            voice.connections["session-race"] = context
            voice._assert_start_context = lambda _context, _db: {"epoch": 1}

            start_entered = threading.Event()
            release_start = threading.Event()
            cancel_published = threading.Event()
            calls: list[str] = []
            errors: list[BaseException] = []

            def controlled_submit(_server, method, _params, _callback) -> None:
                if method == "thread/realtime/start":
                    start_entered.set()
                    if not release_start.wait(2):
                        raise TimeoutError("test did not release reserved start")
                calls.append(method)

            voice._submit = controlled_submit
            voice._publish_voice = lambda _agent: cancel_published.set()

            def submit_start() -> None:
                try:
                    voice._submit_start("session-race", "v=0\\r\\n")
                except BaseException as error:
                    errors.append(error)

            start_thread = threading.Thread(target=submit_start)
            end_thread = threading.Thread(target=lambda: voice.end("lead", "session-race"))
            start_thread.start()
            try:
                self.assertTrue(start_entered.wait(1), "start did not reach RPC dispatch")
                end_thread.start()
                self.assertTrue(cancel_published.wait(1), "cancellation was not committed")
                self.assertEqual(calls, [], "stop must not overtake the paused start RPC")
                self.assertTrue(context["cancel"])
                self.assertEqual(voice.session("lead", "session-race")["state"], "stopping")
                release_start.set()
                start_thread.join(2)
                end_thread.join(2)
                self.assertFalse(start_thread.is_alive())
                self.assertFalse(end_thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(
                    calls,
                    ["thread/realtime/start", "thread/realtime/stop"],
                )

                self.assertTrue(voice.native_notification(
                    {
                        "method": "thread/realtime/closed",
                        "params": {"threadId": "thread-a", "reason": "requested"},
                    },
                    "default",
                    "connection-a",
                ))
                final = voice.session("lead", "session-race")
                self.assertEqual(final["state"], "ended")
                self.assertIsNotNone(final["ended"])
            finally:
                release_start.set()
                start_thread.join(2)
                if end_thread.ident is not None:
                    end_thread.join(2)

    async def test_native_callback_publishes_after_nested_commit_and_locks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = _VoiceRuntime(root)
            voice = VoiceStore(runtime)
            with runtime.db() as db:
                db.execute(
                    "INSERT INTO voice_sessions(id,agent,created,state) VALUES(?,?,?,?)",
                    ("session-a", "lead", 1.0, "ready"),
                )
            voice.connections["session-a"] = {
                "agent": "lead", "thread": "thread-a", "account": "default",
                "connection": "connection-a", "cancel": False,
            }

            loop = asyncio.get_running_loop()
            hub = ResourceHub("workspace-a")
            register_resource_hub(root, hub)
            subscription = hub.subscribe(
                [ResourceRef(VoiceResource(kind="voice", agentId="lead"))], loop=loop
            )
            original_publish = voice._publish_voice
            publication_checks: list[tuple[bool, int]] = []

            def publish_after_boundary(agent_id: str) -> None:
                acquired: list[bool] = []

                def check_locks() -> None:
                    runtime_acquired = runtime.lock.acquire(blocking=False)
                    native_acquired = voice.native_lock.acquire(blocking=False)
                    acquired.append(runtime_acquired and native_acquired)
                    if native_acquired:
                        voice.native_lock.release()
                    if runtime_acquired:
                        runtime.lock.release()

                checker = threading.Thread(target=check_locks)
                checker.start()
                checker.join(1)
                publication_checks.append((acquired == [True], runtime.active_transactions))
                original_publish(agent_id)

            voice._publish_voice = publish_after_boundary
            try:
                handled = voice.native_notification(
                    {
                        "method": "thread/realtime/item/completed",
                        "params": {
                            "threadId": "thread-a", "realtimeSessionId": "session-a",
                            "item": {
                                "id": "item-a", "type": "transcriptSegment",
                                "role": "user", "text": "committed transcript",
                            },
                        },
                    },
                    "default",
                    "connection-a",
                )
                self.assertTrue(handled)
                event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(event)
                assert event is not None
                self.assertEqual(
                    {resource.root.kind for resource in event.resources}, {"voice"}
                )
                self.assertEqual(publication_checks, [(True, 0)])

                runtime.fail_changed_transaction = True
                with self.assertRaisesRegex(RuntimeError, "simulated voice transaction failure"):
                    voice.native_notification(
                        {
                            "method": "thread/realtime/item/completed",
                            "params": {
                                "threadId": "thread-a", "realtimeSessionId": "session-a",
                                "item": {
                                    "id": "item-b", "type": "transcriptSegment",
                                    "role": "user", "text": "rolled back transcript",
                                },
                            },
                        },
                        "default",
                        "connection-a",
                    )
                self.assertIsNone(await subscription.next_event(timeout=0.05))
                self.assertEqual(publication_checks, [(True, 0)])
                self.assertEqual(voice.records("lead")["records"][-1]["id"], "native:session-a:item-a")
            finally:
                subscription.close()
                unregister_resource_hub(root, hub)


if __name__ == "__main__":
    unittest.main()
