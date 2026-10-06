"""The entity sequence is announced once after its SQLite transaction commits."""

from __future__ import annotations

import asyncio
from pathlib import Path
import tempfile
import unittest

from codex_sqlite import connect
from codex_sync_entities import ensure_tables, put, register_functions
from studio_api.sync.resources.hub import ResourceHub, register_resource_hub, unregister_resource_hub
from studio_api.sync.resources.models import ResourceRef, StateResource


class EntityCommitPublisherTests(unittest.IsolatedAsyncioTestCase):
    async def test_entity_sequence_advances_publish_once_after_commit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            database = root / "canvas.sqlite3"
            hub = ResourceHub("workspace-a")
            register_resource_hub(root, hub)
            try:
                loop = asyncio.get_running_loop()
                state = ResourceRef(StateResource(kind="state"))
                subscription = hub.subscribe([state], loop=loop)
                db = connect(database, site="test.entity-write")
                register_functions(db)
                ensure_tables(db)
                with db:
                    put(db, "project", "/tmp/project", {"id": "/tmp/project", "name": "Project"})
                event = await subscription.next_event(timeout=2)
                self.assertIsNotNone(event)
                assert event is not None
                self.assertEqual(event.resources, [state])
                self.assertEqual(event.resourceVersions[0].revision, 1)
                with db:
                    put(db, "project", "/tmp/project", {"id": "/tmp/project", "name": "Project"})
                self.assertIsNone(await subscription.next_event(timeout=0.05))
                with db:
                    db.execute("UPDATE sync_entities SET hash=hash WHERE collection='project'")
                self.assertIsNone(await subscription.next_event(timeout=0.05))
                self.assertEqual(db.execute("SELECT max(seq) FROM sync_entities").fetchone()[0], 1)
                subscription.close()
                db.close()
            finally:
                unregister_resource_hub(root, hub)


if __name__ == "__main__":
    unittest.main()
