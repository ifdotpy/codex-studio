"""Run discovered HTTP consumers against Rust and record operation responses."""
from __future__ import annotations

import argparse
import ast
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import urllib.parse
from collections import defaultdict
from typing import Any


ROOT = Path(__file__).resolve().parents[5]
TESTS = ROOT / "workspaces/runtime/apps/server/tests"
SPEC = ROOT / "workspaces/runtime/packages/runtime-http/openapi.json"
HARNESS = TESTS / "http_backend_harness.py"
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


def appendix_http_assertions(
    known_operations: set[str],
) -> dict[str, set[str]]:
    """Resolve the plan's 23 H witnesses to named test methods."""
    plan = ROOT / "docs/plans/rust-runtime-work-breakdown.md"
    text = plan.read_text(encoding="utf-8").split("<!-- operation-map -->", 1)[1]
    witnesses: dict[str, set[str]] = defaultdict(set)
    for line in text.splitlines():
        if not line.startswith("| `"):
            continue
        cells = [cell.strip() for cell in line.strip("|").split("|")]
        if len(cells) < 8 or cells[6] != "H":
            continue
        operation = cells[0].strip("`")
        if operation not in known_operations:
            continue
        witness = cells[7].strip("`")
        match = re.fullmatch(r"T/([^:]+):(\d+)", witness)
        if match is None:
            continue
        relative, line_number_text = match.groups()
        source_path = TESTS / relative
        module = ast.parse(source_path.read_text(encoding="utf-8"), filename=str(source_path))
        source_tests: set[str] = set()

        def collect_test_ids(node: ast.AST, ancestors: list[str]) -> None:
            current = ancestors
            if isinstance(node, ast.ClassDef):
                current = [*ancestors, node.name]
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                if node.name.startswith("test_"):
                    source_tests.add(f"workspaces/runtime/apps/server/tests/{relative}::__main__.{'.'.join([*ancestors, node.name])}")
                current = [*ancestors, node.name]
            for child in ast.iter_child_nodes(node):
                collect_test_ids(child, current)

        collect_test_ids(module, [])
        line_number = int(line_number_text)
        candidates: list[tuple[list[str], int]] = []
        enclosing_functions: list[str] = []

        def find_test(node: ast.AST, ancestors: list[str]) -> None:
            start = getattr(node, "lineno", 0)
            end = getattr(node, "end_lineno", start)
            if not isinstance(node, ast.Module) and not start <= line_number <= end:
                return
            current = ancestors
            if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
                current = [*ancestors, node.name]
                if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    enclosing_functions.append(node.name)
                if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
                    candidates.append((current, end - start))
            for child in ast.iter_child_nodes(node):
                find_test(child, current)

        find_test(module, [])
        selected = [min(candidates, key=lambda candidate: candidate[1])[0]] if candidates else []
        helper = enclosing_functions[-1] if enclosing_functions else None
        if not selected and helper is not None:
            def find_helper_callers(node: ast.AST, ancestors: list[str]) -> None:
                current = ancestors
                if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
                    current = [*ancestors, node.name]
                    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
                        calls_helper = any(
                            isinstance(call, ast.Call)
                            and ((isinstance(call.func, ast.Name) and call.func.id == helper)
                                 or (isinstance(call.func, ast.Attribute) and call.func.attr == helper))
                            for call in ast.walk(node)
                        )
                        if calls_helper:
                            selected.append(current)
                for child in ast.iter_child_nodes(node):
                    find_helper_callers(child, current)
            find_helper_callers(module, [])
        if not selected:
            raise ValueError(f"plan witness {witness} does not resolve to a named test")
        resolved = {
            f"workspaces/runtime/apps/server/tests/{relative}::__main__.{'.'.join(names)}"
            for names in selected
        }
        missing = resolved - source_tests
        if missing:
            raise ValueError(f"plan witness {witness} resolves to missing test IDs: {sorted(missing)}")
        witnesses[operation].update(resolved)
    return witnesses


def rust_operation_manifest() -> dict[tuple[str, str], dict[str, Any]]:
    process = subprocess.run(
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "studio-runtime-http",
            "print_operation_manifest",
            "--",
            "--nocapture",
        ],
        cwd=ROOT,
        env=os.environ.copy(),
        text=True,
        capture_output=True,
        check=False,
    )
    if process.returncode:
        raise RuntimeError(
            "could not read the Rust route manifest:\n"
            + (process.stderr + process.stdout)[-4000:]
        )
    marker = "RUST_HTTP_OPERATION_MANIFEST="
    for line in process.stdout.splitlines():
        if line.startswith(marker):
            return {
                (item["method"], item["path"]): item
                for item in json.loads(line[len(marker):])
            }
    raise RuntimeError("Rust route manifest test did not print its operation manifest")


def resolve_schema(schema: dict[str, Any], document: dict[str, Any]) -> dict[str, Any]:
    reference = schema.get("$ref")
    if not reference:
        return schema
    if not reference.startswith("#/components/schemas/"):
        raise ValueError(f"unsupported OpenAPI reference in probe fixture: {reference}")
    value: Any = document
    for part in reference[2:].split("/"):
        value = value[part.replace("~1", "/").replace("~0", "~")]
    return value


def sample_value(schema: dict[str, Any], document: dict[str, Any], active: frozenset[str] = frozenset()) -> Any:
    reference = schema.get("$ref")
    if reference in active:
        return {}
    schema = resolve_schema(schema, document)
    active = active | ({reference} if reference else set())
    for union_key in ("anyOf", "oneOf"):
        if union_key in schema:
            for member in schema[union_key]:
                try:
                    return sample_value(member, document, active)
                except (ValueError, KeyError):
                    continue
    if "const" in schema:
        return schema["const"]
    if "enum" in schema:
        return schema["enum"][0]
    if "default" in schema:
        return schema["default"]
    kind = schema.get("type")
    if kind == "object" or "properties" in schema:
        properties = schema.get("properties", {})
        required = schema.get("required", list(properties))
        return {name: sample_value(properties[name], document, active) for name in required if name in properties}
    if kind == "array":
        minimum = schema.get("minItems", 0)
        return [sample_value(schema.get("items", {}), document, active) for _ in range(minimum)]
    if kind == "string":
        return "x" * schema.get("minLength", 1)
    if kind == "integer":
        return max(1, int(schema.get("minimum", schema.get("exclusiveMinimum", 0) + 1)))
    if kind == "number":
        return max(1.0, float(schema.get("minimum", 0)))
    if kind == "boolean":
        return True
    if kind == "null":
        return None
    return {}


def valid_probe(method: str, template: str) -> tuple[str, Any | None]:
    document = json.loads(SPEC.read_text(encoding="utf-8"))
    operation = document["paths"][template][method.lower()]
    path_values: dict[str, Any] = {}
    query_values: list[tuple[str, str]] = []
    for parameter in operation.get("parameters", []):
        schema = resolve_schema(parameter.get("schema", {}), document)
        value = sample_value(schema, document)
        if parameter.get("in") == "path":
            path_values[parameter["name"]] = value
        elif parameter.get("in") == "query":
            query_values.append((parameter["name"], str(value).lower() if isinstance(value, bool) else str(value)))
    path = re.sub(r"\{([^}:]+)(?::[^}]+)?\}", lambda match: urllib.parse.quote(str(path_values.get(match.group(1), "probe")), safe=""), template)
    if query_values:
        path += "?" + urllib.parse.urlencode(query_values)
    body = None
    body_schema = operation.get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema")
    if body_schema:
        body = sample_value(body_schema, document)
    return path, body


def probe_operations(
    manifest: dict[tuple[str, str], dict[str, Any]],
    planned_assertions: dict[str, set[str]],
    successful_assertions: dict[str, set[str]],
) -> list[dict[str, Any]]:
    sys.path.insert(0, str(TESTS))
    from http_backend_harness import BackendProcess

    with tempfile.TemporaryDirectory(
        prefix="rust-http-ledger-", dir=DEFAULT_CACHE
    ) as scratch_name:
        scratch = Path(scratch_name).resolve(strict=True)
        backend = BackendProcess(scratch)
        try:
            backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
            backend.start()
            token = backend.session["token"]
            rows = []
            for method, template in operations():
                path, body = valid_probe(method, template)
                headers = {
                    "X-Canvas-Token": token,
                    "X-Studio-API-Schema": backend.schema_hash,
                }
                if method in JSON_METHODS or method == "DELETE":
                    headers["X-Canvas-Workspace"] = backend.identity["workspaceId"]
                result = backend.request(
                    method,
                    path,
                    body=body if method in JSON_METHODS else None,
                    headers=headers,
                    timeout=2.0,
                )
                operation_manifest = manifest.get((method, template))
                implementation = operation_manifest["status"] if operation_manifest else "unimplemented"
                dependencies = (
                    operation_manifest.get("dependencies", [])
                    if operation_manifest
                    else []
                )
                schema_header = result.headers.get("x-studio-api-schema")
                schema_header_matches = schema_header == backend.schema_hash
                try:
                    response_body = json.loads(result.body)
                except (ValueError, TypeError):
                    response_body = None
                operation_key = f"{method} {template}"
                probe_event = {"operation": operation_key, "body": response_body}
                standard_501_envelope = result.status != 501 or valid_501_envelope(probe_event, manifest)
                dependency_response_matches = implementation != "partial" or result.status != 501 or standard_501_envelope
                classification = classify_observation(
                    result.status,
                    implementation,
                    schema_header_matches,
                    standard_501_envelope,
                    dependency_response_matches,
                )
                rows.append({
                    "method": method,
                    "path": template,
                    "status": result.status,
                    "implementation": implementation,
                    "observedStatus": result.status,
                    "classification": classification,
                    "covered": bool(successful_assertions.get(operation_key)),
                    "expectedStatus": (
                        "not_501"
                        if implementation == "implemented"
                        else "any"
                        if implementation == "partial"
                        else 501
                    ),
                    "dependencies": dependencies,
                    "schemaHeader": schema_header,
                    "schemaHeaderMatches": schema_header_matches,
                    "standard501Envelope": standard_501_envelope,
                    "dependencyResponseMatches": dependency_response_matches,
                    "httpAssertions": sorted(planned_assertions.get(operation_key, set())),
                    "observedSuccessfulAssertions": sorted(successful_assertions.get(operation_key, set())),
                })
            return rows
        finally:
            backend.close()


def case_results(log_text: str) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    current: str | None = None
    for line in log_text.splitlines():
        start = re.match(r"^(test_[\w]+)\s+\(([^)]*)\)\s+\.\.\.\s*(.*)$", line)
        if start:
            current = start.group(1)
            full_id = start.group(2)
            tail = start.group(3).strip()
            if tail in {"ok", "FAIL", "ERROR"} or tail.startswith("skipped"):
                cases.append({"test": current, "testId": full_id, "status": tail.split()[0].lower()})
                current = None
            continue
        if current is not None and line.strip() in {"ok", "FAIL", "ERROR"}:
            cases.append({"test": current, "testId": full_id, "status": line.strip().lower()})
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
        raise HTTPError(error.url, error.code, error.msg, error.headers, io.BytesIO(body))
    _record(request, response.status)
    return response

unittest.TestCase.run = _run
urllib.request.urlopen = _urlopen
''',
        encoding="utf-8",
    )
    return hook


def canonical_operation(method: str, path: str, known: list[tuple[str, str]]) -> str | None:
    parts = path.split("/")
    for expected_method, template in known:
        expected_parts = template.split("/")
        if method == expected_method and len(parts) == len(expected_parts) and all(
            want.startswith("{") or want == actual
            for want, actual in zip(expected_parts, parts)
        ):
            return f"{method} {template}"
    hidden_voice = ("POST", "/api/voice/{action:path}")
    if method == hidden_voice[0] and path.startswith("/api/voice/") and len(path) > len("/api/voice/"):
        return f"{hidden_voice[0]} {hidden_voice[1]}"
    return None


def run_suite(
    path: Path,
    environment: dict[str, str],
    trace_root: Path,
    known_operations: list[tuple[str, str]],
    manifest: dict[tuple[str, str], dict[str, Any]],
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
            event["assertion"] = (
                f"{relative}::{event['test']}" if event.get("test") else None
            )
            requests.append(event)
    by_test: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for event in requests:
        if event["test"]:
            by_test[event["test"]].append(event)
        else:
            by_test[None].append(event)
    unattributed = by_test.get(None, [])
    failures = []
    for failure in failure_headings(log_text):
        test_id = failure["context"]
        evidence = unattributed if failure["test"] == "setUpClass" else by_test.get(test_id, [])
        blockers = sorted({event["operation"] for event in evidence if event["status"] == 501 and event["operation"]})
        failure.update({
            "requests": evidence,
            "blocking501": blockers,
            "standard501Envelope": all(
                valid_501_envelope(event, manifest)
                for event in evidence if event["status"] == 501
            ),
        })
        failures.append(failure)
    for case in cases:
        case_requests = by_test.get(case["testId"], [])
        case["requests"] = case_requests
        case["blocking501"] = [
            event["operation"] for event in case_requests
            if event["status"] == 501 and event["operation"]
        ]
        case["standard501Envelope"] = all(valid_501_envelope(event, manifest) for event in case_requests if event["status"] == 501)
    return {
        "suite": relative,
        "status": result.get("status", "failed"),
        "passed": result.get("passed") or 0,
        "failed": result.get("failed") or 0,
        "skipped": result.get("skipped") or 0,
        "exitCode": result.get("exitCode", process.returncode),
        "tests": cases,
        "failures": failures,
        "requests": requests,
        "unattributedRequests": unattributed,
        **({"diagnostic": result["diagnostic"]} if "diagnostic" in result else {}),
    }


def valid_501_envelope(event: dict[str, Any], manifest: dict[tuple[str, str], dict[str, Any]]) -> bool:
    operation = event.get("operation")
    if operation:
        method, path = operation.split(" ", 1)
        declared = manifest.get((method, path))
        if declared and declared.get("status") == "partial":
            dependencies = declared.get("dependencies", [])
            return bool(dependencies) and event.get("body") == {
                "error": f"Not implemented: requires {', '.join(dependencies)}"
            }
    return event.get("body") == {"error": "Not implemented"}


def classify_observation(
    status: int,
    implementation: str,
    schema_header_matches: bool,
    standard_501_envelope: bool,
    dependency_response_matches: bool,
) -> str:
    if not schema_header_matches or not standard_501_envelope or not dependency_response_matches:
        return "unexpected"
    if implementation == "unimplemented":
        return "not_implemented" if status == 501 else "unexpected"
    if implementation == "implemented":
        if status >= 500:
            return "unexpected"
        return "implemented" if status < 400 else "unverified"
    if implementation == "partial":
        if status == 501:
            return "partial"
        if status >= 500:
            return "unexpected"
        return "partial" if status < 400 else "unverified"
    return "unexpected"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=DEFAULT_LEDGER)
    options = parser.parse_args()
    command = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
    if not command:
        parser.error("STUDIO_HTTP_BACKEND_COMMAND must name the Rust server executable")

    environment = os.environ.copy()
    differential = subprocess.run(
        [
            sys.executable,
            str(ROOT / "workspaces/runtime/apps/server/src/codex_python.py"),
            "--exec",
            str(TESTS / "http_runtime_extractor_differential.py"),
        ],
        cwd=ROOT,
        env=environment,
        check=False,
    )
    if differential.returncode:
        raise RuntimeError("HTTP extractor differential contract failed")
    trace_regression = subprocess.run(
        [sys.executable, str(TESTS / "http_runtime_conformance_ledger_test.py")],
        cwd=ROOT, env=environment, check=False,
    )
    if trace_regression.returncode:
        raise RuntimeError("HTTP ledger error-body trace regression failed")
    routes = operations()
    manifest = rust_operation_manifest()
    DEFAULT_CACHE.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rust-http-traces-", dir=DEFAULT_CACHE) as trace_dir_name:
        trace_root = Path(trace_dir_name)
        hook = install_request_trace(trace_root)
        existing_pythonpath = environment.get("PYTHONPATH", "")
        environment["PYTHONPATH"] = str(hook.parent) + (
            os.pathsep + existing_pythonpath if existing_pythonpath else ""
        )
        suites = [
            run_suite(path, environment, trace_root, routes, manifest)
            for path in consumer_suites()
        ]
    planned_assertions: dict[str, set[str]] = defaultdict(set)
    known_operations = {f"{method} {path}" for method, path in operations()}
    for operation, assertions in appendix_http_assertions(known_operations).items():
        planned_assertions[operation].update(assertions)
    successful_assertions: dict[str, set[str]] = defaultdict(set)
    for suite in suites:
        passing = {
            f"{suite['suite']}::{test['testId']}"
            for test in suite.get("tests", []) if test.get("status") == "ok"
        }
        for event in suite.get("requests", []):
            if event.get("operation") and event.get("assertion") in passing and 200 <= event["status"] < 300:
                successful_assertions[event["operation"]].add(event["assertion"])
    operations_result = probe_operations(manifest, planned_assertions, successful_assertions)
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
        "partial": sum(row["classification"] == "partial" for row in operations_result),
        "notImplemented": sum(row["classification"] == "not_implemented" for row in operations_result),
        "unverified": sum(row["classification"] == "unverified" for row in operations_result),
        "uncovered": sum(not row["covered"] for row in operations_result),
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
    print(json.dumps({"totals": totals}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
