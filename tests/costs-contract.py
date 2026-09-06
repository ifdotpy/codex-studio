#!/usr/bin/env python3
"""Free tests of the local scanner bridge, no user logs or model requests."""

import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_costs import CostReader, normalize


def report():
    return [
        {
            "provider": "codex",
            "source": "local",
            "currencyCode": "USD",
            "sessionCostUSD": 12.5,
            "last30DaysCostUSD": 100.25,
            "sessionTokens": 1000,
            "last30DaysTokens": 4000,
            "historyCoverageIsEstablished": True,
            "coverage": {"priced": 2, "unpriced": 0},
            "daily": [
                {
                    "date": "2026-09-06",
                    "modelsUsed": ["known"],
                    "modelBreakdowns": [
                        {"modelName": "known", "cost": 12.5, "totalTokens": 1000}
                    ],
                }
            ],
        }
    ]


def wait(reader):
    end = time.monotonic() + 5
    while reader.busy and time.monotonic() < end:
        time.sleep(0.01)
    assert not reader.busy
    return reader.snapshot()


class CostsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.payload = self.root / "payload.json"
        self.payload.write_text(json.dumps(report()))
        self.cli = self.root / "codexbar"
        self.cli.write_text(
            f'#!{sys.executable}\nimport pathlib,sys,time\np=pathlib.Path({str(self.root)!r})\nassert sys.argv[1:]==["cost","--provider","codex","--format","json"]\nwith (p/"calls").open("a") as f: f.write("call\\n")\nif (p/"delay").exists(): time.sleep(3)\nprint((p/"payload.json").read_text())\n'
        )
        self.cli.chmod(0o700)
        self.reader = CostReader(self.root, str(self.cli))

    def tearDown(self):
        self.reader.close()
        self.tmp.cleanup()

    def test_estimate_is_not_bill_and_does_not_add_token_categories(self):
        row = report()
        row[0]["totals"] = {
            "inputTokens": 100000,
            "outputTokens": 200,
            "cacheReadTokens": 90000,
        }
        value = normalize(row)
        self.assertEqual(value["todayUSD"], 12.5)
        self.assertEqual(value["last30DaysUSD"], 100.25)
        self.assertIsNone(value["billedUSD"])
        self.assertEqual(value["coverage"], "reported")

    def test_unknown_models_and_zeros_are_not_free(self):
        row = report()
        row[0]["sessionCostUSD"] = row[0]["last30DaysCostUSD"] = 0
        row[0]["daily"][0]["modelBreakdowns"][0]["cost"] = 0
        value = normalize(row)
        self.assertIsNone(value["todayUSD"])
        self.assertEqual(value["coverage"], "partial")
        self.assertEqual(value["unknownModels"], ["known"])

    def test_old_cli_and_incomplete_history_are_explicit(self):
        row = report()
        del row[0]["coverage"]
        del row[0]["historyCoverageIsEstablished"]
        self.assertEqual(normalize(row)["coverage"], "unverified")
        row[0]["historyCoverageIsEstablished"] = False
        self.assertEqual(normalize(row)["coverage"], "partial")

    def test_missing_invalid_and_cross_provider_data(self):
        for value in (
            None,
            {},
            [{"provider": "claude"}],
            [{"provider": "codex", "source": "web"}],
        ):
            with self.assertRaises(ValueError):
                normalize(value)
        row = report()
        row[0]["sessionCostUSD"] = float("nan")
        self.assertIsNone(normalize(row)["todayUSD"])
        row[0]["last30DaysCostUSD"] = True
        self.assertIsNone(normalize(row)["last30DaysUSD"])

    def test_concurrent_reads_reuse_scan_and_durable_snapshot(self):
        (self.root / "delay").touch()
        start = time.monotonic()
        for _ in range(50):
            self.reader.snapshot()
        self.assertLess(time.monotonic() - start, 0.5)
        value = wait(self.reader)
        self.assertEqual((self.root / "calls").read_text().splitlines(), ["call"])
        self.assertEqual(value["data"]["todayUSD"], 12.5)
        other = CostReader(self.root, str(self.cli))
        self.assertEqual(other.snapshot()["data"], value["data"])
        self.assertFalse(other.busy)
        self.assertEqual((self.root / "local-costs.json").stat().st_mode & 0o777, 0o600)
        other.close()

    def test_repeated_report_does_not_accumulate_and_error_retains_previous(self):
        self.reader.snapshot()
        value = wait(self.reader)
        self.reader.state["at"] = 1
        self.reader.attempt = 0
        self.reader.snapshot()
        repeated = wait(self.reader)
        self.assertEqual(repeated["data"], value["data"])
        self.payload.write_text("bad json")
        self.reader.state["at"] = 1
        self.reader.attempt = 0
        self.reader.snapshot()
        failed = wait(self.reader)
        self.assertEqual(failed["data"], value["data"])
        self.assertTrue(failed["error"])
        self.assertTrue(failed["stale"])

    def test_timeout_ends_only_owned_scanner(self):
        (self.root / "delay").touch()
        self.reader.timeout = 0.05
        self.reader.snapshot()
        value = wait(self.reader)
        self.assertIn("timed out", value["error"])
        self.assertIsNone(value["data"])
        self.assertIsNone(self.reader.process)

    def test_scope_change_does_not_reuse_totals(self):
        self.reader.snapshot()
        wait(self.reader)
        with patch.dict(os.environ, {"CODEX_HOME": str(self.root / "another-home")}):
            other = CostReader(self.root, str(self.cli))
            self.assertIsNone(other.state["data"])
            other.close()


if __name__ == "__main__":
    unittest.main()
