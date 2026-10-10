#!/usr/bin/env python3
"""Run a Python unittest suite with machine-readable result records."""

import json
import os
from pathlib import Path
import runpy
import sys
import unittest
from typing import Any

SERVER_APP_BOOTSTRAP = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SERVER_APP_BOOTSTRAP / "src"))
from codex_layout import SERVER_SOURCE_ROOT  # noqa: E402

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

RESULT_FILE: str | None = None


class StructuredTextTestResult(unittest.TextTestResult):
    def _record(self, test: Any, outcome: str) -> None:
        self._record_id(test.id(), outcome)

    def addSuccess(self, test: Any) -> None:
        super().addSuccess(test)
        self._record(test, "passed")

    def addFailure(self, test: Any, err: Any) -> None:
        super().addFailure(test, err)
        self._record(test, "failed")

    def addError(self, test: Any, err: Any) -> None:
        super().addError(test, err)
        self._record(test, "error")

    def addSkip(self, test: Any, reason: str) -> None:
        super().addSkip(test, reason)
        self._record(test, "skipped")

    def addExpectedFailure(self, test: Any, err: Any) -> None:
        super().addExpectedFailure(test, err)
        self._record(test, "expected_failure")

    def addUnexpectedSuccess(self, test: Any) -> None:
        super().addUnexpectedSuccess(test)
        self._record(test, "unexpected_success")

    def addSubTest(self, test: Any, subtest: Any, err: Any) -> None:
        super().addSubTest(test, subtest, err)
        if err is not None:
            outcome = "failed" if issubclass(err[0], test.failureException) else "error"
            self._record_id(f"{test.id()} {subtest}", outcome)

    def _record_id(self, test_id: str, outcome: str) -> None:
        if RESULT_FILE:
            with Path(RESULT_FILE).open("a", encoding="utf-8") as stream:
                stream.write(json.dumps({"id": test_id, "outcome": outcome}) + "\n")


class StructuredTextTestRunner(unittest.TextTestRunner):
    resultclass: Any = StructuredTextTestResult


def main() -> None:
    global RESULT_FILE
    if len(sys.argv) < 3 or sys.argv[1] not in {"path", "module"}:
        raise SystemExit("usage: suite_entry.py path|module TARGET [unittest args]")
    kind, target, *arguments = sys.argv[1:]
    RESULT_FILE = os.environ.pop("CODEX_SERVER_TEST_RESULT_FILE", None)
    setattr(unittest, "TextTestRunner", StructuredTextTestRunner)
    original_main = getattr(unittest, "main")

    def structured_main(*args: Any, **kwargs: Any) -> Any:
        kwargs.setdefault("testRunner", StructuredTextTestRunner)
        return original_main(*args, **kwargs)

    setattr(unittest, "main", structured_main)
    if kind == "path":
        sys.path[0] = str(Path(target).resolve().parent)
        sys.argv = [target, *arguments]
        runpy.run_path(target, run_name="__main__")
    else:
        sys.argv = [target, *arguments]
        unittest.main(module=target, argv=sys.argv, verbosity=2)


if __name__ == "__main__":
    main()
