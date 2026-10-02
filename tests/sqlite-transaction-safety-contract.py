#!/usr/bin/env python3
"""Reproduce an unscoped sqlite write and verify guarded recovery."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import sqlite3
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_sqlite import connect, diagnostics, scope


with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "fixture.sqlite3"
    setup = sqlite3.connect(path)
    setup.execute("CREATE TABLE events(id INTEGER PRIMARY KEY, value TEXT)")
    setup.commit()
    setup.close()

    leaked = connect(path, timeout=0.02, site="contract.leaked_helper")
    leaked.execute("INSERT INTO events(value) VALUES ('uncommitted helper write')")
    assert leaked.in_transaction

    competing = sqlite3.connect(path, timeout=0.02)
    try:
        try:
            competing.execute("INSERT INTO events(value) VALUES ('blocked writer')")
            competing.commit()
        except sqlite3.OperationalError as error:
            assert "locked" in str(error).lower() or "busy" in str(error).lower()
        else:
            raise AssertionError("the incident fixture did not reproduce the writer lock")

        try:
            with scope(leaked, "Runtime.db reused helper", strict=True):
                pass
        except RuntimeError as error:
            assert "Runtime.db reused helper" in str(error)
        else:
            raise AssertionError("strict transaction guard did not fail loudly")
        assert not leaked.in_transaction

        competing.execute("INSERT INTO events(value) VALUES ('writer after recovery')")
        competing.commit()
        rows = competing.execute("SELECT value FROM events ORDER BY id").fetchall()
        assert [row[0] for row in rows] == ["writer after recovery"]
        assert diagnostics()["leaks"]["count"] >= 1

        holder = connect(path, timeout=1, site="contract.lock_holder")
        waiter = connect(path, timeout=1, site="contract.contended_writer",
                         check_same_thread=False)
        holder.execute("BEGIN IMMEDIATE")
        outcome = []

        def write_after_lock():
            with scope(waiter, "contract.contended_writer"):
                waiter.execute("INSERT INTO events(value) VALUES ('after contention')")
            outcome.append(True)

        thread = threading.Thread(target=write_after_lock)
        thread.start()
        time.sleep(0.05)
        holder.commit()
        thread.join(timeout=2)
        assert outcome == [True]
        measured = diagnostics()["writeWait"]["sites"]["contract.contended_writer"]
        assert measured["writeWaits"] >= 1
        assert measured["writeWaitMs"] >= 30
        assert diagnostics()["transaction"]["sites"]["contract.contended_writer"]["transactions"] >= 1
        waiter.close()
        holder.close()
    finally:
        competing.close()
        leaked.close()

print("SQLite transaction safety contract passed")
