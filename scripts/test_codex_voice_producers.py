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


class VoiceResourcePublicationTests(unittest.IsolatedAsyncioTestCase):
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
