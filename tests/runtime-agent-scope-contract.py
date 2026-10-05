#!/usr/bin/env python3
"""Room sync entities match the base whole-roster emission contract."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import Runtime
from codex_sync_entities import put as sync_entity_put

spec = importlib.util.spec_from_file_location("runtime_contract_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def room_entities(runtime):
    with runtime.db() as db:
        return [(row[0], json.loads(row[1]), row[2]) for row in db.execute(
            "SELECT id,payload,deleted FROM sync_entities WHERE collection='room' ORDER BY id")]


def legacy_put(runtime, db, record):
    """Copy ca123f6 Runtime.put's post-agent-update room emission."""
    previous_row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (record["id"],)).fetchone()
    previous = json.loads(previous_row[0]) if previous_row else None
    runtime.put(db, "agents", record, sync_rooms=False)
    if previous is not None and any(previous.get(key) != record.get(key)
                                    for key in ("name", "rootId", "deletedAt", "sharedRoomId", "cwd")):
        for room in runtime.chat_rooms(db):
            sync_entity_put(db, "room", room["id"], room)


def fixture_records(runtime):
    project_path = "/fixture/project"
    lead_a = runtime.create({"name": "A", "cwd": str(runtime.root), "prompt": "a"}, defer=True)
    lead_b = runtime.create({"name": "B", "cwd": str(runtime.root), "prompt": "b"}, defer=True)
    lead_c = runtime.create({"name": "C", "cwd": str(runtime.root), "prompt": "c"}, defer=True)
    for index, lead in enumerate((lead_a, lead_b, lead_c)):
        lead["id"] = f"lead-{index}"
        lead["rootId"] = lead["id"]
        lead["cwd"] = project_path
    records = [lead_a, lead_b, lead_c]
    for root, names in ((lead_a, ("A worker", "A grandchild")),
                        (lead_b, ("B worker",)), (lead_c, ("C worker",))):
        parent = root
        for index, name in enumerate(names):
            child = dict(root, id=name.lower().replace(" ", "-"), parentId=parent["id"],
                         isLead=False, role="worker", name=name, status="completed",
                         autoWake=False)
            records.append(child)
            parent = child if root is lead_a and index == 0 else parent
    records[-1]["deletedAt"] = 17.0
    return records


def seed(runtime):
    agents = fixture_records(runtime)
    now = 1700000000.0
    rooms = [
        {"id": "broadcast:" + agents[0]["id"], "kind": "broadcast", "rootId": agents[0]["id"], "updated": now},
        {"id": "broadcast:" + agents[1]["id"], "kind": "broadcast", "rootId": agents[1]["id"], "updated": now},
        {"id": "broadcast:all", "kind": "broadcast", "rootId": "all", "updated": now},
        {"id": "private:" + agents[0]["id"] + ":" + agents[3]["id"], "kind": "private",
         "members": sorted((agents[0]["id"], agents[3]["id"])), "updated": now},
        {"id": "shared-a", "kind": "private", "members": sorted((agents[0]["id"], agents[1]["id"])),
         "projectPath": "/fixture/project", "radio": {"direct": True, "teamId": "radio-a",
          "revision": 0, "status": "idle", "speaker": None, "next": [], "active": None,
          "error": None}, "updated": now},
        {"id": "orphan-private", "kind": "private", "members": ["missing-agent"], "updated": now},
    ]
    agents[0]["sharedRoomId"] = "shared-a"
    agents[1]["sharedRoomId"] = "shared-a"
    with runtime.db() as db:
        db.execute("DELETE FROM runtime_agents")
        for record in agents:
            db.execute("INSERT OR REPLACE INTO runtime_agents(id,record) VALUES (?,?)",
                       (record["id"], json.dumps(record)))
        for room in rooms:
            db.execute("INSERT INTO runtime_rooms(id,record) VALUES (?,?)",
                       (room["id"], json.dumps(room)))
        db.execute("INSERT INTO runtime_federation_rooms(id,record) VALUES (?,?)",
                   ("federated-a", json.dumps({"id": "federated-a", "status": "approved",
                                               "localApproved": True, "remoteApproved": True,
                                               "localMembers": [agents[3]["id"]], "remoteMembers": [],
                                               "peerId": "p", "peerLabel": "Peer", "name": "Federated"})))
        for room in runtime.chat_rooms(db):
            sync_entity_put(db, "room", room["id"], room)
    return agents


def legacy_delete(runtime, db, key):
    root = runtime.agent(key, db)
    all_agents = runtime.records(db, "agents")
    ids = {root["id"]}
    while True:
        expanded = ids | {a["id"] for a in all_agents if a.get("parentId") in ids}
        if expanded == ids:
            break
        ids = expanded
    children = {}
    for agent in all_agents:
        if agent["id"] in ids and agent.get("parentId") in ids:
            children.setdefault(agent["parentId"], []).append(agent["id"])
    order, frontier = [root["id"]], {root["id"]}
    while frontier:
        frontier = {child for parent in frontier for child in children.get(parent, ())} - set(order)
        order.extend(sorted(frontier))
    by_id = {agent["id"]: agent for agent in all_agents}
    for agent_id in reversed(order):
        current = by_id[agent_id]
        current.update(deletedAt=current.get("deletedAt") or 42.0, autoWake=False)
        legacy_put(runtime, db, current)


def run_scenario(operation, legacy):
    temp = tempfile.TemporaryDirectory()
    runtime = Runtime(Path(temp.name), fixture.FakeServer)
    runtime.voice = lambda: type("Voice", (), {"delete_agent": staticmethod(lambda *_: None)})()
    runtime.stop = lambda *_args, **_kwargs: None
    try:
        agents = seed(runtime)
        if operation == "single-delete" and not legacy:
            runtime.delete_conversation(agents[3]["id"])
        elif operation == "tree-delete" and not legacy:
            runtime.delete_conversation(agents[0]["id"])
        else:
            with runtime.lock, runtime.db() as db:
                if operation in {"rename", "cwd"}:
                    current = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?",
                                                    (agents[3]["id"],)).fetchone()[0])
                    if operation == "rename":
                        current["name"] = "A worker renamed"
                    else:
                        current["cwd"] = "/fixture/other"
                    if legacy:
                        legacy_put(runtime, db, current)
                    else:
                        runtime.put(db, "agents", current)
                elif operation == "single-delete":
                    if legacy:
                        legacy_delete(runtime, db, agents[3]["id"])
                    else:
                        raise AssertionError("single-delete optimized path runs outside fixture transaction")
                elif operation == "tree-delete":
                    legacy_delete(runtime, db, agents[0]["id"])
                else:
                    raise AssertionError(operation)
        return room_entities(runtime)
    finally:
        runtime.close()
        temp.cleanup()


class RuntimeAgentRoomScope(unittest.TestCase):
    def test_room_entities_match_base_after_agent_and_tree_changes(self):
        expected = json.loads((Path(__file__).parent / "fixtures/runtime-agent-room-entities.json").read_text())
        for operation in ("rename", "cwd", "single-delete", "tree-delete"):
            with self.subTest(operation=operation):
                base = run_scenario(operation, legacy=True)
                optimized = run_scenario(operation, legacy=False)
                self.assertEqual([list(row) for row in base], expected[operation])
                self.assertEqual(optimized, base)
                # Orphans are tolerated by both base and targeted room projection.
                self.assertNotIn("orphan-private", {row[0] for row in optimized})

    def test_scoped_agent_indexes_are_selected_by_sqlite(self):
        temp = tempfile.TemporaryDirectory()
        runtime = Runtime(Path(temp.name), fixture.FakeServer)
        try:
            with runtime.db() as db:
                queries = (
                    ("runtime_agent_root", "SELECT record FROM runtime_agents WHERE json_extract(record,'$.rootId')=?", ("root",)),
                    ("runtime_agent_account_scope", "SELECT record FROM runtime_agents WHERE CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' ELSE json_extract(record,'$.accountKey') END=?", ("default",)),
                    ("runtime_agent_native_scope", "SELECT record FROM runtime_agents WHERE json_extract(record,'$.threadId')=? AND CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' ELSE json_extract(record,'$.accountKey') END=?", ("thread", "default")),
                    ("runtime_agent_native_scope", "SELECT id FROM runtime_agents WHERE json_type(record,'$.threadId')='text' AND json_extract(record,'$.threadId')>''", ()),
                    ("runtime_agent_restart_stage", "SELECT record FROM runtime_agents WHERE json_extract(record,'$.restartRecovery.stage')='pending'", ()),
                    ("runtime_agent_live_lead", "SELECT record FROM runtime_agents WHERE json_extract(record,'$.isLead')=1 AND json_extract(record,'$.deletedAt') IS NULL ORDER BY rowid LIMIT 1", ()),
                )
                for index, query, params in queries:
                    plan = " ".join(str(part) for row in db.execute(
                        "EXPLAIN QUERY PLAN " + query, params) for part in row)
                    self.assertIn(index, plan)
                    self.assertIn("SEARCH", plan)
        finally:
            runtime.close()
            temp.cleanup()


if __name__ == "__main__":
    unittest.main()
