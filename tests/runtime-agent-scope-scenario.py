#!/usr/bin/env python3
"""Run one scenario against the Runtime of the checkout given as argv[1]; print room entities JSON."""
import sys, json, tempfile, importlib.util
from pathlib import Path
src = Path(sys.argv[1]); scenario = sys.argv[2]
sys.dont_write_bytecode = True
sys.path.insert(0, str(src / "tests"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
sys.path.insert(0, str(src / "scripts"))
from codex_runtime import Runtime
from codex_canvas import Canvas
from codex_sync_entities import put as sync_entity_put, upgrade_agent_organization
spec = importlib.util.spec_from_file_location("runtime_contract_fixture", src / "tests" / "runtime-contract.py")
fixture = importlib.util.module_from_spec(spec); spec.loader.exec_module(fixture)

P = "/fixture/project"
NOW = 1700000000.0

def seed(runtime):
    leads = [runtime.create({"name": n, "cwd": str(runtime.root), "prompt": n}, defer=True) for n in "ABC"]
    for i, lead in enumerate(leads):
        lead["id"] = f"lead-{i}"; lead["rootId"] = lead["id"]; lead["cwd"] = P
    recs = list(leads)
    def child(root, cid, name, parent, **extra):
        c = dict(root, id=cid, parentId=parent, isLead=False, role="worker", name=name,
                 status="completed", autoWake=False)
        c.pop("sharedRoomId", None); c.update(extra); recs.append(c); return c
    child(leads[0], "a-worker", "A worker", "lead-0")
    child(leads[0], "a-grandchild", "A grandchild", "a-worker")
    leads[1]["accountKey"] = "account-2"
    child(leads[1], "b-worker", "B worker", "lead-1")
    child(leads[2], "c-worker", "C worker", "lead-2", deletedAt=17.0)
    if scenario == "orphan-tree-delete":
        child(leads[0], "a-orphan", "A orphan", None)
    if scenario == "cross-root-child":
        child(leads[1], "x-child", "X child", "a-worker")
    legacy = None
    if scenario in ("legacy-norootid-delete", "legacy-norootid-budget"):
        legacy = dict(leads[2], id="legacy", name="Legacy", deletedAt=5.0); legacy.pop("rootId")
    rooms = [
        {"id": "broadcast:lead-0", "kind": "broadcast", "rootId": "lead-0", "updated": NOW},
        {"id": "broadcast:lead-1", "kind": "broadcast", "rootId": "lead-1", "updated": NOW},
        {"id": "broadcast:lead-2", "kind": "broadcast", "rootId": "lead-2", "updated": NOW},
        {"id": "broadcast:all", "kind": "broadcast", "rootId": "all", "updated": NOW},
        {"id": "private:lead-0:a-worker", "kind": "private", "members": ["a-worker", "lead-0"], "updated": NOW},
        {"id": "private:lead-1:b-worker", "kind": "private", "members": ["b-worker", "lead-1"], "updated": NOW},
        {"id": "shared-a", "kind": "private", "members": ["lead-0", "lead-1"], "projectPath": P,
         "radio": {"direct": True, "teamId": "radio-a", "revision": 0, "status": "idle", "speaker": None,
                   "next": [], "active": None, "error": None}, "updated": NOW},
        {"id": "pair-0-2", "kind": "private", "members": ["lead-0", "lead-2"], "updated": NOW},
        {"id": "federated-a", "kind": "federated", "members": [], "updated": NOW},
        {"id": "orphan-private", "kind": "private", "members": ["missing-agent"], "updated": NOW},
    ]
    leads[0]["sharedRoomId"] = "shared-a"; leads[1]["sharedRoomId"] = "shared-a"
    canvas = Canvas(runtime.root)
    with runtime.db() as db:
        db.execute("DELETE FROM runtime_agents")
        for r in recs:
            db.execute("INSERT OR REPLACE INTO runtime_agents(id,record) VALUES (?,?)", (r["id"], json.dumps(r)))
        for room in rooms:
            db.execute("INSERT INTO runtime_rooms(id,record) VALUES (?,?)", (room["id"], json.dumps(room)))
        db.execute("INSERT INTO runtime_federation_rooms(id,record) VALUES (?,?)",
                   ("federated-a", json.dumps({"id": "federated-a", "status": "approved", "localApproved": True,
                                               "remoteApproved": True, "localMembers": ["lead-0", "a-worker"],
                                               "remoteMembers": [], "peerId": "p", "peerLabel": "Peer",
                                               "name": "Federated"})))
        if scenario != "peer-team-then-unrelated-rename":
            db.execute("INSERT OR REPLACE INTO runtime_projects(id,record) VALUES (?,?)",
                       (P, json.dumps({"id": P, "path": P, "created": NOW,
                                      "peerTeamsRevision": 1, "peerTeams": [
                           {"id": "team-1", "name": "Peers", "members": ["lead-0", "lead-1", "lead-2"]}]})))
        db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES (?,?,?,?,?,?)",
                   ("m1", "private:lead-1:b-worker", "lead-1", "hello", NOW, "{}"))
        db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES (?,?,?,?,?,?)",
                   ("m-fed", "federated-a", "lead-0", "federated hello", NOW, "{}"))
        for room in runtime.chat_rooms(db):
            sync_entity_put(db, "room", room["id"], room)
        if legacy is not None:
            db.execute("INSERT INTO runtime_agents(id,record) VALUES (?,?)",
                       (legacy["id"], json.dumps(legacy)))
            recs.append(legacy)
        # Seed the pre-recovery entity baseline for the whole roster. The
        # scenario measures which existing entity rows recovery changes.
        for agent in recs:
            sync_entity_put(db, "agent", agent["id"], agent)
        # This synthetic database starts with already-current entity rows, so
        # use the production marker path before measuring recovery writes.
        upgrade_agent_organization(db, runtime, canvas)
    return {r["id"]: r for r in recs}

def get(db, key):
    return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])

def change(runtime, key, **fields):
    with runtime.lock, runtime.db() as db:
        rec = get(db, key)
        for k, v in fields.items():
            if v is KeyError: rec.pop(k, None)
            else: rec[k] = v
        runtime.put(db, "agents", rec)

temp = tempfile.TemporaryDirectory()
runtime = Runtime(Path(temp.name), fixture.FakeServer)
runtime.voice = lambda: type("Voice", (), {"delete_agent": staticmethod(lambda *_: None)})()
runtime.stop = lambda *a, **k: None
out = {"scenario": scenario}
try:
    recs = seed(runtime)
    out["accountKeys"] = sorted({record.get("accountKey", "default") for record in recs.values()})
    try:
        if scenario == "legacy-workspace-operation-recovery":
            paused_owner = dict(recs["lead-0"], workspaceOperation="checkpoint",
                                workspaceReservationId="legacy-paused-capture", status="paused",
                                autoWake=False)
            queued_owner = dict(recs["lead-1"], workspaceOperation="checkpoint",
                                workspaceReservationId="legacy-queued-capture", autoWake=False)
            with runtime.lock, runtime.db() as db:
                runtime.put(db, "agents", paused_owner)
                runtime.put(db, "agents", queued_owner)
            runtime.send(queued_owner["id"], "Continue after snapshot", "legacy-recovery-input")
            with runtime.db() as db:
                close_before = dict(db.execute(
                    "SELECT id,seq FROM sync_entities WHERE collection='agent' AND deleted=0"))
                close_values_before = {key: json.loads(payload).get("value") for key, payload in db.execute(
                    "SELECT id,payload FROM sync_entities WHERE collection='agent' AND deleted=0")}
            runtime.close()
            with runtime.db() as db:
                close_after = dict(db.execute(
                    "SELECT id,seq FROM sync_entities WHERE collection='agent' AND deleted=0"))
                close_advanced = sorted(agent_id for agent_id, seq in close_after.items()
                                        if seq > close_before.get(agent_id, -1))
                close_values_after = {key: json.loads(payload).get("value") for key, payload in db.execute(
                    "SELECT id,payload FROM sync_entities WHERE collection='agent' AND deleted=0")}
                close_fields_changed = {agent_id: sorted(
                    key for key in set(close_values_before.get(agent_id, {}))
                    | set(close_values_after.get(agent_id, {}))
                    if close_values_before.get(agent_id, {}).get(key)
                    != close_values_after.get(agent_id, {}).get(key))
                    for agent_id in close_advanced}
            # Close may persist ordinary shutdown state; take the baseline
            # immediately before the simulated restart recovery begins.
            with runtime.db() as db:
                sync_before = dict(db.execute(
                    "SELECT id,seq FROM sync_entities WHERE collection='agent' AND deleted=0"))
                legacy_records = [
                    {key: record.get(key) for key in (
                        "id", "rootId", "accountKey", "deletedAt", "status", "autoWake",
                        "workspaceOperation", "workspaceReservationId")}
                    for record in runtime.records(db, "agents")
                    if record.get("workspaceOperation")
                ]
                workspace_operation_rows = runtime.records(db, "workspace_operations")
            runtime = Runtime(Path(temp.name), fixture.FakeServer)
            runtime.voice = lambda: type("Voice", (), {"delete_agent": staticmethod(lambda *_: None)})()
            runtime.stop = lambda *a, **k: None
            recovered = {agent_id: runtime.agent(agent_id) for agent_id in (
                paused_owner["id"], queued_owner["id"]
            )}
            with runtime.db() as db:
                pending = db.execute(
                    "SELECT status,agent FROM runtime_events WHERE id='legacy-recovery-input'"
                ).fetchone()
                operation_count = db.execute(
                    "SELECT COUNT(*) FROM runtime_workspace_operations"
                ).fetchone()[0]
                synced = {}
                for agent_id in recovered:
                    row = db.execute(
                        "SELECT seq,payload FROM sync_entities WHERE collection='agent' AND id=?",
                        (agent_id,),
                    ).fetchone()
                    value = json.loads(row[1])["value"]
                    synced[agent_id] = {key: value.get(key) for key in (
                        "workspaceOperation", "workspaceReservationId", "checkpointError", "status", "autoWake")}
                sync_after = dict(db.execute(
                    "SELECT id,seq FROM sync_entities WHERE collection='agent' AND deleted=0"))
            affected = {paused_owner["id"], queued_owner["id"]}
            advanced = {agent_id for agent_id, seq in sync_after.items()
                        if seq > sync_before.get(agent_id, -1)}
            assert advanced == affected, (advanced, affected)
            assert all(sync_after[agent_id] > sync_before[agent_id] for agent_id in affected)
            assert all(synced[agent_id]["workspaceOperation"] is None for agent_id in affected)
            out["result"] = {
                "agents": {agent_id: {key: recovered[agent_id].get(key) for key in (
                    "workspaceOperation", "workspaceReservationId", "checkpointError", "status", "autoWake")}
                    for agent_id in recovered},
                "syncedAgents": synced,
                "agentEntityRowsAdvanced": sorted(advanced),
                "closeEntityRowsAdvanced": close_advanced,
                "closeEntityFieldsChanged": close_fields_changed,
                "pendingInput": dict(pending) if pending else None,
                "workspaceOperationRows": operation_count,
                "legacyRecordsBeforeRestart": legacy_records,
                "workspaceOperationRowsBeforeRestart": len(workspace_operation_rows),
            }
        elif scenario == "spawn-then-rename":
            with runtime.lock, runtime.db() as db:
                runtime.put(db, "agents", dict(recs["b-worker"], id="b-new", name="B new"))
            change(runtime, "b-new", name="B new titled")
        elif scenario == "spawn-then-unrelated-delete":
            with runtime.lock, runtime.db() as db:
                runtime.put(db, "agents", dict(recs["b-worker"], id="b-new", name="B new"))
            runtime.delete_conversation("a-grandchild")
        elif scenario == "peer-team-then-unrelated-rename":
            with runtime.lock, runtime.db() as db:
                runtime.put(db, "projects", {"id": P, "path": P, "peerTeamsRevision": 1, "peerTeams": [
                    {"id": "team-1", "name": "Peers", "members": ["lead-0", "lead-1"]}]})
            change(runtime, "b-worker", name="B worker renamed")
        elif scenario == "direct-message-then-unrelated-rename":
            from types import SimpleNamespace
            import codex_radio
            original_time = codex_radio.time
            codex_radio.time = SimpleNamespace(time=lambda: NOW)
            try:
                with runtime.lock, runtime.db() as db:
                    room = json.loads(db.execute(
                        "SELECT record FROM runtime_rooms WHERE id='shared-a'").fetchone()[0])
                    codex_radio._message(db, room, "m2", "lead-0", "direct insert", created=NOW)
                    codex_radio._save(runtime, db, room)
            finally:
                codex_radio.time = original_time
            change(runtime, "b-worker", name="B worker renamed")
        elif scenario in ("orphan-tree-delete", "tree-delete"):
            out["result"] = runtime.delete_conversation("lead-0")
        elif scenario == "repeat-tree-delete":
            out["result"] = [runtime.delete_conversation("lead-0"), runtime.delete_conversation("lead-0")]
        elif scenario == "cross-root-child":
            out["result"] = runtime.delete_conversation("a-worker")
        elif scenario == "legacy-norootid-delete":
            out["result"] = runtime.delete_conversation("legacy")
        elif scenario == "legacy-norootid-budget":
            from codex_budget import budget_status
            with runtime.lock, runtime.db() as db:
                rec = get(db, "legacy")
                status = budget_status(runtime, db, rec, check_coverage=False)
                out["result"] = {k: status.get(k) for k in ("rootId", "tokensUsed", "tokenBudget")}
                out["memberCountProbe"] = len([a for a in runtime.records(db, "agents")
                                               if (a.get("rootId") or a["id"]) == "legacy"])
        elif scenario == "lead-rename":
            change(runtime, "lead-0", name="A renamed")
        elif scenario == "worker-rename":
            change(runtime, "a-worker", name="A worker renamed")
        elif scenario == "lead-cwd":
            change(runtime, "lead-0", cwd="/fixture/other")
        elif scenario == "restore":
            change(runtime, "c-worker", deletedAt=KeyError)
        elif scenario == "single-delete":
            out["result"] = runtime.delete_conversation("a-worker")
        elif scenario == "rootid-change":
            change(runtime, "b-worker", rootId="lead-0", parentId="lead-0")
        elif scenario == "shared-room-change":
            change(runtime, "lead-1", sharedRoomId=None)
        elif scenario == "federated-delete":
            out["result"] = runtime.delete_conversation("a-worker")
        elif scenario == "exception-mid-tree-delete":
            original_put = runtime.put
            writes = [0]
            def fail_during_delete(db, table, record, **kwargs):
                if table == "agents" and record.get("deletedAt"):
                    writes[0] += 1
                    if writes[0] == 2:
                        raise RuntimeError("injected tree-delete failure")
                return original_put(db, table, record, **kwargs)
            runtime.put = fail_during_delete
            try:
                runtime.delete_conversation("lead-0")
            except Exception as error:
                out["error"] = type(error).__name__ + ": " + str(error)
            finally:
                runtime.put = original_put
        else:
            raise SystemExit("unknown scenario")
    except Exception as error:
        out["error"] = type(error).__name__ + ": " + str(error)
    with runtime.db() as db:
        out["rooms"] = {row[0]: {"deleted": row[2], "value": json.loads(row[1]).get("value", json.loads(row[1]))}
                        for row in db.execute("SELECT id,payload,deleted FROM sync_entities WHERE collection='room' ORDER BY id")}
        out["deletedAgents"] = sorted(r[0] for r in db.execute("SELECT id,record FROM runtime_agents")
                                      if json.loads(r[1]).get("deletedAt"))
finally:
    runtime.close(); temp.cleanup()
print(json.dumps(out, sort_keys=True, default=str))
