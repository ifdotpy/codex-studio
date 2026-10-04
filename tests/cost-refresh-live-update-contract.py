#!/usr/bin/env python3
"""The released cost patch keeps callbacks and rejects unreviewed code."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import ast
from contextlib import contextmanager
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_cost_refresh_update as update
from codex_source import signature, source_function

spec = importlib.util.spec_from_file_location(
    "claude_refresh_fixture", ROOT / "tests/claude-cost-refresh-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
BASE_COSTS = subprocess.check_output(
    ["git", "show", "d4788f80:scripts/codex_session_costs.py"], cwd=ROOT)
BASE_PRICING = subprocess.check_output(
    ["git", "show", "6316a847:scripts/codex_pricing.py"], cwd=ROOT)
RUNTIME = subprocess.check_output(
    ["git", "show", "85f3f8d8:scripts/codex_runtime.py"], cwd=ROOT)


def released_cost_source():
    current = (ROOT / "scripts/codex_session_costs.py").read_bytes()
    def compute(raw):
        return next(node for node in ast.walk(ast.parse(raw))
                    if isinstance(node, ast.FunctionDef) and node.name == "_compute")
    old, new = compute(BASE_COSTS), compute(current)
    lines = BASE_COSTS.decode().splitlines(keepends=True)
    desired = current.decode().splitlines(keepends=True)[new.lineno - 1:new.end_lineno]
    return "".join(lines[:old.lineno - 1] + desired + lines[old.end_lineno:]).encode()


@contextmanager
def reviewed_fixture():
    with tempfile.TemporaryDirectory(prefix="studio-cost-refresh-update-") as folder:
        scripts = Path(folder)
        modules = {}
        for name, raw in (("codex_pricing", (ROOT / "scripts/codex_pricing.py").read_bytes()),
                          ("codex_session_costs", released_cost_source())):
            path = scripts / (name + ".py")
            path.write_bytes(raw)
            module = ModuleType(name)
            module.__file__ = str(path)
            modules[name] = module
            with patch.dict(sys.modules, modules):
                exec(compile(raw, str(path), "exec"), vars(module))
        pricing, costs = modules["codex_pricing"], modules["codex_session_costs"]
        pricing.price_usage.__code__ = source_function(BASE_PRICING, ["price_usage"], vars(pricing))[0].__code__
        costs.SessionCostReader._compute.__code__ = source_function(
            BASE_COSTS, ["SessionCostReader", "_compute"], vars(costs))[0].__code__
        module = ModuleType("codex_runtime")
        module.__file__ = str(scripts / "codex_runtime.py")
        Path(module.__file__).write_bytes(RUNTIME)
        exec("class Runtime:\n    pass\n", vars(module))
        modules["codex_runtime"] = module
        runtime = module.Runtime()
        runtime.lock, runtime.closed = threading.RLock(), False
        runtime.servers = {"retained": object()}
        case = fixture.ClaudeCostRefreshContract()
        case.setUp()
        reader = costs.SessionCostReader(case.path, case.pricing, case.accounts, state_root=case.state)
        with (patch.dict(sys.modules, modules),
              patch.object(update, "__file__", str(scripts / "codex_cost_refresh_update.py"))):
            try:
                yield runtime, modules, reader, case, scripts
            finally:
                case.doCleanups()


class CostRefreshUpdateContract(unittest.TestCase):
    def test_existing_reader_and_pricing_aliases_use_the_fix(self):
        with reviewed_fixture() as (runtime, modules, reader, case, _):
            costs, pricing = modules["codex_session_costs"], modules["codex_pricing"]
            callback = reader._compute
            price_alias = costs.price_usage
            function = callback.__func__
            first = reader._compute_shared("lead", "lead", refresh=True)
            case.append_log({"type": "progress"})
            with patch.object(reader, "_cost_usage_groups", wraps=reader._cost_usage_groups) as scan:
                reader._compute_shared("lead", "lead", refresh=True)
                self.assertEqual(scan.call_count, 1)
            states = {name: getattr(reader, name) for name in
                      ("cache", "file_cache", "refreshing", "inflight", "lock")}
            servers = runtime.servers
            self.assertEqual(update.apply(runtime), {"status": "applied"})
            self.assertIs(callback.__func__, function)
            self.assertIs(reader._compute.__func__, function)
            self.assertIs(costs.price_usage, price_alias)
            self.assertIs(pricing.price_usage, price_alias)
            self.assertIs(runtime.servers, servers)
            for name, state in states.items():
                self.assertIs(getattr(reader, name), state)
            # The legacy cache gets one reviewed digest before it can skip SQL.
            case.append_log({"type": "progress"})
            reader._compute_shared("lead", "lead", refresh=True)
            case.append_log({"type": "tool_result"})
            with patch.object(reader, "_cost_usage_groups", wraps=reader._cost_usage_groups) as scan:
                self.assertEqual(reader._compute_shared("lead", "lead", refresh=True), first)
                scan.assert_not_called()
            catalog = case.pricing.snapshot()
            model = catalog["providers"]["anthropic"]["models"]["claude-opus-5-5"]
            model["cost"]["tiers"] = [{"tier": {"type": "context", "size": 100}, "output": 8}]
            result, status, tier = price_alias(catalog, "anthropic", "claude-opus-5-5",
                {"inputTokens": 100, "cachedInputTokens": 0, "cacheWriteInputTokens": 0,
                 "outputTokens": 10}, context_tokens=100, input_tokens_are_uncached=True)
            self.assertEqual((status, tier), ("priced", True))
            self.assertAlmostEqual(result, .00028)
            codes = function.__code__, price_alias.__code__
            self.assertEqual(update.apply(runtime), {"status": "already_applied"})
            self.assertEqual((function.__code__, price_alias.__code__), codes)

    def test_old_active_frame_completes_once_without_restarting_the_reader(self):
        with reviewed_fixture() as (runtime, _, reader, case, _):
            callback, connect = reader._compute, reader._connect
            entered, release = threading.Event(), threading.Event()
            results, errors = [], []
            def gated_connect():
                entered.set()
                if not release.wait(3):
                    raise AssertionError("The old frame was not released")
                return connect()
            def run():
                try:
                    results.append(callback("lead", "lead"))
                except Exception as error:
                    errors.append(error)
            with patch.object(reader, "_connect", gated_connect):
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
            self.assertEqual(len(results), 1)
            self.assertNotIn("claudeUsageSignature", results[0]["_cacheSource"])
            self.assertIn("claudeUsageSignature", callback("lead", "lead")["_cacheSource"])

    def assert_rejects(self, mutate):
        with reviewed_fixture() as (runtime, modules, reader, _, scripts):
            compute = reader._compute.__func__
            price = modules["codex_pricing"].price_usage
            original = compute.__code__, price.__code__
            with mutate(runtime, modules, scripts):
                with self.assertRaises(RuntimeError):
                    update.apply(runtime)
            self.assertEqual((compute.__code__, price.__code__), original)

    def test_unreviewed_files_reject_before_either_code_swap(self):
        for name in ("codex_runtime", "codex_session_costs", "codex_pricing"):
            @contextmanager
            def changed(runtime, modules, scripts):
                path = Path(modules[name].__file__)
                path.write_bytes(path.read_bytes() + b"\n# foreign\n")
                yield
            with self.subTest(name=name):
                self.assert_rejects(changed)

    def test_changed_dependencies_reject_before_either_code_swap(self):
        for name, path, _ in update.DEPENDENCIES:
            @contextmanager
            def changed(runtime, modules, scripts):
                owner = modules[name]
                for entry in path[:-1]:
                    owner = getattr(owner, entry)
                original = getattr(owner, path[-1])
                alien = FunctionType((lambda *args: None).__code__, vars(modules[name]))
                with patch.object(owner, path[-1], alien):
                    yield
            with self.subTest(path=path):
                self.assert_rejects(changed)

    def test_foreign_class_and_pricing_alias_reject(self):
        @contextmanager
        def class_changed(runtime, modules, scripts):
            owner = modules["codex_session_costs"].SessionCostReader
            with patch.object(owner, "__module__", "foreign"):
                yield
        @contextmanager
        def alias_changed(runtime, modules, scripts):
            costs = modules["codex_session_costs"]
            with patch.object(costs, "price_usage", lambda *args: None):
                yield
        for mutate in (class_changed, alias_changed):
            self.assert_rejects(mutate)

    def test_changed_target_code_rejects_before_the_other_swap(self):
        for name, path, _, _ in update.FUNCTIONS:
            @contextmanager
            def changed(runtime, modules, scripts):
                owner = modules[name]
                for entry in path[:-1]:
                    owner = getattr(owner, entry)
                alien = FunctionType((lambda *args: None).__code__, vars(modules[name]))
                with patch.object(owner, path[-1], alien):
                    yield
            with self.subTest(path=path):
                self.assert_rejects(changed)

    def test_closed_runtime_rejects(self):
        @contextmanager
        def changed(runtime, modules, scripts):
            with patch.object(runtime, "closed", True):
                yield
        self.assert_rejects(changed)


if __name__ == "__main__":
    unittest.main()
