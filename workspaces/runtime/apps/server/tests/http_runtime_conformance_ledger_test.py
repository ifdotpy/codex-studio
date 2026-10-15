"""Regression test for transparent HTTPError tracing in the conformance ledger."""
from __future__ import annotations

import importlib.util
import json
import os
from contextlib import closing
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[5]
DIFFERENTIAL = ROOT / "workspaces/runtime/apps/server/tests/http_runtime_extractor_differential.py"
diff_spec = importlib.util.spec_from_file_location("http_runtime_extractor_differential", DIFFERENTIAL)
assert diff_spec and diff_spec.loader
differential = importlib.util.module_from_spec(diff_spec)
sys.modules[diff_spec.name] = differential
diff_spec.loader.exec_module(differential)

LEDGER = ROOT / "workspaces/runtime/apps/server/tests/http_runtime_conformance_ledger.py"
spec = importlib.util.spec_from_file_location("http_runtime_conformance_ledger", LEDGER)
assert spec and spec.loader
ledger = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = ledger
spec.loader.exec_module(ledger)


class LedgerTraceContract(unittest.TestCase):
    def create_marked_copy(self, root: Path, *, with_agent: bool = True) -> Path:
        root.mkdir(parents=True, exist_ok=True)
        (root / ledger.COPY_MARKER).write_text(ledger.COPY_MARKER_CONTENT)
        with closing(sqlite3.connect(root / "canvas.sqlite3")) as connection:
            connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            if with_agent:
                connection.execute(
                    "INSERT INTO runtime_agents VALUES (?, ?)",
                    ("stored-agent", json.dumps({
                        "id": "stored-agent", "cwd": "/copied/project", "executionMode": "host",
                    })),
                )
            connection.commit()
        with closing(sqlite3.connect(root / "analytics.sqlite3")) as connection:
            connection.execute(
                "CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY, id TEXT, agent TEXT, record TEXT)"
            )
            connection.commit()
        return root

    def test_native_p04_http_suite_is_registered_as_python_baseline_only(self) -> None:
        relative = "workspaces/runtime/apps/server/tests/wave7-p04-native-http-contract.py"
        selected = {str(path.relative_to(ROOT)) for path in ledger.consumer_suites()}
        self.assertIn(relative, selected)
        self.assertIn(relative, ledger.PYTHON_BASELINE_ONLY_SUITES)
        self.assertTrue(
            (ROOT / "workspaces/runtime/packages/runtime-http/tests/wave7_p04_callback_http.rs").is_file()
        )

    def test_setup_class_error_is_a_named_failure_and_counts_as_one_case(self) -> None:
        log = """setUpClass (__main__.NativeNullHTTP) ... ERROR
======================================================================
ERROR: setUpClass (__main__.NativeNullHTTP)
----------------------------------------------------------------------
Traceback (most recent call last):
  RuntimeError: fixture requires the Python backend

----------------------------------------------------------------------
Ran 0 tests in 0.000s
FAILED (errors=1)
FAIL workspaces/runtime/apps/server/tests/wave7-p04-native-http-contract.py: test failure; failing IDs: ['setUpClass (__main__.NativeNullHTTP)']; reproduce: python
Server suites: 0 passed, 1 failed, 0 skipped (opt-in), 0 skipped (environment), 0.1s; 0 runner errors
"""
        self.assertEqual(
            ledger.case_results(log),
            [{
                "test": "setUpClass",
                "testId": "setUpClass (__main__.NativeNullHTTP)",
                "status": "error",
            }],
        )
        with tempfile.TemporaryDirectory() as scratch:
            trace_root = Path(scratch)
            log_path = trace_root / "setup-failure.log"
            log_path.write_text(log)
            process = SimpleNamespace(returncode=1, stderr="", stdout=json.dumps({
                "logPath": str(log_path), "status": "failed", "passed": 0,
                "failed": 1, "skipped": 0, "exitCode": 1,
            }))
            relative = "workspaces/runtime/apps/server/tests/wave7-p04-native-http-contract.py"
            path = ROOT / relative
            with patch.object(ledger.subprocess, "run", return_value=process) as run:
                result = ledger.run_suite(path, {"STUDIO_HTTP_BACKEND_COMMAND": "rust"}, trace_root, [], {})
            self.assertEqual(result["failed"], 1)
            self.assertEqual(result["failures"][0]["context"], "__main__.NativeNullHTTP")
            self.assertNotIn("STUDIO_HTTP_BACKEND_COMMAND", run.call_args.kwargs["env"])
            self.assertNotIn("STUDIO_HTTP_LEDGER_TRACE", run.call_args.kwargs["env"])

    def test_copy_state_root_rejects_live_paths_and_requires_marked_database_copy(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            copy = root / "copy"
            self.create_marked_copy(copy)

            self.assertEqual(ledger.validate_copy_state_root(copy), copy.resolve())
            with self.assertRaisesRegex(ValueError, "live state"):
                ledger.validate_copy_state_root(copy, live_state_root=copy)
            nested = copy / "nested"
            nested.mkdir()
            with self.assertRaisesRegex(ValueError, "live state"):
                ledger.validate_copy_state_root(nested, live_state_root=copy)
            with self.assertRaisesRegex(ValueError, "live state"):
                ledger.validate_copy_state_root(copy, live_state_root=copy)
            with self.assertRaisesRegex(ValueError, "live state"):
                ledger.validate_copy_state_root(copy, configured_state_root=str(copy))

            unexpected = copy / "accounts"
            unexpected.mkdir()
            with self.assertRaisesRegex(ValueError, "unexpected entries"):
                ledger.validate_copy_state_root(copy)

    def test_copy_state_root_rejects_home_state_tree(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            home = Path(scratch) / "home"
            copy = home / ".local/state/codex-agents/rehearsal"
            self.create_marked_copy(copy)
            with patch.object(Path, "home", return_value=home):
                with self.assertRaisesRegex(ValueError, "live state"):
                    ledger.validate_copy_state_root(copy)

    def test_copy_state_root_rejects_missing_marker_and_database_files(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            with self.assertRaisesRegex(ValueError, "marker"):
                ledger.validate_copy_state_root(root)
            (root / ledger.COPY_MARKER).write_text(ledger.COPY_MARKER_CONTENT)
            with self.assertRaisesRegex(ValueError, "canvas.sqlite3"):
                ledger.validate_copy_state_root(root)
            with closing(sqlite3.connect(root / "canvas.sqlite3")) as connection:
                connection.execute(
                    "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)"
                )
                connection.commit()
            with self.assertRaisesRegex(ValueError, "analytics.sqlite3"):
                ledger.validate_copy_state_root(root)

    def test_copy_state_root_rejects_database_hardlinks_before_writing(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch) / "copy"
            root.mkdir()
            (root / ledger.COPY_MARKER).write_text(ledger.COPY_MARKER_CONTENT)
            canvas = root / "canvas.sqlite3"
            with closing(sqlite3.connect(canvas)) as connection:
                connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
                connection.execute("INSERT INTO runtime_agents VALUES ('stored-agent', ?)",
                                   (json.dumps({"id": "stored-agent", "cwd": "/copied/project"}),))
                connection.commit()
            with closing(sqlite3.connect(root / "analytics.sqlite3")) as connection:
                connection.execute("CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY, id TEXT, agent TEXT, record TEXT)")
                connection.commit()
            external_link = Path(scratch) / "linked-live-canvas.sqlite3"
            os.link(canvas, external_link)
            original = canvas.read_bytes()

            with self.assertRaisesRegex(ValueError, "hard links"):
                ledger.validate_copy_state_root(root)

            self.assertEqual(canvas.read_bytes(), original)

    def test_copy_state_root_rejects_a_known_live_database_identity(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch) / "copy"
            live = Path(scratch) / "known-live"
            root.mkdir()
            live.mkdir()
            (root / ledger.COPY_MARKER).write_text(ledger.COPY_MARKER_CONTENT)
            canvas = root / "canvas.sqlite3"
            with closing(sqlite3.connect(canvas)) as connection:
                connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
                connection.execute("INSERT INTO runtime_agents VALUES ('stored-agent', ?)",
                                   (json.dumps({"id": "stored-agent", "cwd": "/copied/project"}),))
                connection.commit()
            with closing(sqlite3.connect(root / "analytics.sqlite3")) as connection:
                connection.execute("CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY, id TEXT, agent TEXT, record TEXT)")
                connection.commit()
            live_canvas = live / "canvas.sqlite3"
            os.link(canvas, live_canvas)
            original_stat = Path.stat

            def report_copy_as_unlinked(path, *args, **kwargs):
                stat = original_stat(path, *args, **kwargs)
                if path == canvas:
                    values = list(stat)
                    values[3] = 1
                    return os.stat_result(values)
                return stat

            with patch.object(Path, "stat", report_copy_as_unlinked):
                with self.assertRaisesRegex(ValueError, "aliases a known live database"):
                    ledger.validate_copy_state_root(root, live_state_root=live)

    def test_copy_state_requires_valid_database_schema_and_a_preexisting_agent(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch) / "copy"
            root.mkdir()
            (root / ledger.COPY_MARKER).write_text(ledger.COPY_MARKER_CONTENT)
            for name in ("canvas.sqlite3", "analytics.sqlite3"):
                (root / name).touch()
            with self.assertRaisesRegex(ValueError, "invalid SQLite header"):
                ledger.validate_copy_state_root(root)

            with closing(sqlite3.connect(root / "canvas.sqlite3")) as connection:
                connection.execute("CREATE TABLE unrelated(value TEXT)")
                connection.commit()
            with self.assertRaisesRegex(ValueError, "incompatible schema"):
                ledger.validate_copy_state_root(root)

            self.create_marked_copy(root, with_agent=False)
            before = {
                name: (root / name).read_bytes()
                for name in ("canvas.sqlite3", "analytics.sqlite3")
            }
            self.assertEqual(ledger.validate_copy_state_root(root), root.resolve())
            with self.assertRaisesRegex(ValueError, "pre-existing native agent"):
                ledger.copied_agent_witness(root)
            self.assertEqual(
                {name: (root / name).read_bytes() for name in before},
                before,
            )

    def test_copy_functional_witness_uses_existing_copy_row_not_seeded_agent(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch) / "copy"
            self.create_marked_copy(root)
            before = {
                name: (root / name).read_bytes()
                for name in ("canvas.sqlite3", "analytics.sqlite3")
            }
            self.assertEqual(ledger.validate_copy_state_root(root), root.resolve())
            self.assertEqual(
                ledger.copied_agent_witness(root),
                {"id": "stored-agent", "cwd": "/copied/project"},
            )
            self.assertEqual(
                {name: (root / name).read_bytes() for name in before},
                before,
            )

    def test_wal_backup_validation_and_witness_do_not_create_sidecars(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch) / "copy"
            self.create_marked_copy(root)
            source_path = Path(scratch) / "source.sqlite3"
            with closing(sqlite3.connect(source_path)) as source:
                source.execute("PRAGMA journal_mode=WAL")
                source.execute(
                    "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)"
                )
                source.execute(
                    "INSERT INTO runtime_agents VALUES (?, ?)",
                    ("wal-agent", json.dumps({"id": "wal-agent", "cwd": "/wal/copied"})),
                )
                source.commit()
                with closing(sqlite3.connect(root / "canvas.sqlite3")) as copied:
                    source.backup(copied)
            before = {
                path.name: path.read_bytes()
                for path in root.iterdir()
                if path.is_file()
            }

            self.assertEqual(ledger.validate_copy_state_root(root), root.resolve())
            self.assertEqual(
                ledger.copied_agent_witness(root),
                {"id": "wal-agent", "cwd": "/wal/copied"},
            )
            after = {
                path.name: path.read_bytes()
                for path in root.iterdir()
                if path.is_file()
            }
            self.assertEqual(after, before)
            self.assertEqual({path.name for path in root.iterdir()}, set(before))

    def test_copy_witness_report_redacts_copied_row_contents(self) -> None:
        probe_case = {}
        response_body = {
            "kind": "native",
            "path": "/private/copied/project",
            "id": "private-agent-id",
            "record": "private-row-content",
        }
        observation = {
            "matched": True,
            "body": ledger.persisted_probe_body(
                probe_case, response_body, copy_mode=True,
            ),
        }
        report = json.dumps(observation)
        self.assertTrue(observation["matched"])
        self.assertIsNone(observation["body"])
        for copied_value in (
            "/private/copied/project",
            "private-agent-id",
            "private-row-content",
        ):
            self.assertNotIn(copied_value, report)

    def test_other_probe_bodies_remain_available_for_diagnostics(self) -> None:
        body = {"error": "Not found"}
        self.assertEqual(ledger.persisted_probe_body({}, body), body)

    def test_api_and_copied_data_gates_require_independent_evidence(self) -> None:
        api_positive = [{
            "implementation": "implemented",
            "classification": "implemented",
            "covered": False,
            "probeCases": [{
                "expected": "2xx", "expectedMatches": True, "status": 200,
            }],
        }]
        api_report = ledger.api_fixture_evidence(api_positive)
        self.assertEqual(api_report["status"], "pass")
        self.assertEqual(api_report["positiveOperations"], 1)
        self.assertEqual(
            ledger.copied_data_evidence_status({
                "existingRowRead": True,
                "unchangedDuringFinderRead": True,
            }),
            "unverified",
            "API fixture success cannot stand in for copy validation or rollback",
        )

        copied_data = {
            "stateCheckBefore": True,
            "existingRowRead": True,
            "preexistingRowsPreservedFromBeforeStartup": True,
            "stateCheckAfter": True,
            "unchangedDuringFinderRead": True,
            "schemaChangesReviewed": True,
            "derivedIndexesReviewed": True,
            "baselineRestore": True,
            "pythonRollbackReadback": True,
        }
        self.assertEqual(ledger.copied_data_evidence_status(copied_data), "pass")
        refusal_only = [{
            "implementation": "implemented",
            "classification": "unverified",
            "covered": False,
            "probeCases": [{
                "expected": "403", "expectedMatches": True, "status": 403,
            }],
        }]
        refusal_report = ledger.api_fixture_evidence(refusal_only)
        self.assertEqual(refusal_report["positiveOperations"], 0)
        self.assertEqual(refusal_report["validatedNegativeCases"], 1)
        self.assertEqual(
            refusal_report["status"], "unverified",
            "a valid refusal cannot count as positive API capability",
        )
        positive_with_refusal_contract = ledger.api_fixture_evidence([{
            "implementation": "implemented",
            "classification": "implemented",
            "probeCases": [
                {"expected": "2xx", "expectedMatches": True, "status": 200},
                {"expected": "403", "expectedMatches": True, "status": 403},
            ],
        }])
        self.assertEqual(positive_with_refusal_contract["status"], "pass")
        self.assertEqual(positive_with_refusal_contract["validatedNegativeCases"], 1)

        legacy_coverage_flag = ledger.api_fixture_evidence([{
            "implementation": "implemented",
            "classification": "implemented",
            "covered": True,
            "probeCases": [{
                "expected": None, "expectedMatches": False, "status": 403,
            }],
        }])
        self.assertEqual(legacy_coverage_flag["status"], "unverified")
        named_success = ledger.api_fixture_evidence([{
            "implementation": "implemented",
            "classification": "implemented",
            "covered": True,
            "observedSuccessfulAssertions": ["fixture::valid request succeeds"],
            "probeCases": [{
                "expected": None, "expectedMatches": False,
                "dispatched": False, "consumerOnly": True,
                "reason": "A named consumer test sends the successful request.",
                "status": None,
            }],
        }])
        self.assertEqual(named_success["status"], "pass")

    def test_api_evidence_does_not_auto_pass_empty_unresolved_or_server_error(self) -> None:
        self.assertEqual(ledger.api_fixture_evidence([])["status"], "unverified")
        unresolved = ledger.api_fixture_evidence([{
            "implementation": "implemented",
            "classification": "implemented",
            "covered": False,
            "probeCases": [{"expected": None, "expectedMatches": False, "status": 200}],
        }])
        self.assertEqual(unresolved["status"], "unverified")
        server_error = ledger.api_fixture_evidence([{
            "implementation": "implemented",
            "classification": "unexpected",
            "covered": True,
            "probeCases": [{"expected": "2xx", "expectedMatches": False, "status": 503}],
        }])
        self.assertEqual(server_error["status"], "failed")
        self.assertEqual(server_error["unexpectedOperations"], 1)

        named_server_error = ledger.api_fixture_evidence([{
            "implementation": "implemented",
            "classification": "unverified",
            "covered": False,
            "probeCases": [
                {"expected": "2xx", "expectedMatches": True, "status": 200},
                {
                    "expected": "503", "expectedMatches": True, "status": 503,
                    "expectedError": "Optional adapter is unavailable",
                    "reason": "the child fixture has no adapter binding",
                },
            ],
        }])
        self.assertEqual(named_server_error["status"], "unverified")
        self.assertEqual(named_server_error["unverifiedOperations"], 1)

        malformed_record = ledger.api_fixture_evidence([{
            "implementation": "implemented",
            "classification": "implemented",
            "covered": False,
            "probeCases": [
                {"expected": "2xx", "expectedMatches": True, "status": 200},
                {
                    "expected": "503", "expectedMatches": True, "status": 503,
                    "expectedError": "Stored row is invalid",
                    "reason": "seeded malformed stored record fault contract",
                },
            ],
        }])
        self.assertEqual(malformed_record["status"], "pass")
        self.assertEqual(malformed_record["positiveOperations"], 1)

    def test_router_junit_evidence_is_exact_revision_hash_and_test_bound(self) -> None:
        recovery_class, recovery_name = ledger.ROUTER_POSITIVE_TESTS[
            ("POST", "/api/connection-recovery")
        ]
        orchestration_class, orchestration_name = ledger.ROUTER_POSITIVE_TESTS[
            ("POST", "/api/servers/orchestration")
        ]
        report = (
            '<testsuites><testsuite>'
            f'<testcase classname="{recovery_class}" name="{recovery_name}" />'
            f'<testcase classname="{orchestration_class}" name="{orchestration_name}" />'
            '</testsuite></testsuites>'
        ).encode()
        digest = ledger.hashlib.sha256(report).hexdigest()
        metadata = {"revision": "exact-candidate", "junitSha256": digest}

        passing = ledger.parse_router_test_evidence(metadata, report, "exact-candidate")
        self.assertEqual(passing["status"], "pass")
        self.assertEqual([item["status"] for item in passing["assertions"]], ["passed", "passed"])
        with tempfile.TemporaryDirectory(prefix="router-junit-evidence-") as scratch:
            root = Path(scratch)
            (root / "junit.xml").write_bytes(report)
            sidecar = root / "evidence.json"
            sidecar.write_text(json.dumps({
                **metadata,
                "junitPath": "junit.xml",
            }))
            with patch.object(
                ledger.subprocess, "run",
                side_effect=[
                    SimpleNamespace(stdout="exact-candidate\n"),
                    SimpleNamespace(stdout=""),
                ],
            ):
                loaded = ledger.load_router_test_evidence(sidecar)
            self.assertEqual(loaded["status"], "pass")
            with patch.object(
                ledger.subprocess, "run",
                side_effect=[
                    SimpleNamespace(stdout="exact-candidate\n"),
                    SimpleNamespace(stdout=" M workspaces/runtime/packages/runtime-http/src/lib.rs\n"),
                ],
            ):
                dirty = ledger.load_router_test_evidence(sidecar)
            self.assertEqual(dirty["status"], "unverified")
            self.assertIn("dirty source worktree", dirty["assertions"][0]["reason"])
        self.assertEqual(ledger.load_router_test_evidence(None)["status"], "unverified")

        stale_revision = ledger.parse_router_test_evidence(metadata, report, "different-candidate")
        self.assertEqual(stale_revision["status"], "unverified")
        dirty_worktree = ledger.parse_router_test_evidence(
            metadata, report, "exact-candidate", worktree_clean=False,
        )
        self.assertEqual(dirty_worktree["status"], "unverified")
        wrong_hash = ledger.parse_router_test_evidence(
            {**metadata, "junitSha256": "0" * 64}, report, "exact-candidate",
        )
        self.assertEqual(wrong_hash["status"], "unverified")
        missing = ledger.parse_router_test_evidence(
            {**metadata, "junitSha256": digest},
            report.replace(
                f'name="{orchestration_name}"'.encode(), b'name="other_test"'
            ),
            "exact-candidate",
        )
        self.assertEqual(missing["status"], "unverified")
        # A JUnit skipped node must never count as a passing route witness.
        skipped_report = (
            '<testsuites><testsuite>'
            f'<testcase classname="{recovery_class}" name="{recovery_name}" />'
            f'<testcase classname="{orchestration_class}" name="{orchestration_name}"><skipped /></testcase>'
            '</testsuite></testsuites>'
        ).encode()
        skipped = ledger.parse_router_test_evidence(
            {"revision": "exact-candidate", "junitSha256": ledger.hashlib.sha256(skipped_report).hexdigest()},
            skipped_report, "exact-candidate",
        )
        self.assertEqual(skipped["status"], "unverified")
        failed_report = report.replace(
            f'name="{orchestration_name}" />'.encode(),
            f'name="{orchestration_name}"><failure /></testcase>'.encode(),
        )
        failed = ledger.parse_router_test_evidence(
            {"revision": "exact-candidate", "junitSha256": ledger.hashlib.sha256(failed_report).hexdigest()},
            failed_report, "exact-candidate",
        )
        self.assertEqual(failed["status"], "failed")

    def test_consumer_only_probe_needs_named_success_and_keeps_copy_gate_independent(self) -> None:
        operation = {"method": "POST", "path": "/api/action"}
        case = ledger.PROBE_CASES[("POST", "/api/action")]["primary"]
        self.assertFalse(case.get("dispatch", True))
        self.assertTrue(case["consumer_only"])
        observation = {
            "expected": None, "expectedMatches": False, "dispatched": False,
            "status": None, "consumerOnly": True,
            "reason": "Existing fake-provider consumer tests send the successful request.",
        }
        without_consumer = ledger.api_fixture_evidence([{
            **operation,
            "implementation": "implemented",
            "classification": "unverified",
            "probeCases": [observation],
        }])
        self.assertEqual(without_consumer["status"], "unverified")
        anonymous_no_dispatch = ledger.api_fixture_evidence([{
            **operation,
            "implementation": "implemented",
            "classification": "unverified",
            "observedSuccessfulAssertions": ["fixture::some 2xx request"],
            "probeCases": [{"expected": None, "dispatched": False, "status": None}],
        }])
        self.assertEqual(anonymous_no_dispatch["status"], "unverified")
        with_consumer = ledger.api_fixture_evidence([{
            **operation,
            "implementation": "implemented",
            "classification": "unverified",
            "observedSuccessfulAssertions": [
                "workspaces/runtime/apps/server/tests/native-action-context-repair-contract.py::"
                "__main__.NativeActionHTTPContract.test_review_request_receipt_replay_targets_one_provider_thread"
            ],
            "probeCases": [observation],
        }])
        self.assertEqual(with_consumer["status"], "pass")
        route = ("POST", "/api/connection-recovery")
        router_test = ledger.ROUTER_POSITIVE_TESTS[route]
        router_only = ledger.api_fixture_evidence([{
            "method": route[0], "path": route[1],
            "implementation": "implemented", "classification": "unverified",
            "probeCases": [{
                "expected": None, "expectedMatches": False,
                "dispatched": False, "consumerOnly": True,
                "reason": "Rust router witness supplies the successful request.",
                "status": None,
            }],
        }], {
            "status": "pass",
            "assertions": [{
                "operation": f"{route[0]} {route[1]}",
                "test": f"{router_test[0]}::{router_test[1]}",
                "status": "passed",
            }],
        })
        self.assertEqual(router_only["status"], "pass")
        self.assertEqual(router_only["routerPositiveOperations"], 1)
        missing_router = ledger.api_fixture_evidence([{
            "method": route[0], "path": route[1],
            "implementation": "implemented", "classification": "unverified",
            "probeCases": [{
                "expected": None, "expectedMatches": False,
                "dispatched": False, "consumerOnly": True,
                "reason": "Rust router witness supplies the successful request.",
                "status": None,
            }],
        }])
        self.assertEqual(missing_router["status"], "unverified")
        self.assertEqual(
            ledger.copied_data_evidence_status({"existingRowRead": True}),
            "unverified",
            "API success cannot stand in for copied-data validation and rollback",
        )

    def test_copy_backend_command_requires_ephemeral_port(self) -> None:
        self.assertEqual(
            ledger.validate_ephemeral_backend_command("/tmp/studio-runtime-server --port 0"),
            ["/tmp/studio-runtime-server", "--port", "0"],
        )
        self.assertEqual(
            ledger.validate_ephemeral_backend_command("/tmp/studio-runtime-server"),
            ["/tmp/studio-runtime-server"],
        )
        for command in (
            "/tmp/studio-runtime-server --port 4620",
            "/tmp/studio-runtime-server --port=4620",
            "/tmp/studio-runtime-server --port 4635",
            "/tmp/studio-runtime-server --port 0 --port 0",
        ):
            with self.subTest(command=command):
                with self.assertRaises(ValueError):
                    ledger.validate_ephemeral_backend_command(command)

    def test_copy_probe_constants_are_namespaced_away_from_existing_rows(self) -> None:
        identifiers = {
            "_fixturePrefix": "copyprobe-unique-",
            "_accountKey": "copyprobe-unique-account",
            "_literalIds": {"60606060-6060-4060-8060-606060606060": "new-request-id"},
        }
        value = {
            "id": "ledger-probe-agent",
            "account_key": "default",
            "request_id": "60606060-6060-4060-8060-606060606060",
        }
        self.assertEqual(
            ledger.namespace_probe_constants(value, identifiers),
            {
                "id": "copyprobe-unique-probe-agent",
                "account_key": "copyprobe-unique-account",
                "request_id": "new-request-id",
            },
        )

    def test_provider_turn_refusals_are_keyed_to_the_undeclared_dependency(self) -> None:
        boundary = "workspaces/runtime/apps/server/tests/http-boundary-contract.py"
        protocol = "workspaces/runtime/apps/server/tests/protocol-fixtures-contract.py"
        boundary_tests = ledger.UNDECLARED_PROVIDER_TURN_TESTS[boundary]
        self.assertIn(
            "__main__.BoundaryContractTests.test_codex_provider_turn_uses_replay_executable",
            boundary_tests,
        )
        self.assertIn(
            "__main__.RestartContractTests.test_restart_serves_sync_receipt_and_provider_facts",
            boundary_tests,
        )
        self.assertIn(
            "__main__.ProtocolFixturesContract.test_supervisor_and_provider_fixtures_match_http_observations",
            ledger.UNDECLARED_PROVIDER_TURN_TESTS[protocol],
        )
        self.assertNotIn(boundary, ledger.MISSING_SERVICE_TESTS)
        self.assertNotIn(protocol, ledger.MISSING_SERVICE_TESTS)
        self.assertEqual(
            ledger.missing_service_tests_for_suite(boundary)[
                "__main__.RestartContractTests.test_restart_serves_sync_receipt_and_provider_facts"
            ]["service"],
            ledger.UNDECLARED_PROVIDER_TURN_SERVICE,
        )
        self.assertIn(
            "cause is not yet known",
            ledger.UNDECLARED_PROVIDER_TURN_TESTS[protocol][
                "__main__.ProtocolFixturesContract.test_supervisor_and_provider_fixtures_match_http_observations"
            ],
        )

    def test_split_suite_counts_include_every_unittest_summary(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            log = root / "full.log"
            content = (
                "test_first (__main__.First.test_first) ... ok\n"
                "Ran 1 test in 0.1s\nOK\n"
                "test_second (__main__.Second.test_second) ... skipped 'fixture'\n"
                "Ran 1 test in 0.2s\nOK (skipped=1)\n"
                "Server suites: 1 passed, 0 failed, 0 skipped (opt-in), 0 skipped (environment)\n"
            )
            log.write_text(content)
            process = SimpleNamespace(returncode=0, stderr="", stdout=json.dumps({
                "logPath": str(log), "status": "passed", "passed": 2,
                "failed": 0, "skipped": 1, "exitCode": 0,
            }))
            suite = ROOT / "workspaces/runtime/apps/server/tests/test_isolation_contract.py"
            with patch.object(ledger.subprocess, "run", return_value=process):
                result = ledger.run_suite(suite, {}, root, [], {})
                self.assertEqual(len(result["tests"]), 2)
                log.write_text(content.replace("Ran 1 test in 0.1s", "Ran 2 tests in 0.1s"))
                with self.assertRaisesRegex(RuntimeError, "result-count mismatch"):
                    ledger.run_suite(suite, {}, root, [], {})

    def test_probe_cases_are_operation_keyed_and_reference_seeded_records(self) -> None:
        documented = set(ledger.operations())
        self.assertLessEqual(set(ledger.PROBE_CASES), documented)
        fixture_keys = {
            "agent", "deleteAgent", "imageAgent", "task", "complaint", "room", "checkpoint", "item",
            "monitor", "terminal", "activeRule", "chat", "message", "filePath",
            "layoutRevision", "emptyQueueRevision", "queueIds", "question", "toolReceipt", "toolCancel",
            "questionDelete", "answerQuestion", "projectPath", "claudeAccount",
            "disconnectedAccount",
            "voiceRecord", "voiceEndedSession", "voiceActiveSession",
            "analyticsAgent", "schemaHash", "approvalRequest", "codexLogin", "activeCommand",
            "liveTerminal",
        }

        def references(value):
            if isinstance(value, str) and value.startswith("$"):
                return {value[1:]}
            if isinstance(value, dict):
                return set().union(*(references(item) for item in value.values()))
            if isinstance(value, list):
                return set().union(*(references(item) for item in value))
            return set()

        for operation, spec in ledger.PROBE_CASES.items():
            cases = [spec["primary"], *spec.get("additional", [])]
            for probe_case in cases:
                with self.subTest(operation=operation, case=probe_case.get("name", "primary")):
                    expected = probe_case.get(
                        "expected", None if probe_case.get("dispatch") is False else "2xx"
                    )
                    if probe_case.get("dispatch") is False:
                        self.assertTrue(probe_case.get("consumer_only"))
                        self.assertTrue(probe_case.get("reason"))
                        self.assertIsNone(expected)
                    elif probe_case.get("unverified"):
                        self.assertIsNone(expected)
                        self.assertTrue(probe_case.get("reason"))
                    else:
                        self.assertTrue(
                            expected in {"2xx", "400", "403", "404", "409"}
                            or (isinstance(expected, str) and expected.isdecimal() and 100 <= int(expected) <= 599)
                        )
                    self.assertLessEqual(references(probe_case), fixture_keys)
                    if probe_case.get("expected") == "404":
                        self.assertTrue(probe_case.get("reason"))

        delete_probe = ledger.PROBE_CASES[("POST", "/api/conversation/delete")]["primary"]
        self.assertEqual(delete_probe["body"]["id"], "$deleteAgent")

    def test_reset_redemption_uses_its_fake_provider_contract(self) -> None:
        self.assertNotIn(("POST", "/api/limits/reset"), ledger.PROBE_CASES)

    def test_new_partial_probes_have_success_and_named_refusal_shapes(self) -> None:
        for operation in (
            ("GET", "/api/changes"),
            ("POST", "/api/federation"),
            ("POST", "/api/messages"),
            ("POST", "/api/multi-server"),
            ("POST", "/api/projects"),
        ):
            with self.subTest(operation=operation):
                spec = ledger.PROBE_CASES[operation]
                self.assertEqual(spec["primary"]["expected"], "2xx")
                refusals = spec.get("additional", [])
                if operation == ("GET", "/api/changes"):
                    self.assertEqual(refusals, [])
                    continue
                self.assertTrue(refusals)
                if operation == ("POST", "/api/federation"):
                    self.assertEqual(len(refusals), 1)
                    self.assertEqual(refusals[0]["expected"], "400")
                    self.assertTrue(refusals[0]["assert_no_write"])
                    self.assertFalse(refusals[0].get("expected_error"))
                elif operation == ("POST", "/api/messages"):
                    self.assertEqual(len(refusals), 1)
                    self.assertEqual(refusals[0]["expected"], "400")
                    self.assertIn("schema rejects attachment objects", refusals[0]["reason"])
                elif operation == ("POST", "/api/multi-server"):
                    self.assertEqual(len(refusals), 1)
                    self.assertEqual(refusals[0]["expected"], "400")
                    self.assertTrue(refusals[0]["assert_no_write"])
                    self.assertIn("Tailscale Serve", refusals[0]["reason"])
                else:
                    self.assertEqual(len(refusals), 1)
                    self.assertEqual(refusals[0]["expected"], "403")
                    self.assertTrue(refusals[0]["assert_no_write"])
                    self.assertIn("not paired", refusals[0]["reason"])

    def test_implemented_route_expectations_follow_the_current_manifest(self) -> None:
        operation = ("POST", "/api/ledger-test")
        ledger.PROBE_CASES[operation] = {
            "primary": {"expected": "501", "expected_error": "obsolete refusal"},
            "additional": [{"expected": "501", "expected_error": "obsolete refusal"}],
        }
        try:
            ledger.ensure_operation_probe_cases({
                operation: {"status": "implemented", "dependencies": []},
            })
            cases = [
                ledger.PROBE_CASES[operation]["primary"],
                *ledger.PROBE_CASES[operation]["additional"],
            ]
            self.assertTrue(all(case.get("expected") is None for case in cases))
            self.assertTrue(all(case["unverified"] for case in cases))
            self.assertTrue(all("expected_error" not in case for case in cases))
        finally:
            ledger.PROBE_CASES.pop(operation, None)

    def test_unverified_additional_probe_never_accepts_a_server_error(self) -> None:
        cases = [
            ("primary", {"expected": "2xx"}),
            ("legacy refusal", {"expected": None, "unverified": True}),
        ]
        observations = [
            {"status": 200, "expectedMatches": True},
            {"status": 503, "expectedMatches": ledger.probe_status_matches(
                None, 503, standard_501=False, response_body={"error": "unavailable"},
            )},
        ]
        self.assertFalse(observations[1]["expectedMatches"])
        self.assertEqual(
            ledger.apply_probe_expectations("implemented", cases, observations),
            "unexpected",
        )
        self.assertEqual(
            ledger.apply_probe_expectations("unexpected", cases, [
                {"status": 200, "expectedMatches": True},
                {"status": 400, "expectedMatches": False},
            ]),
            "unexpected",
        )
        observations[0] = {"status": 503, "expectedMatches": False}
        observations[1] = {"status": 400, "expectedMatches": False}
        self.assertEqual(
            ledger.apply_probe_expectations("implemented", cases, observations),
            "unexpected",
        )
        self.assertFalse(ledger.probe_status_matches(
            "not_501", 200, standard_501=True, response_body={},
        ))

    def test_named_refusal_rejects_wrong_body_even_when_status_matches(self) -> None:
        expected_error = "The server is not paired or is revoked"
        self.assertFalse(ledger.probe_status_matches(
            "403", 403, standard_501=False,
            response_body={"error": "different 403 reason"},
            expected_error=expected_error,
        ))
        cases = [("unpaired remote project", {
            "expected": "403", "expected_error": expected_error,
        })]
        observations = [{
            "status": 403,
            "expectedMatches": ledger.probe_status_matches(
                "403", 403, standard_501=False,
                response_body={"error": "different 403 reason"},
                expected_error=expected_error,
            ),
        }]
        self.assertEqual(
            ledger.apply_probe_expectations("implemented", cases, observations),
            "unexpected",
        )

    def test_message_attachment_probe_is_an_explicit_schema_rejection(self) -> None:
        probe = ledger.PROBE_CASES[("POST", "/api/messages")]["additional"][0]
        self.assertEqual(probe["expected"], "400")
        self.assertNotIn("expected_error", probe)
        self.assertIn("schema rejects attachment objects", probe["reason"])

    def test_named_refusal_expectations_remain_owned_by_partial_manifest(self) -> None:
        operation = ("POST", "/api/ledger-test")
        ledger.PROBE_CASES[operation] = {
            "primary": {"expected": "501", "expected_error": "Not implemented: requires test/service"},
        }
        try:
            ledger.ensure_operation_probe_cases({
                operation: {"status": "partial", "dependencies": ["test/service"]},
            })
            self.assertEqual(ledger.PROBE_CASES[operation]["primary"]["expected"], "501")
            self.assertEqual(
                ledger.PROBE_CASES[operation]["primary"]["expected_error"],
                "Not implemented: requires test/service",
            )
        finally:
            ledger.PROBE_CASES.pop(operation, None)

    def test_disabled_signed_federation_probes_expect_python_gate_before_headers(self) -> None:
        for path in (
            "/api/federation/v1/message",
            "/api/federation/v1/pair",
            "/api/federation/v1/pull",
            "/api/federation/v1/status",
        ):
            with self.subTest(path=path):
                probe = ledger.PROBE_CASES[("POST", path)]["primary"]
                self.assertEqual(probe["expected"], "403")
                self.assertNotIn("headers", probe)

    def test_answer_probe_covers_the_implemented_async_question_flow(self) -> None:
        probe = ledger.PROBE_CASES[("POST", "/api/answer")]["primary"]
        self.assertEqual(probe["expected"], "2xx")
        self.assertEqual(probe["body"]["id"], "$answerQuestion")
        self.assertEqual(
            probe["body"]["answers"],
            {"value": {"answers": ["Ledger answer"]}},
        )
        self.assertNotIn("expected_error", probe)

    def test_answer_probe_preserves_the_later_defer_question(self) -> None:
        identifiers = {
            "question": "ledger-probe-question",
            "answerQuestion": "ledger-probe-answer-question",
        }
        answer_path, answer_body, _ = ledger.valid_probe(
            "POST", "/api/answer", identifiers,
            ledger.PROBE_CASES[("POST", "/api/answer")]["primary"],
        )
        defer_path, defer_body, _ = ledger.valid_probe(
            "POST", "/api/questions/defer", identifiers,
            ledger.PROBE_CASES[("POST", "/api/questions/defer")]["primary"],
        )
        self.assertEqual(answer_path, "/api/answer")
        self.assertEqual(answer_body["id"], "ledger-probe-answer-question")
        self.assertEqual(defer_path, "/api/questions/defer")
        self.assertEqual(defer_body["id"], "ledger-probe-question")
        self.assertNotEqual(answer_body["id"], defer_body["id"])

    def test_native_question_delete_uses_provider_callback_owner_witness(self) -> None:
        probe = ledger.PROBE_CASES[("POST", "/api/questions/delete")]
        self.assertEqual(probe["primary"]["expected"], "2xx")
        self.assertNotIn("additional", probe)

    def test_native_command_probe_uses_a_distinct_seeded_running_task(self) -> None:
        probe = ledger.PROBE_CASES[("POST", "/api/native-command")]["primary"]
        identifiers = {"activeCommand": "ledger-probe-active-command"}
        _, body, _ = ledger.valid_probe("POST", "/api/native-command", identifiers, probe)
        self.assertEqual(probe["expected"], "2xx")
        self.assertEqual(body["id"], "ledger-probe-active-command")
        self.assertNotEqual(body["id"], "ledger-probe-task")

    def test_changes_and_models_probes_use_slice_contract_request_shapes(self) -> None:
        identifiers = {"agent": "lead-agent", "claudeAccount": "claude-account"}
        changes = ledger.PROBE_CASES[("GET", "/api/changes")]["primary"]
        changes_path, _, _ = ledger.valid_probe("GET", "/api/changes", identifiers, changes)
        self.assertEqual(changes_path, "/api/changes?agent=lead-agent")
        self.assertNotIn("additional", ledger.PROBE_CASES[("GET", "/api/changes")])

        models = ledger.PROBE_CASES[("GET", "/api/models")]["additional"][0]
        models_path, _, _ = ledger.valid_probe("GET", "/api/models", identifiers, models)
        self.assertEqual(models_path, "/api/models?account_key=claude-account")

    def test_probe_fixture_restore_runs_when_the_probe_raises(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-probe-restore-") as scratch:
            state = Path(scratch)
            with sqlite3.connect(state / "canvas.sqlite3") as connection:
                connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
                connection.execute("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
                connection.execute(
                    "INSERT INTO runtime_agents VALUES(?, ?)",
                    ("lead-agent", json.dumps({"id": "lead-agent", "executionMode": "host"})),
                )
                connection.execute(
                    "INSERT INTO runtime_tasks VALUES(?, ?)",
                    ("active-command", json.dumps({"id": "active-command", "status": "completed"})),
                )
            identity_path = state / "multi-server" / "identity.json"
            identity_path.parent.mkdir()
            identity_path.write_bytes(b'{"serverId":"local"}')
            identity_path.chmod(0o600)
            case = {
                "fixture_mutations": {
                    "agent_execution_mode": "vm", "remove_identity": True,
                    "activate_native_command": True,
                },
                "restore": ["agent_record", "identity_file", "active_command_record"],
            }

            with self.assertRaisesRegex(RuntimeError, "probe failed"):
                with ledger.probe_fixture_state(
                    SimpleNamespace(state=state), {
                        "agent": "lead-agent", "activeCommand": "active-command",
                    }, case,
                ):
                    self.assertFalse(identity_path.exists())
                    with sqlite3.connect(state / "canvas.sqlite3") as connection:
                        changed = json.loads(connection.execute(
                            "SELECT record FROM runtime_agents WHERE id='lead-agent'",
                        ).fetchone()[0])
                        active = json.loads(connection.execute(
                            "SELECT record FROM runtime_tasks WHERE id='active-command'",
                        ).fetchone()[0])
                    self.assertEqual(changed["executionMode"], "vm")
                    self.assertEqual(active["status"], "running")
                    raise RuntimeError("probe failed")

            self.assertEqual(identity_path.read_bytes(), b'{"serverId":"local"}')
            self.assertEqual(identity_path.stat().st_mode & 0o777, 0o600)
            with sqlite3.connect(state / "canvas.sqlite3") as connection:
                restored = json.loads(connection.execute(
                    "SELECT record FROM runtime_agents WHERE id='lead-agent'",
                ).fetchone()[0])
            self.assertEqual(restored["executionMode"], "host")
            with sqlite3.connect(state / "canvas.sqlite3") as connection:
                restored_task = json.loads(connection.execute(
                    "SELECT record FROM runtime_tasks WHERE id='active-command'",
                ).fetchone()[0])
            self.assertEqual(restored_task["status"], "completed")

    def test_session_cost_fixture_restores_cache_and_temporary_pricing_catalog(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-pricing-restore-") as scratch:
            state = Path(scratch)
            root_id = "analytics-agent"
            cache = ledger.session_cost_cache_path(state, root_id)
            cache.parent.mkdir()
            original_cache = json.dumps({
                "rootId": root_id,
                "source": {"usage": {"generation": 1}},
            })
            cache.write_text(original_cache)
            case = {
                "fixture_mutations": {
                    "stale_session_cost_cache": True,
                    "seed_pricing_catalog": True,
                },
                "restore": ["session_cost_cache", "pricing_file"],
            }

            with self.assertRaisesRegex(RuntimeError, "probe failed"):
                with ledger.probe_fixture_state(
                    SimpleNamespace(state=state),
                    {"analyticsAgent": root_id},
                    case,
                ):
                    self.assertEqual(
                        json.loads(cache.read_text())["source"]["usage"]["generation"],
                        2,
                    )
                    pricing = state / "model-pricing" / "models-dev-v1.json"
                    self.assertEqual(json.loads(pricing.read_text())["version"], 1)
                    raise RuntimeError("probe failed")

            self.assertEqual(cache.read_text(), original_cache)
            self.assertFalse((state / "model-pricing").exists())

    def test_action_route_keeps_fake_provider_http_cases_in_ledger_coverage(self) -> None:
        suite = "workspaces/runtime/apps/server/tests/native-action-context-repair-contract.py"
        self.assertNotIn(
            "__main__.NativeActionHTTPContract.test_review_request_receipt_replay_targets_one_provider_thread",
            ledger.UNDECLARED_PROVIDER_TURN_TESTS.get(suite, {}),
        )
        self.assertNotIn(
            "__main__.NativeActionHTTPContract.test_compact_request_receipt_replay_targets_one_provider_thread",
            ledger.UNDECLARED_PROVIDER_TURN_TESTS.get(suite, {}),
        )
        primary = ledger.PROBE_CASES[("POST", "/api/action")]["primary"]
        self.assertFalse(primary.get("dispatch", True))
        self.assertTrue(primary["consumer_only"])
        self.assertIn("completed provider thread", primary["reason"])

    def test_analytics_seeder_namespaces_ids_in_synthetic_copy_mode(self) -> None:
        suite = LEDGER.parent / "rust-analytics-http-contract.py"
        analytics_spec = importlib.util.spec_from_file_location("ledger_copy_analytics_seed", suite)
        self.assertIsNotNone(analytics_spec)
        assert analytics_spec is not None and analytics_spec.loader is not None
        analytics_module = importlib.util.module_from_spec(analytics_spec)
        analytics_spec.loader.exec_module(analytics_module)

        with tempfile.TemporaryDirectory(prefix="ledger-copy-analytics-") as scratch:
            root = Path(scratch)
            state = root / "state"
            cwd = root / "project"
            home = root / "home"
            codex_home = root / "codex"
            for directory in (state, cwd, home, codex_home):
                directory.mkdir()
            namespace = "copyprobe-ledger-test-"
            environment = {
                "HOME": str(home),
                "CODEX_HOME": str(codex_home),
                "CODEX_AGENTS_STATE_DIR": str(state),
                "CODEX_CANVAS_CWD": str(cwd),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_DATA_HOME": str(root / "data"),
                "XDG_CACHE_HOME": str(root / "cache"),
                "XDG_STATE_HOME": str(root / "xdg-state"),
            }
            backend = SimpleNamespace(
                copy_mode=True,
                fixture_namespace=namespace,
                cwd=cwd,
                _environment=lambda: environment,
            )
            analytics_module.seed_analytics(backend)

            with closing(sqlite3.connect(state / "canvas.sqlite3")) as connection:
                agent_ids = {
                    row[0] for row in connection.execute("SELECT id FROM runtime_agents")
                }
            with closing(sqlite3.connect(state / "analytics.sqlite3")) as connection:
                usage_agents = {
                    row[0] for row in connection.execute("SELECT DISTINCT agent FROM analytics_usage")
                }
            self.assertIn(f"{namespace}analytics-contract-agent", agent_ids)
            self.assertIn(f"{namespace}analytics-contract-teammate", agent_ids)
            self.assertEqual(usage_agents, {
                f"{namespace}analytics-contract-agent",
                f"{namespace}analytics-population",
            })
            self.assertNotIn("analytics-contract-agent", agent_ids)
            self.assertNotIn("analytics-contract-agent", usage_agents)

    def test_sqlite_snapshot_uses_data_version_fast_path_without_scanning_rows(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-sqlite-snapshot-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("CREATE TABLE mixed(value, payload)")
                connection.executemany(
                    "INSERT INTO mixed VALUES (?, ?)",
                    [(None, None), (1, b"blob\x00"), ("1", b""), (1.0, None)],
                )
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with patch.object(
                    ledger,
                    "_sqlite_content_digest",
                    side_effect=AssertionError("unchanged data version must skip row scans"),
                ):
                    self.assertTrue(snapshot.unchanged())

    def test_sqlite_snapshot_fallback_compares_schema_and_unordered_mixed_rows(self) -> None:
        rows = [
            (None, None), (1, b"blob\x00"), ("1", b""), (1.0, None),
            (-0.0, b"negative zero"), (0.0, b"positive zero"),
            (1, b"blob\x00"),
        ]
        with tempfile.TemporaryDirectory(prefix="ledger-sqlite-snapshot-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("CREATE TABLE mixed(value, payload)")
                connection.executemany("INSERT INTO mixed VALUES (?, ?)", rows)
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("DELETE FROM mixed")
                    connection.executemany("INSERT INTO mixed VALUES (?, ?)", reversed(rows))
                    connection.commit()
                self.assertTrue(snapshot.unchanged())

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("UPDATE mixed SET payload=? WHERE rowid=1", (b"changed",))
                    connection.commit()
                self.assertFalse(snapshot.unchanged())

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("CREATE INDEX mixed_payload ON mixed(payload)")
                    connection.commit()
                self.assertFalse(snapshot.unchanged())

    def test_sqlite_snapshot_detects_identical_content_file_replacement(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-sqlite-replaced-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            replacement = Path(scratch) / "replacement.sqlite3"
            for path in (database, replacement):
                with closing(sqlite3.connect(path)) as connection:
                    connection.execute("CREATE TABLE state(value TEXT)")
                    connection.execute("INSERT INTO state VALUES ('same logical contents')")
                    connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                original_identity = ledger._sqlite_file_identity(database)
                replacement_identity = ledger._sqlite_file_identity(replacement)
                self.assertNotEqual(original_identity, replacement_identity)
                os.replace(replacement, database)
                self.assertFalse(snapshot.unchanged())

    def test_old_row_preservation_allows_new_tables_and_rows(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-old-rows-additions-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("CREATE TABLE records(id INTEGER PRIMARY KEY, value TEXT)")
                connection.execute("INSERT INTO records VALUES (1, 'kept')")
                connection.execute("CREATE TABLE duplicate_values(value BLOB)")
                connection.executemany(
                    "INSERT INTO duplicate_values VALUES (?)",
                    [(b"same",), (b"same",)],
                )
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("INSERT INTO records VALUES (2, 'new row')")
                    connection.execute(
                        "INSERT INTO duplicate_values VALUES (?)", (b"same",),
                    )
                    connection.execute("CREATE TABLE new_table(value TEXT)")
                    connection.execute("INSERT INTO new_table VALUES ('new table row')")
                    connection.commit()
                result = snapshot.preserved_existing_rows()

            self.assertTrue(result["preserved"])
            self.assertEqual(result["newTables"], 1)

    def test_old_row_preservation_rejects_deleted_or_changed_rows(self) -> None:
        for mutation in ("delete", "update"):
            with self.subTest(mutation=mutation):
                with tempfile.TemporaryDirectory(prefix="ledger-old-rows-change-") as scratch:
                    database = Path(scratch) / "canvas.sqlite3"
                    with closing(sqlite3.connect(database)) as connection:
                        connection.execute("PRAGMA journal_mode=WAL")
                        connection.execute(
                            "CREATE TABLE records(id INTEGER PRIMARY KEY, value TEXT)"
                        )
                        connection.execute("INSERT INTO records VALUES (1, 'kept')")
                        connection.commit()

                    with ledger.sqlite_snapshot(database) as snapshot:
                        with closing(sqlite3.connect(database)) as connection:
                            statement = (
                                "DELETE FROM records WHERE id=1"
                                if mutation == "delete"
                                else "UPDATE records SET value='changed' WHERE id=1"
                            )
                            connection.execute(statement)
                            connection.commit()
                        result = snapshot.preserved_existing_rows()
                    self.assertFalse(result["preserved"])

    def test_old_row_preservation_allows_added_columns_but_checks_old_columns(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-old-columns-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("CREATE TABLE records(id INTEGER PRIMARY KEY, value BLOB)")
                connection.execute("INSERT INTO records VALUES (1, ?)", (b"kept",))
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("ALTER TABLE records ADD COLUMN added TEXT")
                    connection.execute(
                        "UPDATE records SET added='new metadata' WHERE id=1"
                    )
                    connection.commit()
                result = snapshot.preserved_existing_rows()
            self.assertTrue(result["preserved"])
            self.assertEqual(result["newColumns"], 1)
            self.assertEqual(result["changedTableDefinitions"], 1)

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute(
                        "UPDATE records SET value=? WHERE id=1", (b"changed",),
                    )
                    connection.commit()
                result = snapshot.preserved_existing_rows()
            self.assertFalse(result["preserved"])

            with tempfile.TemporaryDirectory(prefix="ledger-old-column-drop-") as second:
                database = Path(second) / "canvas.sqlite3"
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("PRAGMA journal_mode=WAL")
                    connection.execute(
                        "CREATE TABLE records(id INTEGER PRIMARY KEY, old_value TEXT)"
                    )
                    connection.execute("INSERT INTO records VALUES (1, 'kept')")
                    connection.commit()
                with ledger.sqlite_snapshot(database) as snapshot:
                    with closing(sqlite3.connect(database)) as connection:
                        connection.execute("ALTER TABLE records DROP COLUMN old_value")
                        connection.commit()
                    result = snapshot.preserved_existing_rows()
                self.assertFalse(result["preserved"])

    def test_no_primary_key_duplicate_tables_use_rowid_or_report_unverified(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-no-primary-key-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("CREATE TABLE duplicates(value TEXT)")
                connection.executemany(
                    "INSERT INTO duplicates VALUES (?)", [("same",), ("same",)],
                )
                connection.execute(
                    "CREATE TABLE shadowed(rowid TEXT, _rowid_ TEXT, oid TEXT)"
                )
                connection.execute(
                    "INSERT INTO shadowed VALUES ('a', 'b', 'c')"
                )
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute(
                        "INSERT INTO duplicates VALUES ('same')"
                    )
                    connection.commit()
                result = snapshot.preserved_existing_rows()
            self.assertIsNone(result["preserved"])
            self.assertEqual(result["unsupportedTables"], 1)

            with tempfile.TemporaryDirectory(prefix="ledger-no-pk-delete-") as second:
                database = Path(second) / "canvas.sqlite3"
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("PRAGMA journal_mode=WAL")
                    connection.execute("CREATE TABLE duplicates(value TEXT)")
                    connection.executemany(
                        "INSERT INTO duplicates VALUES (?)", [("same",), ("same",)],
                    )
                    connection.commit()
                with ledger.sqlite_snapshot(database) as snapshot:
                    with closing(sqlite3.connect(database)) as connection:
                        connection.execute(
                            "DELETE FROM duplicates WHERE rowid=(SELECT MIN(rowid) FROM duplicates)"
                        )
                        connection.commit()
                    result = snapshot.preserved_existing_rows()
                self.assertFalse(result["preserved"])

    def test_old_row_preservation_compares_exact_types_and_text_bytes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-old-row-types-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("CREATE TABLE mixed(value)")
                connection.executemany(
                    "INSERT INTO mixed VALUES (?)",
                    [(None,), (b"blob\x00",), (1,), ("1",), (1.0,)],
                )
                connection.execute(
                    "CREATE TABLE collated(value TEXT COLLATE NOCASE)"
                )
                connection.execute("INSERT INTO collated VALUES ('Ab')")
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute(
                        "UPDATE mixed SET value=1.0 WHERE rowid=("
                        "SELECT rowid FROM mixed WHERE typeof(value)='integer' LIMIT 1)"
                    )
                    connection.execute("UPDATE collated SET value='ab'")
                    connection.commit()
                result = snapshot.preserved_existing_rows()
            self.assertFalse(result["preserved"])

    def test_rowid_alias_must_be_available_in_both_schema_versions(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-rowid-alias-change-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("CREATE TABLE records(value TEXT)")
                connection.execute("INSERT INTO records VALUES ('kept')")
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("ALTER TABLE records RENAME TO old_records")
                    connection.execute(
                        "CREATE TABLE records(_rowid_ TEXT, value TEXT)"
                    )
                    connection.execute(
                        "INSERT INTO records(rowid, _rowid_, value) "
                        "SELECT rowid, 'shadow', value FROM old_records"
                    )
                    connection.execute(
                        "INSERT INTO records(rowid, _rowid_, value) "
                        "VALUES (2, 'shadow-new', 'new row')"
                    )
                    connection.execute("DROP TABLE old_records")
                    connection.commit()
                result = snapshot.preserved_existing_rows()

            self.assertTrue(result["preserved"])
            self.assertEqual(result["newColumns"], 1)

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute(
                        "UPDATE records SET value='changed' WHERE rowid=1"
                    )
                    connection.commit()
                result = snapshot.preserved_existing_rows()
            self.assertFalse(result["preserved"])

    def test_autoincrement_sequence_changes_are_reported_outside_user_rows(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-sequence-bookkeeping-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute(
                    "CREATE TABLE records(id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT)"
                )
                connection.execute("INSERT INTO records(value) VALUES ('old row')")
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("INSERT INTO records(value) VALUES ('new row')")
                    connection.commit()
                result = snapshot.preserved_existing_rows()

            self.assertTrue(result["preserved"])
            self.assertTrue(result["sqliteSequenceChanged"])
            self.assertEqual(result["internalBookkeepingTables"], 1)

    def test_known_contentless_fts_is_reported_as_unreadable_derived_index(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-contentless-fts-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute(
                    "CREATE TABLE runtime_search_next_meta "
                    "(search_rowid INTEGER PRIMARY KEY, agent_id TEXT NOT NULL, kind TEXT NOT NULL)"
                )
                connection.execute(
                    "CREATE VIRTUAL TABLE runtime_search_next USING "
                    "fts5(body,content='',contentless_delete=1,tokenize='unicode61')"
                )
                connection.execute(
                    "INSERT INTO runtime_search_next_meta VALUES (1, 'agent-old', 'turn')"
                )
                connection.execute(
                    "INSERT INTO runtime_search_next(rowid,body) VALUES (1, 'old terms')"
                )
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute(
                        "INSERT INTO runtime_search_next_meta VALUES (2, 'agent-new', 'turn')"
                    )
                    connection.execute(
                        "INSERT INTO runtime_search_next(rowid,body) VALUES (2, 'new terms')"
                    )
                    connection.commit()
                result = snapshot.preserved_existing_rows()

            self.assertTrue(result["preserved"])
            self.assertEqual(result["unreadableDerivedIndexes"], 1)
            self.assertEqual(result["unsupportedTables"], 0)
            self.assertEqual(result["derivedIndexSchemaChanges"], 0)
            self.assertEqual(
                ledger.copied_data_evidence_status({
                    "stateCheckBefore": True,
                    "existingRowRead": True,
                    "preexistingRowsPreservedFromBeforeStartup": True,
                    "unchangedDuringFinderRead": True,
                    "schemaChangesReviewed": True,
                    "stateCheckAfter": True,
                    "baselineRestore": True,
                    "pythonRollbackReadback": True,
                }),
                "unverified",
                "contentless FTS body preservation needs separate review evidence",
            )

    def test_unknown_virtual_tables_remain_unverified(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-unknown-virtual-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("CREATE TABLE state(value INTEGER)")
                connection.execute("INSERT INTO state VALUES (1)")
                connection.execute(
                    "CREATE VIRTUAL TABLE other_search USING fts5(body)"
                )
                connection.commit()

            with ledger.sqlite_snapshot(database) as snapshot:
                with closing(sqlite3.connect(database)) as connection:
                    connection.execute("INSERT INTO state VALUES (2)")
                    connection.commit()
                result = snapshot.preserved_existing_rows()

            self.assertIsNone(result["preserved"])
            self.assertEqual(result["unsupportedTables"], 1)
            self.assertEqual(result["unreadableDerivedIndexes"], 0)

    def test_copy_privacy_and_reset_probe_is_always_preserved(self) -> None:
        copied_response = {
            "stateDir": "/copied/private/state",
            "entities": [{"id": "private-agent", "record": "private record"}],
        }
        self.assertIsNone(ledger.persisted_probe_body({}, copied_response, copy_mode=True))
        self.assertEqual(ledger.persisted_probe_body({}, copied_response), copied_response)
        self.assertIsNone(ledger.persisted_probe_body(
            {"functional_witness": "copy-existing-agent"}, copied_response,
        ))
        self.assertEqual(ledger.copied_data_evidence_status({}), "unverified")

        with tempfile.TemporaryDirectory(prefix="ledger-copy-skip-") as scratch:
            database = Path(scratch) / "canvas.sqlite3"
            original = '{"scope":"user","fields":{"theme":"private"}}'
            with closing(sqlite3.connect(database)) as connection:
                connection.execute(
                    "CREATE TABLE sync_preferences(scope TEXT PRIMARY KEY, record TEXT NOT NULL)"
                )
                connection.execute(
                    "INSERT INTO sync_preferences VALUES ('user', ?)", (original,),
                )
                connection.commit()

            self.assertIsNone(ledger.unsafe_probe_exclusion(
                "POST", "/api/sync/preferences",
            ), "global preference writes are allowed only on disposable scratch state")
            self.assertIn("reset-credit", ledger.unsafe_probe_exclusion(
                "POST", "/api/limits/reset",
            ))
            with closing(sqlite3.connect(database)) as connection:
                self.assertEqual(
                    connection.execute(
                        "SELECT record FROM sync_preferences WHERE scope='user'"
                    ).fetchone()[0],
                    original,
                )

    def test_probe_inputs_match_exact_panel_and_live_queue_contracts(self) -> None:
        panel = ledger.PROBE_CASES[("POST", "/api/panel/layout")]["primary"]["body"]
        self.assertEqual(
            set(panel),
            {
                "agent", "client", "sequence", "renderer", "revision", "width",
                "height", "contentWidth", "contentHeight", "overflowX", "overflowY",
                "totalLines", "visibleLines", "lastVisibleLine", "lastVisibleHeading",
                "fits", "reason",
            },
        )
        queue = ledger.PROBE_CASES[("POST", "/api/queue")]["primary"]["body"]
        self.assertTrue(ledger.PROBE_CASES[("POST", "/api/queue")]["primary"]["refresh_queue"])
        self.assertEqual(queue["expected_revision"], "$emptyQueueRevision")
        self.assertEqual(queue["ordered_ids"], "$queueIds")

    def test_partial_operations_with_multiple_request_shapes_check_both_results(self) -> None:
        expected_cases = {
            ("GET", "/api/file"): ["2xx"],
            ("GET", "/api/file-info"): ["2xx"],
            ("GET", "/api/sync/pull"): ["2xx", "2xx"],
            ("POST", "/api/sync/drafts"): ["2xx", "400"],
            ("POST", "/api/rules"): ["2xx", "2xx"],
            ("POST", "/api/agents"): ["2xx"],
            ("POST", "/api/leads"): ["2xx"],
            ("POST", "/api/configure"): ["2xx", "400"],
            ("POST", "/api/conversation"): ["2xx", "400"],
            ("POST", "/api/checkpoint"): ["2xx", "400"],
            ("POST", "/api/checkpoint/preview"): ["2xx"],
            ("POST", "/api/voice/end"): ["2xx", "2xx"],
            ("POST", "/api/voice/speech"): ["2xx", "400"],
            ("POST", "/api/voice/submit"): ["2xx", "400"],
        }
        for operation, expected in expected_cases.items():
            with self.subTest(operation=operation):
                spec = ledger.PROBE_CASES[operation]
                self.assertEqual(
                    [spec["primary"]["expected"], *[case["expected"] for case in spec.get("additional", [])]],
                    expected,
                )
        rules = ledger.PROBE_CASES[("POST", "/api/rules")]
        self.assertEqual(rules["primary"]["python_citation"], "codex_rules.py:337-339")
        self.assertEqual(
            rules["additional"][0]["python_citation"],
            "codex_rules.py:179-181,236-245",
        )
        smart_title = ledger.PROBE_CASES[("POST", "/api/rename")]["additional"][0]
        self.assertEqual(smart_title["expected"], "2xx")
        self.assertEqual(smart_title["python_citation"], "codex_rename.py:195-199")

    def test_classifier_rejects_server_errors_before_allowances(self) -> None:
        case = {"path": "/x", "method": "GET", "label": "known"}
        document = {"paths": {"/x": {"get": {"operationId": "op"}}}}
        with self.assertRaisesRegex(AssertionError, "returned 500"):
            differential.classify(
                [case],
                [{"status": 500, "accepted": False}],
                [{"status": 200, "accepted": True}],
                document,
            )

    def test_classifier_requires_located_envelopes_for_validation_refusals(self) -> None:
        case = {"path": "/x", "method": "POST", "label": "body:type:str"}
        document = {"paths": {"/x": {"post": {"operationId": "op"}}}}
        refusal = {"status": 400, "accepted": False, "body": {"error": "Invalid request"}}
        with self.assertRaisesRegex(AssertionError, "Rust refusal lacks its located error envelope"):
            differential.classify([case], [refusal], [refusal], document)


    def test_classifier_does_not_accept_wildcard_allowances(self) -> None:
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES[("*", "query:extra")] = (
                400, 200, "must never be wildcarded"
            )
            case = {"path": "/x", "method": "GET", "label": "query:extra"}
            document = {"paths": {"/x": {"get": {"operationId": "op"}}}}
            with self.assertRaisesRegex(AssertionError, "invalid expected-difference entry"):
                differential.classify(
                    [case],
                    [{"status": 400, "accepted": False, "body": {"error": "bad", "location": ["query"]}}],
                    [{"status": 200, "accepted": True, "value": {}}],
                    document,
                )
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_classifier_rejects_unused_allowance(self) -> None:
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES[("op", "unused")] = (
                400, 200, "expired"
            )
            case = {"path": "/x", "method": "GET", "label": "baseline"}
            document = {"paths": {"/x": {"get": {"operationId": "op"}}}}
            with self.assertRaisesRegex(AssertionError, "entries were unused"):
                differential.classify(
                    [case],
                    [{"status": 200, "accepted": True, "value": {}}],
                    [{"status": 200, "accepted": True, "value": {}}],
                    document,
                )
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_classifier_rejects_allowances_without_both_statuses_and_a_cause(self) -> None:
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES[("op", "case")] = (400, 200)
            with self.assertRaisesRegex(AssertionError, "invalid expected-difference entry"):
                differential.classify([], [], [], {"paths": {}})
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_classifier_uses_strict_json_types(self) -> None:
        self.assertFalse(differential.strict_equal(True, 1))

    def test_echo_comparison_uses_declared_number_semantics_only(self) -> None:
        operation = {
            "operationId": "numeric_echo",
            "parameters": [
                {"in": "query", "name": "amount", "schema": {"type": "number"}},
                {"in": "query", "name": "count", "schema": {"type": "integer"}},
            ],
        }
        document = {"paths": {"/x": {"get": operation}}}
        rust = {"value": {"query": {"amount": 1, "count": 1}, "headers": {}}}
        python = {"value": {"query": {"amount": 1.0, "count": 1}, "headers": {}}}
        self.assertTrue(differential.observation_values_equal(rust, python, operation, document))
        python["value"]["query"]["amount"] = True
        self.assertFalse(differential.observation_values_equal(rust, python, operation, document))
        python["value"]["query"]["amount"] = 1.0
        python["value"]["query"]["count"] = 1.5
        self.assertFalse(differential.observation_values_equal(rust, python, operation, document))









    def test_opaque_json_precision_is_a_located_rust_refusal(self) -> None:
        case = {"path": "/widgets", "method": "POST", "label": "payload:lax:18446744073709551616"}
        operation = {
            "operationId": "create_widget",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/JsonValue"}}}},
        }
        document = {
            "paths": {"/widgets": {"post": operation}},
            "components": {"schemas": {"JsonValue": {"anyOf": [{"type": "null"}, {"type": "integer"}, {"type": "object"}]}}},
        }
        rust = {"status": 400, "accepted": False, "body": {"error": "Invalid request", "location": ["body", "payload"]}}
        python = {"status": 200, "accepted": True, "value": {"body": {"payload": 2**64, "host": "same"}, "query": {}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify([case], [rust], [python], document)
            self.assertEqual(result["mismatches_by_cause"], {differential.OPAQUE_JSON_INTEGER_CAUSE: 1})
            self.assertEqual(result["mismatch_direction"], {"rust_reject_python_accept": 1})
            self.assertEqual(result["expected_difference_cases"][0]["pointer"], "#/components/schemas/JsonValue")
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_opaque_precision_classifier_resolves_additional_properties_inside_unions(self) -> None:
        case = {"path": "/answers", "method": "POST", "label": "answers/key:lax:18446744073709551616"}
        operation = {
            "operationId": "answer_api_answer_post",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/AnswerBody"}}}},
        }
        document = {
            "paths": {"/answers": {"post": operation}},
            "components": {"schemas": {
                "JsonValue": {"anyOf": [{"type": "null"}, {"type": "number"}, {"type": "object"}]},
                "AnswerBody": {"type": "object", "properties": {
                    "answers": {"anyOf": [
                        {"type": "object", "additionalProperties": {"$ref": "#/components/schemas/JsonValue"}},
                        {"type": "null"},
                    ]},
                }},
            }},
        }
        rust = {"status": 400, "accepted": False, "body": {"error": "Invalid request", "location": ["body", "answers", "key"]}}
        python = {"status": 200, "accepted": True, "value": {"body": {"answers": {"key": 2**64}}, "query": {}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify([case], [rust], [python], document)
            self.assertEqual(result["mismatches_by_cause"], {differential.OPAQUE_JSON_INTEGER_CAUSE: 1})
            self.assertEqual(result["expected_difference_cases"][0]["pointer"], "#/components/schemas/JsonValue")
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_opaque_precision_allowance_does_not_apply_to_typed_integer(self) -> None:
        case = {"path": "/widgets", "method": "POST", "label": "count:lax:18446744073709551617"}
        operation = {"operationId": "create_widget", "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Widget"}}}}}
        document = {"paths": {"/widgets": {"post": operation}}, "components": {"schemas": {"Widget": {"type": "object", "properties": {"count": {"type": "integer"}}}}}}
        rust = {"status": 200, "accepted": True, "value": {"body": {"count": float(2**64)}, "query": {}, "headers": {}}}
        python = {"status": 200, "accepted": True, "value": {"body": {"count": 2**64 + 1}, "query": {}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            with self.assertRaisesRegex(AssertionError, "unclassified typed-value mismatch"):
                differential.classify([case], [rust], [python], document)
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_consumer_counts_all_runner_slices(self) -> None:
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            log = root / "consumer.log"
            log.write_text(
                "test_one (__main__.Example.test_one) ... ok\n"
                "Ran 1 test in 0.1s\n"
                "test_two (__main__.Example.test_two) ... ok\n"
                "Ran 1 test in 0.1s\n"
                "Server suites: 1 passed, 0 failed, 0 skipped (opt-in), 0 skipped (environment)\n"
            )
            result = {"status":"passed", "passed":3, "failed":0, "skipped":0, "logPath":str(log)}
            process = SimpleNamespace(stdout=json.dumps(result), stderr="", returncode=0)
            with patch.object(ledger.subprocess, "run", return_value=process) as run:
                observed = ledger.run_suite(ledger.TESTS / "fixture.py", {}, root, [], {})
            self.assertEqual(run.call_args.kwargs["env"]["CODEX_SERVER_TEST_JOBS"], "1")
            self.assertEqual(observed["passed"], 3)
            self.assertEqual(len(observed["tests"]), 2)

    def test_consumer_reconciles_status_after_interleaved_warning(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            log = root / "consumer.log"
            log.write_text(
                "test_skipped (__main__.Example.test_skipped) ... skipped 'fixture'\n"
                "test_passed (__main__.Example.test_passed) ... RuntimeWarning: noisy stderr\n"
                "test_later_skipped (__main__.Example.test_later_skipped) ... skipped 'fixture'\n"
                "ok\n"
                "Ran 3 tests in 0.1s\n"
                "OK (skipped=2)\n"
                "Server suites: 1 passed, 0 failed, 0 skipped (opt-in), 0 skipped (environment)\n"
            )
            result = {"status": "passed", "passed": 2, "failed": 0, "skipped": 2, "logPath": str(log)}
            process = SimpleNamespace(stdout=json.dumps(result), stderr="", returncode=0)
            with patch.object(ledger.subprocess, "run", return_value=process):
                observed = ledger.run_suite(ledger.TESTS / "fixture.py", {}, root, [], {})
            self.assertEqual(observed["passed"], 2)
            self.assertEqual(observed["skipped"], 2)
            self.assertEqual(
                [(case["testId"], case["status"]) for case in observed["tests"]],
                [
                    ("__main__.Example.test_skipped", "skipped"),
                    ("__main__.Example.test_passed", "ok"),
                    ("__main__.Example.test_later_skipped", "skipped"),
                ],
            )

    def test_unittest_ids_keep_their_class_and_are_not_duplicated(self) -> None:
        cases = ledger.case_results(
            "test_example (__main__.ExampleContract.test_example) ... ok"
        )
        self.assertEqual(
            cases,
            [{
                "test": "test_example",
                "testId": "__main__.ExampleContract.test_example",
                "status": "ok",
            }],
        )

    def test_observation_classification_requires_a_valid_probe_result(self) -> None:
        classify = ledger.classify_observation
        valid = (True, True, True)
        self.assertEqual(classify(501, "unimplemented", *valid), "not_implemented")
        self.assertEqual(classify(200, "unimplemented", *valid), "unexpected")
        self.assertEqual(classify(400, "implemented", *valid), "unverified")
        self.assertEqual(classify(302, "implemented", *valid), "unverified")
        self.assertEqual(classify(500, "implemented", *valid), "unexpected")
        self.assertEqual(classify(501, "partial", *valid), "partial")
        self.assertEqual(classify(403, "partial", *valid), "unverified")
        self.assertEqual(classify(502, "partial", *valid), "unexpected")
        self.assertEqual(classify(200, "implemented", False, True, True), "unexpected")

    def test_traced_error_body_remains_readable_by_the_suite(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            hook = ledger.install_request_trace(Path(scratch))
            script = r'''
import io, sitecustomize, urllib.request
from urllib.error import HTTPError
payload = b'{"error":"Invalid request"}'
def failing(_request, *args, **kwargs):
    raise HTTPError("http://127.0.0.1/", 400, "bad", {}, io.BytesIO(payload))
sitecustomize._original_urlopen = failing
try:
    urllib.request.urlopen("http://127.0.0.1/")
except HTTPError as error:
    assert error.read() == payload, error.read()
else:
    raise AssertionError("expected HTTPError")
'''
            environment = os.environ.copy()
            environment["PYTHONPATH"] = str(hook.parent)
            subprocess.run([sys.executable, "-c", script], env=environment, check=True)


if __name__ == "__main__":
    unittest.main()
