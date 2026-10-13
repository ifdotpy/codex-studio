#!/usr/bin/env python3
"""Tool request endpoint contracts and explicit Rust capability gaps."""

from __future__ import annotations

import os
import json
import sqlite3
import sys
import importlib.util
import unittest
from pathlib import Path
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def database_snapshot(state: Path):
    with sqlite3.connect(f"file:{state / 'canvas.sqlite3'}?mode=ro", uri=True) as database:
        tables = database.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
        ).fetchall()
        return {
            name: tuple(sorted(database.execute(f'SELECT * FROM "{name}"').fetchall(), key=repr))
            for (name,) in tables
        }


def write_receipts(backend, records=(), aliases=()):
    backend.stop()
    spec = importlib.util.spec_from_file_location(
        "tool_requests_runtime_fixture", Path(__file__).with_name("runtime-contract.py")
    )
    fixture = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(fixture)

    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass

    runtime = make_runtime(QuietRuntime, backend, fixture.FakeServer)
    saved = []
    try:
        with runtime.db() as database:
            for record in records:
                runtime.put(database, "tool_requests", record)
                saved.append(database.execute("SELECT id,record FROM runtime_tool_requests WHERE id=?", (record["id"],)).fetchone())
            for alias in aliases:
                database.execute(
                    "INSERT INTO runtime_tool_request_aliases(agent,alias,request) VALUES(?,?,?)",
                    alias,
                )
    finally:
        runtime.close()
    backend.start()
    with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
        database.executemany("UPDATE runtime_tool_requests SET record=?2 WHERE id=?1", saved)


def seed_agents(backend):
    backend.stop()
    spec = importlib.util.spec_from_file_location(
        "tool_requests_runtime_fixture", Path(__file__).with_name("runtime-contract.py")
    )
    fixture = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(fixture)

    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass

    runtime = make_runtime(QuietRuntime, backend, fixture.FakeServer)
    result = []
    try:
        for name in ("Tool request actor", "Deleted tool request actor", "Foreign actor"):
            agent = runtime.create({
                "name": name, "cwd": str(backend.cwd), "prompt": "Tool request HTTP fixture.",
                "autoWake": False,
            }, defer=True)
            agent.update(status="idle", autoWake=False)
            with runtime.db() as database:
                runtime.put(database, "agents", agent)
            result.append(agent["id"])
        deleted = runtime.agent(result[1])
        deleted["deletedAt"] = 123.0
        with runtime.db() as database:
            runtime.put(database, "agents", deleted)
    finally:
        runtime.close()
    backend.start()
    return result


def receipt(request_id, actor, *, stage="completed", outcome="applied", **extra):
    return {
        "id": request_id, "agent": actor, "stage": stage, "outcome": outcome,
        "tool": "orchestration_peers", "updated": 123.0, "signature": "secret-signature",
        "result": {"success": True, "contentItems": [{"type": "inputText", "text": "result"}]},
        **extra,
    }


class ToolRequestsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        cls.lead_id, cls.deleted_id, cls.foreign_id = seed_agents(cls.backend)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    @classmethod
    def headers(cls):
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    def test_terminal_metadata_list_reads_python_state_and_strips_payload_fields(self):
        write_receipts(self.backend, [receipt("terminal-receipt", self.lead_id)])
        before = database_snapshot(self.backend.state)
        response = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        record = next(row for row in response.json()["requests"] if row["id"] == "terminal-receipt")
        self.assertNotIn("result", record)
        self.assertNotIn("signature", record)
        self.assertNotIn("_payloadBlobs", record)
        self.assertEqual(database_snapshot(self.backend.state), before)

    def test_cancel_missing_is_unknown_and_read_only(self):
        before = database_snapshot(self.backend.state)
        response = self.backend.request(
            "POST", "/api/tool-requests/cancel",
            body={"agent": self.lead_id, "request_id": "missing-request"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["stage"], "not_found")
        self.assertEqual(response.json()["outcome"], "unknown")
        self.assertEqual(database_snapshot(self.backend.state), before)

    def test_get_terminal_receipt_and_deleted_actor_refusal(self):
        write_receipts(self.backend, [receipt("terminal-get", self.lead_id)])
        response = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id, "request_id": "terminal-get"}),
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["id"], "terminal-get")
        self.assertNotIn("signature", response.json())
        deleted = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.deleted_id}),
            headers=self.headers(),
        )
        self.assertEqual(deleted.status, 400, deleted.body[:1000])

    def test_payload_backed_pending_receipt_is_a_narrow_capability_gap_without_writes(self):
        write_receipts(self.backend, [receipt(
            "pending-payload", self.lead_id, stage="queued", outcome="pending",
            result={"success": True, "contentItems": [
                {"type": "inputText", "text": "payload-" * 20_000},
            ]},
        )])
        before = database_snapshot(self.backend.state)
        response = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id}),
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertIn("payload-store/tool-request-recovery", response.json()["error"])
        else:
            self.assertEqual(response.status, 200, response.body[:1000])
        if self.rust_backend:
            self.assertEqual(database_snapshot(self.backend.state), before)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("DELETE FROM runtime_tool_requests WHERE id='pending-payload'")

    def test_cancel_queued_receipt_is_durable_replayed_and_preserves_other_tables(self):
        write_receipts(self.backend, [receipt(
            "queued-cancel", self.lead_id, stage="queued", outcome="pending",
            cancelRequested=False, accountKey="default", threadId="thread-a", epoch=1, result=None,
        )])
        before = database_snapshot(self.backend.state)
        first = self.backend.request(
            "POST", "/api/tool-requests/cancel",
            body={"agent": self.lead_id, "request_id": "queued-cancel"},
            headers=self.headers(),
        )
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertEqual(first.json()["stage"], "cancelled")
        self.assertEqual(first.json()["outcome"], "not_applied")
        after_first = database_snapshot(self.backend.state)
        for table, rows in before.items():
            if table != "runtime_tool_requests":
                self.assertEqual(after_first[table], rows, table)
        second = self.backend.request(
            "POST", "/api/tool-requests/cancel",
            body={"agent": self.lead_id, "request_id": "queued-cancel"},
            headers=self.headers(),
        )
        self.assertEqual(second.status, 200, second.body[:1000])
        self.assertEqual(second.json()["stage"], "cancelled")
        self.assertEqual(database_snapshot(self.backend.state), after_first)

    def test_running_cancel_preserves_pending_and_foreign_actor_is_not_exposed(self):
        write_receipts(self.backend, [receipt("running-receipt", self.lead_id, stage="running", outcome="pending", result=None)])
        first=self.backend.request("POST", "/api/tool-requests/cancel", body={"agent":self.lead_id,"request_id":"running-receipt"},headers=self.headers())
        self.assertEqual(first.status,200,first.body)
        self.assertEqual((first.json()["stage"],first.json()["outcome"],first.json()["cancelRequested"]),("running","pending",True))
        before=database_snapshot(self.backend.state)
        second=self.backend.request("POST", "/api/tool-requests/cancel", body={"agent":self.lead_id,"request_id":"running-receipt"},headers=self.headers())
        self.assertEqual(second.json(),first.json())
        hidden=self.backend.request("GET","/api/tool-requests?"+urlencode({"agent":self.foreign_id,"request_id":"running-receipt"}),headers=self.headers())
        self.assertEqual(hidden.status,200,hidden.body)
        self.assertEqual(hidden.json()["stage"],"not_found")
        self.assertEqual(database_snapshot(self.backend.state),before)

    def test_saved_payload_result_uses_canonical_id_for_alias_and_cancel(self):
        from codex_payloads import externalize_result, release_db_writer_lock
        identity="saved-payload-canonical"
        write_receipts(self.backend,[receipt(identity,self.lead_id,stage="running",outcome="pending",result=None)],[(self.lead_id,"saved-payload-alias",identity)])
        database=sqlite3.connect(self.backend.state / "canvas.sqlite3")
        try:
            result=externalize_result(self.backend.state,database,{"success":True,"contentItems":[{"type":"inputText","text":"cached-"*20000}]})
            database.execute("INSERT INTO runtime_tool_results(id,result) VALUES(?,?)",(identity,json.dumps(result)))
            database.commit()
        finally:
            release_db_writer_lock(database)
            database.close()
        try:
            before=database_snapshot(self.backend.state)
            for method,key in (("GET","saved-payload-alias"),("GET",identity),("POST","saved-payload-alias")):
                body={"agent":self.lead_id,"request_id":key}
                url="/api/tool-requests/cancel" if method=="POST" else "/api/tool-requests?"+urlencode(body)
                response=self.backend.request(method,url,body=body if method=="POST" else None,headers=self.headers())
                self.assertEqual(response.status,501 if self.rust_backend else 200,response.body[:500])
                if self.rust_backend:
                    self.assertIn("payload-store/tool-request-recovery",response.json()["error"])
                    self.assertEqual(database_snapshot(self.backend.state),before)
                else:self.assertEqual(response.json()["id"],identity)
        finally:
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                db.execute("DELETE FROM runtime_tool_requests WHERE id=?",(identity,))
                db.execute("DELETE FROM runtime_tool_results WHERE id=?",(identity,))
                db.execute("DELETE FROM runtime_tool_request_aliases WHERE request=?",(identity,))

    def test_alias_ambiguity_is_preserved_for_get_and_cancel(self):
        write_receipts(
            self.backend,
            [receipt("alias-one", self.lead_id), receipt("alias-two", self.lead_id)],
            [(self.lead_id, "short-alias", "alias-one"), (self.lead_id, "short-alias", "alias-two")],
        )
        before = database_snapshot(self.backend.state)
        for method, path, body in (
            ("GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id, "request_id": "short-alias"}), None),
            ("POST", "/api/tool-requests/cancel", {"agent": self.lead_id, "request_id": "short-alias"}),
        ):
            response = self.backend.request(method, path, body=body, headers=self.headers())
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(response.json()["stage"], "ambiguous")
            self.assertEqual(response.json()["outcome"], "unknown")
        self.assertEqual(database_snapshot(self.backend.state), before)

    def test_invalid_cancel_body_is_rejected_before_dependency(self):
        response = self.backend.request(
            "POST", "/api/tool-requests/cancel", body={"agent": self.lead_id},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])

    def test_inline_completion_is_settled_before_get_list_and_cancel(self):
        records=[receipt(f"inline-{action}",self.lead_id,stage="queued",outcome="pending") for action in ("get","list","cancel")]
        write_receipts(self.backend,records)
        for action in ("get","list","cancel"):
            query={"agent":self.lead_id}
            if action!="list":query["request_id"]=f"inline-{action}"
            response=self.backend.request("POST" if action=="cancel" else "GET", "/api/tool-requests/cancel" if action=="cancel" else "/api/tool-requests?"+urlencode(query),body=query if action=="cancel" else None,headers=self.headers())
            self.assertEqual(response.status,200,response.body)
            record=next(r for r in response.json()["requests"] if r["id"]=="inline-list") if action=="list" else response.json()
            self.assertEqual((record["stage"],record["outcome"]),("completed","applied"))

    def test_expired_outputs_are_redacted_on_get_and_terminal_cancel(self):
        write_receipts(self.backend,[receipt("expired",self.lead_id,result={"success":True,"serverOutputExpiresAt":1,"contentItems":[{"type":"inputText","text":"expired output still visible"}]})])
        for method,path,body in (("GET","/api/tool-requests?"+urlencode({"agent":self.lead_id,"request_id":"expired"}),None),("POST","/api/tool-requests/cancel",{"agent":self.lead_id,"request_id":"expired"})):
            response=self.backend.request(method,path,body=body,headers=self.headers())
            self.assertEqual(response.status,200,response.body)
            self.assertNotIn(b"expired output still visible",response.body)
            self.assertTrue(json.loads(response.json()["result"]["contentItems"][0]["text"])["expired"])

    def test_payload_alias_and_canonical_gate_and_terminal_list_fast_path(self):
        large={"success":True,"contentItems":[{"type":"inputText","text":"large-"*20000}]}
        write_receipts(self.backend,[receipt("canonical-payload",self.lead_id,result=large)],[(self.lead_id,"payload-alias","canonical-payload")])
        before=database_snapshot(self.backend.state)
        for identity in ("canonical-payload","payload-alias"):
            response=self.backend.request("GET","/api/tool-requests?"+urlencode({"agent":self.lead_id,"request_id":identity}),headers=self.headers())
            self.assertEqual(response.status,501 if self.rust_backend else 200,response.body)
            if self.rust_backend:self.assertIn("payload-store/tool-request-recovery",response.json()["error"])
        response=self.backend.request("GET","/api/tool-requests?"+urlencode({"agent":self.lead_id,"request_id":""}),headers=self.headers())
        self.assertEqual(response.status,200,response.body)
        self.assertIn("requests",response.json())
        self.assertEqual(database_snapshot(self.backend.state),before)

    def test_operation_evidence_and_child_observations(self):
        write_receipts(self.backend,[receipt("operation-evidence",self.lead_id,stage="running",outcome="pending",result=None,agentIds=[self.foreign_id,"absent-child"])])
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?,?,?)",("operation-evidence","sig",json.dumps({"accepted":True})))
            raw=db.execute("SELECT record FROM runtime_agents WHERE id=?",(self.foreign_id,)).fetchone()[0]
            child=json.loads(raw);child["parentId"]=self.lead_id
            db.execute("UPDATE runtime_agents SET record=? WHERE id=?",(json.dumps(child),self.foreign_id))
        response=self.backend.request("GET","/api/tool-requests?"+urlencode({"agent":self.lead_id,"request_id":"operation-evidence"}),headers=self.headers())
        self.assertEqual(response.status,200,response.body)
        value=response.json()
        self.assertEqual(value["operationResult"],{"accepted":True})
        self.assertTrue(value["operationApplied"])
        self.assertEqual(value["evidenceSource"],"operation_receipt")
        self.assertEqual(value["agents"][0],{"id":self.foreign_id,"name":"Foreign actor","status":"idle","turnId":None,"model":child.get("model"),"deleted":False})
        self.assertEqual(value["agents"][1],{"id":"absent-child","absent":True})
        self.assertGreater(value["registryObservedAt"],0)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            db.execute("UPDATE runtime_agents SET record=? WHERE id=?",(raw,self.foreign_id))

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"),"strict stored-state refusal")
    def test_invalid_stored_request_refuses_without_writes_before_commit(self):
        write_receipts(self.backend,[receipt("invalid-cancel",self.lead_id,stage="queued",outcome="pending",result=None)])
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            original=db.execute("SELECT record FROM runtime_tool_requests WHERE id='invalid-cancel'").fetchone()[0]
        for patch in json.loads(Path(__file__).with_name("fixtures").joinpath("system-tools-inputs.json").read_text())["invalid_stored_receipts"] + [{"id":"wrong-row"}]:
            record=json.loads(original);record.update(patch)
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:db.execute("UPDATE runtime_tool_requests SET record=? WHERE id='invalid-cancel'",(json.dumps(record),))
            before=database_snapshot(self.backend.state)
            response=self.backend.request("POST","/api/tool-requests/cancel",body={"agent":self.lead_id,"request_id":"invalid-cancel"},headers=self.headers())
            self.assertEqual(response.status,500,response.body)
            self.assertIn("runtime_tool_requests/invalid-cancel",response.json()["error"])
            self.assertEqual(database_snapshot(self.backend.state),before)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:db.execute("DELETE FROM runtime_tool_requests WHERE id='invalid-cancel'")

    def test_cancel_reconciles_execution_effect_and_replay_has_identical_write_set(self):
        write_receipts(self.backend,[receipt("effect-cancel",self.lead_id,stage="queued",outcome="pending",result=None)])
        from codex_execution import _new_run, _save_run, effect
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            actor=json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?",(self.lead_id,)).fetchone()[0])
            run=_new_run(actor,"effect-run");_save_run(db,run)
            effect(db,run,"tool_requests","effect-cancel",status="queued",outcome="pending",resultReference=None,requestId=None)
        response=self.backend.request("POST","/api/tool-requests/cancel",body={"agent":self.lead_id,"request_id":"effect-cancel"},headers=self.headers())
        self.assertEqual(response.status,200,response.body)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            effect_row=json.loads(db.execute("SELECT record FROM runtime_execution_effects WHERE id='tool_requests:effect-cancel'").fetchone()[0])
        self.assertEqual(effect_row,{"id":"tool_requests:effect-cancel","runId":run["id"],"kind":"tool_requests","referenceId":"effect-cancel","attemptId":None,"requestId":None,"status":"cancelled","outcome":"not_applied","resultReference":"effect-cancel"})
        before=database_snapshot(self.backend.state)
        replay=self.backend.request("POST","/api/tool-requests/cancel",body={"agent":self.lead_id,"request_id":"effect-cancel"},headers=self.headers())
        self.assertEqual(replay.json(),response.json())
        self.assertEqual(database_snapshot(self.backend.state),before)


    def test_cancel_schema_and_lifecycle_corpus(self):
        from codex_tool_requests import _lifecycle, _apply_decision
        corpus=json.loads(Path(__file__).with_name("fixtures").joinpath("system-tools-inputs.json").read_text())
        before=database_snapshot(self.backend.state)
        for template in corpus["invalid_cancel_bodies"]:
            body={k:self.lead_id if v=="ACTOR" else v for k,v in template.items()}
            response=self.backend.request("POST","/api/tool-requests/cancel",body=body,headers=self.headers())
            self.assertEqual(response.status,400,response.body)
        self.assertEqual(database_snapshot(self.backend.state),before)
        rows=[receipt(f"corpus-{i}",self.lead_id,**patch) for i,patch in enumerate(corpus["valid_cancel_patches"])]
        write_receipts(self.backend,rows)
        for record in rows:
            expected=record.copy()
            if _apply_decision(expected,_lifecycle(expected,"Cancel")):
                expected["cancelRequested"]=True
            response=self.backend.request("POST","/api/tool-requests/cancel",body={"agent":self.lead_id,"request_id":record["id"]},headers=self.headers())
            self.assertEqual(response.status,200,response.body)
            actual=response.json()
            for key in ("stage","outcome","cancelRequested"):
                self.assertEqual(actual.get(key),expected.get(key),key)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"),"named dependency and stored-state refusal")
    def test_move_dependencies_and_invalid_actors_do_not_write(self):
        write_receipts(self.backend,[receipt("move-receipt",self.lead_id,tool="orchestration_move")])
        path=self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(path) as db:
            original=db.execute("SELECT record FROM runtime_agents WHERE id=?",(self.lead_id,)).fetchone()[0]
        cases=[(original,"move-receipt",501,"multi-server/move-receipt"),
               (json.dumps({**json.loads(original),"movedFrom":{"serverId":"remote"}}),"missing",501,"multi-server/move-receipt"),
               ("[]","missing",500,"runtime_agents/"+self.lead_id),
               (json.dumps({**json.loads(original),"id":"mismatched"}),"missing",500,"runtime_agents/"+self.lead_id)]
        try:
            for raw,identity,status,error in cases:
                with sqlite3.connect(path) as db:db.execute("UPDATE runtime_agents SET record=? WHERE id=?",(raw,self.lead_id))
                before=database_snapshot(self.backend.state)
                response=self.backend.request("GET","/api/tool-requests?"+urlencode({"agent":self.lead_id,"request_id":identity}),headers=self.headers())
                self.assertEqual(response.status,status,response.body)
                self.assertIn(error,response.json()["error"])
                self.assertEqual(database_snapshot(self.backend.state),before)
        finally:
            with sqlite3.connect(path) as db:
                db.execute("UPDATE runtime_agents SET record=? WHERE id=?",(original,self.lead_id))
                db.execute("DELETE FROM runtime_tool_requests WHERE id='move-receipt'")

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"),"Rust read-only database checker")
    def test_state_check_accepts_python_receipts_and_detects_corruption_readonly(self):
        import shlex
        import subprocess
        write_receipts(self.backend,[receipt("state-check",self.lead_id)])
        before=database_snapshot(self.backend.state)
        command=shlex.split(os.environ["STUDIO_HTTP_BACKEND_COMMAND"])+["state-check",str(self.backend.state)]
        result=subprocess.run(command,capture_output=True,text=True,check=True)
        report=json.loads(result.stdout)
        tables={r["table"]:r for r in report["tables"]}
        for name in ("runtime_tool_requests","runtime_tool_results","runtime_operation_receipts","runtime_execution_runs","runtime_execution_effects"):
            self.assertIn(name,tables)
            self.assertEqual(tables[name]["rejected_count"],0,tables[name])
        self.assertEqual(database_snapshot(self.backend.state),before)


if __name__ == "__main__":
    unittest.main()
