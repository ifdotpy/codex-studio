#!/usr/bin/env python3
"""Retained cost callbacks release WAL snapshots and reject foreign owners."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import ast
from contextlib import contextmanager, closing
import hashlib
import importlib.util
from pathlib import Path
import sqlite3
import sys
import threading
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_cost_snapshot_update as update
from codex_source import signature

spec = importlib.util.spec_from_file_location(
    "cost_snapshot_refresh_fixture", ROOT / "tests/cost-refresh-live-update-contract.py")
refresh_fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(refresh_fixture)


def reviewed_cost_source():
    """Keep the released module and replace only the two reviewed methods."""
    base = refresh_fixture.released_cost_source()
    current = (ROOT / "scripts/codex_session_costs.py").read_bytes()
    def methods(raw):
        return {node.name: node for node in ast.walk(ast.parse(raw))
                if isinstance(node, ast.FunctionDef) and node.name in {"_compute", "_cost_usage_groups"}}
    old, new = methods(base), methods(current)
    lines = base.decode().splitlines(keepends=True)
    current_lines = current.decode().splitlines(keepends=True)
    for name in sorted(old, key=lambda name: old[name].lineno, reverse=True):
        prior, desired = old[name], new[name]
        lines[prior.lineno - 1:prior.end_lineno] = current_lines[desired.lineno - 1:desired.end_lineno]
    raw = "".join(lines).encode()
    if hashlib.sha256(raw).hexdigest() != update.SOURCE_HASH:
        raise AssertionError("The fixture is not the reviewed cost source")
    return raw


@contextmanager
def reviewed_fixture():
    with refresh_fixture.reviewed_fixture() as values:
        runtime, modules, reader, case, scripts = values
        # The prior release must validate its disk source before replacement.
        if refresh_fixture.update.apply(runtime) != {"status": "applied"}:
            raise AssertionError("The prior cost patch was not applied")
        if signature(reader._compute.__func__) != update.COMPUTE_BEFORE:
            raise AssertionError("The fixture has another cost caller")
        (scripts / "codex_session_costs.py").write_bytes(reviewed_cost_source())
        with patch.object(update, "__file__", str(scripts / "codex_cost_snapshot_update.py")):
            yield values


class CostSnapshotUpdateContract(unittest.TestCase):
    def test_retained_callback_releases_the_wal_snapshot_during_the_price_loop(self):
        with reviewed_fixture() as (runtime, modules, reader, case, _):
            callback = reader._cost_usage_groups
            function = callback.__func__
            compute_callback = reader._compute
            compute_function = compute_callback.__func__
            # Populate actual cache and file-cache entries before publication.
            first = reader._compute_shared("lead", "lead", refresh=True)
            self.assertAlmostEqual(first["totalUSD"], .00035)
            pending = {"event": threading.Event(), "result": None, "error": None}
            reader.inflight["another-root"] = pending
            reader.refreshing.add("another-root")
            states = {name: getattr(reader, name) for name in
                      ("cache", "file_cache", "refreshing", "inflight", "lock")}
            entries = reader.cache["lead"], tuple(reader.file_cache.items())
            servers = runtime.servers
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            self.assertIs(callback.__func__, function)
            self.assertIs(reader._cost_usage_groups.__func__, function)
            self.assertIs(reader._compute.__func__, compute_function)
            self.assertEqual(signature(function), update.AFTER)
            self.assertEqual(signature(reader._compute.__func__), update.COMPUTE_AFTER)
            self.assertIs(runtime.servers, servers)
            for name, state in states.items():
                self.assertIs(getattr(reader, name), state)
            self.assertIs(reader.inflight["another-root"], pending)
            self.assertIs(reader.cache["lead"], entries[0])
            self.assertEqual(tuple(reader.file_cache.items()), entries[1])

            with closing(sqlite3.connect(case.path, timeout=.05)) as writer:
                self.assertEqual(writer.execute("PRAGMA journal_mode=WAL").fetchone()[0], "wal")
                writer.execute("PRAGMA wal_autocheckpoint=0")
                writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
                entered, release = threading.Event(), threading.Event()
                results, errors, priced = [], [], []
                costs = modules["codex_session_costs"]
                price = costs.price_usage
                def gated_price(*args, **kwargs):
                    priced.append(args[2])
                    if not entered.is_set():
                        entered.set()
                        if not release.wait(3):
                            raise AssertionError("The price loop was not released")
                    return price(*args, **kwargs)
                def run():
                    try:
                        results.append(compute_callback("lead", "lead"))
                    except BaseException as error:
                        errors.append(error)
                # Calling the pre-update bound method must use the new code.
                with (patch.object(reader, "_cost_usage_groups", callback),
                      patch.object(reader, "_load_persisted", return_value=None),
                      patch.object(costs, "price_usage", gated_price)):
                    worker = threading.Thread(target=run)
                    # Bypass the ready cache to exercise the actual SQL cursor.
                    reader.cache.clear()
                    worker.start()
                    try:
                        self.assertTrue(entered.wait(2))
                        case.insert_usage(writer, 3, "codex-b", "gpt-6-luna", 2000, 0)
                        writer.execute("UPDATE analytics_usage_roots SET generation=generation+1 WHERE root='lead'")
                        writer.commit()
                        self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))
                        self.assertTrue(worker.is_alive())
                    finally:
                        release.set()
                        worker.join(3)
                self.assertFalse(worker.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(len(results), 1)
                self.assertAlmostEqual(results[0]["totalUSD"], first["totalUSD"])
                self.assertEqual(results[0]["pricedSamples"], 2)
                self.assertEqual(len(priced), 2)
                self.assertEqual(results[0]["_cacheSource"]["usage"], {"maxSeq": 2, "generation": 1})
                fresh = reader._compute("lead", "lead")
                self.assertEqual(fresh["pricedSamples"], 3)
                self.assertAlmostEqual(fresh["totalUSD"], .00055)

    def test_already_applied_keeps_code_and_live_state(self):
        with reviewed_fixture() as (runtime, _, reader, _, _):
            function = reader._cost_usage_groups.__func__
            cache, inflight, files, servers = reader.cache, reader.inflight, reader.file_cache, runtime.servers
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            code, caller = function.__code__, reader._compute.__func__.__code__
            self.assertEqual(update.apply(runtime), {"status": "already_applied"})
            self.assertIs(function.__code__, code)
            self.assertIs(reader._compute.__func__.__code__, caller)
            self.assertIs(reader.cache, cache)
            self.assertIs(reader.inflight, inflight)
            self.assertIs(reader.file_cache, files)
            self.assertIs(runtime.servers, servers)

    def test_retained_compute_releases_claude_only_snapshot_without_usage_table(self):
        with reviewed_fixture() as (runtime, modules, reader, case, _):
            callback = reader._compute
            function = callback.__func__
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            self.assertIs(reader._compute.__func__, function)
            with closing(sqlite3.connect(case.path, timeout=.05)) as writer:
                self.assertEqual(writer.execute("PRAGMA journal_mode=WAL").fetchone()[0], "wal")
                writer.execute("PRAGMA wal_autocheckpoint=0")
                writer.execute("DROP TABLE analytics_usage")
                writer.commit()
                self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))
                costs = modules["codex_session_costs"]
                price, prices = costs.price_usage, []
                def checkpoint_price(*args, **kwargs):
                    writer.execute("UPDATE analytics_usage_roots SET generation=2 WHERE root='lead'")
                    writer.commit()
                    self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))
                    result = price(*args, **kwargs)
                    prices.append(result)
                    return result
                with patch.object(costs, "price_usage", checkpoint_price):
                    old = callback("lead", "lead")
                self.assertEqual(len(prices), 1)
                self.assertEqual(old["pricedSamples"], 1)
                self.assertAlmostEqual(old["totalUSD"], .00025)
                self.assertEqual(old["_cacheSource"]["usage"], {"maxSeq": 0, "generation": 1})
                fresh = callback("lead", "lead")
                self.assertEqual(fresh["_cacheSource"]["usage"], {"maxSeq": 0, "generation": 2})
                self.assertEqual(fresh["totalUSD"], old["totalUSD"])

    def test_old_active_group_frame_finishes_once_without_replay(self):
        with reviewed_fixture() as (runtime, _, reader, case, _):
            callback = reader._cost_usage_groups
            entered, release = threading.Event(), threading.Event()
            results, errors, submissions = [], [], []
            class GatedConnection(sqlite3.Connection):
                def execute(self, sql, parameters=()):
                    if "CREATE TEMP TABLE session_cost_usage AS" in sql:
                        submissions.append(sql)
                        entered.set()
                        if not release.wait(3):
                            raise AssertionError("The old group frame was not released")
                    return super().execute(sql, parameters)
            def run():
                try:
                    with closing(sqlite3.connect(case.path, factory=GatedConnection)) as db:
                        db.execute("BEGIN")
                        db.execute("CREATE TEMP TABLE session_cost_claude_messages (account_key TEXT,thread_id TEXT,response_id TEXT)")
                        rows = list(callback(db, "lead"))
                        results.append((rows, db.in_transaction))
                        db.rollback()
                except BaseException as error:
                    errors.append(error)
            worker = threading.Thread(target=run)
            worker.start()
            try:
                self.assertTrue(entered.wait(2))
                self.assertEqual(update.apply(runtime), {"status": "applied"})
                self.assertTrue(worker.is_alive())
            finally:
                release.set()
                worker.join(3)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(len(submissions), 1)
            self.assertEqual(len(results), 1)
            self.assertTrue(results[0][1])
            with closing(sqlite3.connect(case.path)) as db:
                db.execute("BEGIN")
                db.execute("CREATE TEMP TABLE session_cost_claude_messages (account_key TEXT,thread_id TEXT,response_id TEXT)")
                self.assertEqual(list(callback(db, "lead")), results[0][0])
                self.assertFalse(db.in_transaction)

    def assert_rejects(self, mutate):
        with reviewed_fixture() as (runtime, modules, reader, _, scripts):
            target, caller = reader._cost_usage_groups.__func__, reader._compute.__func__
            before = target.__code__, target.__defaults__, target.__kwdefaults__, caller.__code__
            servers = runtime.servers
            with mutate(runtime, modules, scripts):
                with self.assertRaises(RuntimeError):
                    update.apply(runtime)
            self.assertEqual((target.__code__, target.__defaults__, target.__kwdefaults__, caller.__code__), before)
            self.assertIs(runtime.servers, servers)

    def test_unreviewed_sources_reject_without_a_swap(self):
        for name in ("codex_runtime", "codex_session_costs"):
            @contextmanager
            def changed(runtime, modules, scripts):
                path = Path(modules[name].__file__)
                path.write_bytes(path.read_bytes() + b"\n# foreign\n")
                yield
            with self.subTest(name=name):
                self.assert_rejects(changed)

    def test_foreign_module_paths_and_runtime_type_reject(self):
        for name in ("codex_runtime", "codex_session_costs"):
            @contextmanager
            def changed(runtime, modules, scripts):
                with patch.object(modules[name], "__file__", str(scripts / "foreign.py")):
                    yield
            with self.subTest(name=name):
                self.assert_rejects(changed)
        @contextmanager
        def wrong_type(runtime, modules, scripts):
            with patch.object(modules["codex_runtime"], "Runtime", type("Runtime", (), {})):
                yield
        self.assert_rejects(wrong_type)

    def test_foreign_class_owner_and_runtime_owner_reject(self):
        for name, class_name in (("codex_runtime", "Runtime"), ("codex_session_costs", "SessionCostReader")):
            @contextmanager
            def changed(runtime, modules, scripts):
                with patch.object(getattr(modules[name], class_name), "__module__", "foreign"):
                    yield
            with self.subTest(name=name):
                self.assert_rejects(changed)

    def test_changed_caller_and_target_reject_without_a_swap(self):
        for name in ("_compute", "_cost_usage_groups", "_connect"):
            for foreign_globals in (False, True):
                @contextmanager
                def changed(runtime, modules, scripts):
                    owner = modules["codex_session_costs"].SessionCostReader
                    current = getattr(owner, name)
                    code = current.__code__ if foreign_globals else (lambda *args: None).__code__
                    globals_ = {} if foreign_globals else vars(modules["codex_session_costs"])
                    alien = FunctionType(code, globals_, current.__name__, current.__defaults__)
                    alien.__kwdefaults__ = current.__kwdefaults__
                    with patch.object(owner, name, alien):
                        yield
                with self.subTest(name=name, foreign_globals=foreign_globals):
                    self.assert_rejects(changed)

    def test_closed_runtime_rejects_before_publication(self):
        @contextmanager
        def changed(runtime, modules, scripts):
            with patch.object(runtime, "closed", True):
                yield
        self.assert_rejects(changed)

    def test_owner_and_close_races_reject_under_the_lock(self):
        for change in ("closed", "module"):
            @contextmanager
            def changed(runtime, modules, scripts):
                class MutatingLock:
                    def __enter__(self):
                        if change == "closed":
                            runtime.closed = True
                        else:
                            sys.modules["codex_session_costs"] = ModuleType("codex_session_costs")
                    def __exit__(self, *args):
                        return False
                with patch.object(runtime, "lock", MutatingLock()):
                    try:
                        yield
                    finally:
                        runtime.closed = False
                        sys.modules["codex_session_costs"] = modules["codex_session_costs"]
            with self.subTest(change=change):
                self.assert_rejects(changed)


if __name__ == "__main__":
    unittest.main()
