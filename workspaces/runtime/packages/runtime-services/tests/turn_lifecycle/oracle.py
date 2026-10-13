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
    global _CHECKING_PATH
    if event in {"open", "os.listdir", "os.scandir", "os.stat", "os.lstat", "os.fstatat"}:
        path = args[0] if args else None
        if isinstance(path, (str, bytes, os.PathLike)):
            checking = globals().get("_CHECKING_PATH", False)
            _CHECKING_PATH = True
            try:
                resolved = pathlib.Path(os.fsdecode(path)).resolve()
            finally:
                _CHECKING_PATH = checking
            if any(resolved == forbidden or forbidden in resolved.parents for forbidden in _FORBIDDEN):
                raise IsolationViolation("oracle refused access to a real provider home")
    if event == "socket.connect":
        raise IsolationViolation("network is forbidden in lifecycle oracle")


sys.addaudithook(guard)
# CPython emits no audit event for stat/lstat; enforce the same boundary there.
_ORIGINAL_STAT, _ORIGINAL_LSTAT = os.stat, os.lstat
_CHECKING_PATH = False
def guarded_stat(original, path, *args, **kwargs):
    global _CHECKING_PATH
    if not _CHECKING_PATH:
        _CHECKING_PATH = True
        try:
            guard("os.stat", (path,))
        finally:
            _CHECKING_PATH = False
    return original(path, *args, **kwargs)
os.stat = lambda path, *a, **kw: guarded_stat(_ORIGINAL_STAT, path, *a, **kw)
os.lstat = lambda path, *a, **kw: guarded_stat(_ORIGINAL_LSTAT, path, *a, **kw)
if "--guard-test" in sys.argv:
    for forbidden in _FORBIDDEN:
        try:
            open(forbidden / "oracle-guard-must-not-open", "rb")
        except IsolationViolation:
            pass
        else:
            raise AssertionError("real provider home was not blocked")
        for operation in (os.stat, os.lstat, os.listdir, os.scandir):
            try:
                operation(forbidden / "oracle-guard-must-not-open")
            except IsolationViolation:
                pass
            else:
                raise AssertionError("provider-home metadata access was not blocked")
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
import concurrent.futures
import hashlib
import datetime
import math
import re
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
    "runtime_budget", "runtime_budget_usage", "runtime_requests", "runtime_tasks", "runtime_complaints", "runtime_operation_receipts",
    "sync_entities", "sync_documents", "sync_versions", "runtime_search", "runtime_search_rows",
    "runtime_search_indexed", "runtime_search_next_meta",
)
TIMES = {"created", "createdAt", "created_at", "finished", "at", "lastEvent", "reconciledAt", "updated", "updatedAt", "acceptedAt", "enqueuedAt", "dispatchPickedAt", "observedAt"}
_PRESTATES = {}
_PAIRS = {}
_SHAPES = {}


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


def valid_time(value):
    if type(value) in (int, float):
        return math.isfinite(value) and value >= 0
    if isinstance(value, str):
        try:
            parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
            return parsed.tzinfo is not None
        except ValueError:
            return False
    return False


def normalize(value, key=""):
    if key in TIMES and value is not None:
        # Malformed seed fields must remain visible for refusal/no-write cases.
        # Never replace an invalid stored value with an opaque clock token.
        if not valid_time(value):
            return value
        return "<time>"
    if key in {"seq", "revision", "transcriptRevision"} and value is not None:
        if type(value) is not int or value < 0:
            return value
        return "<revision>"
    if isinstance(value, dict):
        return {k: normalize(v, k) for k, v in sorted(value.items())}
    if isinstance(value, list):
        return [normalize(v) for v in value]
    return value


def snapshot(root, tables=TABLES, ordered_tables=("runtime_items",)):
    root = str(root)
    shapes = {}
    def inspect(value, path=(), key=""):
        if value is not None and key in TIMES | {"seq", "revision", "transcriptRevision"}:
            shape = ("number" if type(value) in (int, float) else type(value).__name__)
            valid = valid_time(value) if key in TIMES else type(value) is int and value >= 0
            shapes[path] = (shape, valid)
        if isinstance(value, dict):
            for key, field in value.items():
                inspect(field, path + (key,), key)
        elif isinstance(value, list):
            for i, field in enumerate(value):
                inspect(field, path + (str(i),))
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
                inspect(record, (table, *(str(record[k]) for k in keys)))
                if table == "sync_entities":
                    record["hash"] = hashlib.sha256(json.dumps(normalize(record["payload"]), sort_keys=True).encode()).hexdigest()
                if table == "sync_versions":
                    assert isinstance(record["hash"], str) and re.fullmatch(r"[0-9a-f]{64}", record["hash"]), "invalid version hash"
                    record["hash"] = "<revision>"
                rows.append(normalize(record))
            if table not in ordered_tables:
                rows.sort(key=lambda r: json.dumps([r[k] for k in keys], sort_keys=True))
            result[table] = rows
        if "runtime_search_next" in present:
            db.execute("CREATE VIRTUAL TABLE temp.corpus_vocab USING fts5vocab(main,'runtime_search_next','instance')")
            result["runtime_search_next_postings"] = [list(row) for row in db.execute("SELECT term,doc,col,offset FROM corpus_vocab ORDER BY 1,2,3,4")]
        result.setdefault("runtime_search_next_postings", [])
        if root in _PAIRS:
            reference = _SHAPES[_PAIRS[root]]
            for path, shape in shapes.items():
                assert shape == reference.get(path, (shape[0], True)), f"stored opaque field shape changed: {path}"
        _SHAPES[root] = shapes
        if root in _PRESTATES:
            assert_revisions(_PRESTATES[root], opaque_state(db))
        return result


def assert_backup_equivalent(left, right):
    """SQLite backup equivalence: exact unnormalized SQL content, not file bytes."""
    with sqlite3.connect(pathlib.Path(left) / "canvas.sqlite3") as a, \
         sqlite3.connect(pathlib.Path(right) / "canvas.sqlite3") as b:
        assert list(a.iterdump()) == list(b.iterdump()), "unnormalized backup content differs"
        _PRESTATES[str(left)] = opaque_state(a)
        _PRESTATES[str(right)] = opaque_state(b)
        _PAIRS[str(right)] = str(left)


def opaque_state(db):
    state = {}
    tables = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    for table, keys, fields in (
        ("sync_entities", "collection,id", "seq,hash,payload,deleted"),
        ("sync_documents", "scope,id", "seq,payload,deleted"),
        ("sync_versions", "scope", "seq,hash,deleted"),
        ("runtime_transcript_revisions", "agent", "revision"),
    ):
        if table in tables:
            count = len(keys.split(','))
            state[table] = {tuple(r[:count]):tuple(r[count:]) for r in db.execute(f"SELECT {keys},{fields} FROM {table}")}
    return state


def assert_revisions(before, after):
    # These services do not write the legacy scope-version table. Its hashes
    # have no payload in that table from which to recompute them; preserve the
    # Python-written version records exactly instead of hiding valid-looking corruption.
    assert before.get("sync_versions") == after.get("sync_versions"), "legacy version/hash changed"
    for table, rows in after.items():
        for key, row in rows.items():
            assert type(row[0]) is int and row[0] >= 0, f"invalid stored revision: {table} {key}"
            previous = before.get(table, {}).get(key)
            if previous is not None:
                assert row[0] >= previous[0], f"revision moved backwards: {table} {key}"
                if row[1:] != previous[1:]:
                    assert row[0] > previous[0], f"content changed without revision advance: {table} {key}"


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


_SEED_AGENT = None

def seed(runtime, case):
    global _SEED_AGENT
    if _SEED_AGENT is None:
        _SEED_AGENT = runtime.create({"name": "lifecycle fixture", "cwd": str(runtime.root), "prompt": "seed"}, defer=True)
    agent = copy.deepcopy(_SEED_AGENT)
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
            attempt.update(case.get("attempt", {}))
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
        if case.get("expectRefusal"):
            agent["corpusPrivate"] = "SECRET_STORED_SENTINEL"
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
    if case.get("seedRecovery"):
        if case["seedRecovery"] == "restart":
            runtime = transition(runtime, {"transition":"restart"})
        else:
            runtime.disconnected()
        with runtime.db() as db:
            agent = runtime.agent("agent",db)
            agent.update(case.get("agent",{}))
            for key in case.get("removeAfterRecovery", []):
                agent.pop(key, None)
            runtime.put(db,"agents",agent)
    for corruption in case.get("corrupt", []):
        table, key = corruption["table"], corruption["id"]
        assert table in TABLES
        with runtime.db() as db:
            row=json.loads(db.execute('SELECT record FROM "'+table+'" WHERE id=?',(key,)).fetchone()[0])
            row[corruption["field"]]=corruption["value"]
            db.execute('UPDATE "'+table+'" SET record=? WHERE id=?',(json.dumps(row),key))
    if case.get("phase") == "queued":
        from codex_native_tools import mark_current
        mark_current(agent, runtime.tool_definitions(agent))
        with runtime.db() as db:
            runtime.put(db, "agents", agent)
    return runtime


def transition(runtime, case):
    runtime.corpus_result = None
    agent = runtime.agent("agent")
    attempt = copy.deepcopy(agent.get("startAttempt", {}))
    action = case["transition"]
    if action == "accepted":
        # Scope mutations occur after submission; the callback retains original scope.
        attempt.update(accountKey="default", threadId="thread")
        runtime.corpus_result = runtime.start_accepted("agent", attempt, {"turn": {"id": "turn"}})
    elif action in {"unknown", "rejected", "not_sent"}:
        runtime.corpus_result = runtime.start_error("agent", "attempt", ValueError(case.get("message", action)),
                            unknown=action == "unknown", preparation=action == "not_sent")
    elif action in {"completed", "failed", "interrupted", "started"}:
        turn = {"id": "turn", "status": action}
        turn.update(case.get("turn", {}))
        runtime.corpus_result = runtime.notification({"method": "turn/" + ("started" if action == "started" else "completed"),
                              "params": {"threadId": "thread", "turn": turn}})
    elif action in {"disconnect", "generation_change"}:
        runtime.corpus_result = runtime.disconnected()
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
    elif action == "reattach":
        import codex_connection_recovery as recovery
        server = object()
        with patch.object(runtime,"server_for",lambda *_:server), patch.object(runtime,"connection_current",lambda *_:True):
            if case.get("seedRecovery") == "restart":
                runtime.corpus_result = recovery.adopt_restart_turn(runtime,agent,None,server,{"id":"turn","status":"inProgress"})
            else:
                runtime.corpus_result = recovery.adopt_transport_turn(runtime,agent,None,server,{"id":"turn","status":"inProgress"},None)
    elif action == "claim":
        with patch.object(codex_runtime, "uid", lambda:"attempt"):
            runtime.corpus_result = runtime.dispatch_candidates("agent")
    elif action == "submission":
        capture_submission(runtime)
        runtime.corpus_result = {"boundary":"before_provider_submission"}
    elif action.startswith("interrupt_"):
        class InterruptProvider:
            def call(self, method, params):
                assert method == "turn/interrupt"
                if action == "interrupt_rejected": raise ValueError("rejected")
                if action == "interrupt_unknown": raise TimeoutError("interrupt response unknown")
        with patch.object(runtime,"server_for",lambda *_:InterruptProvider()):
            runtime.corpus_result = Runtime.interrupt(runtime,agent)
    elif action in {"stop", "pause"}:
        runtime.corpus_result = runtime.stop("agent", descendants=False)
    else:
        raise AssertionError("unimplemented oracle transition " + action)
    return runtime


class LifecycleCases:
    """Reuse only initialized runtime services and Python-written seed backups."""
    def __init__(self, template):
        self.template = template
        self.runtime = None
        self.pristine = None
        self.seeds = {}

    def run(self, command):
        if self.runtime is None:
            root = pathlib.Path(command["root"])
            shutil.copytree(self.template, root)
            self.runtime = QuietRuntime(root, lambda *_a, **_kw: None)
            self.pristine = sqlite3.connect(":memory:")
            with self.runtime.read_db() as db:
                db.backup(self.pristine)
        runtime, case = self.runtime, command["case"]
        runtime.close_notification_database()
        runtime.__dict__.pop("_agent_records_cache", None)
        key = json.dumps({"legacy":case.get("state")=="python_unknown",**{k:case[k] for k in ("phase","agent","attempt","remove","event","run","corrupt","seedRecovery","expectRefusal") if k in case}}, sort_keys=True)
        if key not in self.seeds:
            with sqlite3.connect(runtime.db_path) as db:
                self.pristine.backup(db)
            self.runtime = runtime = seed(runtime, case)
            saved = sqlite3.connect(":memory:")
            with runtime.read_db() as db:
                db.backup(saved)
            self.seeds[key] = saved
        saved = self.seeds[key]
        with sqlite3.connect(runtime.db_path) as db:
            saved.backup(db)
        runtime.__dict__.pop("_agent_records_cache", None)
        rust = pathlib.Path(command["rust"])
        rust.mkdir(parents=True)
        shutil.copytree(runtime.root / "multi-server", rust / "multi-server")
        shutil.copyfile(self.template / "analytics.sqlite3", rust / "analytics.sqlite3")
        with sqlite3.connect(rust / "canvas.sqlite3") as db:
            saved.backup(db)
        assert_backup_equivalent(runtime.root, rust)
        before = snapshot(runtime.root)
        assert snapshot(rust) == before, "pre-states differ"
        python_error = None
        try:
            self.runtime = transition(runtime, case) or runtime
        except Exception as error:
            if not case.get("expectRefusal"): raise
            python_error = type(error).__name__
        if case.get("afterEventText"):
            with self.runtime.db() as db:
                db.execute("UPDATE runtime_events SET text=? WHERE id='event'",(case["afterEventText"],))
        return {"before":before, "after":snapshot(self.runtime.root), "pythonRoot":str(self.runtime.root),"pythonError":python_error,"entryResult":normalize(getattr(self.runtime,"corpus_result",None))}

    def close(self):
        if self.runtime is not None:
            self.runtime.close()
            self.pristine.close()
        for saved in self.seeds.values():
            saved.close()


def main():
    # No scheduler, archive, WAL, timer, pool worker or provider may execute.
    with patch.object(codex_accounts, "codex_home", lambda: pathlib.Path(os.environ["CODEX_HOME"])), \
         patch.object(codex_accounts, "auth_metadata", lambda *_: {"status":"signedOut","accountId":None,"email":None,"plan":None}), \
         patch.object(concurrent.futures.ThreadPoolExecutor, "submit", lambda *_a, **_kw: concurrent.futures.Future()), \
         patch.object(threading.Thread, "start", lambda self: None), \
         patch.object(threading.Thread, "join", lambda self, *a, **k: None), \
         patch.object(codex_runtime, "_RuntimeWalKeeper", lambda *a: None):
        template = pathlib.Path(tempfile.mkdtemp(prefix="lifecycle-schema-"))
        initialized = QuietRuntime(template, lambda *_a, **_kw: None)
        initialized.close()
        lifecycle = LifecycleCases(template)
        for line in sys.stdin:
            command = json.loads(line)
            if command["command"] == "close":
                break
            module_name = command.get("module", command.get("case", {}).get("module"))
            if module_name is not None:
                assert module_name == "notification_dispatch", "unknown oracle module"
                import notification_dispatch as module
            else:
                module = sys.modules[__name__]
            try:
                if command["command"] == "normalize":
                    result = normalize(command["value"])
                elif command["command"] == "snapshot":
                    result = module.snapshot(command["root"])
                elif module is sys.modules[__name__]:
                    result = lifecycle.run(command)
                elif hasattr(module, "run_case"):
                    result = module.run_case(command, template, QuietRuntime)
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
        lifecycle.close()
        if "notification_dispatch" in sys.modules:
            sys.modules["notification_dispatch"].close()
        shutil.rmtree(template)


if __name__ == "__main__":
    main()
