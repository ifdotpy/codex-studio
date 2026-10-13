"""Actual Python supervisor decisions and Python-written journal template; no child launch."""
import io
import os
import json
import sys
import threading
import sqlite3
from contextlib import contextmanager
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_process_supervisor import Journal, Child, Supervisor

root = Path(sys.argv[1])
journal = Journal(root)
with journal.db() as db:
    db.execute("INSERT INTO handles(id,signature,pid,created,generation) VALUES ('fixture','signature',123,10,1)")
    db.execute("INSERT INTO child_identities(handle,pid,pgid,start_time) VALUES ('fixture',123,123,'fixture-start')")
if "--legacy-template" in sys.argv:
    journal.close()
    with sqlite3.connect(root / "supervisor.sqlite3") as legacy:
        legacy.execute("DROP INDEX operations_monitor_native")
        for table, columns in [("handles", ["closed_at", "closed_reason", "generation"]),
                               ("events", ["generation"]),
                               ("child_identities", ["job_name", "job_kill_on_close"]),
                               ("operations", ["generation", "response", "response_sequence"])]:
            for column in columns:
                legacy.execute(f"ALTER TABLE {table} DROP COLUMN {column}")
        legacy.execute("INSERT INTO operations VALUES ('fixture','monitor:legacy','digest',1,11)")
        legacy.execute("INSERT INTO events VALUES ('fixture',1,'stdout','null',4)")
        legacy.execute("UPDATE handles SET sequence=1")
        legacy.commit()
        expected = root / "expected"
        expected.mkdir()
        with sqlite3.connect(expected / "supervisor.sqlite3") as target:
            legacy.backup(target)
    Journal(expected).close()
    raise SystemExit(0)
if "--template-only" in sys.argv:
    journal.close()
    raise SystemExit(0)
# The durable seed above is written by Python once. Differential decisions run
# on an SQLite backup in memory; clocks and persistence latency are not inputs.
memory = sqlite3.connect(":memory:")
memory.row_factory = sqlite3.Row
with journal.db() as source:
    source.backup(memory)
journal.close()
@contextmanager
def memory_db():
    with memory:
        yield memory
journal.db = memory_db
supervisor = Supervisor(root)
supervisor.journal = journal
child = Child.__new__(Child)
child.handle = "fixture"
child.lock = threading.RLock()
child.process = SimpleNamespace(supervisor=supervisor, stdin=io.StringIO(), poll=lambda: None)
child.paused = threading.Event()
supervisor.children["fixture"] = child
if "--request-cwd" in sys.argv:
    child.process.pid = 123
    requests = []
    for kind in ("missing", "null", "value"):
        request = {"action":"open", "handle":"fixture", "command":["fixture"], "env":{}}
        if kind != "missing":
            request["cwd"] = None if kind == "null" else "/fixture"
        with patch.object(supervisor, "open_handle", return_value=(child, False)) as opened:
            try:
                supervisor.handle(request)
                accepted = True
            except KeyError as error:
                assert str(error) == "'cwd'"
                accepted = False
            assert opened.called == accepted
        requests.append({"request":request,"accepted":accepted})
    (root / "requests.json").write_text(json.dumps(requests))
    raise SystemExit(0)
cases = []
messages = [
    {"method": "echo", "params": {}},
    {"method": "echo", "params": {"text": "é雪😀", "fraction": 1e-6}},
    {"method": "initialized"},
    {"id": 991, "result": {"ok": True}},
]
if os.environ.get("STUDIO_RUNTIME_CORPUS_FULL") == "1":
    messages.extend({"method": "echo", "params": {"payload": value}} for value in [
        None, True, False, 0, -1, 1e20, 1e-20, "\u0000", "\t\n", "𝄞" * 4, [], {"nested": [{}, None]},
    ])
for index, message in enumerate(messages):
    for native in (None, 17):
        operation = f"monitor:{index}:{native}"
        for retry in ("new", "duplicate", "conflict"):
            value = {**message, "id": 700 if retry == "new" else 701}
            if retry == "conflict":
                value["params"] = {"changed": True}
            with journal.db() as db:
                row = dict(db.execute("SELECT * FROM handles WHERE id='fixture'").fetchone())
                prior = db.execute("SELECT * FROM operations WHERE handle='fixture' AND operation_id=?", (operation,)).fetchone()
                prior = dict(prior) if prior else None
            child.process.stdin = io.StringIO()
            with patch("codex_process_supervisor.time.perf_counter_ns", return_value=0):
                try:
                    expected = {"result": child.write(operation, native, value)}
                except ValueError as error:
                    expected = {"error": str(error)}
            cases.append(dict(name=f"write-{index}-{native}-{retry}", action="write", row=row, prior=prior,
                              message=value, native=native, expected=expected,
                              outbound=json.loads(child.process.stdin.getvalue()) if child.process.stdin.getvalue() else None))
# Drive cursor decisions through Supervisor.handle on actual stored rows.
with journal.db() as db:
    db.execute("UPDATE handles SET sequence=3,acknowledged=1 WHERE id='fixture'")
    for seq in (2, 3):
        db.execute("INSERT INTO events VALUES ('fixture',?,'stdout',?,7,1)", (seq, json.dumps({"n": seq})))
for action in ("ack", "replay", "next"):
    for cursor in (0, 1, 2, 3, 4):
        with journal.db() as db:
            db.execute("UPDATE handles SET acknowledged=1 WHERE id='fixture'")
            row = dict(db.execute("SELECT * FROM handles WHERE id='fixture'").fetchone())
        child.reader = SimpleNamespace(is_alive=lambda: True)
        child.stderr = SimpleNamespace(is_alive=lambda: True)
        try:
            supervisor.handle({"action": action, "handle": "fixture", "sequence": cursor, "cursor": cursor})
            expected = None
        except ValueError as error:
            expected = str(error)
        cases.append(dict(name=f"{action}-{cursor}", action="cursor", row=row, cursor=cursor, ack=action == "ack", error=expected))
# Restore a minimal reusable Python-written template for journal/failpoint tests.
with journal.db() as db:
    db.execute("DELETE FROM operations")
    db.execute("DELETE FROM events")
    db.execute("UPDATE handles SET sequence=0,acknowledged=0,rpc_sequence=0")
journal.close()
(root / "cases.json").write_text(json.dumps(cases))
print(len(cases))
