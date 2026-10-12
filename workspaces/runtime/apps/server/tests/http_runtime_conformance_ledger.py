"""Run discovered HTTP consumers against Rust and record operation responses."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
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


def run_suite(path: Path, environment: dict[str, str]) -> dict[str, Any]:
    relative = str(path.relative_to(ROOT))
    process = subprocess.run(
        ["just", "test-json", "server", relative],
        cwd=ROOT,
        env=environment,
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
    return {
        "suite": relative,
        "status": result.get("status", "failed"),
        "passed": result.get("passed") or 0,
        "failed": result.get("failed") or 0,
        "skipped": result.get("skipped") or 0,
        "exitCode": result.get("exitCode", process.returncode),
        **({"diagnostic": result["diagnostic"]} if "diagnostic" in result else {}),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summary", type=Path, required=True)
    options = parser.parse_args()
    command = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
    if not command:
        parser.error("STUDIO_HTTP_BACKEND_COMMAND must name the Rust server executable")

    environment = os.environ.copy()
    suites = [run_suite(path, environment) for path in consumer_suites()]
    operations_result = probe_operations()
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
    }
    ledger = {
        "schemaHash": hashlib.sha256(SPEC.read_bytes()).hexdigest(),
        "operations": operations_result,
        "suites": suites,
        "totals": totals,
    }
    options.output.parent.mkdir(parents=True, exist_ok=True)
    options.output.write_text(json.dumps(ledger, indent=2) + "\n", encoding="utf-8")
    options.summary.write_text(
        "# Rust HTTP conformance ledger\n\n"
        f"Operations: {totals['operations']} total; {totals['implemented']} implemented; "
        f"{totals['notImplemented']} return 501; {totals['unexpected']} unexpected; "
        f"{totals['schemaHeaderMismatches']} schema-header mismatches.\n\n"
        f"Harness suites: {totals['suitePassed']} passed; {totals['suiteFailed']} failed "
        f"of {totals['suites']}. Tests: {totals['testsPassed']} passed; "
        f"{totals['testsFailed']} failed; {totals['testsSkipped']} skipped.\n",
        encoding="utf-8",
    )
    print(json.dumps(totals, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
