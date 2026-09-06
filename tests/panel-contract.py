#!/usr/bin/env python3
"""Panel tool delivery, persistence, ownership, and replay contracts."""

import importlib.util
import json
from pathlib import Path
import threading
import unittest
from unittest.mock import patch
import urllib.request

spec = importlib.util.spec_from_file_location("panel_fixture", Path(__file__).with_name("workspace-contract.py"))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class PanelContract(unittest.TestCase):
    def setUp(self):
        f.WorkspaceContract.setUp(self)
        self.capture = patch("codex_runtime.render_panel", side_effect=lambda panel: {
            "data_url": "data:image/png;base64,fixture", "width": 1000, "height": 150, "version": panel["version"]})
        self.mock_capture = self.capture.start()
        self.addCleanup(self.capture.stop)
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    tool = f.WorkspaceContract.tool

    def test_replace_clear_restart_and_small_snapshot(self):
        a = self.lead()
        self.assertEqual(self.runtime.panel(a["id"])["version"], 0)
        self.runtime.panel_action(a["id"], {"action": "set", "html": "<b>Phase 1</b>", "css": "b{color:red}"})
        self.runtime.panel_action(a["id"], {"action": "set", "html": "<b>Phase 2</b>"})
        self.assertEqual(self.runtime.panel(a["id"])["css"], "")
        self.assertEqual(self.runtime.snapshot()["agents"][0]["panelVersion"], 2)
        self.assertNotIn("Phase 2", json.dumps(self.runtime.snapshot()))
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        self.assertEqual(self.runtime.panel(a["id"])["html"], "<b>Phase 2</b>")
        cleared = self.runtime.panel_action(a["id"], {"action": "clear"})
        self.assertEqual(cleared["version"], 3)
        self.assertEqual(self.runtime.panel(a["id"])["html"], "")

    def test_exact_retry_cannot_overwrite_new_content(self):
        a = self.lead()
        original = {"action": "set", "html": "First"}
        first = self.runtime.panel_action(a["id"], original, "panel-first")
        self.runtime.panel_action(a["id"], {"action": "set", "html": "Second"}, "panel-second")
        self.assertEqual(self.runtime.panel_action(a["id"], original, "panel-first"), first)
        self.assertEqual(self.runtime.panel(a["id"])["html"], "Second")
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.panel_action(a["id"], {"action": "clear"}, "panel-first")

    def test_ownership_epoch_and_deleted_boundary(self):
        a, b = self.lead(), self.lead("Other")
        worker = self.worker(a)
        for actor in [a, b, worker]:
            self.runtime.panel_action(actor["id"], {"action": "set", "html": actor["name"]})
        with self.assertRaisesRegex(ValueError, "calling agent"):
            self.runtime.panel_action(worker["id"], {"action": "set", "html": "overwrite", "agent": a["id"]})
        self.assertEqual(self.runtime.panel(a["id"])["html"], a["name"])
        self.agent_update(worker, epoch=worker["epoch"] + 1)
        with self.assertRaisesRegex(ValueError, "stopped"):
            self.runtime.panel_action(worker["id"], {"action": "clear"}, epoch=worker["epoch"])
        self.agent_update(worker, autoWake=False)
        with self.assertRaisesRegex(ValueError, "stopped"):
            self.runtime.panel_action(worker["id"], {"action": "clear"})
        self.agent_update(b, deletedAt=1)
        with self.assertRaisesRegex(ValueError, "deleted"):
            self.runtime.panel(b["id"])

    def test_input_limits_fail_without_mutation(self):
        a = self.lead()
        for invalid in [
            {"action": "set"}, {"action": "set", "html": 4},
            {"action": "set", "html": "x", "css": "x" * 32769},
            {"action": "set", "html": "€" * 50000},
            {"action": "clear", "html": "x"}, {"action": "append"},
        ]:
            with self.assertRaises(ValueError):
                self.runtime.panel_action(a["id"], invalid)
        self.assertEqual(self.runtime.panel(a["id"])["version"], 0)

    def test_dynamic_tool_and_legacy_fallback(self):
        a = self.lead()
        result = self.tool(a, "orchestration_panel", {"action": "set", "html": "Native"})
        self.assertTrue(result["success"], result)
        self.assertTrue(any(item["type"] == "inputImage" for item in result["contentItems"]))
        result = self.tool(a, "orchestration_send", {
            "agent_id": "workspace",
            "text": json.dumps({"tool": "orchestration_panel", "arguments": {"action": "set", "html": "Legacy"}}),
        })
        self.assertTrue(result["success"], result)
        self.assertEqual(self.runtime.panel(a["id"])["html"], "Legacy")
        params = self.runtime.new_thread_params(self.runtime.agent(a["id"]))
        self.assertIn("orchestration_panel", [t["name"] for t in params["dynamicTools"]])

    def test_native_cached_identity_never_reapplies_old_or_changed_content(self):
        a = self.runtime.prepare(self.lead())
        request = {"id": "panel-native", "params": {
            "threadId": a["threadId"], "callId": "panel-native",
            "tool": "orchestration_panel", "arguments": {"action": "set", "html": "First"},
        }}
        self.runtime.dynamic(request)
        first = self.runtime.server.responses[-1]["result"]
        self.assertTrue(first["success"], first)
        self.runtime.panel_action(a["id"], {"action": "set", "html": "Second"})
        for html in ["First", "Changed retry"]:
            request["params"]["arguments"]["html"] = html
            self.runtime.dynamic(request)
            self.assertEqual(self.runtime.server.responses[-1]["result"], first)
            self.assertEqual(self.runtime.panel(a["id"])["html"], "Second")
            self.assertEqual(self.runtime.panel(a["id"])["version"], 2)

    def test_image_failure_retains_write_and_get_can_recover(self):
        a = self.lead()
        self.mock_capture.side_effect = RuntimeError("fixture renderer failed")
        result = self.tool(a, "orchestration_panel", {"action": "set", "html": "Retained"})
        self.assertFalse(result["success"])
        meta = json.loads(result["contentItems"][0]["text"])
        self.assertTrue(meta["panelSaved"])
        self.assertEqual(self.runtime.panel(a["id"])["html"], "Retained")
        self.mock_capture.side_effect = lambda panel: {
            "data_url": "data:image/png;base64,recovered", "width": 1000, "height": 150, "version": panel["version"]}
        result = self.tool(a, "orchestration_panel", {"action": "get"})
        self.assertTrue(result["success"])
        self.assertEqual(self.runtime.panel(a["id"])["version"], 1)
        self.assertTrue(any(item["type"] == "inputImage" for item in result["contentItems"]))

    def test_http_reads_committed_tool_content(self):
        from codex_canvas import Canvas, make_server
        canvas = Canvas(self.state)
        canvas.runtime = self.runtime
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            a = self.lead()
            self.tool(a, "orchestration_panel", {"action": "set", "html": "<svg></svg>", "css": "body{margin:0}"})
            with urllib.request.urlopen(f"http://127.0.0.1:{server.server_port}/api/panel?agent={a['id']}") as response:
                panel = json.load(response)
            self.assertEqual(panel["html"], "<svg></svg>")
            self.assertEqual(panel["version"], self.runtime.snapshot()["agents"][0]["panelVersion"])
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
