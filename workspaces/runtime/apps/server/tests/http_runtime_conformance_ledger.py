"""Run discovered HTTP consumers against Rust and record operation responses."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
from collections import defaultdict
from typing import Any


ROOT = Path(__file__).resolve().parents[5]
TESTS = ROOT / "workspaces/runtime/apps/server/tests"
SPEC = ROOT / "workspaces/runtime/packages/runtime-http/openapi.json"
HARNESS = TESTS / "http_backend_harness.py"
IMPLEMENTED = {
    ("GET", "/api/session"),
    ("GET", "/api/sync/identity"),
    ("GET", "/api/desktop"),
}
JSON_METHODS = {"POST", "PUT", "PATCH"}
DEFAULT_CACHE = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
DEFAULT_LEDGER = DEFAULT_CACHE / "codex-studio" / "rust-runtime-http-conformance-ledger.json"


def consumer_suites() -> list[Path]:
    return sorted(
        path
        for path in TESTS.glob("*.py")
        if path != HARNESS
        and path.name != Path(__file__).name
        and re.search(r"(?:from\s+http_backend_harness\s+import|import\s+http_backend_harness)",
                      path.read_text(encoding="utf-8"))
    )


def operations() -> list[tuple[str, str]]:
    document = json.loads(SPEC.read_text(encoding="utf-8"))
    methods = {"get", "put", "post", "delete", "options", "head", "patch", "trace"}
    return sorted(
        (method.upper(), path)
        for path, item in document.get("paths", {}).items()
        for method in item
        if method in methods
    )


def probe_operations() -> list[dict[str, Any]]:
    sys.path.insert(0, str(TESTS))
    from http_backend_harness import BackendProcess

    with tempfile.TemporaryDirectory(prefix="rust-http-ledger-") as scratch_name:
        scratch = Path(scratch_name).resolve(strict=True)
        backend = BackendProcess(scratch)
        try:
            backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
            backend.start()
            token = backend.session["token"]
            rows = []
            for method, template in operations():
                path = re.sub(r"\{[^}]+\}", "ledger-probe", template)
                headers = {
                    "X-Canvas-Token": token,
                    "X-Studio-API-Schema": backend.schema_hash,
                }
                if method in JSON_METHODS or method == "DELETE":
                    headers["X-Canvas-Workspace"] = backend.identity["workspaceId"]
                result = backend.request(
                    method,
                    path,
                    body={} if method in JSON_METHODS else None,
                    headers=headers,
                    timeout=2.0,
                )
                declared_implementation = (method, template) in IMPLEMENTED
                expected_status = 200 if declared_implementation else 501
                schema_header = result.headers.get("x-studio-api-schema")
                schema_header_matches = schema_header == backend.schema_hash
                rows.append({
                    "method": method,
                    "path": template,
                    "status": result.status,
                    "classification": (
                        "implemented" if declared_implementation and result.status == 200 and schema_header_matches
                        else "not_implemented" if not declared_implementation and result.status == 501 and schema_header_matches
                        else "unexpected"
                    ),
                    "expectedStatus": expected_status,
                    "schemaHeader": schema_header,
                    "schemaHeaderMatches": schema_header_matches,
                })
            return rows
        finally:
            backend.close()


def case_results(log_text: str) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    current: str | None = None
    for line in log_text.splitlines():
        start = re.match(r"^(test_[\w]+)\s+\([^)]*\)\s+\.\.\.\s*(.*)$", line)
        if start:
            current = start.group(1)
            tail = start.group(2).strip()
            if tail in {"ok", "FAIL", "ERROR"} or tail.startswith("skipped"):
                cases.append({"test": current, "status": tail.split()[0].lower()})
                current = None
            continue
        if current is not None and line.strip() in {"ok", "FAIL", "ERROR"}:
            cases.append({"test": current, "status": line.strip().lower()})
            current = None
    return cases


def failure_headings(log_text: str) -> list[dict[str, str]]:
    failures: list[dict[str, str]] = []
    pattern = re.compile(r"^(FAIL|ERROR):\s+(test_[\w]+|setUpClass)\s+\(([^)]*)\)")
    for line in log_text.splitlines():
        match = pattern.match(line)
        if match:
            failures.append({"kind": match.group(1).lower(), "test": match.group(2), "context": match.group(3)})
    return failures


def install_request_trace(directory: Path) -> Path:
    """Trace consumer requests in the harness process without changing its fixtures."""
    hook = directory / "sitecustomize.py"
    hook.write_text(
        '''import io, json, os, threading, unittest
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import Request
import urllib.request

_trace_local = threading.local()
_trace_lock = threading.Lock()
_original_run = unittest.TestCase.run
_original_urlopen = urllib.request.urlopen

def _run(test, result=None):
    _trace_local.case = test.id()
    try:
        return _original_run(test, result)
    finally:
        _trace_local.case = None

def _record(request, status, body=None):
    destination = os.environ.get("STUDIO_HTTP_LEDGER_TRACE")
    if not destination:
        return
    if isinstance(request, Request):
        method = request.get_method()
        url = request.full_url
    else:
        method = "GET"
        url = str(request)
    record = {"test": getattr(_trace_local, "case", None), "method": method,
              "path": urlsplit(url).path, "status": status}
    if body is not None:
        try:
            record["body"] = json.loads(body)
        except (ValueError, TypeError):
            record["body"] = body.decode("utf-8", "replace")
    with _trace_lock, open(destination, "a", encoding="utf-8") as stream:
        stream.write(json.dumps(record, separators=(",", ":")) + "\\n")

def _urlopen(request, *args, **kwargs):
    try:
        response = _original_urlopen(request, *args, **kwargs)
    except HTTPError as error:
        body = error.read()
        _record(request, error.code, body)
        error.fp = io.BytesIO(body)
        raise
    _record(request, response.status)
    return response

unittest.TestCase.run = _run
urllib.request.urlopen = _urlopen
''',
        encoding="utf-8",
    )
    return hook


def canonical_operation(method: str, path: str, known: list[tuple[str, str]]) -> str | None:
    hidden_voice = ("POST", "/api/voice/{action:path}")
    if method == hidden_voice[0] and path.startswith("/api/voice/") and len(path) > len("/api/voice/"):
        return f"{hidden_voice[0]} {hidden_voice[1]}"
    parts = path.split("/")
    for expected_method, template in known:
        expected_parts = template.split("/")
        if method == expected_method and len(parts) == len(expected_parts) and all(
            want.startswith("{") or want == actual
            for want, actual in zip(expected_parts, parts)
        ):
            return f"{method} {template}"
    return None


def run_suite(
    path: Path,
    environment: dict[str, str],
    trace_root: Path,
    known_operations: list[tuple[str, str]],
) -> dict[str, Any]:
    relative = str(path.relative_to(ROOT))
    trace_path = trace_root / f"{path.stem}.jsonl"
    suite_environment = environment.copy()
    suite_environment["STUDIO_HTTP_LEDGER_TRACE"] = str(trace_path)
    process = subprocess.run(
        ["just", "test-json", "server", relative],
        cwd=ROOT,
        env=suite_environment,
        text=True,
        capture_output=True,
        check=False,
    )
    try:
        result = json.loads(process.stdout.strip().splitlines()[-1])
    except (IndexError, json.JSONDecodeError):
        result = {
            "status": "failed",
            "passed": 0,
            "failed": 1,
            "skipped": 0,
            "executed": 1,
            "exitCode": process.returncode or 1,
            "diagnostic": (process.stderr + process.stdout)[-4000:],
        }
    log_path = result.get("logPath")
    log_text = Path(log_path).read_text(encoding="utf-8") if log_path else ""
    cases = case_results(log_text)
    requests: list[dict[str, Any]] = []
    if trace_path.exists():
        for line in trace_path.read_text(encoding="utf-8").splitlines():
            event = json.loads(line)
            event["operation"] = canonical_operation(event["method"], event["path"], known_operations)
            requests.append(event)
    by_test: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for event in requests:
        if event["test"]:
            by_test[event["test"].rsplit(".", 1)[-1]].append(event)
        else:
            by_test[None].append(event)
    unattributed = by_test.get(None, [])
    failures = []
    for failure in failure_headings(log_text):
        evidence = unattributed if failure["test"] == "setUpClass" else by_test.get(failure["test"], [])
        blockers = sorted({event["operation"] for event in evidence if event["status"] == 501 and event["operation"]})
        failure.update({
            "requests": evidence,
            "blocking501": blockers,
            "standard501Envelope": all(
                event.get("body") == {"error": "Not implemented"}
                for event in evidence if event["status"] == 501
            ),
        })
        failures.append(failure)
    for case in cases:
        case_requests = by_test.get(case["test"], [])
        case["requests"] = case_requests
        case["blocking501"] = [
            event["operation"] for event in case_requests
            if event["status"] == 501 and event["operation"]
        ]
        case["standard501Envelope"] = all(
            event.get("body") == {"error": "Not implemented"}
            for event in case_requests if event["status"] == 501
        )
    return {
        "suite": relative,
        "status": result.get("status", "failed"),
        "passed": result.get("passed") or 0,
        "failed": result.get("failed") or 0,
        "skipped": result.get("skipped") or 0,
        "exitCode": result.get("exitCode", process.returncode),
        "tests": cases,
        "failures": failures,
        "unattributedRequests": unattributed,
        **({"diagnostic": result["diagnostic"]} if "diagnostic" in result else {}),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=DEFAULT_LEDGER)
    options = parser.parse_args()
    command = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
    if not command:
        parser.error("STUDIO_HTTP_BACKEND_COMMAND must name the Rust server executable")

    environment = os.environ.copy()
    routes = operations()
    DEFAULT_CACHE.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rust-http-traces-", dir=DEFAULT_CACHE) as trace_dir_name:
        trace_root = Path(trace_dir_name)
        hook = install_request_trace(trace_root)
        existing_pythonpath = environment.get("PYTHONPATH", "")
        environment["PYTHONPATH"] = str(hook.parent) + (
            os.pathsep + existing_pythonpath if existing_pythonpath else ""
        )
        suites = [
            run_suite(path, environment, trace_root, routes)
            for path in consumer_suites()
        ]
    operations_result = probe_operations()
    operation_tests: dict[str, set[str]] = defaultdict(set)
    failures_without_501: list[str] = []
    envelope_mismatches: list[str] = []
    for suite in suites:
        for index, failure in enumerate(suite["failures"]):
            test_id = f"{suite['suite']}::{failure['context']}::{failure['test']}#{index + 1}"
            blockers = set(failure["blocking501"])
            if not blockers:
                failures_without_501.append(test_id)
            for operation in blockers:
                operation_tests[operation].add(test_id)
            if blockers and not failure["standard501Envelope"]:
                envelope_mismatches.append(test_id)
    blocker_ranking = [
        {"operation": operation, "failedTests": len(tests), "tests": sorted(tests)}
        for operation, tests in sorted(
            operation_tests.items(), key=lambda entry: (-len(entry[1]), entry[0])
        )
    ]
    client_schema = ROOT / "workspaces/client/apps/web/src/generated/apiSchema.ts"
    schema_match = re.search(r'"([0-9a-f]{64})"', client_schema.read_text(encoding="utf-8"))
    if schema_match is None:
        raise ValueError("generated TypeScript schema hash is missing")
    totals = {
        "operations": len(operations_result),
        "implemented": sum(row["classification"] == "implemented" for row in operations_result),
        "notImplemented": sum(row["classification"] == "not_implemented" for row in operations_result),
        "unexpected": sum(row["classification"] == "unexpected" for row in operations_result),
        "schemaHeaderMismatches": sum(not row["schemaHeaderMatches"] for row in operations_result),
        "suites": len(suites),
        "suitePassed": sum(row["status"] == "passed" for row in suites),
        "suiteFailed": sum(row["status"] != "passed" for row in suites),
        "testsPassed": sum(row["passed"] for row in suites),
        "testsFailed": sum(row["failed"] for row in suites),
        "testsSkipped": sum(row["skipped"] for row in suites),
        "failedTestsWithout501": len(failures_without_501),
        "standard501EnvelopeMismatches": len(envelope_mismatches),
    }
    ledger = {
        "schemaHash": schema_match.group(1),
        "operations": operations_result,
        "suites": suites,
        "blockingOperationRanking": blocker_ranking,
        "failedTestsWithout501": failures_without_501,
        "standard501EnvelopeMismatches": envelope_mismatches,
        "totals": totals,
    }
    options.output.parent.mkdir(parents=True, exist_ok=True)
    options.output.write_text(json.dumps(ledger, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"totals": totals, "blockingOperationRanking": blocker_ranking}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
