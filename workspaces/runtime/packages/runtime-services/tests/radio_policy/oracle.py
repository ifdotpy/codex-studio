"""Exercise radio authorization against records produced by the Python runtime."""
import json
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard, IsolationViolation

GUARD = install_guard(block_processes=True)
for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"):
    if not os.environ.get(key, "").startswith("/var/tmp/"):
        raise IsolationViolation("disposable /var/tmp homes required")

SERVER_TESTS = Path(__file__).resolve().parents[6] / "workspaces/runtime/apps/server/tests"
sys.path.insert(0, str(SERVER_TESTS))
import importlib.util

spec = importlib.util.spec_from_file_location("radio_contract_fixture", SERVER_TESTS / "radio-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
Radio = fixture.Radio


def records_for(radio):
    db = radio.runtime.db()
    try:
        rooms = {key: raw for key, raw in db.execute(
            "SELECT id,record FROM runtime_rooms WHERE id GLOB 'radio:*' "
            "AND json_type(record,'$.radio')='object' ORDER BY id")}
        decoded = {key: json.loads(raw) for key, raw in rooms.items()}
        member_ids = {member for room in decoded.values() for member in room["members"]}
        agents = {key: raw for key, raw in db.execute(
            "SELECT id,record FROM runtime_agents WHERE id IN (%s) ORDER BY id" %
            ",".join("?" for _ in member_ids), tuple(sorted(member_ids)))} if member_ids else {}
        project_paths = {room["projectPath"] for room in decoded.values() if not room["radio"].get("direct")}
        projects = {key: raw for key, raw in db.execute(
            "SELECT id,record FROM runtime_projects ORDER BY id")
            if key in project_paths}
        resolved = {}
        from codex_peer_teams import _path
        for raw in [*agents.values(), *projects.values()]:
            value = json.loads(raw)
            field = "cwd" if "cwd" in value else "path"
            path = value.get(field)
            if isinstance(path, str) and path.strip():
                resolved[path] = _path(path)
        return {
            "recipient_id": radio.room()["radio"]["active"]["agentId"],
            "rooms": rooms,
            "agents": agents,
            "projects": projects,
            "resolved_paths": resolved,
        }
    finally:
        db.close()


def evaluate(case):
    radio = Radio("test_radio_policy")
    radio.setUp()
    try:
        radio.action("send", text="A shared instruction")
        state = radio.tick()
        active = state["active"]
        agent_id = active["agentId"]
        peer_id = next(member for member in radio.room()["members"] if member != agent_id)
        room = radio.room()
        radio_value = dict(room["radio"])
        if case.get("status") is not None:
            radio_value["status"] = case["status"]
        if case.get("active") is False:
            radio_value["active"] = None
        if case.get("direct"):
            radio_value["direct"] = True
            radio.mutate_agent(agent_id, sharedRoomId=room["id"])
            radio.mutate_agent(peer_id, sharedRoomId=room["id"])
        active_value = dict(radio_value["active"] or {})
        if case.get("active_agent_id") is not None:
            active_value["agentId"] = case["active_agent_id"]
        if case.get("active_thread_id") is not None:
            active_value["threadId"] = case["active_thread_id"]
        if radio_value["active"] is not None:
            radio_value["active"] = active_value
        room["radio"] = radio_value
        with radio.runtime.db() as db:
            radio.runtime.put(db, "rooms", room)
        recipient_changes = {}
        peer_changes = {}
        if "auto_wake" in case:
            recipient_changes["autoWake"] = case["auto_wake"]
        if case.get("native_failure_hold") is not None:
            recipient_changes["nativeFailureHold"] = case["native_failure_hold"]
        if case.get("peer_auto_wake") is not None:
            peer_changes["autoWake"] = case["peer_auto_wake"]
        if case.get("peer_account_transfer_id") is not None:
            peer_changes["accountTransferId"] = case["peer_account_transfer_id"]
        if case.get("peer_native_thread_block") is not None:
            peer_changes["nativeThreadBlock"] = case["peer_native_thread_block"]
        if case.get("peer_cwd") is not None:
            peer_changes["cwd"] = case["peer_cwd"]
        if case.get("bind_thread"):
            recipient_changes.update(threadId="thread-radio", compactions=case.get("compactions", 3))
            recipient_changes["startAttempt"] = {
                "events": [case.get("attempt_event", active["eventId"])],
                "epoch": case.get("attempt_epoch", active["epoch"]),
                "threadId": None,
            }
        radio.mutate_agent(agent_id, **recipient_changes)
        if peer_changes:
            radio.mutate_agent(peer_id, **peer_changes)
        with radio.runtime.db() as db:
            agents = {key: json.loads(raw) for key, raw in db.execute(
                "SELECT id,record FROM runtime_agents")}
            event = dict(db.execute("SELECT * FROM runtime_events WHERE id=?", (active["eventId"],)).fetchone())
            result = fixture.validate_event(
                radio.runtime, db, agents[agent_id], event)
        binding = None
        persisted = radio.room()["radio"].get("active")
        if result is None and case.get("bind_thread") and persisted and persisted.get("identity"):
            binding = {
                "room_id": room["id"],
                "event_id": active["eventId"],
                "agent_id": agent_id,
                "thread_id": persisted["threadId"],
                "epoch": persisted["identity"][1],
                "compactions": persisted["identity"][2],
            }
        return {
            "allowed": result is None,
            "denial": result,
            "binding": binding,
            "records": records_for(radio),
            "event_id": active["eventId"],
        }
    finally:
        radio.tearDown()
        radio.doCleanups()


for line in sys.stdin:
    case = json.loads(line)
    if case.get("close"):
        break
    try:
        print(json.dumps(evaluate(case), separators=(",", ":")), flush=True)
    except BaseException as error:
        print(json.dumps({"oracle_error": type(error).__name__ + ": " + str(error)}), flush=True)
