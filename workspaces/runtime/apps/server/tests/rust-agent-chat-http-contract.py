#!/usr/bin/env python3
"""Canonical room views, receipts and atomic visibility on both HTTP backends."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import importlib.util
import json
import os
import sqlite3
import sys
import unittest
from pathlib import Path
from urllib.parse import urlencode

isolate_supervisor_environment()
sys.path.insert(0,str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime
from http_backend_harness import TimedHTTPContractTest, start_contract_backend
from studio_api.work.models import ChatRead

spec=importlib.util.spec_from_file_location("chat_fixture",Path(__file__).with_name("runtime-contract.py"))
fixture=importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(fixture)


def snapshot(path):
    with sqlite3.connect(path) as db:
        return {name:sorted(db.execute(f'SELECT * FROM "{name}"').fetchall(),key=repr) for (name,) in db.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")}


def seed(backend):
    backend.stop()
    class QuietRuntime(fixture.Runtime):
        def schedule(self):
            pass
    runtime=make_runtime(QuietRuntime,backend,fixture.FakeServer)
    try:
        agents=[]
        for name in ("Lead","Worker","Peer"):
            agent=runtime.create({"name":name,"cwd":str(backend.cwd),"prompt":"Stored chat fixture","autoWake":False},defer=True)
            agent.update(status="idle",autoWake=False)
            agents.append(agent)
        lead,worker,peer=agents
        worker.update(rootId=lead["id"],parentId=lead["id"],isLead=False)
        with runtime.db() as db:
            for agent in agents:runtime.put(db,"agents",agent)
            project={"id":str(backend.cwd),"path":str(backend.cwd),"name":"Project","peerTeams":[{"id":"peers","name":"Current peers","members":[lead["id"],peer["id"]]}]}
            runtime.put(db,"projects",project)
            from codex_federation import FederationService
            FederationService.ensure_tables(db)
            federation={"id":"federated","status":"approved","localApproved":True,"remoteApproved":True,"localMembers":[worker["id"],lead["id"],lead["id"]],"remoteMembers":[{"id":"other","name":"Remote"}],"localParticipants":[],"peerId":"peer","peerLabel":"Peer server","name":"Federated room"}
            db.execute("INSERT INTO runtime_federation_rooms(id,record) VALUES(?,?)",("federated",json.dumps(federation)))
            rooms=[
                {"id":"ordered","kind":"private","members":[lead["id"],worker["id"]],"customName":""},
                {"id":"broadcast","kind":"broadcast","rootId":lead["id"]},
                {"id":"peer-room","kind":"private","members":[lead["id"],peer["id"]],"peerTeamId":"stale","peerTeamName":"Stale"},
                {"id":"federated","kind":"federated","members":[worker["id"],lead["id"]]},
                {"id":"unavailable","kind":"private","members":["deleted-member"]},
                {"id":"empty","kind":"private","members":[]},
                {"id":"large","kind":"private","members":[lead["id"]]},
            ]
            for room in rooms:
                room["updated"]=123.0
                runtime.put(db,"rooms",room)
            for room in rooms:
                if room["id"] in {"unavailable","empty"}:continue
                for index in range(4):
                    sender="remote:peer:other" if room["id"]=="federated" else worker["id"]
                    text="x"*600000 if room["id"]=="large" else f"message-{index}"
                    db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",(f"{room['id']}-{index}",room["id"],sender,text,123.0+index,json.dumps({lead["id"]:"queued"})))
            runtime.enqueue(db,lead,"chat","delivery",f"chat:ordered-3:{lead['id']}")
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?",(f"chat:ordered-3:{lead['id']}",))
            for key,kind,owner in (("é"*150,"user",lead),("merged","user",lead),("not-user","chat",lead),("foreign","user",peer)):
                runtime.enqueue(db,owner,kind,"input",key)
                db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?",(key,))
            runtime.item(db,lead["id"],"é"*150,"user","input")
            runtime.item(db,lead["id"],"merged-item","user","merged",inputs=[{"id":"merged","kind":"user","text":"merged"}])
            db.execute("INSERT OR REPLACE INTO runtime_event_meta(id,record) VALUES(?,?)",("merged",json.dumps({"transcriptItemId":lead["id"]+":merged-item"})))
        expected={}
        for room in ("ordered","broadcast","peer-room","federated","large","feed:"+lead["id"]):
            for query in ({"limit":2},{"limit":2,"after":1},{"limit":2,"before":999999}):
                value=runtime.chat_read(room,**query)
                value["room"].pop("federation",None)  # Python leaks an undeclared detail field.
                expected[(room,tuple(query.items()))]=ChatRead.model_validate(value).model_dump(mode="json",by_alias=True,exclude_unset=True)
        receipts=runtime.user_delivery_receipts(lead["id"],["é"*150,"merged","not-user","foreign"])
        return [a["id"] for a in agents],expected,receipts
    finally:
        runtime.close()
        backend.start()


class AgentChatHTTPContract(TimedHTTPContractTest,unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.rust_backend=bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend=start_contract_backend(cls)
        cls.agents,cls.expected,cls.receipts=seed(cls.backend)

    @classmethod
    def tearDownClass(cls):
        cls.backend.stop()

    def headers(self):
        return {"Origin":self.backend.origin or "","X-Canvas-Token":self.backend.session["token"],"X-Studio-API-Schema":self.backend.schema_hash}

    def test_complete_room_pages_match_python_names_order_peers_federation_and_cutoff(self):
        before=snapshot(self.backend.state / "canvas.sqlite3")
        for (room,query),expected in self.expected.items():
            with self.subTest(room=room,query=query):
                response=self.backend.request("GET","/api/agent-chat?"+urlencode({"room":room,**dict(query)}),headers=self.headers())
                self.assertEqual(response.status,200,response.body[:1000])
                actual=response.json()
                if room=="federated" and not self.rust_backend:
                    # The Python handler emits this undeclared legacy alias.
                    self.assertTrue(actual["room"].pop("federation"))
                self.assertEqual(actual,expected)
        self.assertEqual(snapshot(self.backend.state / "canvas.sqlite3"),before)

    def test_receipts_unicode_merged_inputs_and_foreign_nonuser_events_match_python(self):
        response=self.backend.request("GET","/api/messages/receipts?"+urlencode({"agent":self.agents[0],"ids":json.dumps(["é"*150,"merged","not-user","foreign"])}),headers=self.headers())
        self.assertEqual(response.status,200,response.body)
        actual=response.json();actual.pop("workspaceId",None)
        self.assertEqual(actual,self.receipts)
        self.assertTrue(all(item["materialized"] for item in actual["items"]))

    def test_hide_unavailable_empty_and_visible_rooms_has_canonical_projection_and_exact_replay(self):
        from studio_api.sync.models import RoomEntityDto
        for room in ("ordered","unavailable","empty"):
            response=self.backend.request("POST","/api/room/delete",body={"id":room},headers=self.headers())
            self.assertEqual(response.status,200,response.body)
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                published=db.execute("SELECT payload,deleted FROM sync_entities WHERE collection='room' AND id=?",(room,)).fetchone()
                payload,deleted=published if published else (None,True)
                stored=json.loads(db.execute("SELECT record FROM runtime_rooms WHERE id=?",(room,)).fetchone()[0])
            self.assertTrue(stored["userHidden"])
            if room=="ordered":
                expected=self.expected[(room,(("limit",2),))]["room"].copy()
                expected.update(userHidden=True,lastMessage={"seq":self.expected[(room,(("limit",2),))]["messages"][-1]["seq"],"text":"message-3","created":126.0,"sender":self.agents[1]})
                expected=RoomEntityDto.model_validate({k:v for k,v in expected.items() if k in RoomEntityDto.model_fields}).model_dump(exclude_unset=True)
                self.assertFalse(deleted)
                self.assertEqual(json.loads(payload)["value"],expected)
            else:self.assertTrue(deleted)
            before=snapshot(self.backend.state / "canvas.sqlite3")
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as observer:
                version=observer.execute("PRAGMA data_version").fetchone()[0]
                replay=self.backend.request("POST","/api/room/delete",body={"id":room},headers=self.headers())
                if self.rust_backend:
                    self.assertEqual(observer.execute("PRAGMA data_version").fetchone()[0],version)
            self.assertEqual(replay.status,200,replay.body)
            self.assertEqual(snapshot(self.backend.state / "canvas.sqlite3"),before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"),"strict stored-state refusal")
    def test_corrupt_delivery_and_room_fail_with_identity_without_writes(self):
        for table,key,column,value,url in (("runtime_chat_messages","ordered-3","deliveries",json.dumps({"recipient":4}),"/api/agent-chat?room=ordered"),("runtime_rooms","ordered","record","[]","/api/agent-chat?room=ordered"),("runtime_events","merged","status","invalid","/api/messages/receipts?"+urlencode({"agent":self.agents[0],"ids":json.dumps(["merged"])}))):
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                from codex_sync_entities import register_functions
                register_functions(db)
                original=db.execute(f"SELECT {column} FROM {table} WHERE id=?",(key,)).fetchone()[0]
                db.execute(f"UPDATE {table} SET {column}=? WHERE id=?",(value,key))
            before=snapshot(self.backend.state / "canvas.sqlite3")
            response=self.backend.request("GET",url,headers=self.headers())
            self.assertEqual(response.status,500,response.body)
            self.assertIn(f"{table}/{key}",response.json()["error"])
            self.assertEqual(snapshot(self.backend.state / "canvas.sqlite3"),before)
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                register_functions(db)
                db.execute(f"UPDATE {table} SET {column}=? WHERE id=?",(original,key))


    def test_unnamed_remote_participants_keep_expanded_id_sender_name(self):
        path=self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(path) as db:
            original=db.execute("SELECT record FROM runtime_federation_rooms WHERE id='federated'").fetchone()[0]
        try:
            for person in ({"id":"other"},{"id":"other","name":""}):
                record=json.loads(original); record["remoteMembers"]=[person]
                with sqlite3.connect(path) as db:
                    db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id='federated'",(json.dumps(record),))
                response=self.backend.request("GET","/api/agent-chat?room=federated",headers=self.headers())
                self.assertEqual(response.status,200,response.body)
                self.assertTrue(response.json()["messages"])
                self.assertTrue(all(m["senderName"]=="remote:peer:other" for m in response.json()["messages"]))
        finally:
            with sqlite3.connect(path) as db:db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id='federated'",(original,))

    def test_peer_assignment_is_removed_and_revoked_federation_is_tombstoned(self):
        path=self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(path) as db:
            project_id,raw=db.execute("SELECT id,record FROM runtime_projects WHERE json_extract(record,'$.peerTeams[0].id')='peers'").fetchone()
            project=json.loads(raw);project["peerTeams"]=[]
            db.execute("UPDATE runtime_projects SET record=? WHERE id=?",(json.dumps(project),project_id))
        try:
            response=self.backend.request("GET","/api/agent-chat?room=peer-room",headers=self.headers())
            self.assertEqual(response.status,200,response.body)
            self.assertNotIn("peerTeamId",response.json()["room"])
            self.assertNotIn("peerTeamName",response.json()["room"])
        finally:
            with sqlite3.connect(path) as db:db.execute("UPDATE runtime_projects SET record=? WHERE id=?",(raw,project_id))
        # First publish the approved room; then revocation must retire that view.
        response=self.backend.request("POST","/api/room/delete",body={"id":"federated"},headers=self.headers())
        self.assertEqual(response.status,200,response.body)
        with sqlite3.connect(path) as db:
            original=db.execute("SELECT record FROM runtime_federation_rooms WHERE id='federated'").fetchone()[0]
            revoked=json.loads(original);revoked["localApproved"]=False
            db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id='federated'",(json.dumps(revoked),))
        try:
            response=self.backend.request("POST","/api/room/delete",body={"id":"federated"},headers=self.headers())
            self.assertEqual(response.status,200,response.body)
            with sqlite3.connect(path) as db:
                self.assertEqual(db.execute("SELECT deleted FROM sync_entities WHERE collection='room' AND id='federated'").fetchone()[0],1)
            before=snapshot(path)
            response=self.backend.request("POST","/api/room/delete",body={"id":"federated"},headers=self.headers())
            self.assertEqual(response.status,200,response.body)
            self.assertEqual(snapshot(path),before)
        finally:
            with sqlite3.connect(path) as db:db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id='federated'",(original,))

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"),"strict stored-state and projection refusal")
    def test_hide_response_validation_and_receipt_metadata_errors_do_not_write(self):
        path=self.backend.state / "canvas.sqlite3"
        cases=[("runtime_rooms","ordered",{"kind":"future-kind"},"POST","/api/room/delete",{"id":"ordered"}),
               ("runtime_event_meta","merged",{"transcriptItemId":4},"GET","/api/messages/receipts?"+urlencode({"agent":self.agents[0],"ids":json.dumps(["merged"])}),None),
               ("runtime_agents",self.agents[0],None,"GET","/api/messages/receipts?"+urlencode({"agent":self.agents[0],"ids":json.dumps(["merged"])}),None)]
        for table,key,patch,method,url,body in cases:
            with sqlite3.connect(path) as db:
                original=db.execute(f"SELECT record FROM {table} WHERE id=?",(key,)).fetchone()[0]
                raw=json.loads(original)
                if patch is None:raw=[]
                else:raw.update(patch)
                db.execute(f"UPDATE {table} SET record=? WHERE id=?",(json.dumps(raw),key))
            try:
                before=snapshot(path)
                response=self.backend.request(method,url,body=body,headers=self.headers())
                self.assertEqual(response.status,500,response.body)
                self.assertIn(f"{table}/{key}",response.json()["error"])
                self.assertEqual(snapshot(path),before)
            finally:
                with sqlite3.connect(path) as db:db.execute(f"UPDATE {table} SET record=? WHERE id=?",(original,key))


if __name__=="__main__":
    unittest.main()
