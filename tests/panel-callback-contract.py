#!/usr/bin/env python3
"""Panel callback ownership, durable wake, concurrency, and HTTP boundary."""
import importlib.util
import json
from pathlib import Path
import threading
from concurrent.futures import ThreadPoolExecutor
import unittest
from unittest.mock import patch
import urllib.request
import urllib.error
import uuid

spec = importlib.util.spec_from_file_location("panel_callback_fixture", Path(__file__).with_name("workspace-contract.py"))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_panel import PanelConflict


class CallbackContract(unittest.TestCase):
    def setUp(self):
        f.WorkspaceContract.setUp(self)
        capture = patch("codex_runtime.render_panel", side_effect=lambda panel, **options: {
            "data_url": "data:image/png;base64,fixture", "width": 1000, "height": 150, "version": panel["version"]})
        capture.start()
        self.addCleanup(capture.stop)
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    start = f.WorkspaceContract.start
    events = f.WorkspaceContract.events

    def panel(self, actor):
        return self.runtime.panel_action(actor["id"], {"action": "set", "html": '<form data-callback="review"><input name="note"><button>Send</button></form>',
            "callbacks": [{"id": "review", "label": "Review this result", "fields": ["note"]}]})

    def body(self, actor, version=1, **changes):
        return {"id": str(uuid.uuid4()), "agent": actor["id"], "version": version,
                "callback": "review", "values": {"note": ["Please inspect"]}, **changes}

    def test_callback_wakes_completed_owner_and_preserves_payload(self):
        a = self.start(self.lead())
        self.panel(a)
        self.runtime.server.complete(a["threadId"], a["turnId"])
        response = self.runtime.panel_callback(self.body(a))
        self.assertEqual(response["status"], "pending")
        self.assertEqual(self.runtime.agent(a["id"])["status"], "queued")
        event = self.events(a, "panel_callback")[0]
        payload = json.loads(event["text"])
        self.assertEqual(payload["values"], {"note": ["Please inspect"]})
        self.assertEqual(payload["label"], "Review this result")
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(a["id"])["inFlight"])
        f.eventually(lambda: any("user_panel_interaction" in json.dumps(p) for m,p in self.runtime.server.calls if m=="turn/start"))

    def test_exact_retry_after_update_and_restart_returns_original_receipt(self):
        a = self.lead()
        self.panel(a)
        body = self.body(a)
        first = self.runtime.panel_callback(body)
        self.panel(a)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        self.assertEqual(self.runtime.panel_callback(body), first)
        self.assertEqual(len(self.events(a, "panel_callback")), 1)
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.panel_callback({**body, "values": {"note": ["Changed"]}})

    def test_double_click_concurrent_ids_enqueue_once(self):
        a = self.lead()
        self.panel(a)
        with ThreadPoolExecutor(max_workers=6) as pool:
            results = list(pool.map(lambda _: self.runtime.panel_callback(self.body(a)), range(6)))
        self.assertEqual(len({r["eventId"] for r in results}), 1)
        self.assertEqual(len(self.events(a, "panel_callback")), 1)
        self.assertEqual(self.runtime.panel(a["id"])["submittedCallbacks"], ["review"])
        with self.assertRaises(PanelConflict):
            self.runtime.panel_callback(self.body(a, values={"note": ["Another choice"]}))
        self.panel(a)
        self.runtime.panel_callback(self.body(a, version=2))
        self.assertEqual(len(self.events(a, "panel_callback")), 2)

    def test_worker_routes_to_itself_and_stop_delete_boundaries(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.panel(worker)
        self.runtime.panel_callback(self.body(worker))
        self.assertEqual(len(self.events(worker, "panel_callback")), 1)
        self.assertEqual(len(self.events(lead, "panel_callback")), 0)
        self.panel(worker)
        self.agent_update(worker, autoWake=False)
        with self.assertRaisesRegex(ValueError, "stopped"):
            self.runtime.panel_callback(self.body(worker, version=2))
        self.agent_update(worker, deletedAt=1)
        with self.assertRaisesRegex(ValueError, "deleted"):
            self.runtime.panel_callback(self.body(worker, version=2))

    def test_invalid_values_callbacks_and_stale_versions_do_not_enqueue(self):
        a = self.lead()
        self.panel(a)
        for changes in [{"version": 0}, {"version": True}, {"callback": "unknown"},
                        {"values": {"other": ["x"]}}, {"values": {"note": "x"}},
                        {"values": {"note": ["x"] * 17}}, {"values": {"note": ["x" * 2001]}},
                        {"values": {"note": ["€" * 2000] * 3}}]:
            with self.assertRaises(ValueError):
                self.runtime.panel_callback(self.body(a, **changes))
        for callbacks in [[{"id": "bad id", "label": "Bad"}], [{"id": "ok", "label": "Good", "fields": ["x", "x"]}],
                          [{"id": "ok", "label": "Good", "agent": a["id"]}], [{"id": "x", "label": ""}]]:
            with self.assertRaises(ValueError):
                self.runtime.panel_action(a["id"], {"action": "set", "html": "x", "callbacks": callbacks})
        self.assertEqual(self.events(a, "panel_callback"), [])
        self.assertEqual(self.runtime.panel(a["id"])["version"], 1)
        self.runtime.panel_action(a["id"], {"action": "clear"})
        self.assertEqual(self.runtime.panel(a["id"])["callbacks"], [])

    def test_http_requires_token_origin_and_reports_conflict(self):
        from codex_canvas import Canvas, make_server
        canvas = Canvas(self.state)
        canvas.runtime = self.runtime
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        def post(body, headers):
            return urllib.request.urlopen(urllib.request.Request(base+"/api/panel/callback", data=json.dumps(body).encode(),
                headers={"Content-Type": "application/json", **headers}), timeout=5)
        try:
            token = json.load(urllib.request.urlopen(base+"/api/state"))["token"]
            with urllib.request.urlopen(urllib.request.Request(base+"/assets/panel-bridge.js", headers={
                "Origin": "null", "Sec-Fetch-Site": "cross-site"})) as bridge:
                self.assertEqual(bridge.status, 200)
                self.assertIn(b"panel-callback", bridge.read())
            with self.assertRaises(urllib.error.HTTPError) as denied_read:
                urllib.request.urlopen(urllib.request.Request(base+"/api/state", headers={
                    "Origin": "null", "Sec-Fetch-Site": "cross-site"}))
            self.assertEqual(denied_read.exception.code, 403)
            a = self.lead()
            self.panel(a)
            for headers in [{}, {"Origin": "https://outside.invalid", "X-Canvas-Token": token}]:
                with self.assertRaises(urllib.error.HTTPError) as error:
                    post(self.body(a), headers)
                self.assertEqual(error.exception.code, 403)
            headers = {"Origin": base, "X-Canvas-Token": token}
            self.assertEqual(json.load(post(self.body(a), headers))["status"], "pending")
            with self.assertRaises(urllib.error.HTTPError) as error:
                post(self.body(a, version=0), headers)
            self.assertEqual(error.exception.code, 409)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
