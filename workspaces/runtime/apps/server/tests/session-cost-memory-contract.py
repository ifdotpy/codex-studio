#!/usr/bin/env python3
"""Measure cold and warm session cost requests against a large local fixture."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
from pathlib import Path
import resource
import sqlite3
import subprocess
import sys
import tempfile
import time

ROOT = REPOSITORY_ROOT
COUNT = 300_000
LIMIT_BYTES = 128 * 1024 * 1024


def worker(db_path, state_root, name, expected_samples):
    sys.path.insert(0, os.environ.get("CODEX_SESSION_COSTS_PATH", str(SERVER_SOURCE_ROOT)))
    sys.path.insert(1, str(SERVER_SOURCE_ROOT))
    from codex_session_costs import SessionCostReader

    class FixedPricing:
        def snapshot(self):
            return {"providers": {"openai": {"models": {"gpt-6-luna": {"cost": {
                "input": 0.1, "output": 0.5, "cache_read": 0.01, "cache_write": 0.125}}}}}}
        def refresh_missing(self):
            pass

    clock = [time.time()]
    reader = SessionCostReader(db_path, FixedPricing(), state_root=state_root, clock=lambda: clock[0])
    started = time.perf_counter()
    result = reader.snapshot("lead", wait=True)
    cold = time.perf_counter() - started
    started = time.perf_counter()
    warm = reader.snapshot("lead", wait=True)
    warm_elapsed = time.perf_counter() - started
    clock[0] += 31
    started = time.perf_counter()
    unchanged = reader.snapshot("lead", wait=True)
    unchanged_elapsed = time.perf_counter() - started
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if sys.platform != "darwin":
        peak *= 1024
    assert result["pricedSamples"] == expected_samples
    assert warm["cacheAgeSeconds"] == 0
    assert unchanged["cacheAgeSeconds"] == 31 and not unchanged["refreshing"]
    print(f"fixture={name} rows={COUNT} peakRSS={peak} bytes cold={cold:.3f}s "
          f"warm={warm_elapsed:.6f}s unchangedRefresh={unchanged_elapsed:.6f}s")
    assert peak < LIMIT_BYTES, f"peak RSS {peak} exceeded {LIMIT_BYTES} bytes"


def create_fixture(db_path, *, worst_case=False):
    db = sqlite3.connect(db_path)
    db.executescript("""
      CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
      CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
      CREATE TABLE analytics_usage (
        seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
      CREATE INDEX analytics_usage_team ON analytics_usage(root,at);
      CREATE INDEX analytics_usage_root_seq ON analytics_usage(root,seq);
    """)
    db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
    rows = []
    for index in range(COUNT):
        model = "gpt-6-luna" if not worst_case or index % 2 == 0 else None
        response_id = f"response-{index}" if not worst_case or index % 2 == 0 else None
        turn = f"turn-{index // 2}" if worst_case else f"turn-{index}"
        record = {"agentId": "lead", "threadId": "thread", "turnId": turn,
                  "responseId": response_id, "model": model,
                  "delta": {"inputTokens": 1000 + index % 3, "cachedInputTokens": 100,
                            "cacheWriteInputTokens": 0, "outputTokens": 100}}
        rows.append((index + 1, "lead", "lead", "thread", turn, index, json.dumps(record)))
        if len(rows) == 5000:
            db.executemany("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)", rows)
            rows.clear()
    if rows:
        db.executemany("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)", rows)
    db.commit()
    plan = db.execute("EXPLAIN QUERY PLAN SELECT COALESCE(MAX(seq),0) FROM analytics_usage WHERE root=?",
                      ("lead",)).fetchall()
    assert any("analytics_usage_root_seq" in row[3] for row in plan), plan
    db.close()


if __name__ == "__main__":
    if len(sys.argv) in (3, 4) and sys.argv[1] == "--create":
        folder = Path(sys.argv[2])
        folder.mkdir(parents=True, exist_ok=True)
        create_fixture(folder / "canvas.sqlite3", worst_case=len(sys.argv) == 4 and sys.argv[3] == "--worst")
        sys.exit(0)
    if len(sys.argv) == 6 and sys.argv[1] == "--worker":
        worker(Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4], int(sys.argv[5]))
    else:
        with tempfile.TemporaryDirectory() as folder:
            for name, worst, samples in (("standard", False, COUNT), ("missing-model-response", True, COUNT // 2)):
                fixture_dir = Path(folder) / name
                fixture_dir.mkdir()
                db_path = fixture_dir / "canvas.sqlite3"
                create_fixture(db_path, worst_case=worst)
                subprocess.run([sys.executable, __file__, "--worker", str(db_path),
                                str(fixture_dir), name, str(samples)], check=True)
