"""One-process, same-Python-prestate oracle. Never launches a provider or a timer.

The JSONL protocol accepts seed/transition/snapshot commands. A seed is written
with Runtime.create/put/enqueue and codex_execution's production writers, then
SQLite backup copies that *pre-transition* database for the Rust caller.
"""
# Install before importing any application code, using the account database rather
# than HOME (which is deliberately replaced by the HTTP harness below).
import os
import pathlib
import pwd
import sys

_REAL_HOME = pathlib.Path(pwd.getpwuid(os.getuid()).pw_dir)
_FORBIDDEN = tuple((_REAL_HOME / name).resolve() for name in (".codex", ".claude"))

class IsolationViolation(BaseException):
    pass


def guard(event, args):
    if event in {"open", "os.listdir", "os.scandir", "os.stat", "os.lstat", "os.fstatat"}:
        path = args[0] if args else None
        if isinstance(path, (str, bytes, os.PathLike)):
            resolved = pathlib.Path(os.fsdecode(path)).resolve()
            if any(resolved == forbidden or forbidden in resolved.parents for forbidden in _FORBIDDEN):
                raise IsolationViolation("oracle refused access to a real provider home")
    if event == "socket.connect":
        raise IsolationViolation("network is forbidden in lifecycle oracle")


sys.addaudithook(guard)
if "--guard-test" in sys.argv:
    for forbidden in _FORBIDDEN:
        try:
            open(forbidden / "oracle-guard-must-not-open", "rb")
        except IsolationViolation:
            pass
        else:
            raise AssertionError("real provider home was not blocked")
    print("guard refused both real provider homes")
    sys.exit(0)

SERVER = pathlib.Path(__file__).resolve().parents[4] / "apps/server"
if "--isolated" not in sys.argv:
    # Re-exec the same process using the existing HTTP harness environment
    # builder. No backend or replay provider is started.
    sys.path.insert(0, str(SERVER / "tests"))
    from http_backend_harness import BackendProcess
    scratch = pathlib.Path(sys.argv[1]).resolve(strict=True)
    environment_owner = BackendProcess(scratch)
    binary = environment_owner._directory("disabled-provider") / "codex"
    binary.write_text("#!/bin/sh\nexit 99\n")
    binary.chmod(0o700)
    environment_owner._provider_executable = binary
    environment = environment_owner._environment()
    os.execve(sys.executable, [sys.executable, "-B", __file__, "--isolated"], environment)

import copy
import hashlib
import json
import sqlite3
import threading
import shutil
import tempfile
from unittest.mock import patch
sys.path.insert(0, str(SERVER / "src"))
from codex_runtime import Runtime
import codex_runtime
import codex_accounts

TABLES = (
    "runtime_agents", "runtime_events", "runtime_execution_runs", "runtime_execution_attempts",
    "runtime_execution_inputs", "runtime_execution_effects", "runtime_execution_nodes", "runtime_completed_turns",
    "runtime_event_meta", "runtime_items", "runtime_item_fulltext", "runtime_context_manifests", "runtime_context_reminders",
    "runtime_requests", "runtime_tasks", "runtime_complaints", "runtime_operation_receipts",
    "sync_entities", "sync_documents", "sync_versions", "runtime_search", "runtime_search_rows",
    "runtime_search_indexed", "runtime_search_next_meta",
)
TIMES = {"created", "createdAt", "created_at", "finished", "at", "lastEvent", "reconciledAt", "updated", "updatedAt", "acceptedAt", "enqueuedAt", "dispatchPickedAt"}


class QuietRuntime(Runtime):
    def schedule(self):
        pass

    def interrupt(self, agent):
        self.interrupted = agent["id"]

    def connect(self, *args, **kwargs):
        raise AssertionError("provider connection forbidden in lifecycle oracle")

    connect_agent = connect

    def start_review_archives(self):
        pass

    def resume_read_only_image_bases(self):
        pass


def normalize(value, key=""):
    if key in TIMES and value is not None:
        return "<time>"
    if key in {"seq", "revision", "transcriptRevision"} and value is not None:
        return "<revision>"
    if isinstance(value, dict):
        return {k: normalize(v, k) for k, v in sorted(value.items())}
    if isinstance(value, list):
        return [normalize(v) for v in value]
    return value


def snapshot(root, tables=TABLES, ordered_tables=("runtime_items",)):
    with sqlite3.connect(pathlib.Path(root) / "canvas.sqlite3") as db:
        db.row_factory = sqlite3.Row
        result = {}
        present = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        for table in tables:
            if table not in present:
                result[table] = []
                continue
            rows = []
            columns = list(db.execute('PRAGMA table_info("' + table + '")'))
            keys = [row[1] for row in sorted(columns, key=lambda r:r[5]) if row[5]]
            order = "created,rowid" if table in ordered_tables else "rowid"
            for row in db.execute('SELECT * FROM "' + table + '" ORDER BY ' + order):
                record = dict(row)
                if table == "sync_entities":
                    assert record["hash"] == hashlib.sha256(record["payload"].encode()).hexdigest(), "invalid sync hash"
                for field in ("record", "payload", "result"):
                    if field in record and record[field] is not None:
                        record[field] = json.loads(record[field])
                if table == "sync_entities":
                    record["hash"] = hashlib.sha256(json.dumps(normalize(record["payload"]), sort_keys=True).encode()).hexdigest()
                if table == "sync_versions":
                    record["hash"] = "<revision>"
                rows.append(normalize(record))
            if table not in ordered_tables:
                rows.sort(key=lambda r: json.dumps([r[k] for k in keys], sort_keys=True))
            result[table] = rows
        if "runtime_search_next" in present:
            db.execute("CREATE VIRTUAL TABLE temp.corpus_vocab USING fts5vocab(main,'runtime_search_next','instance')")
            result["runtime_search_next_postings"] = [list(row) for row in db.execute("SELECT term,doc,col,offset FROM corpus_vocab ORDER BY 1,2,3,4")]
        return result


def capture_submission(runtime, *, stop="submitted"):
    """Execute real start preparation; stop only at a caller-selected I/O boundary."""
    class Captured(BaseException):
        pass

    class Provider:
        supervisor_mode = False

    captured = {}
    item = runtime.item
    def record_item(db, *args, **kwargs):
        if args[2] == "user":
            captured.update(args=copy.deepcopy(args), kwargs=copy.deepcopy(kwargs))
            if stop == "before_item":
                db.commit()
                raise Captured()
        result = item(db, *args, **kwargs)
        if args[2] == "user" and stop == "after_item":
            db.commit()
            raise Captured()
        return result

    def submit(*args, **kwargs):
        captured["submission"] = copy.deepcopy(kwargs)
        raise Captured()

    agent = runtime.agent("agent")
    with runtime.read_db() as db:
        rows = [dict(db.execute("SELECT * FROM runtime_events WHERE id=?", (key,)).fetchone())
                for key in agent["startAttempt"]["events"]]
    with patch.object(runtime, "prepare", lambda a:a), \
         patch.object(runtime, "connect_agent", lambda a:Provider()), \
         patch.object(runtime, "agent_connection", lambda a:None), \
         patch.object(runtime, "item", record_item), \
         patch.object(runtime, "submit_reserved", submit):
        try:
            Runtime.start(runtime, agent, rows)
        except Captured:
            return captured
    raise AssertionError("real start did not reach the selected submission boundary")


def seed(runtime, case):
    agent = runtime.create({"name": "lifecycle fixture", "cwd": str(runtime.root), "prompt": "seed"}, defer=True)
    with runtime.db() as db:
        db.execute("DELETE FROM runtime_agents WHERE id=?", (agent["id"],))
        db.execute("DELETE FROM sync_entities WHERE collection='agent' AND id=?", (agent["id"],))
        agent.update(id="agent", rootId="agent", cwd="/fixture", accountKey="default",
                     threadId="thread", model="fixture-model", epoch=0, status="queued",
                     autoWake=True, inFlight=False, turnId=None, error=None)
        runtime.put(db, "agents", agent)
        runtime.enqueue(db, agent, "user", "input", "event")
        phase = case.get("phase", "submitted")
        if phase != "queued":
            attempt = dict(id="attempt", epoch=0, accountKey="default", events=["event"],
                           submitted=phase != "claimed", created=10.0, activeAtReservation=False)
            if phase != "claimed":
                attempt.update(threadId="thread", connectionId=None,
                               nativeOperationId="turn:agent:event:attempt:attempt",
                               supervisorIdentity=None)
                if case.get("state") != "python_unknown":
                    attempt.update(requestTicket="ticket", connectionGeneration=1)
            if phase == "accepted":
                attempt["turnId"] = "turn"
            agent.update(status="running" if phase == "accepted" else "starting", inFlight=True,
                         turnEpoch=0, startAttempt=attempt, turnId="turn" if phase == "accepted" else None)
            runtime.put(db, "agents", agent)
            db.execute("UPDATE runtime_events SET status=?,turn_id=? WHERE id='event'",
                       ("delivered" if phase == "accepted" else "reserved" if phase == "claimed" else "dispatching",
                        "turn" if phase == "accepted" else None))
        for key in case.get("remove", []):
            agent.pop(key, None)
        agent.update(case.get("agent", {}))
        runtime.put(db, "agents", agent)
        if case.get("event"):
            for key, value in case["event"].items():
                assert key in {"status", "text", "epoch", "error", "turn_id"}
                db.execute('UPDATE runtime_events SET "' + key + '"=? WHERE id=\'event\'', (value,))
        if case.get("run"):
            import codex_execution
            run = codex_execution._load(db, "runs", "run:attempt")
            run.update(case["run"])
            codex_execution._save_run(db, run)
    return agent


def transition(runtime, case):
    agent = runtime.agent("agent")
    attempt = copy.deepcopy(agent.get("startAttempt", {}))
    action = case["transition"]
    if action == "accepted":
        # Scope mutations occur after submission; the callback retains original scope.
        attempt.update(accountKey="default", threadId="thread")
        runtime.start_accepted("agent", attempt, {"turn": {"id": "turn"}})
    elif action in {"unknown", "rejected", "not_sent"}:
        runtime.start_error("agent", "attempt", ValueError(case.get("message", action)),
                            unknown=action == "unknown", preparation=action == "not_sent")
    elif action in {"completed", "failed", "interrupted", "started"}:
        turn = {"id": "turn", "status": action}
        turn.update(case.get("turn", {}))
        runtime.notification({"method": "turn/" + ("started" if action == "started" else "completed"),
                              "params": {"threadId": "thread", "turn": turn}})
    elif action in {"disconnect", "generation_change"}:
        runtime.disconnected()
    elif action == "restart":
        # Constructor recovery must see the exact pre-state, without close()'s capture.
        with runtime.db() as db:
            backup = sqlite3.connect(":memory:")
            db.backup(backup)
        root = runtime.root
        runtime.close()
        with sqlite3.connect(root / "canvas.sqlite3") as db:
            backup.backup(db)
        backup.close()
        runtime = QuietRuntime(root, lambda *_a, **_kw: None)
    elif action == "claim":
        with patch.object(codex_runtime, "uid", lambda:"attempt"):
            runtime.dispatch_candidates("agent")
    elif action == "submission":
        capture_submission(runtime)
    elif action in {"stop", "pause"}:
        runtime.stop("agent", descendants=False)
    else:
        raise AssertionError("unimplemented oracle transition " + action)
    return runtime


def main():
    # No scheduler, archive, WAL, timer, pool worker or provider may execute.
    with patch.object(codex_accounts, "codex_home", lambda: pathlib.Path(os.environ["CODEX_HOME"])), \
         patch.object(codex_accounts, "auth_metadata", lambda *_: {"status":"signedOut","accountId":None,"email":None,"plan":None}), \
         patch.object(threading.Thread, "start", lambda self: None), \
         patch.object(threading.Thread, "join", lambda self, *a, **k: None), \
         patch.object(codex_runtime, "_RuntimeWalKeeper", lambda *a: None):
        template = pathlib.Path(tempfile.mkdtemp(prefix="lifecycle-schema-"))
        initialized = QuietRuntime(template, lambda *_a, **_kw: None)
        initialized.close()
        for line in sys.stdin:
            command = json.loads(line)
            module_name = command.get("module", command.get("case", {}).get("module"))
            if module_name is not None:
                assert module_name == "notification_dispatch", "unknown oracle module"
                import notification_dispatch as module
            else:
                module = sys.modules[__name__]
            try:
                if command["command"] == "snapshot":
                    result = module.snapshot(command["root"])
                else:
                    root = pathlib.Path(command["root"])
                    shutil.copytree(template, root)
                    runtime = QuietRuntime(root, lambda *_a, **_kw: None)
                    try:
                        module.seed(runtime, command["case"])
                        rust = pathlib.Path(command["rust"])
                        rust.mkdir(parents=True, exist_ok=True)
                        if (root / "multi-server").exists():
                            shutil.copytree(root / "multi-server", rust / "multi-server")
                        with runtime.read_db() as source, sqlite3.connect(rust / "canvas.sqlite3") as target:
                            source.backup(target)
                        with sqlite3.connect(root / "analytics.sqlite3") as source, sqlite3.connect(rust / "analytics.sqlite3") as target:
                            source.backup(target)
                        before = module.snapshot(root)
                        assert module.snapshot(rust) == before, "pre-states differ"
                        runtime = module.transition(runtime, command["case"]) or runtime
                        result = {"before": before, "after": module.snapshot(root)}
                    finally:
                        runtime.close()
                print(json.dumps({"result": result}), flush=True)
            except Exception:
                import traceback
                print(json.dumps({"error": traceback.format_exc()}), flush=True)
        shutil.rmtree(template)


if __name__ == "__main__":
    main()
