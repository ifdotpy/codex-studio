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

from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot


def seed_analytics(backend) -> None:
    source = Path(__file__).resolve().parents[1] / "src"
    script = f"""
import json, os, sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime
from codex_session_names import identity as native_name_identity
runtime = Runtime(Path(os.environ['CODEX_AGENTS_STATE_DIR']))
try:
    agent = {{'id':'analytics-contract-agent','rootId':'analytics-contract-agent','epoch':0,
        'status':'idle','name':'Analytics contract','role':'orchestrator','cwd':os.environ['CODEX_CANVAS_CWD'],
        'isLead':True,'autoWake':True,'approvalPolicy':'never','threadId':'analytics-thread',
        'turnId':'analytics-turn','accountKey':'default','accountHistory':[],'model':'fixture-model'}}
    teammate = {{**agent,'id':'analytics-contract-teammate','parentId':agent['id'],
        'rootId':agent['id'],'isLead':False,'name':'Analytics teammate'}}
    # Idle agents with threads still participate in background name sync.
    for stored_agent in (agent, teammate):
        stored_agent['nativeNameSynced'] = native_name_identity(stored_agent)
    with runtime.lock, runtime.db(analytics=True) as db:
        runtime.put(db, 'agents', agent, sync_rooms=False)
        runtime.analytics_agent(db, agent)
        runtime.put(db, 'agents', teammate, sync_rooms=False)
        runtime.analytics_agent(db, teammate)
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
        provisional = {{**usage,'id':'analytics-provisional','at':99,'recordedAt':99}}
        provisional.pop('responseId', None)
        db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
            ('analytics-provisional','analytics-contract-agent','analytics-contract-agent',
             'analytics-thread','analytics-turn',99,json.dumps(provisional)))
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
        second_item = {{**item,'id':'analytics-call-second','itemId':'analytics-call-second','at':103}}
        db.execute('INSERT INTO analytics_items(id,agent,root,thread,turn,at,type,name,is_tool,record) VALUES (?,?,?,?,?,?,?,?,?,?)',
            ('analytics-call-second','analytics-contract-agent','analytics-contract-agent','analytics-thread',
             'analytics-turn',103,'commandExecution','shell',1,json.dumps(second_item)))
        message = {{**agent,'id':'analytics-message','agentId':agent['id'],'itemId':'analytics-response',
            'at':104,'recordedAt':104,'startedAt':104,'finishedAt':104,'turnId':'analytics-turn','threadId':'analytics-thread',
            'source':'rollout','timestampSource':'record','type':'agentMessage','name':'','isTool':False,
            'status':'completed','payloadBoundary':'protocol'}}
        db.execute('INSERT INTO analytics_items(id,agent,root,thread,turn,at,type,name,is_tool,record) VALUES (?,?,?,?,?,?,?,?,?,?)',
            ('analytics-message','analytics-contract-agent','analytics-contract-agent','analytics-thread',
             'analytics-turn',104,'agentMessage','',0,json.dumps(message)))
    # Independent population: bounded pages, null measurements, odd/even duration
    # samples, both tool boundaries, compaction precedence and hour overlap.
    population = {{**agent, 'id':'analytics-population','rootId':'analytics-population',
                  'accountKey':'population-account','threadId':None}}
    with runtime.lock, runtime.db(analytics=True) as db:
        runtime.put(db, 'agents', population, sync_rooms=False)
        runtime.analytics_agent(db, population)
        for index in range(106):
            row = {{**usage, 'id':f'population-usage-{{index:03}}',
                   'agentId':population['id'],'rootId':population['id'],
                   'at':index + 1000, 'responseId':f'population-usage-{{index:03}}',
                   'accountKey':'population-account'}}
            if index < 105:
                row.pop('responseId')
            db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                       (row['id'],population['id'],population['id'],'analytics-thread','analytics-turn',row['at'],json.dumps(row)))
        for index in range(510):
            row = {{**usage,'id':f'chart-{{index:03}}','agentId':population['id'],
                   'rootId':population['id'],'responseId':f'chart-{{index:03}}',
                   'at':index*3600,'last':{{'totalTokens':index+1}},
                   'accountKey':'population-account'}}
            db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                       (row['id'],population['id'],population['id'],'analytics-thread','analytics-turn',row['at'],json.dumps(row)))
        for index, duration in enumerate([None, 0, 10, 20, 30, 40]):
            model = index < 4
            row = {{**item,'id':f'population-item-{{index}}','itemId':f'population-item-{{index}}',
                   'agentId':population['id'],'rootId':population['id'],'at':1100 + index,
                   'type':'modelToolCall' if model else 'commandExecution',
                   'name':'model-tool' if model else 'shell',
                   'payloadBoundary':'model' if model else 'protocol',
                   'status':'failed' if index == 2 else 'completed','durationMs':duration}}
            if index == 0:
                row['input'] = None
                row['output'] = None
            runtime.analytics_store_item(db, row)
        for index, kind in enumerate(['contextCompaction','compactionSnapshot','compactionSnapshot']):
            row = {{**message,'id':f'population-compaction-{{index}}', 'itemId':f'population-compaction-{{index}}',
                   'agentId':population['id'],'rootId':population['id'],'type':kind,'at':1200 + index,
                   'turnId':'analytics-turn' if index < 2 else 'snapshot-turn'}}
            runtime.analytics_store_item(db, row)
        for key, count in [('fallback',1),('ambiguous',2)]:
            row = {{**usage,'id':f'population-{{key}}-response','agentId':population['id'],
                   'rootId':population['id'],'responseId':f'population-{{key}}-response','at':1300,'turnId':key}}
            db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                (row['id'],population['id'],population['id'],'analytics-thread',key,1300,json.dumps(row)))
            turn = {{**error,'id':f"{{population['id']}}:analytics-thread:{{key}}",'agentId':population['id'],
                    'rootId':population['id'],'turnId':key,'status':'completed'}}
            db.execute('INSERT INTO analytics_turns(id,agent,root,at,record) VALUES (?,?,?,?,?)',
                       (turn['id'],population['id'],population['id'],1300,json.dumps(turn)))
            for index in range(count):
                row = {{**message,'id':f'population-{{key}}-message-{{index}}','itemId':f'{{key}}-message-{{index}}',
                       'agentId':population['id'],'rootId':population['id'],'turnId':key,'at':1300}}
                runtime.analytics_store_item(db,row)
        for index in range(3):
            db.execute('INSERT INTO analytics_limits(account,at,record) VALUES (?,?,?)',
                       ('population-account',1100 + index,json.dumps({{'fixture':index}})))
            db.execute('INSERT INTO analytics_notifications VALUES (?,?,?,?,?,?,?)',
                       (f'population-notification-{{index}}',population['id'],population['id'],'fixture',index*3600,index+1,index*10))
    with runtime.lock, runtime.db(analytics=True) as db:
        runtime.analytics_history_init(db)
        runtime._analytics_history_record(db,'population-history',population['id'],
            {{'status':'error','agent':population['id'],'error':'fixture error'}})
        monitor = {{'id':'population-monitor','agent':population['id'],'status':'completed',
                    'created':1101,'finished':1102,'bytes':12,'exitCode':0,'error':None,'timeout_ms':3600000}}
        runtime.put(db, 'monitors', monitor)
        runtime._monitor_exit_event(db, population, monitor, wake=False)
        runtime.put(db, 'requests', {{'id':'population-request','agent':population['id'],'status':'approved'}})
    cases = [{{}}, {{'tool':'model-tool'}}, {{'tool':'missing'}}, {{'from':1100,'to':1102}},
             {{'from':99,'to':99}}, {{'scope':'team'}}, {{'limit':1,'offset':1}},
             {{'view':'detail','detail':'rateLimits','limit':1,'offset':1}},
             {{'view':'detail','detail':'turns','limit':1}}]
    oracle = []
    for options in cases:
        value = runtime.analytics(population['id'], **options)
        if options.get('view') != 'detail':
            value = {{key:value[key] for key in ('summary','tools','notifications','rateLimits',
                'timeline','chartBuckets','timelineTotal','provisionalUsage','calls','itemRecords','itemRecordsTotal',
                'compactions','compactionSnapshots','operations','history','pagination','detailPagination')}}
        oracle.append({{'options':options,'value':value}})
    (Path(os.environ['CODEX_CANVAS_CWD']) / 'analytics-oracle.json').write_text(json.dumps(oracle))
    assert runtime.analytics('analytics-contract-agent')['summary']['usageSamples'] == 1
    from codex_session_costs import SessionCostReader
    from codex_cost_usage import source_state as cost_source_state
    class NoPricing:
        def snapshot(self): return None
    state = Path(os.environ['CODEX_AGENTS_STATE_DIR'])
    reader = SessionCostReader(state / 'analytics.sqlite3', NoPricing(), state_root=state)
    db = reader._connect()
    try:
        root = 'analytics-contract-agent'
        agents = reader._root_agents(db, root, root)
        source = {{'usage':reader._usage_state(db,root),
            'pricingSignature':reader._pricing_snapshot()[1],
            'agentsSignature':reader._agent_signature(agents),
            'claudeSignature':reader._claude_signature(reader._claude_agents(agents)),
            'costProjection':cost_source_state(db,root)}}
    finally:
        db.close()
    cached = {{'rootId':root,'pricingState':'ready','unknownModels':[],'breakdown':{{'providers':{{}},'models':{{}}}},'totalUSD':1,'pricedSamples':1,'estimated':True}}
    reader._persist(root,cached,source, __import__('time').time()-30)
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

    def test_differential_populations_and_pages(self) -> None:
        from urllib.parse import urlencode
        cases = json.loads((self.backend.cwd / "analytics-oracle.json").read_text())
        for case in cases:
            options = {"agent": "analytics-population", **case["options"]}
            for old, new in (("from", "from_time"), ("to", "to_time")):
                if old in options:
                    options[new] = options.pop(old)
            with self.subTest(options=options):
                response = self.backend.request("GET", "/api/analytics?" + urlencode(options), headers=self._headers())
                self.assertEqual(response.status, 200, response.body[:1000])
                value = response.json()
                for key, expected in case["value"].items():
                    self.assertEqual(value[key], expected, key)

    def test_single_response_attribution_and_ambiguous_message(self) -> None:
        for item, attributed in (("fallback-message-0", True), ("ambiguous-message-0", False)):
            response = self.backend.request("GET", "/api/analytics?agent=analytics-population&view=message-info&item=" + item,
                                            headers=self._headers())
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(response.json().get("tokens") is not None, attributed)
            if attributed:
                self.assertEqual(response.json()["tokens"]["totalTokens"], 120)

        errors = self.backend.request("GET", "/api/analytics?agent=analytics-population&view=turn-errors&thread=analytics-thread&turns=fallback,ambiguous", headers=self._headers())
        self.assertEqual(errors.status, 200, errors.body[:1000])
        self.assertEqual([row["turnId"] for row in errors.json()["turns"]], ["ambiguous", "fallback"])

    def test_refusals_preserve_analytics_rows(self) -> None:
        import sqlite3
        before = no_write_snapshot(self.backend.state)
        for query in ("from_time=200&to_time=100", "view=message-info&scope=team&item=analytics-response"):
            response = self.backend.request("GET", "/api/analytics?agent=analytics-contract-agent&" + query,
                                            headers=self._headers())
            self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

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
        self.assertEqual(value["pagination"]["total"], 2)
        self.assertEqual(self.backend.request("GET", root + "&tool=missing", headers=headers).json()["pagination"]["total"], 0)
        all_scope = self.backend.request("GET", "/api/analytics?scope=all", headers=headers)
        self.assertEqual(all_scope.status, 200, all_scope.body[:1000])
        self.assertEqual(all_scope.json()["pagination"]["total"], 8)
        team_scope = self.backend.request("GET", root + "&scope=team", headers=headers)
        self.assertEqual(team_scope.status, 200, team_scope.body[:1000])
        self.assertEqual(len(team_scope.json()["agents"]), 2)
        self.assertEqual(team_scope.json()["summary"]["agents"], 1)
        date_filtered = self.backend.request("GET", root + "&from_time=99&to_time=99", headers=headers)
        self.assertEqual(date_filtered.status, 200, date_filtered.body[:1000])
        self.assertEqual(date_filtered.json()["summary"]["usageSamples"], 0)
        self.assertEqual(date_filtered.json()["summary"]["provisionalUsageSamples"], 1)
        boundary = self.backend.request("GET", root + "&from_time=100&to_time=100", headers=headers)
        self.assertEqual(boundary.status, 200, boundary.body[:1000])
        self.assertEqual(boundary.json()["summary"]["usageSamples"], 1)
        filtered_tools = self.backend.request("GET", root + "&tool=missing", headers=headers).json()
        self.assertEqual(filtered_tools["summary"]["observedToolRows"], 0)
        self.assertEqual(self.backend.request("GET", root + "&tool=", headers=headers).json()["pagination"]["total"], 2)
        capped = self.backend.request("GET", root + "&limit=1", headers=headers).json()
        self.assertEqual((len(capped["calls"]), capped["pagination"]["total"]), (1, 2))

        errors = self.backend.request("GET", root + "&view=turn-errors&thread=analytics-thread&turns=analytics-turn", headers=headers)
        self.assertEqual(errors.status, 200, errors.body[:1000])
        self.assertEqual(errors.json()["turns"][0]["error"]["message"], "fixture failure")
        detail = self.backend.request("GET", root + "&view=detail&detail=turns&limit=1", headers=headers)
        self.assertEqual(detail.status, 200, detail.body[:1000])
        self.assertEqual(detail.json()["pagination"]["total"], 1)
        metadata = self.backend.request("GET", root + "&view=message-info&item=missing", headers=headers)
        self.assertEqual(metadata.status, 200, metadata.body[:1000])
        attributed = self.backend.request("GET", root + "&view=message-info&item=analytics-response", headers=headers)
        self.assertEqual(attributed.status, 200, attributed.body[:1000])
        self.assertEqual(attributed.json()["tokens"]["totalTokens"], 120)
        export = self.backend.request("GET", root + "&export=1", headers=headers)
        self.assertEqual(export.status, 200, export.body[:1000])
        disposition = next((value for key, value in export.headers.items()
                            if key.lower() == "content-disposition"), "")
        self.assertIn("codex-studio-analytics.json", disposition)
        self.assertEqual(json.loads(export.body)["summary"]["usageSamples"], 1)
        cost = self.backend.request("GET", "/api/session-cost?agent=analytics-contract-agent", headers=headers)
        self.assertEqual(cost.status, 200, cost.body[:1000])
        if self.rust_backend:
            self.assertEqual(cost.json()["pricingState"], "ready")
        else:
            self.assertEqual(cost.json()["pricingState"], "loading")
        cache_file = next((self.backend.state / "session-costs").glob("cost-*.json"))
        saved = json.loads(cache_file.read_text())
        if self.rust_backend:
            incomplete = json.loads(json.dumps(saved))
            incomplete["source"].pop("pricingSignature")
            cache_file.write_text(json.dumps(incomplete))
            before_refusal = no_write_snapshot(self.backend.state)
            refused = self.backend.request("GET", "/api/session-cost?agent=analytics-contract-agent", headers=headers)
            self.assertEqual(refused.status, 501, refused.body[:1000])
            self.assertEqual(no_write_snapshot(self.backend.state), before_refusal)
        saved["source"]["usage"]["generation"] += 1
        cache_file.write_text(json.dumps(saved))
        stale = self.backend.request("GET", "/api/session-cost?agent=analytics-contract-agent", headers=headers)
        if self.rust_backend:
            self.assertEqual(stale.status, 501, stale.body[:1000])
            self.assertIn("costs/pricing-refresh", stale.body.decode("utf-8"))
            pricing = self.backend.state / "model-pricing" / "models-dev-v1.json"
            pricing.parent.mkdir(parents=True, exist_ok=True)
            pricing.write_text(json.dumps({
                "version": 1,
                "catalog": {"providers": {
                    "openai": {"models": {"gpt-test": {"cost": {"input": 1, "output": 1}}}},
                    "anthropic": {"models": {"claude-sonnet-5-5": {"cost": {"input": 1, "output": 1}}}},
                }},
            }))
            computed = self.backend.request("GET", "/api/session-cost?agent=analytics-contract-agent", headers=headers)
            self.assertEqual(computed.status, 200, computed.body[:1000])
            self.assertEqual(computed.json()["pricingState"], "ready")
            self.assertIsNone(computed.json()["totalUSD"])
            self.assertEqual(computed.json()["unknownModels"], ["fixture-model"])
        else:
            self.assertEqual(stale.status, 200, stale.body[:1000])


if __name__ == "__main__":
    unittest.main()
