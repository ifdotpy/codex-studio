"""Guarded real-Python thread preparation oracle; no Runtime construction/start.

Only effect boundaries (connection, files/guidance, clock and storage adapter)
are replaced. prepare_locked/new_thread_params/tool_definitions/turn_permissions/
preparation_context_versions/prepared_result and catalog hashing run unchanged.
"""
import os
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard

GUARD = install_guard(block_processes=True)
if "--guard-test" in sys.argv:
    GUARD.self_test()
    from oracle_guard import IsolationViolation
    for event in ("subprocess.Popen", "os.system", "os.exec", "os.posix_spawn", "socket.connect", "socket.bind"):
        try:
            GUARD.audit(event, ())
        except IsolationViolation:
            pass
        else:
            raise AssertionError("effect guard failed")
    raise SystemExit(0)

# No broad harness import or Runtime constructor: even an accidental process
# spawn/network attempt during imports is fatal under the audit guard.
for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"):
    assert os.environ.get(key) and pathlib.Path(os.environ[key]).is_relative_to("/var/tmp"), key
assert os.environ.get("RUSTUP_HOME") and os.environ.get("CARGO_HOME")
SERVER = pathlib.Path(__file__).resolve().parents[4] / "apps/server"
sys.path.insert(0, str(SERVER / "src"))
import copy
import concurrent.futures
import contextlib
import json
import sqlite3
import tempfile
import threading
from unittest.mock import patch
import codex_runtime
from codex_runtime import Runtime
from codex_daybreak import turn_params
from codex_native_tools import digest


def forbidden_runtime(*args, **kwargs):
    raise AssertionError("Runtime construction/start forbidden in thread oracle")


Runtime.__init__ = forbidden_runtime
Runtime.start = forbidden_runtime


def actor_of(s):
    a, settings = s["actor"], s["settings"]
    result = dict(id=a["id"], epoch=a["epoch"], threadId=a["thread_id"], cwd=a["cwd"],
                  compactions=a["compactions"], model=settings["model"], effort=settings["effort"],
                  nativeEffort=settings["native_effort"], fastMode=settings["fast_mode"],
                  yoloMode=settings["yolo_mode"], profileInstructions=settings["profile_instructions"],
                  role=settings["role"], isLead=settings["is_lead"], needsTitle=settings["needs_title"],
                  claudeOptions=settings["claude_options"], provider=s["account"]["provider"],
                  accountKey=s["account"]["key"], worktree=False, worktreeReady=True,
                  autoWake=True, rootId="root", parentId=None)
    for key, native in (("prepare_attempt", "prepareAttempt"), ("native_teleport_tool", "nativeTeleportTool"),
                        ("native_role_guidance", "nativeRoleGuidance"), ("native_tool_source", "nativeToolSource"),
                        ("native_tool_update", "nativeToolUpdate"), ("error", "error"), ("native_release", "nativeRelease")):
        if a.get(key) is not None:
            result[native] = copy.deepcopy(a[key])
    if a["deleted"]:
        result["deletedAt"] = "deleted"
    return result


class Recorder:
    """An effect recorder, never a provider object or protocol connection."""
    def __init__(self):
        self.requests = []

    def submit(self, method, params):
        self.requests.append((method, copy.deepcopy(params)))
        return concurrent.futures.Future()

    def on_result(self, submitted, callback):
        self.callback = callback


class Host:
    new_thread_params = Runtime.new_thread_params
    tool_definitions = Runtime.tool_definitions
    preparation_context_versions = Runtime.preparation_context_versions
    preparation_settings = staticmethod(Runtime.preparation_settings)
    operation_current = Runtime.operation_current
    submit_reserved = staticmethod(Runtime.submit_reserved)
    turn_permissions = Runtime.turn_permissions

    def __init__(self, s, root):
        self.snapshot = s
        self.root = root
        self.lock = threading.RLock()
        self.closed = s["closed"]
        self.loaded = set()
        self.preparations = {}
        self.recorder = Recorder()
        self.artifacts = []
        self.connection = sqlite3.connect(root / "oracle.sqlite3")
        self.connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)")
        self.put(self.connection, "agents", actor_of(s))
        self.connection.commit()

    @contextlib.contextmanager
    def db(self):
        with self.connection:
            yield self.connection

    def agent(self, identity, db=None):
        return json.loads(self.connection.execute("SELECT record FROM runtime_agents WHERE id=?", (identity,)).fetchone()[0])

    def put(self, db, table, actor):
        assert table == "agents"
        db.execute("INSERT OR REPLACE INTO runtime_agents VALUES (?,?)", (actor["id"], json.dumps(actor)))

    def connect_agent(self, actor):
        return self.recorder

    def server_for(self, *args):
        return self.recorder

    def agent_connection(self, actor):
        return self.snapshot["account"]["connection_id"]

    def connection_current(self, account, connection):
        return connection == self.snapshot["account"]["connection_id"]

    def progress_file(self, actor):
        directory = self.snapshot["workspace"].get("progress_directory")
        return pathlib.Path(directory) / "PROGRESS.md" if directory else None

    def role_guidance(self, actor):
        return actor.get("nativeRoleGuidance") or self.snapshot["guidance"]["role"]

    def remember_role_text(self, text):
        self.artifacts.append(text)

    def federation(self):
        return self

    def enabled(self):
        return self.snapshot["root"]["federation_enabled"]


def run(command):
    s = command["snapshot"]
    with tempfile.TemporaryDirectory(prefix="thread-oracle-") as name:
        root = pathlib.Path(name)
        legacy = root / ".agents/skills/codex-workspace/references/native-tools.json"
        legacy.parent.mkdir(parents=True)
        legacy.write_text(json.dumps(s["catalog"]["definitions"]))
        h = Host(s, root)
        try:
            with contextlib.ExitStack() as stack:
                for target, value in (
                    ("codex_runtime.REPOSITORY_ROOT", root),
                    ("codex_runtime.TOOLS", s["catalog"]["definitions"]),
                    ("codex_runtime.THREAD_CONFIG", s["guidance"]["thread_config"]),
                    ("codex_runtime.INSTRUCTIONS", s["guidance"]["instructions"]),
                    ("codex_runtime.uid", lambda: command.get("attempt_id", "attempt")),
                    ("codex_vm_agents.progress_context", lambda *a: s["guidance"]["progress"]),
                    ("codex_nested_delegation.can_delegate", lambda *a: s["root"]["can_delegate"]),
                    ("codex_browser.configure_browser", lambda *a: None),
                    ("codex_native_release.reconcile_unknown", lambda *a: None),
                    ("codex_native_release.time.time", lambda: 1000.0),
                ):
                    stack.enter_context(patch(target, value))
                a = h.agent(s["actor"]["id"])
                turn = dict(threadId=a["threadId"], model=a["model"], cwd=a["cwd"],
                            serviceTier="priority" if a["fastMode"] else "default")
                turn.update(h.turn_permissions(a))
                turn.update(turn_params(a))
                if a["nativeEffort"] is not None:
                    turn["effort"] = a["nativeEffort"]
                if a["provider"] == "claude":
                    turn["dynamicTools"] = h.tool_definitions(a)
                if s["pending"]:
                    h.preparations[a["id"]] = {"id": s["pending"]["attempt_id"],
                        "connectionId": s["pending"]["connection_id"], "future": concurrent.futures.Future()}
                if s["loaded"]:
                    h.loaded.add(a["id"])
                before = copy.deepcopy(a)
                prepared = Runtime.prepare_locked(h, a)
                if not h.recorder.requests:
                    return {"outcome": "hold" if isinstance(prepared, concurrent.futures.Future) else "ready", "requests": [], "turn": turn}
                method, params = h.recorder.requests[0]
                operation = h.preparations[a["id"]]
                result = dict(outcome=method, params=params, before=h.agent(a["id"]),
                              context_versions=operation["contextVersions"], turn=turn,
                              artifacts=h.artifacts, catalog_digest=digest(h.tool_definitions(a)))
                if "current" in command:
                    current = command["current"]
                    h.snapshot = current
                    current_actor = actor_of(current)
                    for key in ("nativeTeleportTool", "nativeRoleGuidance", "nativeToolSource"):
                        current_actor.setdefault(key, result["before"][key])
                    h.put(h.connection, "agents", current_actor)
                    h.connection.commit()
                    h.closed = current["closed"]
                stored = h.agent(a["id"])
                stored.update(copy.deepcopy(command.get("stored", {})))
                h.put(h.connection, "agents", stored)
                h.connection.commit()
                result["receipt_before"] = h.agent(a["id"])
                if command.get("receipt") is None:
                    result["settlement"] = "unknown"
                    return result
                r = command["receipt"]
                receipt = dict(r["fields"], thread={"id":r["thread_id"]}, reattached=r["reattached"])
                operation["unloaded"] = command.get("unloaded", False)
                operation["claudeReadSubmitted"] = r["claude_read_submitted"]
                native = concurrent.futures.Future()
                native.set_result(receipt)
                Runtime.prepared_result(h, operation, native)
                try:
                    operation["future"].result()
                    result["settlement"] = "accepted"
                except Exception as error:
                    result["settlement"] = "unknown" if "outcome unknown" in str(error) else "stale"
                result["after"] = h.agent(a["id"])
                result["loaded"] = a["id"] in h.loaded
                return result
        finally:
            h.connection.close()


for line in sys.stdin:
    command = json.loads(line)
    if command.get("command") == "close":
        break
    try:
        print(json.dumps({"result": run(command)}, ensure_ascii=False), flush=True)
    except BaseException as error:
        import traceback
        traceback.print_exc(file=sys.stderr)
        print(json.dumps({"error":type(error).__name__ + ": " + str(error)}), flush=True)
