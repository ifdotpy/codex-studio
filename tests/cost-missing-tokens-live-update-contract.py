#!/usr/bin/env python3
"""A guarded cost code swap preserves readers and rejects foreign code."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import closing, contextmanager
import importlib.util
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_cost_missing_tokens_update as update
from codex_source import signature, source_function

spec = importlib.util.spec_from_file_location(
    "missing_token_fixture", ROOT / "tests/session-cost-missing-tokens-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
BASELINE = subprocess.check_output(
    ["git", "show", "85f3f8d8:scripts/codex_session_costs.py"], cwd=ROOT)
# This released updater accepts only its reviewed source versions.
REVIEWED_COSTS = subprocess.check_output(
    ["git", "show", "d4788f80:scripts/codex_session_costs.py"], cwd=ROOT)
REVIEWED_RUNTIME = subprocess.check_output(
    ["git", "show", "85f3f8d8:scripts/codex_runtime.py"], cwd=ROOT)


@contextmanager
def reviewed_fixture():
    with tempfile.TemporaryDirectory(prefix="studio-nullable-cost-update-") as folder:
        scripts = Path(folder)
        path = scripts / "codex_session_costs.py"
        raw = REVIEWED_COSTS
        path.write_bytes(raw)
        costs = ModuleType("codex_session_costs")
        costs.__file__ = str(path)
        exec(compile(raw, str(path), "exec"), vars(costs))
        old, _ = source_function(BASELINE, ["SessionCostReader", "_compute"],
                                 vars(costs), "<baseline-85f3f8d8>")
        costs.SessionCostReader._compute = old
        runtime_module = ModuleType("codex_runtime")
        runtime_path = scripts / "codex_runtime.py"
        runtime_path.write_bytes(REVIEWED_RUNTIME)
        runtime_module.__file__ = str(runtime_path)
        exec("class Runtime:\n    pass\n", vars(runtime_module))
        runtime = runtime_module.Runtime()
        runtime.closed = False
        runtime.lock = threading.RLock()
        runtime.servers = {"retained": object()}
        runtime.root = scripts / "state"
        runtime.root.mkdir()
        runtime.db_path = runtime.root / "canvas.sqlite3"
        runtime.accounts = {}
        with closing(sqlite3.connect(runtime.db_path)) as db, db:
            db.executescript("""
              CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);
              CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
              CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT,
                root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
              CREATE INDEX usage_root ON analytics_usage(root,seq);
            """)
            db.execute("INSERT INTO analytics_agents VALUES ('lead',?)",
                       (json.dumps({"rootId": "lead"}),))
            record = {"responseId": "legacy", "model": "claude-opus-5-5",
                      "inputTokensAreUncached": True,
                      "delta": {"inputTokens": 100, "cachedInputTokens": 20,
                                "outputTokens": 10}}
            db.execute("INSERT INTO analytics_usage VALUES (1,'lead','lead',"
                       "'thread','turn',1,?)", (json.dumps(record),))
        reader = costs.SessionCostReader(runtime.db_path, fixture.FixedPricing(),
                                         runtime.accounts, state_root=runtime.root)
        with (patch.dict(sys.modules, {"codex_runtime": runtime_module,
                                      "codex_session_costs": costs}),
              patch.object(update, "__file__", str(scripts / "codex_cost_missing_tokens_update.py"))):
            yield runtime, costs, reader, path, runtime_path


class CostMissingTokensUpdateContract(unittest.TestCase):
    def test_existing_bound_reader_prices_the_same_nullable_row_after_the_swap(self):
        with reviewed_fixture() as (runtime, costs, reader, _, _):
            callback = reader._compute
            function = callback.__func__
            self.assertEqual(signature(function), update.BEFORE)
            with self.assertRaises(TypeError):
                callback("lead", "lead")
            reader.cache["other"] = object()
            reader.file_cache["other"] = object()
            reader.refreshing.add("other")
            reader.inflight["other"] = {"event": threading.Event()}
            preserved = {name: getattr(reader, name) for name in
                         ("cache", "file_cache", "refreshing", "inflight", "lock")}
            values = {name: value.copy() for name, value in preserved.items()
                      if name != "lock"}
            servers = runtime.servers
            namespace = function.__globals__
            groups = costs.SessionCostReader._cost_usage_groups
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            self.assertIs(costs.SessionCostReader._compute, function)
            self.assertIs(reader._compute.__func__, function)
            self.assertIs(callback.__func__, function)
            self.assertIs(function.__globals__, namespace)
            self.assertIs(costs.SessionCostReader._cost_usage_groups, groups)
            self.assertIs(runtime.servers, servers)
            for name, value in preserved.items():
                self.assertIs(getattr(reader, name), value)
                if name != "lock":
                    self.assertEqual(value, values[name])
            result = callback("lead", "lead")
            self.assertEqual(result["pricedSamples"], 1)
            self.assertAlmostEqual(result["totalUSD"], .00025)
            self.assertEqual(result["unknownModels"], [])
            self.assertAlmostEqual(reader.snapshot("lead", wait=True)["totalUSD"], .00025)
            code = function.__code__
            self.assertEqual(update.apply(runtime), {"status": "already_applied"})
            self.assertIs(function.__code__, code)

    def test_an_old_active_frame_finishes_without_a_reader_or_thread_restart(self):
        with reviewed_fixture() as (runtime, _, reader, _, _):
            callback = reader._compute
            connect = reader._connect
            entered, release = threading.Event(), threading.Event()
            errors = []

            def gated_connect():
                entered.set()
                if not release.wait(3):
                    raise AssertionError("The old reader frame was not released")
                return connect()

            def compute():
                try:
                    callback("lead", "lead")
                except Exception as error:
                    errors.append(error)

            with patch.object(reader, "_connect", gated_connect):
                worker = threading.Thread(target=compute)
                worker.start()
                try:
                    self.assertTrue(entered.wait(2))
                    self.assertEqual(update.apply(runtime), {"status": "applied"})
                    self.assertTrue(worker.is_alive())
                finally:
                    release.set()
                    worker.join(3)
            self.assertFalse(worker.is_alive())
            self.assertEqual(len(errors), 1)
            self.assertIsInstance(errors[0], TypeError)
            self.assertAlmostEqual(callback("lead", "lead")["totalUSD"], .00025)

    def test_foreign_cost_and_runtime_sources_reject_before_mutation(self):
        for name in ("cost", "runtime"):
            with self.subTest(name=name), reviewed_fixture() as (runtime, _, reader, path, runtime_path):
                code = reader._compute.__func__.__code__
                target = path if name == "cost" else runtime_path
                target.write_bytes(target.read_bytes() + b"\n# foreign change\n")
                with self.assertRaisesRegex(RuntimeError, "source differs"):
                    update.apply(runtime)
                self.assertIs(reader._compute.__func__.__code__, code)

    def test_foreign_module_path_rejects_before_mutation(self):
        with reviewed_fixture() as (runtime, costs, reader, path, _):
            code = reader._compute.__func__.__code__
            costs.__file__ = str(path.parent / "foreign" / path.name)
            with self.assertRaisesRegex(RuntimeError, "source identity differs"):
                update.apply(runtime)
            self.assertIs(reader._compute.__func__.__code__, code)

    def test_module_replacement_after_source_validation_rejects_before_mutation(self):
        with reviewed_fixture() as (runtime, costs, reader, _, _):
            code = reader._compute.__func__.__code__
            extract = update.source_function

            def replace_module(*args, **kwargs):
                result = extract(*args, **kwargs)
                foreign = ModuleType("codex_session_costs")
                vars(foreign).update(vars(costs))
                sys.modules["codex_session_costs"] = foreign
                return result

            with patch.object(update, "source_function", side_effect=replace_module):
                with self.assertRaisesRegex(RuntimeError, "module identity differs"):
                    update.apply(runtime)
            self.assertIs(reader._compute.__func__.__code__, code)

    def test_foreign_runtime_instance_rejects_before_mutation(self):
        with reviewed_fixture() as (runtime, _, reader, _, _):
            code = reader._compute.__func__.__code__
            class ForeignRuntime(type(runtime)):
                pass
            foreign = ForeignRuntime()
            foreign.__dict__.update(vars(runtime))
            with self.assertRaisesRegex(RuntimeError, "source identity differs"):
                update.apply(foreign)
            self.assertIs(reader._compute.__func__.__code__, code)

    def test_foreign_runtime_and_cost_class_owners_reject_before_mutation(self):
        for name in ("runtime", "cost"):
            with self.subTest(name=name), reviewed_fixture() as (runtime, costs, reader, _, _):
                code = reader._compute.__func__.__code__
                owner = type(runtime) if name == "runtime" else costs.SessionCostReader
                owner.__module__ = "foreign"
                with self.assertRaisesRegex(RuntimeError, "owner differs"):
                    update.apply(runtime)
                self.assertIs(reader._compute.__func__.__code__, code)

    def test_foreign_globals_reject_before_mutation(self):
        for name in ("_compute", "_cost_usage_groups"):
            with self.subTest(name=name), reviewed_fixture() as (runtime, costs, _, _, _):
                owner = costs.SessionCostReader
                original = getattr(owner, name)
                foreign = FunctionType(original.__code__, dict(vars(costs)),
                                       original.__name__, original.__defaults__)
                foreign.__kwdefaults__ = original.__kwdefaults__
                setattr(owner, name, foreign)
                code = owner._compute.__code__
                with self.assertRaisesRegex(RuntimeError, "function owner differs"):
                    update.apply(runtime)
                self.assertIs(owner._compute.__code__, code)

    def test_changed_groups_dependency_rejects_before_mutation(self):
        with reviewed_fixture() as (runtime, costs, reader, _, _):
            code = reader._compute.__func__.__code__
            unknown, _ = source_function("def _cost_usage_groups(self,db,root):\n    return ()\n",
                                         ["_cost_usage_groups"], vars(costs))
            costs.SessionCostReader._cost_usage_groups = unknown
            with self.assertRaisesRegex(RuntimeError, "cost guard differs"):
                update.apply(runtime)
            self.assertIs(reader._compute.__func__.__code__, code)

    def test_unknown_current_function_rejects_without_mutation(self):
        with reviewed_fixture() as (runtime, costs, reader, _, _):
            unknown, _ = source_function("def _compute(self,agent_id,root):\n    return None\n",
                                         ["_compute"], vars(costs))
            costs.SessionCostReader._compute.__code__ = unknown.__code__
            code = reader._compute.__func__.__code__
            with self.assertRaisesRegex(RuntimeError, "running cost function differs"):
                update.apply(runtime)
            self.assertIs(reader._compute.__func__.__code__, code)

    def test_wrong_desired_function_rejects_before_mutation(self):
        with reviewed_fixture() as (runtime, costs, reader, _, _):
            code = reader._compute.__func__.__code__
            old, _ = source_function(BASELINE, ["SessionCostReader", "_compute"], vars(costs))
            with patch.object(update, "source_function", return_value=(old, False)):
                with self.assertRaisesRegex(RuntimeError, "reviewed cost function differs"):
                    update.apply(runtime)
            self.assertIs(reader._compute.__func__.__code__, code)

    def test_closed_runtime_rejects_before_mutation(self):
        with reviewed_fixture() as (runtime, _, reader, _, _):
            code = reader._compute.__func__.__code__
            runtime.closed = True
            with self.assertRaisesRegex(RuntimeError, "source identity differs"):
                update.apply(runtime)
            self.assertIs(reader._compute.__func__.__code__, code)

    def test_close_while_waiting_for_runtime_lock_preserves_the_old_code(self):
        with reviewed_fixture() as (runtime, _, reader, _, _):
            code = reader._compute.__func__.__code__
            errors = []

            def apply():
                try:
                    update.apply(runtime)
                except Exception as error:
                    errors.append(error)

            with runtime.lock:
                worker = threading.Thread(target=apply)
                worker.start()
                deadline = time.monotonic() + 2
                waiting = False
                while time.monotonic() < deadline:
                    frame = sys._current_frames().get(worker.ident)
                    while frame is not None:
                        if frame.f_code is update.apply.__code__ and "desired" in frame.f_locals:
                            waiting = True
                            break
                        frame = frame.f_back
                    if waiting:
                        break
                    threading.Event().wait(.005)
                self.assertTrue(waiting, "The update did not reach the runtime lock")
                self.assertIs(reader._compute.__func__.__code__, code)
                runtime.closed = True
            worker.join(2)
            self.assertFalse(worker.is_alive())
            self.assertEqual(len(errors), 1)
            self.assertRegex(str(errors[0]), "backend is closed")
            self.assertIs(reader._compute.__func__.__code__, code)


if __name__ == "__main__":
    unittest.main()
