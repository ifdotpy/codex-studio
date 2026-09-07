#!/usr/bin/env python3
"""EC2 read scope, reservation semantics, and JSONL process output without AWS."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/examples/ec2-panel-feed.py"
spec = importlib.util.spec_from_file_location("ec2_panel_feed", SCRIPT)
feed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(feed)
RESOURCES = [
    {"label": "Primary", "instanceId": "i-0123456789abcdef0", "profile": "project", "region": "eu-north-1", "resource": "aws-builder"},
    {"label": "Secondary", "instanceId": "i-0fedcba9876543210", "profile": "project", "region": "eu-north-1", "resource": "aws-builder-2"},
]
AWS = {"Reservations": [{"Instances": [
    {"InstanceId": RESOURCES[1]["instanceId"], "State": {"Name": "stopped"}, "InstanceType": "c7i.24xlarge"},
    {"InstanceId": RESOURCES[0]["instanceId"], "State": {"Name": "running"}, "InstanceType": "c7a.24xlarge"},
    {"InstanceId": "i-aaaaaaaaaaaaaaaaa", "State": {"Name": "running"}, "InstanceType": "t3.nano"},
]}]}


class Ec2PanelFeedContract(unittest.TestCase):
    def test_explicit_ids_grouped_and_unrequested_instances_ignored(self):
        with patch.object(feed.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(AWS), "")) as run:
            rows = feed.describe(RESOURCES, 15)
        args = run.call_args.args[0]
        self.assertEqual(args[:4], ["aws", "ec2", "describe-instances", "--instance-ids"])
        self.assertEqual(args[4:6], [r["instanceId"] for r in RESOURCES])
        self.assertEqual(args[args.index("--profile") + 1], "project")
        self.assertEqual(args[args.index("--region") + 1], "eu-north-1")
        self.assertEqual(run.call_args.kwargs["timeout"], 15)
        self.assertEqual(len(rows), 2)
        live = feed.snapshot(RESOURCES, {"aws-builder-2": {"thread": "build-worker"}}, rows, "2026-09-07T11:15:00+00:00")
        self.assertEqual(live["instances"]["0"]["state"], "running")
        self.assertEqual(live["instances"]["0"]["lease"], "No reservation")
        self.assertEqual(live["instances"]["1"]["state"], "stopped")
        self.assertEqual(live["instances"]["1"]["lease"], "Reserved")
        self.assertEqual(live["summary"], "1 running · 1 reserved")

    def test_missing_board_is_unknown_and_invalid_board_fails(self):
        self.assertEqual(feed.reservation(RESOURCES[0], None)["lease"], "Lease unknown")
        self.assertEqual(feed.reservation(RESOURCES[0], {"aws-builder": {"worker": "a"}})["owner"], "a")
        with self.assertRaises(ValueError):
            feed.reservation(RESOURCES[0], {"aws-builder": {"worker": "a", "thread": "b"}})
        with tempfile.TemporaryDirectory() as directory:
            board = Path(directory) / "board.json"
            with self.assertRaises(FileNotFoundError):
                feed.load_claims(board)
            board.write_text('{"claims": []}')
            with self.assertRaises(ValueError):
                feed.load_claims(board)

    def test_panel_has_no_agent_callbacks_and_selection_outside_feed(self):
        panel = feed.panel_spec(RESOURCES)
        self.assertEqual(panel["spec"]["state"]["selected"], "0")
        self.assertNotIn("callbacks", panel)
        self.assertEqual(panel["spec"]["elements"]["selector"]["props"]["value"], {"$bindState": "/selected"})
        self.assertEqual(panel["spec"]["elements"]["cloud"]["props"]["name"], "cloud")
        self.assertEqual(panel["spec"]["elements"]["instance-0-icon"]["props"]["name"], "server")
        self.assertEqual(panel["spec"]["state"]["live"]["instances"]["0"]["state"], "Not checked")

    def test_aws_failure_does_not_emit_false_snapshot(self):
        with patch.object(feed.subprocess, "run", return_value=subprocess.CompletedProcess([], 42, "", "private AWS detail")):
            with self.assertRaisesRegex(RuntimeError, "exit 42") as error:
                feed.describe(RESOURCES, 15)
        self.assertNotIn("private", str(error.exception))

    def test_cli_jsonl_and_config_cannot_expand_to_account_wide_query(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "config.json"
            config.write_text(json.dumps({"resources": RESOURCES}))
            fake = root / "aws"
            fake.write_text(f"#!{sys.executable}\nimport json, sys\nassert sys.argv[1:4] == ['ec2', 'describe-instances', '--instance-ids']\nprint({json.dumps(AWS)!r})\n")
            fake.chmod(0o755)
            env = {**os.environ, "PATH": str(root) + os.pathsep + os.environ.get("PATH", "")}
            result = subprocess.run([sys.executable, "-B", str(SCRIPT), "--config", str(config), "--once"], capture_output=True, text=True, env=env, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(result.stdout.splitlines()), 1)
            self.assertEqual(json.loads(result.stdout)["summary"], "1 running · lease unknown")
            config.write_text('{"resources": [{"profile": "project", "region": "eu-north-1", "label": "all"}]}')
            result = subprocess.run([sys.executable, "-B", str(SCRIPT), "--config", str(config), "--once"], capture_output=True, text=True, env=env, timeout=5)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, "")
            self.assertIn("instanceId", result.stderr)


if __name__ == "__main__":
    unittest.main()
