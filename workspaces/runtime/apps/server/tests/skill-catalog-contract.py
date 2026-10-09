#!/usr/bin/env python3
"""Scoped composer inventory through the real HTTP adapter; no model requests."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import argparse
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_canvas import Canvas, make_server
from codex_workspace import WorkspaceMixin, SKILL_CATALOG_TIMEOUT_SECONDS


class Inventory(WorkspaceMixin):
    def __init__(self):
        self.lock = threading.RLock()
        self.actor = {"id": "lead", "cwd": "/project", "accountKey": "selected"}
        self.calls = []
        self.response = {"data": [{"cwd": "/project", "skills": [], "errors": []}]}
        self.failure = None
        self.entered = None
        self.release = None

    def checked_actor_in_own_db(self, key):
        with self.lock:
            if key != self.actor["id"]:
                raise ValueError("Unknown agent")
            return dict(self.actor)

    def connect(self, key):
        self.account = key
        return self

    def connect_agent(self, actor):
        return self.connect(actor["accountKey"])

    def call(self, method, params, timeout):
        self.calls.append((self.account, method, params, timeout))
        if self.entered:
            self.entered.set()
            if not self.release.wait(3):
                raise TimeoutError("Fixture timeout")
        if self.failure:
            raise self.failure
        return self.response


class SkillCatalogContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.runtime = Inventory()
        self.canvas = Canvas(Path(self.temp.name))
        self.canvas.runtime = self.runtime
        self.server = make_server(self.canvas)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.close_server)
        self.url = f"http://127.0.0.1:{self.server.server_port}/api/skills?agent="

    def close_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(3)

    def get(self, agent="lead", headers=None):
        request = urllib.request.Request(self.url + agent, headers=headers or {})
        with urllib.request.urlopen(request, timeout=4) as response:
            return json.load(response)

    def test_selected_account_project_and_enabled_native_precedence(self):
        self.runtime.response = {"data": [
            {"cwd": "/other", "skills": [{"name": "foreign", "path": "/foreign"}]},
            {"cwd": "/project", "skills": [
                {"name": "zebra", "path": "/repo/zebra", "description": "Local", "enabled": True},
                {"name": "zebra", "path": "/global/zebra", "enabled": True},
                {"name": "hidden", "path": "/hidden", "enabled": False},
                {"name": "alpha", "path": "/alpha"},
                {"name": "missing-path"}, None,
            ], "errors": [{"path": "/broken", "message": "Invalid skill metadata"}]},
        ]}
        self.assertEqual(self.get(), {
            "skills": [
                {"name": "alpha", "path": "/alpha", "description": ""},
                {"name": "zebra", "path": "/repo/zebra", "description": "Local"},
            ], "errors": ["Invalid skill metadata"],
        })
        self.assertEqual(self.runtime.calls, [("selected", "skills/list",
            {"cwds": ["/project"], "forceReload": False}, SKILL_CATALOG_TIMEOUT_SECONDS)])
        self.runtime.actor.update(cwd="/other", accountKey="other-account")
        self.assertEqual(self.get()["skills"][0]["name"], "foreign")
        self.assertEqual(self.runtime.calls[-1][0], "other-account")

    def test_failure_and_empty_are_distinct_without_fallback(self):
        self.assertEqual(self.get(), {"skills": [], "errors": []})
        self.runtime.failure = TimeoutError("Provider unavailable")
        self.assertEqual(self.get(), {"skills": [], "errors": ["Provider unavailable"]})
        self.runtime.failure = None
        self.runtime.response = {}
        self.assertEqual(self.get()["errors"], ["The provider did not return a skill catalog"])

    def test_unknown_actor_and_cross_origin_never_reach_provider(self):
        for agent, headers, status in [
            ("missing", {}, 400),
            ("lead", {"Origin": "https://untrusted.example"}, 403),
        ]:
            with self.subTest(agent=agent, headers=headers):
                with self.assertRaises(urllib.error.HTTPError) as error:
                    self.get(agent, headers)
                self.assertEqual(error.exception.code, status)
                error.exception.close()
        self.assertEqual(self.runtime.calls, [])

    def test_slow_provider_does_not_hold_runtime_lock(self):
        self.runtime.entered = threading.Event()
        self.runtime.release = threading.Event()
        result = []
        worker = threading.Thread(target=lambda: result.append(self.get()))
        worker.start()
        try:
            self.assertTrue(self.runtime.entered.wait(2))
            acquired = self.runtime.lock.acquire(timeout=0.2)
            self.assertTrue(acquired, "Typing/state updates must not wait on discovery")
            if acquired:
                self.runtime.lock.release()
        finally:
            self.runtime.release.set()
            worker.join(4)
        self.assertFalse(worker.is_alive())
        self.assertEqual(result, [{"skills": [], "errors": []}])


def verify_live(args):
    """Read-only receipt and catalog verification; never starts a model turn."""
    from urllib.parse import urlencode
    with urllib.request.urlopen(args.live_url + "/api/desktop", timeout=10) as response:
        desktop = json.load(response)
    assert desktop["pid"] == args.expected_pid, "Backend identity changed"
    receipt = desktop.get("liveUpdate", {})
    assert receipt.get("manifestId") == args.release, receipt
    assert receipt.get("status") == "applied", receipt
    started = time.monotonic()
    with urllib.request.urlopen(args.live_url + "/api/skills?" + urlencode({"agent": args.agent}), timeout=10) as response:
        body = response.read()
    catalog = json.loads(body)
    assert catalog.get("errors") == [], catalog.get("errors")
    skills = catalog["skills"]
    assert isinstance(skills, list) and skills, "Expected this account's installed skills"
    assert all(isinstance(s.get(k), str) for s in skills for k in ("name", "description", "path"))
    assert len({s["name"] for s in skills}) == len(skills), "Duplicate native names"
    evidence = {"pid": desktop["pid"], "release": args.release, "receipt": receipt,
                "skillCount": len(skills), "bytes": len(body),
                "elapsedMs": round((time.monotonic() - started) * 1000),
                "names": [s["name"] for s in skills], "checkedAt": time.time()}
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({k: evidence[k] for k in ("pid", "release", "skillCount", "bytes", "elapsedMs")}))


if __name__ == "__main__":
    if "--live-url" in sys.argv:
        parser = argparse.ArgumentParser()
        parser.add_argument("--live-url", required=True)
        parser.add_argument("--agent", required=True)
        parser.add_argument("--expected-pid", type=int, required=True)
        parser.add_argument("--release", required=True)
        parser.add_argument("--evidence", type=Path, required=True)
        verify_live(parser.parse_args())
    else:
        unittest.main()
