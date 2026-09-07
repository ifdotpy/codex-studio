#!/usr/bin/env python3
"""Verify every EC2 selector view in the real hidden Electron panel renderer."""

import base64
import importlib.util
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_panel_render import render_panel

spec = importlib.util.spec_from_file_location("ec2_panel_feed", ROOT / "scripts/examples/ec2-panel-feed.py")
feed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(feed)


class Ec2PanelRenderContract(unittest.TestCase):
    def test_initial_and_live_data_fit_for_twelve_instances(self):
        resources = [{"label": f"Build resource {i + 1}", "instanceId": f"i-{i + 1:017x}",
                      "profile": "fixture", "region": "eu-north-1", "resource": f"builder-{i}"}
                     for i in range(12)]
        panel = feed.panel_spec(resources)
        panel.update(format="json-render", callbacks=[], version=1)
        initial = render_panel(panel)
        self.assertTrue(initial["layout"]["fits"])
        instances = {(r["profile"], r["region"], r["instanceId"]):
                     {"InstanceId": r["instanceId"], "State": {"Name": "shutting-down"}, "InstanceType": "c7i.24xlarge"}
                     for r in resources}
        panel["spec"]["state"]["live"] = feed.snapshot(resources, None, instances, "2026-09-07T11:15:30+00:00")
        result = render_panel(panel)
        self.assertTrue(result["layout"]["fits"])
        self.assertEqual([v["width"] for v in result["layout"]["viewports"]], [320, 640, 1000])
        self.assertTrue(base64.b64decode(result["data_url"].split(",")[1]).startswith(b"\x89PNG"))


if __name__ == "__main__":
    unittest.main()
