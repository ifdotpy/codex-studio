#!/usr/bin/env python3
"""Focused tests for the isolated native Codex cost scanner."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
BUILDER = ROOT / "scripts" / "cost-scanner" / "build.py"
REQUESTED_BINARY = None


def binary_from_argv():
    global REQUESTED_BINARY
    if "--binary" not in sys.argv:
        return None
    index = sys.argv.index("--binary")
    try:
        value = sys.argv[index + 1]
    except IndexError as error:
        raise SystemExit("--binary needs a path") from error
    del sys.argv[index : index + 2]
    REQUESTED_BINARY = Path(value).expanduser().resolve()
    return REQUESTED_BINARY


binary_from_argv()


class ScannerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if platform.system() != "Darwin":
            raise unittest.SkipTest("The helper is macOS-only.")
        cls.tmp = tempfile.TemporaryDirectory(prefix="codex-cost-scanner-test-")
        cls.root = Path(cls.tmp.name)
        cls.binary = REQUESTED_BINARY
        if cls.binary is None:
            cls.binary = cls.root / "bin" / "codex-cost-scanner"
            result = subprocess.run(
                [sys.executable, str(BUILDER), "--output", str(cls.binary)],
                capture_output=True,
                text=True,
                timeout=130,
            )
            if result.returncode:
                raise RuntimeError(result.stderr or result.stdout)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def setUp(self):
        self.case = Path(tempfile.mkdtemp(prefix="case-", dir=self.root))
        self.timestamp = (datetime.now(timezone.utc) - timedelta(minutes=1)).replace(
            microsecond=0
        )
        self.day = self.timestamp.strftime("%Y-%m-%d")

    def session_path(self, home, session_id):
        day = self.day
        path = (
            self.case
            / home
            / "sessions"
            / day[:4]
            / day[5:7]
            / day[8:10]
            / f"{session_id}.jsonl"
        )
        path.parent.mkdir(parents=True, exist_ok=True)
        return path

    def append_rows(self, path, rows):
        with path.open("a") as stream:
            for row in rows:
                stream.write(json.dumps(row, separators=(",", ":")) + "\n")

    def rows(self, session_id, model, input_tokens, output_tokens, *, parent=None, total=None):
        stamp = self.timestamp.isoformat().replace("+00:00", "Z")
        total_input = total if total is not None else input_tokens
        total_output = total if total is not None else output_tokens
        metadata = {"id": session_id, "timestamp": stamp}
        if parent is not None:
            metadata["forked_from_id"] = parent
        return [
            {"timestamp": stamp, "type": "session_meta", "payload": metadata},
            {"timestamp": stamp, "type": "turn_context", "payload": {"model": model}},
            {
                "timestamp": stamp,
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": input_tokens,
                            "cached_input_tokens": 0,
                            "output_tokens": output_tokens,
                        },
                        "total_token_usage": {
                            "input_tokens": total_input,
                            "cached_input_tokens": 0,
                            "output_tokens": total_output,
                        },
                    },
                },
            },
        ]

    def scan(self, home, cache):
        home_path = self.case / home
        cache_path = self.case / cache
        result = subprocess.run(
            [
                str(self.binary),
                "--home",
                str(home_path),
                "--cache",
                str(cache_path),
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)[0]

    def test_profile_roots_and_caches_are_independent(self):
        a = self.session_path("home-a", "a1")
        b = self.session_path("home-b", "b1")
        foreign = self.session_path("foreign", "foreign1")
        self.append_rows(a, self.rows("a1", "gpt-5", 1000, 1000))
        self.append_rows(b, self.rows("b1", "gpt-5-mini", 2000, 2000))
        self.append_rows(foreign, self.rows("foreign1", "gpt-5", 9000, 9000))

        first_a = self.scan("home-a", "cache-a")
        first_b = self.scan("home-b", "cache-b")

        self.assertEqual(first_a["source"], "local")
        self.assertEqual(first_a["provider"], "codex")
        self.assertEqual(first_a["sessionTokens"], 2000)
        self.assertEqual(first_b["sessionTokens"], 4000)
        self.assertNotEqual(first_a["last30DaysCostUSD"], first_b["last30DaysCostUSD"])
        self.assertEqual(first_a["coverage"], {"priced": 1, "unpriced": 0})
        self.assertFalse(first_a["historyCoverageIsEstablished"])

        cache = json.loads(
            (self.case / "cache-a" / "cost-usage" / "codex-v8.json").read_text()
        )
        files = set(cache["files"])
        self.assertTrue(files)
        self.assertTrue(all(path.startswith(str((self.case / "home-a").resolve())) for path in files))
        self.assertFalse(any("foreign" in path for path in files))
        installed_pricing = Path.home() / "Library/Caches/CodexBar/model-pricing/models-dev-v1.json"
        self.assertEqual((self.case / "cache-a" / "model-pricing" / "models-dev-v1.json").exists(), installed_pricing.exists())
        self.assertTrue((self.case / "cache-b" / "cost-usage" / "codex-v8.json").exists())

    def test_changed_log_uses_persistent_incremental_cache(self):
        path = self.session_path("home", "one")
        self.append_rows(path, self.rows("one", "gpt-5", 1000, 1000))
        first = self.scan("home", "cache")
        self.assertEqual(first["sessionTokens"], 2000)

        stamp = self.timestamp.isoformat().replace("+00:00", "Z")
        self.append_rows(
            path,
            [
                {
                    "timestamp": stamp,
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "last_token_usage": {
                                "input_tokens": 500,
                                "cached_input_tokens": 0,
                                "output_tokens": 500,
                            },
                            "total_token_usage": {
                                "input_tokens": 1500,
                                "cached_input_tokens": 0,
                                "output_tokens": 1500,
                            },
                        },
                    },
                }
            ],
        )
        second = self.scan("home", "cache")
        self.assertEqual(second["sessionTokens"], 3000)
        self.assertNotEqual(first["last30DaysCostUSD"], second["last30DaysCostUSD"])

    def test_fork_counts_child_delta_once(self):
        parent = self.session_path("home", "parent")
        child = self.session_path("home", "child")
        self.append_rows(parent, self.rows("parent", "gpt-5", 1000, 1000))
        self.append_rows(
            child,
            self.rows("child", "gpt-5", 500, 500, parent="parent", total=1500),
        )
        report = self.scan("home", "cache")
        self.assertEqual(report["sessionTokens"], 3000)
        self.assertEqual(report["last30DaysTokens"], 3000)

    def test_unknown_model_stays_unpriced(self):
        path = self.session_path("home", "unknown")
        self.append_rows(path, self.rows("unknown", "model-never-priced", 100, 100))
        report = self.scan("home", "cache")
        self.assertEqual(report["coverage"]["unpriced"], 1)
        breakdown = report["daily"][0]["modelBreakdowns"][0]
        self.assertEqual(breakdown["modelName"], "model-never-priced")
        self.assertNotIn("cost", breakdown)


if __name__ == "__main__":
    unittest.main()
