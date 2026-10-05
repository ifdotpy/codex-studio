#!/usr/bin/env python3
"""Reviewed controls preserve legacy callbacks and reject changed sources."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import ast
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing, contextmanager
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import threading
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_workspace_controls_update as update
from codex_source import signature

spec = importlib.util.spec_from_file_location("checkpoint_live_fixture", ROOT / "tests/checkpoint-live-update-contract.py")
previous = importlib.util.module_from_spec(spec)
spec.loader.exec_module(previous)


def git_source(revision, name):
    return subprocess.check_output(["git", "show", revision + ":scripts/" + name + ".py"], cwd=ROOT)


def methods(raw, owner=None):
    tree = ast.parse(raw)
    if owner is not None:
        tree = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == owner)
    return tree, {node.name: node for node in tree.body if isinstance(node, ast.FunctionDef)}


def splice(base, current, names, owner=None):
    _, old = methods(base, owner)
    _, new = methods(current, owner)
    lines, desired = base.decode().splitlines(keepends=True), current.decode().splitlines(keepends=True)
    edits = [(old[name].lineno - 1, old[name].end_lineno,
              desired[new[name].lineno - 1:new[name].end_lineno]) for name in names]
    for start, stop, replacement in sorted(edits, reverse=True):
        lines[start:stop] = replacement
    return "".join(lines).encode()


def installed_workspace():
    owner, old = methods(previous.BASE, "WorkspaceMixin")
    fixed = git_source("4ca328c2", "codex_workspace")
    _, new = methods(fixed, "WorkspaceMixin")
    lines, desired = previous.BASE.decode().splitlines(keepends=True), fixed.decode().splitlines(keepends=True)
    edits, additions = [], []
    for name, before, _after in previous.update.FUNCTIONS:
        node = new[name]
        method = desired[node.lineno - 1:node.end_lineno]
        if before is None:
            additions.extend(["\n", *method, "\n"])
        else:
            node = old[name]
            edits.append((node.lineno - 1, node.end_lineno, method))
    edits.append((owner.end_lineno, owner.end_lineno, additions))
    for start, stop, replacement in sorted(edits, reverse=True):
        lines[start:stop] = replacement
    raw = "".join(lines).encode()
    assert hashlib.sha256(raw).hexdigest() == "0fa845cb60e5b7000fbd0c7f62514fbe40d6e3ffbf920dee6ab78a2bd345879a"
    return raw


WORKSPACE = installed_workspace()
CLAUDE = git_source("23b88cc842153aa03c40e9d741f35195d5f7bb73", "codex_claude_controls")
assert hashlib.sha256(CLAUDE).hexdigest() == "ab5f90540aadc1695110e02fb43fa7031bc07ae2e5b3a5103d405a6cb00eca7a"
CANDIDATE_WORKSPACE = splice(WORKSPACE, git_source("67511ec5", "codex_workspace"),
                             [name for name, _before, _after in update.FUNCTIONS], "WorkspaceMixin")
CANDIDATE_CLAUDE = splice(CLAUDE, git_source("67511ec5", "codex_claude_controls"), ["action"])
assert hashlib.sha256(CANDIDATE_WORKSPACE).hexdigest() == update.WORKSPACE_HASH
assert hashlib.sha256(CANDIDATE_CLAUDE).hexdigest() == update.CLAUDE_HASH


@contextmanager
def controls_fixture():
    runtime_bytes = previous.RUNTIME
    source_path = os.environ.get("STUDIO_WORKSPACE_CONTROLS_RUNTIME")
    if source_path:
        runtime_bytes = Path(source_path).read_bytes()
        if hashlib.sha256(runtime_bytes).hexdigest() != "9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7":
            raise AssertionError("The private installed Runtime fixture differs")
    with (patch.object(previous, "RUNTIME", runtime_bytes),
          patch.object(previous, "reviewed_source", return_value=WORKSPACE),
          previous.reviewed_fixture() as (runtime, workspace, module, scripts, project)):
        assert previous.update.apply(runtime) == {"status": "applied"}
        controls = ModuleType("codex_claude_controls")
        controls.__file__ = str(scripts / "codex_claude_controls.py")
        Path(controls.__file__).write_bytes(CANDIDATE_CLAUDE)
        exec(compile(CLAUDE, controls.__file__, "exec"), vars(controls))
        Path(workspace.__file__).write_bytes(CANDIDATE_WORKSPACE)
        for name in ("codex-orchestrator", "codex-subagent", "codex-workspace"):
            path = scripts.parent / ".agents" / "skills" / name / "SKILL.md"
            path.parent.mkdir(parents=True)
            path.write_bytes((ROOT / ".agents" / "skills" / name / "SKILL.md").read_bytes())
        with (patch.dict(sys.modules, {"codex_claude_controls": controls}),
              patch.object(update, "__file__", str(scripts / "codex_workspace_controls_update.py"))):
            yield runtime, workspace, controls, module, scripts, project


class WorkspaceControlsLiveUpdateContract(unittest.TestCase):
    @staticmethod
    def targets(workspace, controls):
        values = [vars(workspace.WorkspaceMixin).get(name) for name, _before, _after in update.FUNCTIONS]
        values.append(controls.action)
        return tuple((id(function), function.__code__, signature(function)) if function is not None
                     else (None, None, None) for function in values)

    def rejects(self, change, message="differ"):
        with controls_fixture() as (runtime, workspace, controls, module, scripts, project):
            retained = [getattr(workspace.WorkspaceMixin, name) for name, _before, _after in update.FUNCTIONS]
            retained.append(controls.action)
            retained_codes = [function.__code__ for function in retained]
            with change(runtime, workspace, controls, module, scripts, project):
                before = self.targets(workspace, controls)
                with self.assertRaisesRegex(RuntimeError, message):
                    update.apply(runtime)
                self.assertEqual(self.targets(workspace, controls), before)
                self.assertEqual([function.__code__ for function in retained], retained_codes)

    def test_retained_callbacks_pools_servers_and_reserved_inputs_survive_repeat(self):
        with controls_fixture() as (runtime, workspace, controls, _module, _scripts, project):
            agent = runtime.create({"name": "Lead", "cwd": str(project), "prompt": ""}, draft=True, defer=True)
            server = runtime.connect(agent["accountKey"])
            callbacks = [getattr(runtime, name) for name, _before, _after in update.FUNCTIONS]
            action = controls.action
            objects = {name: getattr(runtime, name) for name in
                       ("lock", "pool", "tool_pool", "coordination_pool", "servers", "prepare_locks", "connection_ids")}
            with runtime.lock, runtime.db() as db:
                agent = runtime.agent(agent["id"], db)
                operation_id = runtime._reserve_checkpoint(db, agent, "capture", "old-turn")
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,?,?,?,?,?)",
                           ("reserved-user-id", agent["id"], "user", "Exact original input", "reserved", 1, agent["epoch"]))
                rows = {table: [tuple(row) for row in db.execute("SELECT * FROM " + table)] for table in
                        ("runtime_agents", "runtime_events", "runtime_workspace_operations")}
            inflight = runtime.__dict__.setdefault("_claude_control_inflight", {"old-control-request"})
            calls = list(server.calls)
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            for callback, (name, _before, after) in zip(callbacks, update.FUNCTIONS):
                self.assertIs(callback.__func__, vars(workspace.WorkspaceMixin)[name])
                self.assertEqual(signature(callback.__func__), after)
            self.assertIs(action, controls.action)
            self.assertEqual(signature(action), update.CLAUDE_FUNCTION[2])
            self.assertEqual(signature(runtime.checkpoint_after_turn.__func__),
                             "2496fa20127c050cf96901774d6abf544c7490fbfac7019ea62bf0fd42324135")
            for name, original in objects.items():
                self.assertIs(getattr(runtime, name), original)
            self.assertIs(runtime._claude_control_inflight, inflight)
            self.assertEqual(inflight, {"old-control-request"})
            self.assertIs(runtime.connect(agent["accountKey"]), server)
            self.assertEqual(server.calls, calls)
            with runtime.db() as db:
                for table, before in rows.items():
                    self.assertEqual([tuple(row) for row in db.execute("SELECT * FROM " + table)], before)
                self.assertEqual(runtime._workspace_operation(db, operation_id)["phase"], "capture_pending")
            after = self.targets(workspace, controls)
            self.assertEqual(update.apply(runtime), {"status": "already_applied"})
            self.assertEqual(self.targets(workspace, controls), after)

    def test_foreign_runtime_class_and_workspace_owner_reject(self):
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(module, "Runtime", type("Runtime", (), {"__module__": "codex_runtime"})))
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(ws, "WorkspaceMixin", type("WorkspaceMixin", (), {"__module__": "codex_workspace"})))

    def test_foreign_modules_reject(self):
        for name in ("codex_runtime", "codex_workspace", "codex_claude_controls"):
            def changed(rt, ws, cc, module, scripts, project):
                original = {"codex_runtime": module, "codex_workspace": ws, "codex_claude_controls": cc}[name]
                clone = ModuleType(name)
                vars(clone).update(vars(original))
                return patch.dict(sys.modules, {name: clone})
            with self.subTest(name=name):
                self.rejects(changed)

    def test_foreign_paths_and_changed_files_reject(self):
        for name in ("codex_runtime", "codex_workspace", "codex_claude_controls"):
            def path(rt, ws, cc, module, scripts, project):
                original = {"codex_runtime": module, "codex_workspace": ws, "codex_claude_controls": cc}[name]
                return patch.object(original, "__file__", str(scripts.parent / (name + ".py")))
            @contextmanager
            def source(rt, ws, cc, module, scripts, project):
                current = scripts / (name + ".py")
                current.write_bytes(current.read_bytes() + b"\n# unreviewed source\n")
                yield
            with self.subTest(name=name):
                self.rejects(path)
                self.rejects(source)

    def test_unreviewed_workspace_and_claude_targets_reject_before_edits(self):
        for name in ("checkpoint_capture", "_restore_checkpoint_locked", "action"):
            def changed(rt, ws, cc, module, scripts, project):
                owner = cc if name == "action" else ws.WorkspaceMixin
                namespace = vars(cc) if name == "action" else vars(ws)
                return patch.object(owner, name, FunctionType((lambda *args: None).__code__, namespace))
            with self.subTest(name=name):
                self.rejects(changed, "function differs")

    def test_foreign_target_globals_reject_before_edits(self):
        for name in ("checkpoint_capture", "action"):
            def changed(rt, ws, cc, module, scripts, project):
                owner = cc if name == "action" else ws.WorkspaceMixin
                function = getattr(owner, name)
                clone = FunctionType(function.__code__, dict(function.__globals__), function.__name__, function.__defaults__)
                clone.__kwdefaults__ = function.__kwdefaults__
                return patch.object(owner, name, clone)
            with self.subTest(name=name):
                self.rejects(changed, "globals differ")

    def test_dependencies_and_descriptor_modes_reject_before_edits(self):
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(ws.WorkspaceMixin, "_resolve_workspace_idle", FunctionType((lambda *args: None).__code__, vars(ws))),
                     "dependency differs")
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(cc, "_pending", FunctionType((lambda *args: None).__code__, vars(cc))), "dependency differs")
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(ws.WorkspaceMixin, "_workspace_source", ws.WorkspaceMixin._workspace_source), "globals differ")
        for name in ("_resolve_workspace_idle", "_pending"):
            def changed(rt, ws, cc, module, scripts, project):
                owner = cc if name == "_pending" else ws.WorkspaceMixin
                function = getattr(owner, name)
                clone = FunctionType(function.__code__, dict(function.__globals__), function.__name__, function.__defaults__)
                clone.__kwdefaults__ = function.__kwdefaults__
                return patch.object(owner, name, clone)
            with self.subTest(name=name):
                self.rejects(changed, "differ")

    def test_runtime_override_and_changed_contract_reject(self):
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(module.Runtime, "checkpoint_capture", ws.WorkspaceMixin.checkpoint_capture, create=True), "owner differs")
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(ws.WorkspaceMixin, "WORKSPACE_OPERATION_ACTIVE", {"completed"}), "contract differs")
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(cc, "_MODES", {"default"}), "contract differs")
        self.rejects(lambda rt, ws, cc, module, scripts, project:
                     patch.object(rt, "checkpoint_capture", rt.checkpoint_capture), "owner differs")

    def test_changed_desired_signatures_reject_before_edits(self):
        pins = [list(value) for value in update.FUNCTIONS]
        pins[-1][2] = "0" * 64
        self.rejects(lambda *args: patch.object(update, "FUNCTIONS", tuple(tuple(pin) for pin in pins)),
                     "reviewed workspace control function differs")
        self.rejects(lambda *args: patch.object(update, "CLAUDE_FUNCTION", ("action", update.CLAUDE_FUNCTION[1], "0" * 64)),
                     "reviewed Claude control function differs")

    def test_closed_runtime_rejects(self):
        self.rejects(lambda rt, *args: patch.object(rt, "closed", True))

    def test_close_and_source_change_while_waiting_for_the_lock_reject(self):
        for change in ("closed", "source"):
            with self.subTest(change=change), controls_fixture() as (runtime, workspace, controls, _module, _scripts, _project):
                before = self.targets(workspace, controls)
                compiled = threading.Event()
                extract = update.source_function
                def observed(*args, **kwargs):
                    result = extract(*args, **kwargs)
                    if args[1] == ["action"]:
                        compiled.set()
                    return result
                with patch.object(update, "source_function", observed), ThreadPoolExecutor(max_workers=1) as pool:
                    with runtime.lock:
                        applying = pool.submit(update.apply, runtime)
                        self.assertTrue(compiled.wait(3))
                        if change == "closed":
                            runtime.closed = True
                        else:
                            path = Path(controls.__file__)
                            path.write_bytes(path.read_bytes() + b"\n# changed during the lock wait\n")
                    with self.assertRaisesRegex(RuntimeError, "differ"):
                        applying.result(timeout=3)
                self.assertEqual(self.targets(workspace, controls), before)

    @staticmethod
    def agent(runtime, project):
        agent = runtime.create({"name": "Lead", "cwd": str(project), "prompt": ""}, draft=True, defer=True)
        with runtime.lock, runtime.db() as db:
            agent = runtime.agent(agent["id"], db)
            agent.update(autoWake=True, status="completed", inFlight=False,
                         threadId="logical-thread", turnId=None, worktreeReady=True)
            runtime.put(db, "agents", agent)
            db.execute("CREATE TABLE fixture_controls_probe(id TEXT PRIMARY KEY)")
        return agent

    @contextmanager
    def resolving(self, runtime, agent, callback, *args):
        entered, release = threading.Event(), threading.Event()
        resolve = Path.resolve
        def blocked(path, *args, **kwargs):
            if str(path) == agent["cwd"] and not entered.is_set():
                entered.set()
                if not release.wait(5):
                    raise TimeoutError("The controls fixture did not release the path check")
            return resolve(path, *args, **kwargs)
        with patch.object(Path, "resolve", blocked), ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(callback, *args)
            try:
                self.assertTrue(entered.wait(3), "The retained callback must reach its real path check")
                yield future
            finally:
                release.set()

    def assert_progress(self, runtime):
        acquired = runtime.lock.acquire(blocking=False)
        if acquired:
            runtime.lock.release()
        self.assertTrue(acquired, "The retained control callback must release Runtime.lock before paths")
        with closing(sqlite3.connect(runtime.db_path, timeout=0)) as writer, writer:
            writer.execute("INSERT INTO fixture_controls_probe VALUES ('during-path-check')")
            writer.commit()
            self.assertEqual(writer.execute("SELECT id FROM fixture_controls_probe").fetchall(),
                             [("during-path-check",)])

    @staticmethod
    def repository(project):
        (project / "tracked.txt").write_text("base\n")
        subprocess.check_output(["git", "-C", str(project), "init", "-q"])
        subprocess.check_output(["git", "-C", str(project), "add", "tracked.txt"])
        subprocess.check_output(["git", "-C", str(project), "-c", "user.name=Fixture", "-c",
                                 "user.email=fixture@example.invalid", "commit", "-qm", "Private base"])

    def test_retained_manual_capture_callback_releases_lock_and_writer(self):
        with controls_fixture() as (runtime, _workspace, _controls, _module, _scripts, project):
            callback = runtime.checkpoint_capture
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            self.repository(project)
            agent = self.agent(runtime, project)
            with self.resolving(runtime, agent, callback, agent["id"]) as future:
                self.assert_progress(runtime)
                receipt = runtime.send(agent["id"], "The private next input", "manual-exact-input")
                self.assertEqual(runtime.delivery_receipt(receipt["id"])["status"], "pending")
            checkpoint = future.result(timeout=3)
            self.assertEqual(checkpoint["agent"], agent["id"])
            self.assertEqual(runtime.delivery_receipt("manual-exact-input")["status"], "pending")
            with runtime.db() as db:
                phases = [json.loads(row[0])["phase"] for row in db.execute("SELECT record FROM runtime_workspace_operations")]
            self.assertEqual(phases, ["completed"])

    def test_retained_fresh_restore_callback_releases_lock_and_writer_and_retries_once(self):
        with controls_fixture() as (runtime, _workspace, _controls, _module, _scripts, project):
            callback = runtime._restore_checkpoint_locked
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            self.repository(project)
            agent = self.agent(runtime, project)
            checkpoint = runtime.checkpoint_capture(agent["id"], turn_id="saved-turn")
            (project / "tracked.txt").write_text("private edit\n")
            preview = runtime.checkpoint_preview(agent["id"], checkpoint["id"])
            data = {"checkpoint_id": checkpoint["id"], "expectedTree": preview["expectedTree"]}
            server = runtime.connect(agent["accountKey"])
            with self.resolving(runtime, agent, callback, agent["id"], data) as future:
                self.assert_progress(runtime)
            result = future.result(timeout=3)
            self.assertEqual(result, {"status": "restored", "checkpoint": checkpoint["id"]})
            self.assertEqual((project / "tracked.txt").read_text(), "base\n")
            self.assertEqual(sum(method == "thread/fork" for method, _params in server.calls), 1)
            with patch.object(runtime, "_resolve_workspace_idle", side_effect=AssertionError("Do not repeat admission")):
                self.assertEqual(callback(agent["id"], data), result)
            self.assertEqual(sum(method == "thread/fork" for method, _params in server.calls), 1)

    @contextmanager
    def claude_server(self, runtime):
        get = runtime.accounts.get
        server = runtime.connect()
        state = {"turns": [{"id": "turn-1", "status": "completed"}, {"id": "turn-2", "status": "completed"}], "tasks": []}
        receipts = {}
        def call(method, params, timeout=60):
            server.calls.append((method, copy.deepcopy(params)))
            if method == "claude/state":
                return copy.deepcopy(state)
            if method == "thread/queue/list":
                return {"items": []}
            if method == "claude/settings":
                return {"settings": params["settings"]}
            if method == "thread/rollback":
                receipts.setdefault(params["requestId"], {"threadId": params["threadId"], "nativeId": "private-fork"})
                return receipts[params["requestId"]]
            raise AssertionError("The fixture must not send another native request: " + method)
        with (patch.object(runtime.accounts, "get", side_effect=lambda key: {**get(key), "provider": "claude", "status": "ready"}),
              patch.object(server, "call", side_effect=call)):
            yield server

    def check_claude_path(self, kind):
        with controls_fixture() as (runtime, _workspace, controls, _module, _scripts, project):
            callback = controls.action
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            agent = self.agent(runtime, project)
            body = {"id": agent["id"], "action": kind, "request_id": "exact-control-request"}
            body.update({"settings": {"permissionMode": "plan"}} if kind == "settings" else {"turn_id": "turn-2"})
            with self.claude_server(runtime) as server:
                with self.resolving(runtime, agent, callback, runtime, body) as future:
                    self.assert_progress(runtime)
                result = future.result(timeout=3)
                method = "claude/settings" if kind == "settings" else "thread/rollback"
                self.assertEqual(sum(name == method for name, _params in server.calls), 1)
                with patch.object(runtime, "_resolve_workspace_idle", side_effect=AssertionError("Do not repeat admission")):
                    self.assertEqual(callback(runtime, body), result)
                self.assertEqual(sum(name == method for name, _params in server.calls), 1)
                if kind == "rollback":
                    params = next(params for name, params in server.calls if name == method)
                    self.assertEqual(params["requestId"], body["request_id"])
                    self.assertEqual(params["turnId"], body["turn_id"])
                with runtime.db() as db:
                    operation = runtime._workspace_operation(db, "claude-control:" + body["request_id"])
                self.assertEqual(operation["phase"], "completed")
                self.assertEqual(operation["requestId"], body["request_id"])
                self.assertIsNone(runtime.agent(agent["id"]).get("workspaceReservationId"))

    def test_retained_claude_settings_callback_releases_lock_and_writer(self):
        self.check_claude_path("settings")

    def test_retained_claude_rollback_callback_releases_lock_and_writer(self):
        self.check_claude_path("rollback")

    def test_stop_during_retained_claude_preflight_preserves_input_and_sends_no_action(self):
        with controls_fixture() as (runtime, _workspace, controls, _module, _scripts, project):
            callback = controls.action
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            agent = self.agent(runtime, project)
            body = {"id": agent["id"], "action": "settings", "request_id": "stopped-control",
                    "settings": {"permissionMode": "plan"}}
            with self.claude_server(runtime) as server:
                with self.resolving(runtime, agent, callback, runtime, body) as future:
                    with runtime.lock, runtime.db() as db:
                        current = runtime.agent(agent["id"], db)
                        current.update(epoch=current["epoch"] + 1, autoWake=False, status="paused")
                        runtime.put(db, "agents", current)
                        db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,?,?,?,?,?)",
                                   ("unknown-before-stop", current["id"], "user", "Exact uncertain input", "uncertain", 1, agent["epoch"]))
                with self.assertRaisesRegex(ValueError, "session changed"):
                    future.result(timeout=3)
                self.assertFalse(any(method == "claude/settings" for method, _params in server.calls))
            with runtime.db() as db:
                self.assertIsNone(runtime._workspace_operation(db, "claude-control:stopped-control"))
                row = db.execute("SELECT text,status,epoch FROM runtime_events WHERE id='unknown-before-stop'").fetchone()
            self.assertEqual(tuple(row), ("Exact uncertain input", "uncertain", agent["epoch"]))


if __name__ == "__main__":
    unittest.main()
