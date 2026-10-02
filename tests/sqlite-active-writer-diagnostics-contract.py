#!/usr/bin/env python3
"""Inspect held SQLite transactions without SQL or arbitrary frame data."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import sqlite3
import sys
import tempfile
import threading
import time
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_sqlite
from codex_sqlite import connect, diagnostics, scope


def counter_view(snapshot):
    return {key: value for key, value in snapshot.items()
            if key not in {"activeTransactions", "activeTransactionScan"}}


def verify_real_transactions(path):
    ready = threading.Event()
    read_ready = threading.Event()
    release = threading.Event()
    committed = threading.Event()
    finished = threading.Event()
    state = {}
    failures = []
    secret = "DO_NOT_EXPOSE_SQL_OR_FRAME_PAYLOAD"

    def hold(db):
        # The same connection is present in this frame and its caller.
        state["started"] = db._codex_transaction_started
        ready.set()
        assert release.wait(3)

    def writer():
        db = None
        try:
            db = connect(path, site="contract.writer_connection")
            with scope(db, "contract.active_writer_scope"):
                db.execute("BEGIN IMMEDIATE")
                db.execute("INSERT INTO events(value) VALUES (?)", (secret,))
                hold(db)
            committed.set()
            assert finished.wait(3)
        except BaseException as error:
            failures.append(error)
            ready.set()
            committed.set()
        finally:
            if db is not None:
                db.close()

    def reader():
        db = None
        try:
            db = connect(path, site="contract.read_transaction")
            db.execute("BEGIN")
            db.execute("SELECT count(*) FROM events").fetchone()
            read_ready.set()
            assert release.wait(3)
            db.commit()
        except BaseException as error:
            failures.append(error)
            read_ready.set()
        finally:
            if db is not None:
                db.close()

    owner = threading.Thread(target=writer, name="contract-active-transaction-owner")
    read_owner = threading.Thread(target=reader, name="contract-active-read-owner")
    unstarted = threading.Thread(name="contract-unstarted-thread")
    stale = connect(":memory:", site="contract.closed_connection")
    stale.close()
    globals_before = set(vars(codex_sqlite))
    enumerate_threads = threading.enumerate
    current_frames = sys._current_frames

    def unlocked_frames():
        assert codex_sqlite._LOCK.acquire(blocking=False), "frame scan holds the counter lock"
        codex_sqlite._LOCK.release()
        return current_frames()

    owner.start()
    try:
        assert ready.wait(1), "writer did not start"
        assert not failures, failures
        read_owner.start()
        assert read_ready.wait(1), "reader did not start"
        assert not failures, failures
        contender = sqlite3.connect(path, timeout=0.02)
        try:
            try:
                contender.execute("INSERT INTO events(value) VALUES ('contender')")
            except sqlite3.OperationalError as error:
                assert "locked" in str(error), error
            else:
                raise AssertionError("the held WAL transaction did not block a write")
        finally:
            contender.close()

        before = counter_view(diagnostics())
        with patch.object(threading, "enumerate", lambda: enumerate_threads() + [unstarted]), \
                patch.object(sys, "_current_frames", unlocked_frames), \
                patch.object(codex_sqlite.InstrumentedConnection, "execute",
                             side_effect=AssertionError("diagnostics must not execute SQL")):
            started = time.monotonic()
            snapshot = diagnostics()
            assert time.monotonic() - started < 0.5
        assert counter_view(snapshot) == before, "reading diagnostics changed cumulative counters"
        assert set(vars(codex_sqlite)) == globals_before, "diagnostics added module globals"
        encoded = json.dumps(snapshot)
        assert secret not in encoded
        assert "SELECT" not in encoded and "INSERT" not in encoded
        assert "f_locals" not in encoded and "parameters" not in encoded
        assert "writer" not in snapshot and "currentWriter" not in snapshot
        entries = snapshot["activeTransactions"]
        held = [entry for entry in entries if entry["site"] == "contract.active_writer_scope"]
        assert len(held) == 1, "the same connection was listed more than once"
        held = held[0]
        assert held["startedAtMonotonic"] == state["started"]
        assert 20 <= held["durationMs"] < 1000, held
        assert abs(held["durationMs"] - (time.monotonic() - state["started"]) * 1000) < 50
        references = [item for item in held["threads"] if item["threadId"] == owner.ident]
        assert len(references) == 1
        reference = references[0]
        assert reference["threadName"] == owner.name
        assert {item["function"] for item in reference["frames"]} >= {"hold", "writer"}
        for item in reference["frames"]:
            assert item["file"].endswith(Path(__file__).name)
            assert isinstance(item["line"], int) and item["line"] > 0
        reads = [entry for entry in entries if entry["site"] == "contract.read_transaction"]
        assert len(reads) == 1, "an explicit read transaction must be identified as a transaction"
        assert reads[0]["threads"][0]["threadName"] == read_owner.name
        assert all(entry["site"] != "contract.closed_connection" for entry in entries)
        assert all(item["threadId"] is not None for entry in entries for item in entry["threads"])
        release.set()
        assert committed.wait(1), "writer did not commit"
        read_owner.join(timeout=1)
        assert not read_owner.is_alive()
        assert owner.is_alive(), "the writer must retain its connection after commit"
        after = diagnostics()
        assert not any(entry["site"] in {"contract.active_writer_scope", "contract.read_transaction"}
                       for entry in after["activeTransactions"]), after["activeTransactions"]
        measured = after["transaction"]["sites"]["contract.active_writer_scope"]
        assert measured["transactions"] == 1
        assert measured["longestTransactionMs"] >= held["durationMs"]
        assert not failures, failures
    finally:
        release.set()
        finished.set()
        owner.join(timeout=2)
        if read_owner.ident is not None:
            read_owner.join(timeout=2)
        assert not owner.is_alive() and not read_owner.is_alive()


def verify_bounded_snapshot():
    connections = [sqlite3.connect(":memory:") for _ in range(70)]
    try:
        for db in connections:
            db.execute("BEGIN")
        values = {str(index): db for index, db in enumerate(connections)}
        values["private_sql"] = "THIS_IS_NOT_PUBLIC_FRAME_DATA"
        frame = SimpleNamespace(f_locals=values, f_back=None, f_lineno=123,
                                f_code=SimpleNamespace(co_filename="x" * 1000, co_name="y" * 1000))
        with patch.object(sys, "_current_frames", lambda: {987654321: frame}):
            snapshot = diagnostics()
        assert len(snapshot["activeTransactions"]) == 64
        assert snapshot["activeTransactionScan"]["truncated"] is True
        assert "THIS_IS_NOT_PUBLIC_FRAME_DATA" not in json.dumps(snapshot)
        for entry in snapshot["activeTransactions"]:
            assert entry["durationMs"] is None and entry["startedAtMonotonic"] is None
            reference = entry["threads"][0]
            assert reference["threadName"] == "unknown"
            assert len(reference["frames"][0]["file"]) == 320
            assert len(reference["frames"][0]["function"]) == 120
    finally:
        for db in connections:
            db.rollback()
            db.close()


with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "transactions.sqlite3"
    with sqlite3.connect(path) as setup:
        assert setup.execute("PRAGMA journal_mode=WAL").fetchone()[0] == "wal"
        setup.execute("CREATE TABLE events(value TEXT)")
    setup.close()
    verify_real_transactions(path)
verify_bounded_snapshot()
print("SQLite active transaction diagnostics contract passed")
