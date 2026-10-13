#!/usr/bin/env python3
"""Analytics query, export, and session-cost reads through either backend."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def seed_analytics(backend) -> None:
    source = Path(__file__).resolve().parents[1] / "src"
    script = f"""
import json, os, sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime
runtime = Runtime(Path(os.environ['CODEX_AGENTS_STATE_DIR']))
try:
    agent = {{'id':'analytics-contract-agent','rootId':'analytics-contract-agent','epoch':0,
        'status':'idle','name':'Analytics contract','role':'orchestrator','cwd':os.environ['CODEX_CANVAS_CWD'],
        'isLead':True,'autoWake':True,'approvalPolicy':'never','threadId':'analytics-thread',
        'turnId':'analytics-turn','accountKey':'default','accountHistory':[],'model':'fixture-model'}}
    with runtime.lock, runtime.db(analytics=True) as db:
        runtime.put(db, 'agents', agent, sync_rooms=False)
        runtime.analytics_agent(db, agent)
        usage = {{**agent,'id':'analytics-response','agentId':agent['id'],'at':100,'recordedAt':100,
            'responseId':'analytics-response','model':'fixture-model','source':'rollout','timestampSource':'record',
            'last':{{'inputTokens':100,'cachedInputTokens':25,'outputTokens':20,'totalTokens':120}},
            'total':{{'inputTokens':100,'cachedInputTokens':25,'outputTokens':20,'totalTokens':120}},
            'delta':{{'inputTokens':100,'cachedInputTokens':25,'cacheWriteInputTokens':0,
                'outputTokens':20,'reasoningOutputTokens':0,'totalTokens':120}},
            'cumulativeDelta':{{'inputTokens':100,'cachedInputTokens':25,'cacheWriteInputTokens':0,
                'outputTokens':20,'reasoningOutputTokens':0,'totalTokens':120}},
            'counterDomain':'response','modelContextWindow':10000,'baselineMissing':False,'reset':False}}
        db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
            ('analytics-response','analytics-contract-agent','analytics-contract-agent',
             'analytics-thread','analytics-turn',100,json.dumps(usage)))
        error = {{**agent,'id':'analytics-contract-agent:analytics-thread:analytics-turn','agentId':agent['id'],
            'at':101,'status':'failed','error':{{'message':'fixture failure'}}}}
        db.execute('INSERT INTO analytics_turns(id,agent,root,at,record) VALUES (?,?,?,?,?)',
            (error['id'],'analytics-contract-agent','analytics-contract-agent',101,json.dumps(error)))
        item = {{**agent,'id':'analytics-call','agentId':agent['id'],'itemId':'analytics-call','at':102,
            'startedAt':101,'finishedAt':102,'recordedAt':102,'source':'rollout','timestampSource':'record',
            'turnId':'analytics-turn','threadId':'analytics-thread',
            'type':'commandExecution','name':'shell','isTool':True,'payloadBoundary':'protocol',
            'status':'completed','input':{{'bytes':8,'chars':8,'lines':1,'imageCount':0,
                'imageBytes':0,'images':[],'format':'text'}},
            'output':{{'bytes':14,'chars':14,'lines':1,'imageCount':0,
                'imageBytes':0,'images':[],'format':'text'}},'durationMs':1000}}
        db.execute('INSERT INTO analytics_items(id,agent,root,thread,turn,at,type,name,is_tool,record) VALUES (?,?,?,?,?,?,?,?,?,?)',
            ('analytics-call','analytics-contract-agent','analytics-contract-agent','analytics-thread',
             'analytics-turn',102,'commandExecution','shell',1,json.dumps(item)))
    assert runtime.analytics('analytics-contract-agent')['summary']['usageSamples'] == 1
finally:
    runtime.close()
"""
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    subprocess.run([python, "-B", "-c", script], cwd=backend.cwd,
                   env=backend._environment(), check=True, timeout=30)


class AnalyticsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        rust_command = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
        cls.rust_backend = bool(rust_command)
        if rust_command:
            os.environ.pop("STUDIO_HTTP_BACKEND_COMMAND", None)
        try:
            cls.backend = start_contract_backend(cls)
            cls.backend.stop()
            seed_analytics(cls.backend)
            if rust_command:
                os.environ["STUDIO_HTTP_BACKEND_COMMAND"] = rust_command
            cls.backend.start()
            cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)
        except BaseException:
            if rust_command:
                os.environ["STUDIO_HTTP_BACKEND_COMMAND"] = rust_command
            raise

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def _headers(self):
        return {"Origin": self.backend.origin or "",
                "X-Canvas-Token": self.backend.session["token"],
                "X-Studio-API-Schema": self.backend.schema_hash}

    def test_analytics_rollup_views_detail_export_and_session_cost_dependency(self) -> None:
        headers = self._headers()
        root = "/api/analytics?agent=analytics-contract-agent"
        rollup = self.backend.request("GET", root, headers=headers)
        self.assertEqual(rollup.status, 200, rollup.body[:1000] + b"\\n" + self.backend.stderr.encode())
        value = rollup.json()
        self.assertEqual(value["summary"]["usageSamples"], 1)
        self.assertEqual(value["summary"]["tokens"]["totalTokens"], 120)
        self.assertEqual(value["pagination"]["total"], 1)
        self.assertEqual(self.backend.request("GET", root + "&tool=missing", headers=headers).json()["pagination"]["total"], 0)

        errors = self.backend.request("GET", root + "&view=turn-errors&thread=analytics-thread&turns=analytics-turn", headers=headers)
        self.assertEqual(errors.status, 200, errors.body[:1000])
        self.assertEqual(errors.json()["turns"][0]["error"]["message"], "fixture failure")
        detail = self.backend.request("GET", root + "&view=detail&detail=turns&limit=1", headers=headers)
        self.assertEqual(detail.status, 200, detail.body[:1000])
        self.assertEqual(detail.json()["pagination"]["total"], 1)
        metadata = self.backend.request("GET", root + "&view=message-info&item=missing", headers=headers)
        self.assertEqual(metadata.status, 200, metadata.body[:1000])
        export = self.backend.request("GET", root + "&export=1", headers=headers)
        self.assertEqual(export.status, 200, export.body[:1000])
        disposition = next((value for key, value in export.headers.items()
                            if key.lower() == "content-disposition"), "")
        self.assertIn("codex-studio-analytics.json", disposition)
        self.assertEqual(json.loads(export.body)["summary"]["usageSamples"], 1)
        cost = self.backend.request("GET", "/api/session-cost?agent=analytics-contract-agent", headers=headers)
        if self.rust_backend:
            self.assertEqual(cost.status, 501, cost.body[:1000])
            self.assertIn("usage-policy/session-cost-refresh", cost.body.decode("utf-8"))
        else:
            self.assertEqual(cost.status, 200, cost.body[:1000])
            self.assertEqual(cost.json()["pricingState"], "loading")


if __name__ == "__main__":
    unittest.main()
