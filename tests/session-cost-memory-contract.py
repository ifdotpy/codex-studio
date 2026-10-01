#!/usr/bin/env python3
"""Measure cold and warm session cost requests against a large local fixture."""
import json
import os
from pathlib import Path
import resource
import sqlite3
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
COUNT = 300_000
LIMIT_BYTES = 128 * 1024 * 1024


def worker(db_path, state_root):
    sys.path.insert(0, os.environ.get("CODEX_SESSION_COSTS_PATH", str(ROOT / "scripts")))
    sys.path.insert(1, str(ROOT / "scripts"))
    from codex_session_costs import SessionCostReader

    class FixedPricing:
        def snapshot(self):
            return {"providers": {"openai": {"models": {"gpt-6-luna": {"cost": {
                "input": 0.1, "output": 0.5, "cache_read": 0.01, "cache_write": 0.125}}}}}}
        def refresh_missing(self):
            pass

    reader = SessionCostReader(db_path, FixedPricing(), state_root=state_root)
    started = time.perf_counter()
    result = reader.snapshot("lead")
    cold = time.perf_counter() - started
    started = time.perf_counter()
    warm = reader.snapshot("lead")
    warm_elapsed = time.perf_counter() - started
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if sys.platform != "darwin":
        peak *= 1024
    assert result["pricedSamples"] == COUNT
    assert warm["cacheAgeSeconds"] == 0
    print(f"rows={COUNT} peakRSS={peak} bytes cold={cold:.3f}s warm={warm_elapsed:.6f}s")
    assert peak < LIMIT_BYTES, f"peak RSS {peak} exceeded {LIMIT_BYTES} bytes"


def create_fixture(db_path):
    db = sqlite3.connect(db_path)
    db.executescript("""
      CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
      CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
      CREATE TABLE analytics_usage (
        seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
      CREATE INDEX analytics_usage_team ON analytics_usage(root,at);
    """)
    db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
    rows = []
    for index in range(COUNT):
        model = ("gpt-6-luna", "gpt-6-luna", "gpt-6-luna")[index % 3]
        record = {"agentId": "lead", "threadId": "thread", "turnId": f"turn-{index}",
                  "responseId": f"response-{index}", "model": model,
                  "delta": {"inputTokens": 1000 + index % 3, "cachedInputTokens": 100,
                            "cacheWriteInputTokens": 0, "outputTokens": 100}}
        rows.append((index + 1, "lead", "lead", "thread", f"turn-{index}", index, json.dumps(record)))
        if len(rows) == 5000:
            db.executemany("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)", rows)
            rows.clear()
    if rows:
        db.executemany("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)", rows)
    db.commit()
    db.close()


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--create":
        folder = Path(sys.argv[2])
        folder.mkdir(parents=True, exist_ok=True)
        create_fixture(folder / "canvas.sqlite3")
        sys.exit(0)
    if len(sys.argv) == 4 and sys.argv[1] == "--worker":
        worker(Path(sys.argv[2]), Path(sys.argv[3]))
    else:
        with tempfile.TemporaryDirectory() as folder:
            db_path = Path(folder) / "canvas.sqlite3"
            create_fixture(db_path)
            subprocess.run([sys.executable, __file__, "--worker", str(db_path), folder], check=True)
