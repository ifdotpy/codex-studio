#!/usr/bin/env python3
"""The checkpoint update preserves retained work and rejects changed owners."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import ast
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing, contextmanager
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_checkpoint_update as update
from codex_source import signature

BASE = subprocess.check_output(
    ["git", "show", "23b88cc842153aa03c40e9d741f35195d5f7bb73:scripts/codex_workspace.py"], cwd=ROOT)
RUNTIME = subprocess.check_output(
    ["git", "show", "85f3f8d8:scripts/codex_runtime.py"], cwd=ROOT)
assert hashlib.sha256(BASE).hexdigest() == "06fcd5b89e6c8895a1df314986732055ab828a13a99b24e814f7543446aa3f54"
assert hashlib.sha256(RUNTIME).hexdigest() == "7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450"

spec = importlib.util.spec_from_file_location("checkpoint_workspace_fixture", ROOT / "tests/workspace-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def workspace_methods(raw):
    owner = next(node for node in ast.parse(raw).body
                 if isinstance(node, ast.ClassDef) and node.name == "WorkspaceMixin")
    return owner, {node.name: node for node in owner.body if isinstance(node, ast.FunctionDef)}


def reviewed_source():
    """Recreate the installed source with only the eight reviewed methods."""
    owner, old = workspace_methods(BASE)
    current = (ROOT / "scripts/codex_workspace.py").read_bytes()
    _, new = workspace_methods(current)
    lines = BASE.decode().splitlines(keepends=True)
    desired = current.decode().splitlines(keepends=True)
    edits, additions = [], []
    for name, before, _after in update.FUNCTIONS:
        node = new[name]
        method = desired[node.lineno - 1:node.end_lineno]
        if before is None:
            additions.extend(["\n", *method, "\n"])
        else:
            previous = old[name]
            edits.append((previous.lineno - 1, previous.end_lineno, method))
    edits.append((owner.end_lineno, owner.end_lineno, additions))
    for start, stop, replacement in sorted(edits, reverse=True):
        lines[start:stop] = replacement
    raw = "".join(lines).encode()
    if hashlib.sha256(raw).hexdigest() != update.SOURCE_HASH:
        raise AssertionError("The recreated reviewed checkpoint source differs")
    return raw


@contextmanager
def reviewed_fixture():
    with tempfile.TemporaryDirectory(prefix="studio-checkpoint-update-") as temporary:
        root = Path(temporary)
        scripts = root / "scripts"
        scripts.mkdir()
        project = root / "project"
        project.mkdir()
        workspace = ModuleType("codex_workspace")
        workspace.__file__ = str(scripts / "codex_workspace.py")
        Path(workspace.__file__).write_bytes(reviewed_source())
        exec(compile(BASE, workspace.__file__, "exec"), vars(workspace))
        module = ModuleType("codex_runtime")
        module.__file__ = str(scripts / "codex_runtime.py")
        Path(module.__file__).write_bytes(RUNTIME)
        modules = {"codex_workspace": workspace, "codex_runtime": module}
        with (patch.dict(sys.modules, modules),
              patch.dict(os.environ, {"CODEX_BOARD_STATE_DIR": str(root / "board")}),
              patch.object(update, "__file__", str(scripts / "codex_checkpoint_update.py"))):
            exec(compile(RUNTIME, module.__file__, "exec"), vars(module))
            # Keep the released Runtime and real pools. Disable automatic dispatch.
            module.Runtime.schedule = fixture.ControlledRuntime.schedule
            runtime = module.Runtime(root / "state", fixture.WorkspaceServer)
            runtime.image_workspace_support = lambda _repo: (False, "disabled in fixture")
            try:
                yield runtime, workspace, module, scripts, project
            finally:
                runtime.closed = False
                runtime.close()


class CheckpointLiveUpdateContract(unittest.TestCase):
    @staticmethod
    def methods(workspace):
        owner = workspace.WorkspaceMixin
        return tuple((name, id(current), current.__code__, signature(current)) if current is not None
                     else (name, None, None, None)
                     for name, _before, _after in update.FUNCTIONS
                     for current in (owner.__dict__.get(name),))

    def assert_rejects(self, mutation, message="differs"):
        with reviewed_fixture() as (runtime, workspace, module, scripts, _project):
            with mutation(runtime, workspace, module, scripts):
                before = self.methods(workspace)
                with self.assertRaisesRegex(RuntimeError, message):
                    update.apply(runtime)
                self.assertEqual(self.methods(workspace), before)

    def test_retained_callbacks_and_native_objects_survive_apply_and_repeat(self):
        with reviewed_fixture() as (runtime, workspace, _module, _scripts, project):
            agent = runtime.create({"name": "Lead", "cwd": str(project), "prompt": ""}, draft=True, defer=True)
            server = runtime.connect(agent["accountKey"])
            callbacks = {name: getattr(runtime, name) for name, before, _after in update.FUNCTIONS if before is not None}
            dependency = runtime.checkpoint_after_turn
            objects = {name: getattr(runtime, name) for name in
                       ("pool", "servers", "connection_ids", "prepare_locks", "lock")}
            with runtime.db() as db:
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,?,?,?,?,?)",
                           ("original-input", agent["id"], "user", "Exact input", "reserved", 1, agent["epoch"]))
                before_rows = [tuple(row) for row in db.execute("SELECT * FROM runtime_events")]
            calls = list(server.calls)
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            for name, callback in callbacks.items():
                self.assertIs(callback.__func__, getattr(workspace.WorkspaceMixin, name))
                self.assertEqual(signature(callback.__func__), next(after for target, _before, after in update.FUNCTIONS if target == name))
            self.assertIs(runtime.checkpoint_after_turn.__func__, dependency.__func__)
            self.assertEqual(signature(dependency.__func__), "2496fa20127c050cf96901774d6abf544c7490fbfac7019ea62bf0fd42324135")
            for name, original in objects.items():
                self.assertIs(getattr(runtime, name), original)
            self.assertIs(runtime.connect(agent["accountKey"]), server)
            self.assertEqual(server.calls, calls)
            with runtime.db() as db:
                self.assertEqual([tuple(row) for row in db.execute("SELECT * FROM runtime_events")], before_rows)
            applied = self.methods(workspace)
            self.assertEqual(update.apply(runtime), {"status": "already_applied"})
            self.assertEqual(self.methods(workspace), applied)

    def test_foreign_runtime_class_and_module_are_rejected(self):
        def foreign_class(runtime, _workspace, module, _scripts):
            return patch.object(module, "Runtime", type("Runtime", (), {"__module__": "codex_runtime"}))
        self.assert_rejects(foreign_class)
        def foreign_module(_runtime, _workspace, module, _scripts):
            return patch.object(module.Runtime, "__module__", "foreign_runtime")
        self.assert_rejects(foreign_module)

    def test_foreign_workspace_module_and_owner_are_rejected(self):
        def foreign_module(_runtime, workspace, _module, _scripts):
            foreign = ModuleType("codex_workspace")
            vars(foreign).update(vars(workspace))
            return patch.dict(sys.modules, {"codex_workspace": foreign})
        self.assert_rejects(foreign_module)
        def foreign_owner(_runtime, workspace, _module, _scripts):
            return patch.object(workspace, "WorkspaceMixin", type("WorkspaceMixin", (), {"__module__": "codex_workspace"}))
        self.assert_rejects(foreign_owner)

    def test_foreign_source_paths_are_rejected(self):
        for name in ("codex_runtime", "codex_workspace"):
            def mutation(_runtime, workspace, module, scripts):
                return patch.object(module if name == "codex_runtime" else workspace,
                                    "__file__", str(scripts.parent / (name + ".py")))
            with self.subTest(name=name):
                self.assert_rejects(mutation)

    def test_changed_files_are_rejected_before_any_method_change(self):
        for name in ("codex_runtime", "codex_workspace"):
            @contextmanager
            def mutation(_runtime, _workspace, _module, scripts):
                path = scripts / (name + ".py")
                path.write_bytes(path.read_bytes() + b"\n# unreviewed fixture source\n")
                yield
            with self.subTest(name=name):
                self.assert_rejects(mutation)

    def test_dependency_code_and_globals_are_rejected(self):
        def changed(_runtime, workspace, _module, _scripts):
            foreign = FunctionType((lambda *args: None).__code__, vars(workspace))
            return patch.object(workspace.WorkspaceMixin, "checkpoint_after_turn", foreign)
        self.assert_rejects(changed, "dependency differs")
        def foreign_globals(_runtime, workspace, _module, _scripts):
            current = workspace.WorkspaceMixin.checkpoint_after_turn
            foreign = FunctionType(current.__code__, dict(vars(workspace)), current.__name__, current.__defaults__)
            foreign.__kwdefaults__ = current.__kwdefaults__
            return patch.object(workspace.WorkspaceMixin, "checkpoint_after_turn", foreign)
        self.assert_rejects(foreign_globals, "dependency differs")

    def test_unreviewed_current_method_rejects_before_helper_additions(self):
        def changed(_runtime, workspace, _module, _scripts):
            foreign = FunctionType((lambda *args: None).__code__, vars(workspace))
            return patch.object(workspace.WorkspaceMixin, "_assert_workspace_idle", foreign)
        self.assert_rejects(changed, "function differs")

    def test_changed_desired_signature_rejects_before_any_method_change(self):
        def changed(_runtime, _workspace, _module, _scripts):
            pins = [list(pin) for pin in update.FUNCTIONS]
            pins[-1][2] = "0" * 64
            return patch.object(update, "FUNCTIONS", pins)
        self.assert_rejects(changed, "reviewed checkpoint function differs")

    def test_an_added_helper_with_another_owner_is_rejected(self):
        def shadowed(_runtime, _workspace, module, _scripts):
            return patch.object(module.Runtime, "_resolve_workspace_idle", lambda *args: None, create=True)
        self.assert_rejects(shadowed, "another owner")

    def test_phase_and_monitor_contract_changes_are_rejected(self):
        def phase(_runtime, workspace, _module, _scripts):
            return patch.object(workspace.WorkspaceMixin, "WORKSPACE_OPERATION_ACTIVE", {"completed"})
        self.assert_rejects(phase)
        def monitor(_runtime, workspace, _module, _scripts):
            return patch.object(workspace, "ACTIVE_MONITOR_STATUSES", ("completed",))
        self.assert_rejects(monitor)

    def test_closed_runtime_is_rejected(self):
        def closed(runtime, _workspace, _module, _scripts):
            return patch.object(runtime, "closed", True)
        self.assert_rejects(closed)

    def test_close_while_apply_waits_for_the_lock_rejects_without_changes(self):
        with reviewed_fixture() as (runtime, workspace, _module, _scripts, _project):
            before = self.methods(workspace)
            compiled = threading.Event()
            extract = update.source_function
            def observed(*args, **kwargs):
                result = extract(*args, **kwargs)
                if args[1][-1] == update.FUNCTIONS[-1][0]:
                    compiled.set()
                return result
            with patch.object(update, "source_function", observed), ThreadPoolExecutor(max_workers=1) as pool:
                with runtime.lock:
                    applying = pool.submit(update.apply, runtime)
                    self.assertTrue(compiled.wait(3))
                    runtime.closed = True
                with self.assertRaisesRegex(RuntimeError, "owner differs"):
                    applying.result(timeout=3)
            self.assertEqual(self.methods(workspace), before)

    def test_retained_notification_releases_locks_before_the_committed_turn_path_check(self):
        with reviewed_fixture() as (runtime, _workspace, _module, _scripts, project):
            notice_callback = runtime.notification
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            self.assertIs(runtime.notification.__func__, notice_callback.__func__)
            agent = runtime.create({"name": "Lead", "cwd": str(project), "prompt": ""}, draft=True, defer=True)
            with runtime.lock, runtime.db() as db:
                agent = runtime.agent(agent["id"], db)
                agent.update(autoWake=True, status="running", inFlight=True,
                             threadId="checkpoint-thread", turnId="checkpoint-turn",
                             turnEpoch=agent["epoch"], worktreeReady=True)
                runtime.put(db, "agents", agent)
                db.execute("CREATE TABLE fixture_writer(id TEXT PRIMARY KEY)")
            entered, release = threading.Event(), threading.Event()
            captures, jobs = [], []
            resolve, submit = Path.resolve, runtime.pool.submit
            def blocked(path, *args, **kwargs):
                if str(path) == agent["cwd"] and not entered.is_set():
                    entered.set()
                    if not release.wait(5):
                        raise TimeoutError("The fixture did not release the notification path check")
                return resolve(path, *args, **kwargs)
            def submitted(function, *args, **kwargs):
                future = submit(function, *args, **kwargs)
                if function.__name__ in {"_checkpoint_after_committed_turn", "checkpoint_after_turn"}:
                    jobs.append(future)
                return future
            def capture(key, label, turn_id):
                self.assertFalse(runtime.lock._is_owned())
                captures.append((key, label, turn_id))
                return {"id": "private-checkpoint"}
            with (patch.object(Path, "resolve", blocked),
                  patch.object(runtime.pool, "submit", side_effect=submitted),
                  patch.object(runtime, "capture_checkpoint", side_effect=capture),
                  ThreadPoolExecutor(max_workers=1) as pool):
                notice = pool.submit(notice_callback, {"method": "turn/completed", "params": {
                    "threadId": agent["threadId"], "turn": {"id": agent["turnId"], "status": "completed"}}})
                try:
                    self.assertTrue(entered.wait(3))
                    acquired = runtime.lock.acquire(blocking=False)
                    if acquired:
                        runtime.lock.release()
                    self.assertTrue(acquired, "The retained notification must release Runtime.lock before paths")
                    with closing(sqlite3.connect(runtime.db_path, timeout=0)) as writer, writer:
                        writer.execute("INSERT INTO fixture_writer VALUES ('during-notification-path-check')")
                        writer.commit()
                        self.assertIsNotNone(writer.execute("SELECT id FROM runtime_completed_turns WHERE id=?",
                                             (agent["id"] + ":" + agent["turnId"],)).fetchone())
                    notice.result(timeout=1)
                finally:
                    release.set()
                    notice.result(timeout=3)
                    for job in jobs:
                        job.result(timeout=3)
            self.assertEqual(captures, [(agent["id"], "After turn", agent["turnId"])])
            with runtime.db() as db:
                self.assertEqual([json.loads(row[0])["phase"] for row in
                                  db.execute("SELECT record FROM runtime_workspace_operations")], ["completed"])

    def test_queued_old_reservation_releases_locks_when_paths_block_after_apply(self):
        with reviewed_fixture() as (runtime, workspace, _module, _scripts, project):
            agent = runtime.create({"name": "Lead", "cwd": str(project), "prompt": ""}, draft=True, defer=True)
            with runtime.lock, runtime.db() as db:
                agent = runtime.agent(agent["id"], db)
                agent.update(autoWake=True, status="completed", inFlight=False, turnId=None)
                runtime.put(db, "agents", agent)
                operation_id = runtime._reserve_checkpoint(db, agent, "checkpoint", "old-turn")
                before = runtime._workspace_operation(db, operation_id)
                self.assertNotIn("source", before)
                db.execute("CREATE TABLE fixture_writer(id TEXT PRIMARY KEY)")
            callback = runtime._capture_reserved_checkpoint
            captures = []
            occupied, start_jobs, entered, release = (threading.Event() for _ in range(4))
            count_lock = threading.Lock()
            count = 0
            def occupy():
                nonlocal count
                with count_lock:
                    count += 1
                    if count == runtime.pool._max_workers:
                        occupied.set()
                if not start_jobs.wait(5):
                    raise TimeoutError("The fixture did not release the pool")
            blockers = [runtime.pool.submit(occupy) for _ in range(runtime.pool._max_workers)]
            resolve = Path.resolve
            def blocked(path, *args, **kwargs):
                if str(path) == agent["cwd"] and not entered.is_set():
                    entered.set()
                    if not release.wait(5):
                        raise TimeoutError("The fixture did not release the path check")
                return resolve(path, *args, **kwargs)
            def capture(key, label, turn_id):
                self.assertFalse(runtime.lock._is_owned())
                captures.append((key, label, turn_id))
                return {"id": "private-checkpoint"}
            with patch.object(Path, "resolve", blocked), patch.object(runtime, "capture_checkpoint", side_effect=capture):
                future = None
                try:
                    self.assertTrue(occupied.wait(3))
                    future = runtime.pool.submit(callback, agent["id"], "After turn", "old-turn", operation_id)
                    self.assertFalse(future.done())
                    pool = runtime.pool
                    self.assertEqual(update.apply(runtime), {"status": "applied"})
                    self.assertIs(runtime.pool, pool)
                    self.assertIs(callback.__func__, workspace.WorkspaceMixin._capture_reserved_checkpoint)
                    with runtime.db() as db:
                        self.assertEqual(runtime._workspace_operation(db, operation_id), before)
                    start_jobs.set()
                    self.assertTrue(entered.wait(3))
                    acquired = runtime.lock.acquire(blocking=False)
                    if acquired:
                        runtime.lock.release()
                    self.assertTrue(acquired, "Path.resolve must release the retained Runtime lock")
                    with closing(sqlite3.connect(runtime.db_path, timeout=0)) as writer, writer:
                        writer.execute("INSERT INTO fixture_writer VALUES ('while-blocked')")
                        writer.commit()
                finally:
                    start_jobs.set()
                    release.set()
                    for blocker in blockers:
                        blocker.result(timeout=3)
                    if future is not None:
                        future.result(timeout=3)
            self.assertEqual(captures, [(agent["id"], "After turn", "old-turn")])
            with runtime.db() as db:
                self.assertEqual(runtime._workspace_operation(db, operation_id)["phase"], "completed")
                self.assertIsNone(runtime.agent(agent["id"], db).get("workspaceReservationId"))


if __name__ == "__main__":
    unittest.main()
