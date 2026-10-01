#!/usr/bin/env python3
"""Large synthetic entity store. No live state is copied or changed."""
import contextlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_sync import SyncStore
from codex_sync_entities import ensure_tables


def measure():
    with tempfile.TemporaryDirectory(prefix="studio-renderer-sync-") as directory:
        path = Path(directory) / "fixture.sqlite3"

        @contextlib.contextmanager
        def connect():
            db = sqlite3.connect(path, timeout=30)
            try:
                with db:
                    yield db
            finally:
                db.close()

        with connect() as db:
            ensure_tables(db)
            db.executescript("""CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, kind TEXT,
                    status TEXT, created REAL, error TEXT);
                CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                INSERT INTO sync_entity_meta VALUES ('seeded','1');""")
            db.execute("INSERT INTO runtime_agents VALUES ('owner',?)",
                       (json.dumps({"id": "owner", "deletedAt": None}),))
            seq = 0
            rows = []
            for collection, count, deleted, padding in (
                ("task", 81915, 1, 90), ("agent", 1129, 1, 80),
                ("event", 12203, 0, 330), ("monitor", 3412, 0, 1050),
                ("agent", 356, 0, 2900), ("room", 551, 0, 500),
                ("misc", 1484, 0, 250),
            ):
                for index in range(count):
                    seq += 1
                    key = f"{collection}-{deleted}-{index}"
                    payload = json.dumps({"collection": collection, "id": key,
                                          "value": {} if deleted else {"id": key, "pad": "x" * padding}},
                                         separators=(",", ":"))
                    rows.append((collection, key, seq, "fixture", payload, deleted))
                    if collection == "event":
                        db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?)",
                                   (key, "owner", "message", "delivered", index, None))
                    elif collection == "monitor":
                        record = {"id": key, "agent": "owner", "created": index,
                                  "status": "running" if index < 2 else "completed"}
                        db.execute("INSERT INTO runtime_monitors VALUES (?,?)", (key, json.dumps(record)))
            db.executemany("INSERT INTO sync_entities VALUES (?,?,?,?,?,?)", rows)

        store = SyncStore(connect, lambda: {}, lambda _key: {})
        before_started = time.perf_counter()
        with connect() as db:
            cursor = 0
            before_pages = before_docs = before_bytes = 0
            while True:
                rows = db.execute("""SELECT collection,id,seq,payload,deleted FROM sync_entities
                    WHERE collection NOT LIKE 'transcript:%' AND seq>?
                    ORDER BY seq LIMIT 100""", (cursor,)).fetchall()
                if not rows:
                    break
                before_pages += 1
                before_docs += len(rows)
                before_bytes += sum(len(row[3].encode()) for row in rows)
                cursor = rows[-1][2]
        before_seconds = time.perf_counter() - before_started

        after_started = time.perf_counter()
        cursor = initial_high = 0
        after_pages = after_docs = after_bytes = 0
        while True:
            result = store.pull("state:entities:v1", cursor, 500, fresh=True,
                                initial_high=initial_high)
            after_pages += 1
            after_docs += len(result["documents"])
            after_bytes += sum(len(row["payload"].encode()) for row in result["documents"])
            cursor = result["checkpoint"]["seq"]
            initial_high = result["initialHigh"]
            if cursor >= result["maxSeq"]:
                break
        after_seconds = time.perf_counter() - after_started
        return {"fixtureRows": seq,
                "before": {"pages": before_pages, "documents": before_docs,
                           "payloadBytes": before_bytes, "seconds": round(before_seconds, 3)},
                "after": {"pages": after_pages, "documents": after_docs,
                          "payloadBytes": after_bytes, "seconds": round(after_seconds, 3)}}


if __name__ == "__main__":
    print(json.dumps(measure(), sort_keys=True))
