#!/usr/bin/env python3
"""Restart-visible question and monitor state on Python and Rust HTTP backends."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import runtime_environment
from http_backend_harness import TimedHTTPContractTest, start_contract_backend

EXPECTED = Path(__file__).resolve().parents[1] / "tests" / "startup-recovery-http-expected.json"


def seed_records(backend) -> None:
    source = Path(__file__).resolve().parents[1] / "src"
    script = f"""
import os, sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
sys.path.insert(0, {str(source.parent / 'tests')!r})
from runtime_test_environment import make_runtime
from codex_runtime import Runtime

class QuietRuntime(Runtime):
    def schedule(self): pass
    def schedule_fast_dispatch(self, *args, **kwargs): pass

root = Path(os.environ['CODEX_AGENTS_STATE_DIR'])
runtime = make_runtime(QuietRuntime, root, environment=dict(os.environ))
try:
    agent = runtime.create({{
        'name':'Startup HTTP', 'cwd':str(Path(os.environ['CODEX_CANVAS_CWD'])),
        'prompt':'Fixture only', 'threadId':'startup-http-thread', 'autoWake':True,
    }}, defer=True)
    agent.update(autoWake=True, threadId='startup-http-thread', turnId='startup-http-turn')
    with runtime.lock, runtime.db() as db:
        runtime.put(db, 'agents', agent)
    (root / 'startup-recovery-agent-id.txt').write_text(agent['id'], encoding='utf-8')
    runtime.notification({{'method':'item/completed','params':{{
        'threadId':'startup-http-thread',
        'item':{{'type':'agentMessage','id':'retained-question','text':'Choose',
                'questions':[{{'title':'Retained question','options':['yes','no']}}]}}
    }}}})
    monitor = runtime.monitor(agent['id'], {{'command':'printf approval'}}, key='startup-http-monitor')
    (root / 'startup-recovery-monitor-id.txt').write_text(monitor['id'], encoding='utf-8')
finally:
    runtime.close()
"""
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", script],
        cwd=backend.cwd,
        env=runtime_environment(backend),
        check=True,
        timeout=30,
    )


class StartupRecoveryHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        seed_records(cls.backend)
        cls.backend.start()

    def headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_questions_and_monitor_projection_show_recovered_state_after_restart(self) -> None:
        agent_id = (self.backend.state / "startup-recovery-agent-id.txt").read_text(encoding="utf-8")
        monitor_id = (self.backend.state / "startup-recovery-monitor-id.txt").read_text(encoding="utf-8")
        questions = self.backend.request(
            "GET", f"/api/questions?agent={agent_id}", headers=self.headers()
        )
        self.assertEqual(questions.status, 200, questions.body[:1000])
        question_items = json.loads(questions.body)["items"]
        retained = [item for item in question_items if item["id"] == f"{agent_id}:question:retained-question"]
        self.assertEqual(len(retained), 1, question_items)
        self.assertEqual(retained[0]["status"], "pending")

        pull = self.backend.request(
            "GET",
            "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1",
            headers=self.headers(),
        )
        self.assertEqual(pull.status, 200, pull.body[:1000])
        documents = json.loads(pull.body)["documents"]
        request_entities = []
        monitor_entities = []
        monitor_list = []
        for document in documents:
            identifier = document.get("id", "")
            if not identifier.startswith(("entity:request:", "entity:monitor:")):
                continue
            payload = json.loads(document["payload"])
            value = payload.get("value", {})
            normalized = {
                "deleted": bool(document.get("_deleted")),
                "method": value.get("method"),
                "status": value.get("status"),
            }
            if identifier.startswith("entity:request:"):
                request_entities.append(normalized)
            else:
                monitor_entities.append(normalized)
                monitor_list.append({"status": value.get("status"), "error": value.get("error")})
        normalized = {
            "pendingRequests": [{"status": retained[0]["status"],
                                 "title": retained[0]["questions"][0]["question"]}],
            "monitors": sorted(monitor_list, key=lambda item: (item["status"] or "", item["error"] or "")),
            "syncEntities": {
                "requests": sorted(request_entities, key=lambda item: (item["method"] or "", item["status"] or "", item["deleted"])),
                "monitors": sorted(monitor_entities, key=lambda item: (item["status"] or "", item["deleted"])),
            },
        }
        if os.environ.get("STARTUP_HTTP_WRITE_EXPECTATION") == "1":
            EXPECTED.write_text(json.dumps(normalized, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        self.assertTrue(EXPECTED.exists(), "committed Python-generated HTTP expectation is missing")
        expected = json.loads(EXPECTED.read_text(encoding="utf-8"))
        self.assertEqual(normalized, expected)


if __name__ == "__main__":
    unittest.main()
