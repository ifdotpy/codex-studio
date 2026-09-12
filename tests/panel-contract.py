#!/usr/bin/env python3
"""Progress-file delivery and retained legacy panel persistence and replay contracts."""

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
        self.capture = patch("codex_runtime.render_panel", side_effect=lambda panel, **options: {
            "data_url": "data:image/png;base64,fixture", "width": 1000, "height": 150, "version": panel["version"]})
        self.mock_capture = self.capture.start()
        self.addCleanup(self.capture.stop)
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    tool = f.WorkspaceContract.tool

    def test_new_lead_and_worker_read_bundled_guidance_on_demand_outside_studio(self):
        guide = Path(__file__).resolve().parents[1] / ".agents/skills/codex-workspace/references/panel.md"
        content = guide.read_text().strip()
        lead = self.lead(cwd=str(self.state))
        worker = self.worker(lead)
        for actor in (lead, worker):
            f.WorkspaceContract.start(self, actor)
        calls = [params for method, params in self.runtime.server.calls if method == "thread/start"]
        self.assertEqual(len(calls), 2)
        for actor, params in zip((lead, worker), calls):
            self.assertEqual(Path(params["cwd"]).resolve(), self.state.resolve())
            self.assertTrue(params["config"]["features.context_management.experimental_mode"])
            self.assertNotIn(content, params["developerInstructions"])
            self.assertIn("topic=panel", params["developerInstructions"])
            self.assertIn(str(self.state / "progress" / actor["id"] / "PROGRESS.md"), params["developerInstructions"])
            self.assertTrue((self.state / "progress" / actor["id"] / "PROGRESS.md").is_file())
        for actor in (lead, worker):
            result = self.tool(actor, "orchestration_context", {"topic": "panel"})
            self.assertTrue(result["success"], result)
            text = json.loads(result["contentItems"][0]["text"])["content"]
            self.assertIn(content, text)
            self.assertIn(str(guide), text)
            self.assertIn(str(self.state / "progress" / actor["id"] / "PROGRESS.md"), text)

    def test_missing_bundled_guidance_fails_visibly(self):
        lead = self.lead()
        with patch("codex_runtime.Path.read_text", side_effect=FileNotFoundError):
            with self.assertRaisesRegex(ValueError, "panel guidance is missing"):
                self.runtime.model_context(lead["id"], {"topic": "panel"})

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

    def test_retired_tools_and_workspace_fallback_return_exact_file_path(self):
        a = self.lead()
        self.runtime.panel_action(a["id"], {"action": "set", "html": "Retained"})
        before = self.runtime.panel(a["id"])
        self.mock_capture.reset_mock()
        for name in ("orchestration_panel", "orchestration_panel_feed"):
            for fallback in (False, True):
                args = {"action": "set", "html": "Must not replace retained data"}
                tool_name = name
                if fallback:
                    tool_name = "orchestration_send"
                    args = {"agent_id": "workspace", "text": json.dumps({"tool": name, "arguments": args})}
                result = self.tool(a, tool_name, args)
                self.assertFalse(result["success"], result)
                request_id = self.runtime.server.responses[-1]["id"]
                request_key = self.runtime.agent(a["id"])["threadId"] + ":" + request_id
                receipt = self.runtime.tool_request(request_key)
                self.assertEqual(receipt["outcome"], "not_applied")
                self.assertEqual(receipt["stage"], "failed")
                self.assertIn(str(self.state / "progress" / a["id"] / "PROGRESS.md"), result["contentItems"][0]["text"])
                self.assertEqual(self.runtime.panel(a["id"]), before)
        self.mock_capture.assert_not_called()
        params = self.runtime.new_thread_params(self.runtime.agent(a["id"]))
        self.assertFalse({"orchestration_panel", "orchestration_panel_feed"} & {t["name"] for t in params["dynamicTools"]})

    def test_native_cached_identity_never_reapplies_old_or_changed_content(self):
        a = self.runtime.prepare(self.lead())
        request = {"id": "panel-native", "params": {
            "threadId": a["threadId"], "callId": "panel-native",
            "tool": "orchestration_panel", "arguments": {"action": "set", "html": "First"},
        }}
        # Simulate a successful response accepted before the tool was retired.
        self.runtime.panel_action(a["id"], {"action": "set", "html": "First"})
        receipt = self.runtime.reserve_tool_request(request)
        first = {"success": True, "contentItems": [{"type": "inputText", "text": "retained old response"}]}
        self.runtime.finish_tool_request(receipt["id"], first, outcome="applied")
        self.runtime.panel_action(a["id"], {"action": "set", "html": "Second"})
        for html in ["First", "Changed retry"]:
            request["params"]["arguments"]["html"] = html
            self.runtime.dynamic(request)
            result = self.runtime.server.responses[-1]["result"]
            if html == "First":
                self.assertEqual(result, first)
            else:
                self.assertFalse(result["success"])
                self.assertIn("different content", result["contentItems"][0]["text"])
            self.assertEqual(self.runtime.panel(a["id"])["html"], "Second")
            self.assertEqual(self.runtime.panel(a["id"])["version"], 2)

    def test_legacy_image_failure_rejects_write_and_preserves_callbacks(self):
        a = self.lead()
        self.runtime.panel_action(a["id"], {"action": "set", "html": "Accepted",
            "callbacks": [{"id": "go", "label": "Go"}]})
        prior = self.runtime.panel(a["id"])
        self.mock_capture.side_effect = RuntimeError("fixture renderer failed")
        with self.assertRaisesRegex(RuntimeError, "fixture renderer failed"):
            self.runtime.panel_action(a["id"], {"action": "set", "html": "Rejected"})
        self.assertEqual(self.runtime.panel(a["id"]), prior)
        self.mock_capture.side_effect = lambda panel, **options: {
            "data_url": "data:image/png;base64,recovered", "width": 1000, "height": 150, "version": panel["version"]}
        result = self.runtime.capture_panel(self.runtime.panel(a["id"]), strict_layout=False)
        self.assertEqual(result["version"], 1)
        self.assertFalse(self.mock_capture.call_args.kwargs["strict_layout"])

    def test_capture_once_before_commit_and_outside_lock(self):
        a = self.lead()
        original = self.mock_capture.side_effect
        observed = []
        def capture(panel, **options):
            thread = threading.Thread(target=lambda: observed.append(self.runtime.panel(a["id"])["version"]))
            thread.start()
            thread.join(2)
            self.assertFalse(thread.is_alive(), "Renderer holds the runtime lock")
            self.assertTrue(options["strict_layout"])
            return original(panel)
        self.mock_capture.side_effect = capture
        self.runtime.panel_action(a["id"], {"action": "set", "html": "Accepted"})
        self.assertEqual(observed, [0])
        self.assertEqual(self.mock_capture.call_count, 1)
        self.assertEqual(self.runtime.panel(a["id"])["version"], 1)

    def test_stop_during_render_prevents_commit(self):
        a = self.lead()
        original = self.mock_capture.side_effect
        def capture(panel, **options):
            self.agent_update(a, epoch=a["epoch"] + 1)
            return original(panel)
        self.mock_capture.side_effect = capture
        with self.assertRaisesRegex(ValueError, "stopped"):
            self.runtime.panel_action(a["id"], {"action": "set", "html": "Obsolete"})
        self.assertEqual(self.runtime.panel(a["id"])["version"], 0)

    def test_new_turn_during_render_prevents_commit(self):
        a = self.lead()
        original = self.mock_capture.side_effect
        def capture(panel, **options):
            self.agent_update(a, turnId="next-turn")
            return original(panel)
        self.mock_capture.side_effect = capture
        with self.assertRaisesRegex(ValueError, "caller changed"):
            self.runtime.panel_action(a["id"], {"action": "set", "html": "Previous turn"})
        self.assertEqual(self.runtime.panel(a["id"])["version"], 0)

    def test_concurrent_newer_panel_wins(self):
        a = self.lead()
        original = self.mock_capture.side_effect
        def capture(panel, **options):
            self.runtime.panel_action(a["id"], {"action": "clear"})
            return original(panel)
        self.mock_capture.side_effect = capture
        with self.assertRaisesRegex(ValueError, "changed during validation"):
            self.runtime.panel_action(a["id"], {"action": "set", "html": "Obsolete"})
        panel = self.runtime.panel(a["id"])
        self.assertEqual(panel["version"], 1)
        self.assertEqual(panel["html"], "")

    def structured_spec(self, text="Phase 1"):
        return {"root": "summary", "elements": {
            "summary": {"type": "Text", "props": {"text": text}},
        }, "state": {"choice": "first"}}

    def test_catalog_uses_built_catalog_without_panel_mutation_or_capture(self):
        import codex_panel
        a = self.lead()
        catalog_path = self.state / "panel-catalog.json"
        expected = {"components": {"Text": {"props": {"text": "string"}}}}
        catalog_path.write_text(json.dumps(expected))
        with patch.object(codex_panel, "CATALOG_FILE", catalog_path):
            self.assertEqual(self.runtime.panel_action(a["id"], {"action": "catalog"}), expected)
            with self.assertRaisesRegex(ValueError, "no content"):
                self.runtime.panel_action(a["id"], {"action": "catalog", "html": "x"})
            catalog_path.unlink()
            with self.assertRaisesRegex(ValueError, "catalog is unavailable"):
                self.runtime.panel_action(a["id"], {"action": "catalog"})
        self.mock_capture.assert_not_called()
        self.assertEqual(self.runtime.panel(a["id"])["version"], 0)

    def test_structured_input_limits_and_mixed_payload_reject_before_render(self):
        a = self.lead()
        for invalid in [
            {"spec": None}, {"spec": []}, {"spec": {}},
            {"spec": {"root": "r", "elements": []}},
            {"spec": {"root": "r", "elements": {}, "state": []}},
            {"spec": {**self.structured_spec(), "other": True}},
            {"spec": self.structured_spec(), "html": ""},
            {"spec": self.structured_spec(), "css": ""},
            {"spec": self.structured_spec("€" * 50000)},
            {"spec": {**self.structured_spec(), "state": {"count": float("nan")}}},
        ]:
            with self.subTest(invalid=str(invalid)[:100]), self.assertRaises(ValueError):
                self.runtime.panel_action(a["id"], {"action": "set", **invalid})
        self.mock_capture.assert_not_called()
        self.assertEqual(self.runtime.panel(a["id"])["version"], 0)

    def test_structured_capture_persistence_replay_and_html_transition(self):
        a = self.lead()
        data = {"action": "set", "spec": self.structured_spec(),
                "callbacks": [{"id": "go", "label": "Go"}]}
        first = self.runtime.panel_action(a["id"], data, "structured-first")
        panel = self.runtime.panel(a["id"])
        self.assertEqual(panel["format"], "json-render")
        self.assertEqual(panel["spec"], data["spec"])
        self.assertEqual((panel["html"], panel["css"]), ("", ""))
        captured = self.mock_capture.call_args.args[0]
        self.assertEqual(captured["spec"], data["spec"])
        self.assertEqual(captured["version"], first["version"])
        self.runtime.panel_action(a["id"], {"action": "set", "spec": self.structured_spec("Phase 2")})
        self.assertEqual(self.runtime.panel_action(a["id"], data, "structured-first"), first)
        self.assertEqual(self.runtime.panel(a["id"])["spec"], self.structured_spec("Phase 2"))
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.panel_action(a["id"], {**data, "spec": self.structured_spec("Changed")}, "structured-first")
        self.runtime.panel_action(a["id"], {"action": "set", "html": "Legacy"})
        panel = self.runtime.panel(a["id"])
        self.assertEqual(panel["format"], "html")
        self.assertNotIn("spec", panel)
        self.runtime.panel_action(a["id"], data)
        self.runtime.panel_action(a["id"], {"action": "clear"})
        panel = self.runtime.panel(a["id"])
        self.assertNotIn("spec", panel)
        self.assertEqual((panel["format"], panel["html"], panel["callbacks"]), ("html", "", []))

    def test_structured_renderer_rejection_preserves_previous_panel(self):
        a = self.lead()
        self.runtime.panel_action(a["id"], {"action": "set", "html": "Previous",
            "callbacks": [{"id": "go", "label": "Go"}]})
        previous = self.runtime.panel(a["id"])
        self.mock_capture.side_effect = RuntimeError("Unknown component in structured panel")
        with self.assertRaisesRegex(RuntimeError, "Unknown component"):
            self.runtime.panel_action(a["id"], {"action": "set", "spec": self.structured_spec()})
        self.assertEqual(self.runtime.panel(a["id"]), previous)

    def test_legacy_structured_capture_uses_exact_revision(self):
        a = self.lead()
        data = {"action": "set", "spec": self.structured_spec()}
        self.runtime.panel_action(a["id"], data)
        self.assertEqual(self.mock_capture.call_args.args[0]["spec"], data["spec"])
        self.assertEqual(self.mock_capture.call_count, 1)
        result = self.runtime.capture_panel(self.runtime.panel(a["id"]), strict_layout=False)
        self.assertEqual(result["version"], 1)
        self.assertEqual(self.mock_capture.call_args.args[0]["spec"], data["spec"])
        self.assertFalse(self.mock_capture.call_args.kwargs["strict_layout"])

    def test_retired_call_preserves_uncached_legacy_operation_receipt(self):
        a = self.runtime.prepare(self.lead())
        original = {"action": "set", "spec": self.structured_spec("Original")}
        request_id = "structured-replay"
        first = self.runtime.panel_action(a["id"], original, a["threadId"] + ":" + request_id)
        self.runtime.panel_action(a["id"], {"action": "set", "spec": self.structured_spec("Newer")})
        self.mock_capture.reset_mock()
        self.runtime.dynamic({"id": request_id, "params": {
            "threadId": a["threadId"], "callId": request_id,
            "tool": "orchestration_panel", "arguments": original,
        }})
        result = self.runtime.server.responses[-1]["result"]
        self.assertFalse(result["success"], result)
        self.assertIn("PROGRESS.md", result["contentItems"][0]["text"])
        receipt = self.runtime.tool_request(a["threadId"] + ":" + request_id)
        self.assertEqual(receipt["outcome"], "unknown", "A committed legacy operation cannot become not_applied")
        self.mock_capture.assert_not_called()
        self.assertEqual(self.runtime.panel_action(a["id"], original, a["threadId"] + ":" + request_id), first)
        self.assertEqual(self.runtime.panel(a["id"])["spec"], self.structured_spec("Newer"))

    def test_structured_python_render_accepts_no_html_before_queue(self):
        import codex_panel_render
        # The real Chromium catalog is tested separately. Reaching its queue proves
        # the Python transport accepts a structured panel without an HTML string.
        with patch.object(codex_panel_render._RENDERERS, "acquire", return_value=False):
            with self.assertRaisesRegex(codex_panel_render.PanelRenderError, "busy"):
                codex_panel_render.render_panel({"spec": self.structured_spec(), "version": 1})
        with self.assertRaisesRegex(codex_panel_render.PanelRenderError, "cannot also contain"):
            codex_panel_render.render_panel({"spec": self.structured_spec(), "html": "Mixed"})

    def test_http_reads_file_changes_without_panel_tools(self):
        from codex_canvas import Canvas, make_server
        canvas = Canvas(self.state)
        canvas.runtime = self.runtime
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            a = self.lead()
            from codex_progress import provision_progress
            path = provision_progress(self.state, a["id"])
            path.write_text("# Verified progress\n\n- [x] Backend", encoding="utf-8")
            with urllib.request.urlopen(f"http://127.0.0.1:{server.server_port}/api/panel?agent={a['id']}") as response:
                panel = json.load(response)
            self.assertEqual(panel["format"], "markdown")
            self.assertEqual(panel["markdown"], path.read_text())
            self.assertEqual(panel["path"], str(path))
            self.assertIsNotNone(panel["revision"])
            self.assertNotIn(panel["markdown"], json.dumps(self.runtime.snapshot()))
            self.mock_capture.assert_not_called()
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
