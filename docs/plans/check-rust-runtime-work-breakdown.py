#!/usr/bin/env python3
"""Check the plan's operation partition; measure its Python source references.

This documentation tool imports no runtime code and never opens runtime state.
"""
from __future__ import annotations

import argparse
import ast
from collections import Counter, defaultdict
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "workspaces/runtime/apps/server/src"
SPEC = ROOT / "workspaces/runtime/packages/runtime-http/openapi.json"
DOCUMENT = Path(__file__).with_name("rust-runtime-work-breakdown.md")
METHODS = {"get", "post", "put", "patch", "delete", "head", "options", "trace"}
FALLBACK = "POST /api/voice/{action:path}"
REFERENCE = re.compile(r"([\w/.-]+\.py):(\d+)")


def assignments(text: str) -> list[list[str]]:
    appendix = text.split("<!-- operation-map -->", 1)[1].split("<!-- /operation-map -->", 1)[0]
    return [
        [cell.strip().strip("`") for cell in line.strip().strip("|").split("|")]
        for line in appendix.splitlines()
        if re.match(r"\|\s*`?(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS|TRACE) /", line)
    ]


def check(rows: list[list[str]], known: set[str]) -> list[str]:
    counts = Counter(row[0] for row in rows)
    errors = []
    for operation in sorted(known | {FALLBACK}):
        if counts[operation] != 1:
            errors.append(f"{operation}: expected one assignment, found {counts[operation]}")
    for operation in sorted(counts.keys() - known - {FALLBACK}):
        errors.append(f"unknown operation: {operation}")
    for row in rows:
        if len(row) != 8 or any(not value for value in row):
            errors.append(f"malformed assignment: {row}")
            continue
        if not row[3].isdigit() or row[6] not in {"H", "I", "N"}:
            errors.append(f"invalid wave or coverage: {row[0]}")
    slices: dict[str, set[str]] = defaultdict(set)
    for row in rows:
        if len(row) == 8:
            slices[row[2]].add(row[3])
    errors.extend(f"slice {name} spans waves {sorted(waves)}" for name, waves in slices.items() if len(waves) != 1)
    return errors


def measure(rows: list[list[str]], document: str) -> dict[str, object]:
    """Count physical def-to-end spans, including blank/comment lines, once/group."""
    targets = dict(re.findall(r"^- (C[0-9]+): `([^`]+)`", document, re.MULTILINE))
    definitions: dict[str, dict[int, int]] = {}
    modules: dict[str, int] = {}
    groups: dict[str, dict[str, dict[str, int]]] = defaultdict(lambda: {"handlers": {}, "callees": {}})
    for row in rows:
        for label, cell in (("handlers", row[4]), ("callees", row[5])):
            if label == "callees":
                keys = re.findall(r"C[0-9]+", cell)
                if cell != "—" and not keys:
                    raise ValueError(f"missing callee identifiers: {row[0]}")
                missing = [key for key in keys if key not in targets]
                if missing:
                    raise ValueError(f"unknown callee identifiers: {missing}")
                cell = "; ".join(targets[key] for key in keys)
            for filename, raw_line in REFERENCE.findall(cell):
                if filename not in definitions:
                    content = (SOURCE / filename).read_text(encoding="utf-8")
                    modules[filename] = len(content.splitlines())
                    definitions[filename] = {
                        node.lineno: node.end_lineno - node.lineno + 1
                        for node in ast.walk(ast.parse(content))
                        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
                        and node.end_lineno is not None
                    }
                line = int(raw_line)
                if line not in definitions[filename]:
                    raise ValueError(f"not a function definition: {filename}:{line}")
                groups[row[1]][label][f"{filename}:{line}"] = definitions[filename][line]
    for filename, raw_line in REFERENCE.findall(document):
        source_path = SOURCE / filename
        if raw_line == "1" and source_path.is_file():
            modules[filename] = len(source_path.read_text(encoding="utf-8").splitlines())
    return {
        "groups": {group: {label: {"functions": len(values), "lines": sum(values.values())}
                           for label, values in kinds.items()} for group, kinds in sorted(groups.items())},
        "modulePhysicalLines": dict(sorted(modules.items())),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--document", type=Path, default=DOCUMENT)
    parser.add_argument("--measure", action="store_true")
    args = parser.parse_args()
    spec = json.loads(SPEC.read_text(encoding="utf-8"))
    known = {f"{method.upper()} {path}" for path, item in spec["paths"].items() for method in item if method in METHODS}
    try:
        document = args.document.read_text(encoding="utf-8")
        rows = assignments(document)
        errors = check(rows, known)
        measurements = measure(rows, document) if not errors else {}
    except (IndexError, ValueError, OSError) as error:
        errors = [str(error)]
    if errors:
        print("FAIL\n" + "\n".join(errors))
        return 1
    counts = Counter(row[6] for row in rows if row[0] in known)
    waves = Counter(int(row[3]) for row in rows if row[0] in known)
    print(f"PASS: {len(known)} documented operations + 1 voice fallback; no missing, duplicate, or unknown assignments")
    print(f"Coverage census: H={counts['H']} I={counts['I']} N={counts['N']} (source evidence, not execution)")
    print("Documented operation assignments by wave: " + ", ".join(f"{wave}={count}" for wave, count in sorted(waves.items())))
    if args.measure:
        print(json.dumps(measurements, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
