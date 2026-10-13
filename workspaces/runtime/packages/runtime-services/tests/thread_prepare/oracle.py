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
import ast
import copy
import concurrent.futures
import contextlib
import json
import sqlite3
import tempfile
import threading
from unittest.mock import patch
from types import SimpleNamespace
import codex_runtime
from codex_runtime import Runtime
from codex_daybreak import turn_params
from codex_native_tools import digest
from codex_native_errors import NativeRpcError


def projection_functions():
    """Compile unchanged statements from real callers, never Runtime.start.

    This is an executable source projection: neither the parameter decisions nor
    the delayed-close association rule are reimplemented in the oracle.
    """
    tree = ast.parse((SERVER / "src/codex_runtime.py").read_text())
    runtime = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "Runtime")
    start = next(n for n in runtime.body if isinstance(n, ast.FunctionDef) and n.name == "start")
    selected = None
    for node in ast.walk(start):
        for _, body in ast.iter_fields(node):
            if not isinstance(body, list):
                continue
            for index, item in enumerate(body):
                if (isinstance(item, ast.Assign) and isinstance(item.value, ast.Dict)
                        and any(isinstance(k, ast.Constant) and k.value == "clientUserMessageId" for k in item.value.keys)):
                    end = next(i for i in range(index+1, len(body))
                               if isinstance(body[i], ast.If) and isinstance(body[i].test, ast.Name)
                               and body[i].test.id == "frozen_input")
                    selected = body[index:end]
    assert selected is not None, "turn caller projection moved"
    turn = ast.parse("def projected_turn(self, a, rows, asset_ids, text, program):\n    clocks=[]\n    timing={}\n    current={}\n").body[0]
    turn.body += copy.deepcopy(selected) + [ast.Return(ast.Name("params", ast.Load()))]
    close = next(n for n in ast.walk(runtime) if isinstance(n, ast.If)
                 and any(isinstance(v, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "preparation" for t in v.targets) for v in n.body)
                 and any(isinstance(v, ast.Constant) and v.value == "nativeReleaseId" for v in ast.walk(n)))
    closed = ast.parse("def projected_close(self, a, db, tid, connection_id):\n    pass\n").body[0]
    closed.body = copy.deepcopy(close.body)
    module = ast.fix_missing_locations(ast.Module(body=[turn, closed], type_ignores=[]))
    namespace = dict(vars(codex_runtime))
    exec(compile(module, str(SERVER / "src/codex_runtime.py"), "exec"), namespace)
    return namespace["projected_turn"], namespace["projected_close"]


projected_turn, projected_close = projection_functions()


def forbidden_runtime(*args, **kwargs):
    raise AssertionError("Runtime construction/start forbidden in thread oracle")


Runtime.__init__ = forbidden_runtime
Runtime.start = forbidden_runtime


def actor_of(s):
    a, settings = s["actor"], s["settings"]
    result = dict(id=a["id"], epoch=a["epoch"], threadId=a["thread_id"], cwd=a["cwd"],
                  compactions=a["compactions"], model=settings["model"], effort=settings["effort"],
                  fastMode=settings["fast_mode"],
                  yoloMode=settings["yolo_mode"], profileInstructions=settings["profile_instructions"],
                  role=settings["role"], isLead=settings["is_lead"], needsTitle=settings["needs_title"],
                  claudeOptions=settings["claude_options"], provider=s["account"]["provider"],
                  accountKey=s["account"]["key"], worktree=False, worktreeReady=True,
                  autoWake=a["auto_wake"], rootId=a["root_id"], parentId=a["parent_id"])
    if a["native_effort_present"]:
        result["nativeEffort"] = settings["native_effort"]
    for key, native in (("daybreak_enabled", "daybreakEnabled"), ("cyber_access_program", "cyberAccessProgram")):
        if settings[key]["state"] == "present":
            result[native] = settings[key]["value"]
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
    prepared_result = Runtime.prepared_result
    new_thread_params = Runtime.new_thread_params
    tool_definitions = Runtime.tool_definitions
    preparation_context_versions = Runtime.preparation_context_versions
    preparation_settings = staticmethod(Runtime.preparation_settings)
    operation_current = Runtime.operation_current
    submit_reserved = staticmethod(Runtime.submit_reserved)
    turn_permissions = Runtime.turn_permissions
    validate_execution = staticmethod(Runtime.validate_execution)

    def __init__(self, s, root):
        self.snapshot = s
        self.root = root
        self.lock = threading.RLock()
        self.closed = s["closed"]
        self.loaded = set()
        self.preparations = {}
        self.recorder = Recorder()
        self.artifacts = []
        self.changed = threading.Event()
        self.validation_calls = 0
        identity = s["account"]["supervisor_identity"]
        if identity:
            self.recorder.supervisor_mode = True
            self.recorder.proc = SimpleNamespace(root=identity["stateDir"], handle=identity["handle"], generation=identity["generation"])
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

    def catalog(self, account):
        self.validation_calls += 1
        return {"data": [{"model": self.snapshot["settings"]["model"],
                          "supportedReasoningEfforts": [{"reasoningEffort":v} for v in ("low", "medium", "high", "xhigh")],
                          "defaultReasoningEffort": "medium", "serviceTiers": [{"id":"priority"}]}]}

    def message_inputs(self, agent_id, text, assets):
        return [{"type":"text", "text":text}]

    def preparation_executor(self):
        return self

    def submit(self, fn, *args):
        return fn(*args)

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
                if command.get("tool_probe"):
                    try:
                        h.tool_definitions(h.agent(s["actor"]["id"]))
                    except (TypeError, KeyError, AttributeError):
                        return {"malformed": True}
                    return {"malformed": False}
                a = h.agent(s["actor"]["id"])
                a.update(copy.deepcopy(command.get("stored", {})))
                h.put(h.connection, "agents", a)
                h.connection.commit()
                original = copy.deepcopy(a)
                events = command.get("turn_input", {}).get("events") or [{"kind":"followup", "text":"prompt"}]
                rows = [dict(event, id="event") for event in events]
                assets = ["asset"] if command.get("turn_input", {}).get("has_assets") else []
                turn = projected_turn(h, a, rows, assets, "prompt", "standard")
                turn.pop("input")
                turn.pop("clientUserMessageId")
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
                result = dict(outcome=method, params=params, original=original, before=h.agent(a["id"]),
                              native_release_id=operation.get("nativeReleaseId"), validation_calls=h.validation_calls,
                              context_versions=operation["contextVersions"], turn=turn,
                              artifacts=h.artifacts, catalog_digest=digest(h.tool_definitions(a)))
                stored = h.agent(a["id"])
                external_delta = {}
                if "current" in command:
                    current = command["current"]
                    h.snapshot = current
                    current_actor = actor_of(current)
                    initial_actor = actor_of(s)
                    for key, value in current_actor.items():
                        if key not in initial_actor or value != initial_actor[key]:
                            external_delta[key] = value
                    stored.update(external_delta)
                    h.closed = current["closed"]
                h.put(h.connection, "agents", stored)
                if command.get("close"):
                    close = command["close"]
                    if close.get("release_id") and stored.get("nativeRelease"):
                        stored["nativeRelease"]["id"] = close["release_id"]
                    projected_close(h, stored, h.connection, close["thread_id"], close["connection_id"])
                    closed_actor = h.agent(a["id"])
                    for key, value in closed_actor.items():
                        if key not in result["before"] or value != result["before"][key]:
                            external_delta[key] = copy.deepcopy(value)
                h.connection.commit()
                result["external_delta"] = external_delta
                result["receipt_before"] = h.agent(a["id"])
                if command.get("receipt") is None:
                    result["settlement"] = "unknown"
                    return result
                r = command["receipt"]
                receipt = dict(r["fields"], thread={"id":r["thread_id"]}, reattached=r["reattached"])
                native = concurrent.futures.Future()
                if command.get("claude_working"):
                    native.set_exception(NativeRpcError({"code":-32000,"message":"Claude is still working"}))
                    Runtime.prepared_result(h, operation, native)
                    assert h.recorder.requests[-1] == ("thread/read", {"threadId":a["threadId"], "includeTurns":False})
                    read = concurrent.futures.Future()
                    read.set_result(receipt)
                    h.recorder.callback(read)
                    result["read_request"] = h.recorder.requests[-1]
                else:
                    operation["claudeReadSubmitted"] = r["claude_read_submitted"]
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
