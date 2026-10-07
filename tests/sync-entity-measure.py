"""Synthetic 100-change/min entity-sync transfer and WAL measurement."""
import contextlib
import gzip
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_sync_entities import ensure_tables, put

PAGE = 4096
changes = 100
windows = 2
baseline_bytes = 6_600_000  # Task's measured GET /api/state?view=chat response.

with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "state.sqlite3"
    anchor = sqlite3.connect(path)
    anchor.execute("PRAGMA journal_mode=WAL")
    anchor.execute("PRAGMA wal_autocheckpoint=0")
    with anchor:
        ensure_tables(anchor)
        anchor.execute("BEGIN IMMEDIATE")
        for index in range(40):
            put(anchor, "agent", f"agent-{index}", {
                "id": f"agent-{index}", "kind": "agent", "name": f"Worker {index}", "status": "running",
                "source": "managed", "error": "x" * 200, "prompt": "p" * 400,
                "contextUsage": {"tokens": 1000 + index, "window": 200000, "at": 1},
                "nativeToolCatalog": ["private"] * 40,
            })
    anchor.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    wal_path = Path(str(path) + "-wal")
    wal_before = wal_path.stat().st_size
    delta_wire = []
    pull_count = 0
    clients_after = [0, 0]
    db_cpu_start = time.process_time()
    for index in range(changes):
        status = "failed" if index % 2 else "running"
        with sqlite3.connect(path) as db:
            db.execute("BEGIN IMMEDIATE")
            put(db, "agent", "agent-0", {"id": "agent-0", "name": "Worker 0",
                "kind": "agent", "status": status, "source": "managed", "error": "x" * 200,
                "prompt": "p" * 400, "contextUsage": {"tokens": index, "window": 200000, "at": 1}})
        for client in range(windows):
            with sqlite3.connect(path) as db:
                rows = db.execute("SELECT collection,id,seq,payload,deleted FROM sync_entities WHERE seq>? ORDER BY seq",
                                  (clients_after[client],)).fetchall()
            pull_count += 1
            if rows:
                clients_after[client] = rows[-1][2]
                for collection, key, seq, payload, deleted in rows:
                    if collection == "agent" and key == "agent-0":
                        body = json.dumps({"id": f"entity:{collection}:{key}", "seq": seq,
                                           "payload": payload, "_deleted": bool(deleted)},
                                          separators=(",", ":")).encode()
                        delta_wire.append((len(body), len(gzip.compress(body, compresslevel=3))))
    entity_cpu_ms = (time.process_time() - db_cpu_start) * 1000
    wal_bytes = max(0, wal_path.stat().st_size - wal_before)
    checkpoint = anchor.execute("PRAGMA wal_checkpoint(PASSIVE)").fetchone()

    # The previous sync path serialized and hashed the full compact snapshot per change.
    old_payload = json.dumps({"threads": [{"record": "x" * baseline_bytes}], "runtime": {}},
                             separators=(",", ":")).encode()
    old_cpu_start = time.process_time()
    for _ in range(changes):
        import hashlib
        hashlib.sha256(old_payload).hexdigest()
        json.dumps(json.loads(old_payload), separators=(",", ":")).encode()
    old_cpu_ms = (time.process_time() - old_cpu_start) * 1000
    with sqlite3.connect(path) as db:
        first_rows = db.execute("SELECT collection,id,seq,payload,deleted FROM sync_entities ORDER BY seq").fetchall()
    first_load_raw = sum(len(json.dumps({"id": f"entity:{c}:{k}", "seq": seq,
        "payload": payload, "_deleted": bool(deleted)}, separators=(",", ":")).encode())
        for c, k, seq, payload, deleted in first_rows)
    first_load_gzip = sum(len(gzip.compress(json.dumps({"id": f"entity:{c}:{k}", "seq": seq,
        "payload": payload, "_deleted": bool(deleted)}, separators=(",", ":")).encode(), compresslevel=3))
        for c, k, seq, payload, deleted in first_rows)

    result = {
        "fixture": "40 synthetic agents; 100 visible agent changes; 2 independent checkpoints",
        "baseline": {"firstLoadBytes": baseline_bytes,
                     "singleChangeBytes": baseline_bytes,
                     "snapshotSerializeHashCpuMsPer100Changes": round(old_cpu_ms, 3)},
        "after": {"singleAgentChangeBytesRaw": max(raw for raw, _ in delta_wire),
                  "singleAgentChangeBytesGzip": max(compressed for _, compressed in delta_wire),
                  "pullsPerMinuteTwoWindows": pull_count,
                  "firstLoadBytesRaw": first_load_raw,
                  "firstLoadBytesGzip": first_load_gzip,
                  "entityWriteAndPullCpuMsPer100Changes": round(entity_cpu_ms, 3),
                  "syncEntitiesWalBytesPer100Changes": wal_bytes,
                  "checkpointFramesBytes": checkpoint[1] * PAGE},
    }
    assert result["after"]["singleAgentChangeBytesRaw"] < 20_000
    assert result["after"]["firstLoadBytesRaw"] * 3 < baseline_bytes
    assert pull_count == changes * windows
    print(json.dumps(result, indent=2))
    anchor.close()
