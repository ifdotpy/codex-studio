#!/usr/bin/env python3
"""Compare failed suite sets from saved server-run logs."""

from __future__ import annotations

import argparse
from pathlib import Path
import re


FAILURE = re.compile(r"\bFAIL\s+([^\s:]+\.py):\s*(.*)")
LAYOUT_PREFIXES = (
    ("tests/", "workspaces/runtime/apps/server/tests/"),
    ("scripts/", "workspaces/runtime/apps/server/src/"),
)


def normalize_suite_path(suite: str) -> str:
    """Give old and relocated logs the same repository-relative suite identity."""
    for prefix, replacement in LAYOUT_PREFIXES:
        if suite.startswith(prefix):
            return replacement + suite[len(prefix):]
    return suite


def failures(path: Path) -> dict[str, set[str]]:
    result: dict[str, set[str]] = {}
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        match = FAILURE.search(line)
        if match:
            suite, detail = match.groups()
            suite = normalize_suite_path(suite)
            result.setdefault(suite, set()).add(detail.strip())
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs="+", type=Path, help="saved server-run logs")
    args = parser.parse_args()
    if len(args.logs) < 2:
        parser.error("provide at least two logs to compare")

    runs = [(path, failures(path)) for path in args.logs]
    common = set.intersection(*(set(result) for _, result in runs))
    print(f"intersection ({len(common)}):")
    for suite in sorted(common):
        print(f"  {suite}")
    for path, result in runs:
        extras = set(result) - common
        print(f"extras for {path} ({len(extras)}):")
        for suite in sorted(extras):
            print(f"  {suite}: {'; '.join(sorted(result[suite]))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
