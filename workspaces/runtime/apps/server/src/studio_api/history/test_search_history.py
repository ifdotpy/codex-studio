"""Behavior and lock-scope tests for transcript search."""

from __future__ import annotations

import json
from contextlib import contextmanager
from pathlib import Path
import sqlite3
import sys
import threading
import unittest
from collections.abc import Iterable
from typing import Iterator, cast
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPTS))


class SearchRuntime:
    def __init__(self, connection: sqlite3.Connection) -> None:
        self.connection = connection
        self.lock = threading.RLock()

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        yield self.connection

    @contextmanager
    def read_db(self) -> Iterator[sqlite3.Connection]:
        yield self.connection

    @staticmethod
    def checked_actor(_db: sqlite3.Connection, _agent: str) -> dict[str, str]:
        return {"id": "agent-1"}


def legacy_search_history(
    runtime: SearchRuntime, agent: str, query: str, limit: int = 100
) -> dict[str, object]:
    """Original implementation retained only as the exact behavior reference."""
    query = str(query or "").strip()
    if not query or len(query) > 500:
        raise ValueError("Enter a search query of 1 to 500 characters")
    limit = max(1, min(500, int(limit)))
    results = []
    with runtime.lock, runtime.db() as db:
        runtime.checked_actor(db, agent)
        rows = db.execute(
            "SELECT i.id,i.record FROM runtime_items i WHERE i.agent=? "
            "AND json_extract(i.record,'$.afterRestore') IS NULL "
            "ORDER BY i.created DESC,i.id DESC",
            (agent,),
        ).fetchall()
        from codex_search_text import search_texts

        bodies = search_texts(db, (row["id"] for row in rows))
        for row in rows:
            item = json.loads(row["record"])
            entries = item.get("inputs")
            candidates = []
            if entries is not None:
                for index, entry in enumerate(entries):
                    event = db.execute(
                        "SELECT text FROM runtime_events WHERE id=? AND agent=?",
                        (entry.get("id"), agent),
                    ).fetchone()
                    if event is None and entry.get("truncated"):
                        raise ValueError(
                            "The full transcript input is unavailable: " + str(entry.get("id"))
                        )
                    candidates.append(
                        {
                            **item,
                            **entry,
                            "id": agent + ":" + entry["id"]
                            if entry.get("id")
                            else item["id"] + ":" + str(index),
                            "sourceId": item["id"],
                            "clientMessageId": entry.get("id"),
                            "role": "user"
                            if entry.get("kind") == "user"
                            else item["role"],
                            "text": event[0] if event else entry.get("text", ""),
                        }
                    )
            else:
                if item.get("truncated") and row["id"] not in bodies:
                    raise ValueError(
                        "The complete transcript item is unavailable: " + row["id"]
                    )
                candidates.append(
                    {
                        **item,
                        "sourceId": item["id"],
                        "text": bodies.get(row["id"], item.get("text", "")),
                    }
                )
            for candidate in candidates:
                text = candidate["text"]
                position = text.casefold().find(query.casefold())
                if position < 0:
                    continue
                results.append(
                    {
                        key: candidate.get(key)
                        for key in (
                            "id",
                            "sourceId",
                            "clientMessageId",
                            "role",
                            "turnId",
                            "text",
                        )
                    }
                    | {
                        "agent": agent,
                        "excerpt": text[
                            max(0, position - 80) : position + len(query) + 160
                        ],
                    }
                )
                if len(results) > limit:
                    return {"results": results[:limit], "truncated": True}
    return {"results": results, "truncated": False}


def make_history() -> sqlite3.Connection:
    db = sqlite3.connect(":memory:", check_same_thread=False)
    db.row_factory = sqlite3.Row
    db.executescript(
        "CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL);"
        "CREATE TABLE runtime_events(id TEXT,agent TEXT,text TEXT);"
        "CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT);"
    )
    texts = [
        "ASCII alpha beta",
        "Mixed CASE Alpha",
        "Straße and STRASSE",
        "İstanbul; naïve café",
        "punctuation!?[]_+ and CJK 東京",
        "single a and short x",
        "embedded needle inside needlessly",
        "no searchable term here",
    ]
    records = []
    for index in range(96):
        text = texts[index % len(texts)]
        if index % 3 == 0:
            records.append(
                (
                    f"item-{index:03d}",
                    "agent-1",
                    json.dumps(
                        {
                            "id": f"item-{index:03d}",
                            "role": "assistant",
                            "turnId": f"turn-{index // 2}",
                            "inputs": [
                                {"id": f"event-{index}", "kind": "user", "text": text}
                            ],
                        },
                    ),
                    float(index),
                )
            )
        else:
            records.append(
                (
                    f"item-{index:03d}",
                    "agent-1",
                    json.dumps(
                        {
                            "id": f"item-{index:03d}",
                            "role": "assistant",
                            "turnId": f"turn-{index // 2}",
                            "text": text,
                        },
                    ),
                    float(index),
                )
            )
        if index % 3 == 0:
            db.execute(
                "INSERT INTO runtime_events VALUES (?,?,?)",
                (f"event-{index}", "agent-1", text),
            )
    db.executemany("INSERT INTO runtime_items VALUES (?,?,?,?)", records)
    return db


class SearchHistoryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.connection = make_history()
        self.runtime = SearchRuntime(self.connection)

    def tearDown(self) -> None:
        self.connection.close()

    def test_search_matches_legacy_results_for_generated_histories(self) -> None:
        from codex_transcript_history import search_history

        for query in (
            "alpha",
            "Alpha",
            "STRASSE",
            "straße",
            "İSTANBUL",
            "東京",
            "!?[]_+",
            "a",
            "x",
            "needle",
            "no such match",
        ):
            with self.subTest(query=query):
                self.assertEqual(
                    search_history(self.runtime, "agent-1", query, limit=12),
                    legacy_search_history(self.runtime, "agent-1", query, limit=12),
                )

    def test_runtime_lock_is_available_while_text_scan_is_blocked(self) -> None:
        from codex_search_text import search_texts
        from codex_transcript_history import search_history

        scanning = threading.Event()
        resume_scan = threading.Event()
        other_operation_acquired = threading.Event()
        errors: list[BaseException] = []

        def blocked_search_texts(
            db: sqlite3.Connection, refs: Iterable[str]
        ) -> dict[str, str]:
            scanning.set()
            if not resume_scan.wait(timeout=5):
                raise TimeoutError("test did not release blocked search")
            return cast(dict[str, str], search_texts(db, refs))

        def run_search() -> None:
            try:
                with patch("codex_search_text.search_texts", blocked_search_texts):
                    search_history(self.runtime, "agent-1", "absent", limit=12)
            except BaseException as error:
                errors.append(error)

        def acquire_runtime_lock() -> None:
            with self.runtime.lock:
                other_operation_acquired.set()

        search_thread = threading.Thread(target=run_search)
        lock_thread = threading.Thread(target=acquire_runtime_lock)
        try:
            search_thread.start()
            self.assertTrue(scanning.wait(timeout=5), "search did not reach the blocked scan")
            lock_thread.start()
            self.assertTrue(
                other_operation_acquired.wait(timeout=5),
                "another operation could not acquire runtime.lock during the scan",
            )
        finally:
            resume_scan.set()
            search_thread.join(timeout=5)
            if lock_thread.ident is not None:
                lock_thread.join(timeout=5)
        self.assertFalse(search_thread.is_alive())
        self.assertFalse(lock_thread.is_alive())
        self.assertEqual(errors, [])


if __name__ == "__main__":
    unittest.main()
