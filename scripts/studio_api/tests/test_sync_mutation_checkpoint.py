"""The request boundary and response builder define mutation checkpoints."""
from __future__ import annotations

import json
import sqlite3
import unittest
from contextlib import closing
from types import SimpleNamespace
from typing import cast

from starlette.requests import Request
from studio_api.context import ApiContext
from studio_api.models import ResponseModel


class TestResponse(ResponseModel):
    paired: bool


class MutationCheckpointResponseTests(unittest.TestCase):
    def response(
        self,
        rows: list[tuple[str, str, int, str, int]],
        floor: int,
        after: int = 5,
    ) -> dict[str, object]:
        request = Request(
            {
                "type": "http",
                "asgi": {"version": "3.0"},
                "http_version": "1.1",
                "method": "GET",
                "scheme": "http",
                "path": "/api/test",
                "raw_path": b"/api/test",
                "query_string": b"",
                "root_path": "",
                "headers": [],
                "client": ("127.0.0.1", 50000),
                "server": ("127.0.0.1", 46000),
                "route": SimpleNamespace(response_model=TestResponse),
            }
        )
        request.scope["studio_sync_entities_after"] = after
        with closing(sqlite3.connect(":memory:")) as db:
            db.execute("CREATE TABLE sync_entities(collection, id, seq, payload, deleted)")
            db.execute("CREATE TABLE sync_entity_meta(key, value)")
            db.execute(
                "INSERT INTO sync_entity_meta VALUES ('entity_tombstone_floor', ?)",
                (str(floor),),
            )
            db.executemany("INSERT INTO sync_entities VALUES (?, ?, ?, ?, ?)", rows)
            context = SimpleNamespace(
                sync=lambda: SimpleNamespace(connect=lambda: db),
                _dump_json=ApiContext._dump_json,
                _accepts_gzip=ApiContext._accepts_gzip,
            )
            result = ApiContext.send(cast(ApiContext, context), request, {"paired": True})
            return cast(dict[str, object], json.loads(bytes(result.body)))

    def test_response_uses_sampled_entity_high_when_rows_fit(self) -> None:
        body = self.response([("agent", "a", 6, "{}", 0)], 5)
        self.assertEqual(body["_syncEntitiesAfter"], 5)

    def test_transcript_rows_between_entities_do_not_change_checkpoint(self) -> None:
        body = self.response(
            [
                ("transcript:agent", "transcript", 6, "{}", 0),
                ("agent", "a", 7, "{}", 0),
            ],
            5,
        )

        self.assertEqual(body["_syncEntitiesAfter"], 5)
        self.assertEqual(
            [row["seq"] for row in cast(list[dict[str, object]], body["_syncEntities"])],
            [7],
        )

    def test_response_omits_checkpoint_when_floor_passed_sample(self) -> None:
        body = self.response([("agent", "a", 8, "{}", 1)], 7)
        self.assertIn("_syncEntities", body)
        self.assertNotIn("_syncEntitiesAfter", body)

    def test_response_omits_checkpoint_when_batch_exceeds_cap(self) -> None:
        rows = [("agent", str(index), 6 + index, "{}", 0) for index in range(501)]
        body = self.response(rows, 5)
        self.assertEqual(len(cast(list[object], body["_syncEntities"])), 501)
        self.assertNotIn("_syncEntitiesAfter", body)

    def test_response_without_entity_rows_has_neither_field(self) -> None:
        body = self.response([], 5)
        self.assertNotIn("_syncEntities", body)
        self.assertNotIn("_syncEntitiesAfter", body)


if __name__ == "__main__":
    unittest.main()
