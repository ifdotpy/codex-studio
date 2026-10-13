"""P19 corpus: execute the real Python radio functions on Python-written rows."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
import importlib.util
import hashlib
import json
import sqlite3
import uuid
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[6]
TESTS = ROOT / "workspaces/runtime/apps/server/tests"
sys.path.insert(0, str(TESTS))
spec = importlib.util.spec_from_file_location("p19_fixture", TESTS / "radio-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_radio as radio
import codex_peer_conversion as conversion
from types import SimpleNamespace

TABLES = ("agents", "projects", "rooms", "events", "chat_messages", "requests", "completed_turns", "items", "operation_receipts", "sync_entities", "sidebar_order", "work", "plans", "annotations", "complaints", "event_meta", "rules")

def snapshot(r):
    with r.db() as db:
        return {table: [dict(row) for row in db.execute("SELECT * FROM " + {"sync_entities":"sync_entities", "sidebar_order":"runtime_sidebar_order"}.get(table, "runtime_" + table) + " ORDER BY " + ("seq" if table == "chat_messages" else "collection,id" if table == "sync_entities" else "id"))] for table in TABLES}

def project_changed_events(before, after):
    """Mirror the live SQLite event projection missing from this reduced fixture schema."""
    old = {row["id"]: row for row in before["events"]}
    changed = [row for row in after["events"] if old.get(row["id"]) != row]
    sequence = max((row["seq"] for row in after["sync_entities"]), default=0)
    for event in changed:
        value = {key: event.get(key) for key in ("id", "agent", "kind", "status", "created", "error")}
        payload_obj = {"collection": "event", "id": event["id"], "value": value}
        payload = json.dumps(payload_obj, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
        sequence += 1
        after["sync_entities"].append({
            "collection": "event", "id": event["id"], "seq": sequence,
            "hash": hashlib.sha256(payload.encode()).hexdigest(), "payload": payload, "deleted": 0,
        })
    after["sync_entities"].sort(key=lambda row: (row["collection"], row["id"]))

def template(root):
    schema = json.loads((ROOT / "workspaces/runtime/packages/runtime-store/tests/fixtures/live-state-schema.json").read_text())
    for name in ("canvas.sqlite3", "analytics.sqlite3"):
        db = sqlite3.connect(root / name)
        db.execute("PRAGMA journal_mode=WAL")
        db.execute("PRAGMA synchronous=FULL")
        objects = [o for o in schema[name]["sqlite_master"] if o.get("sql") and o["name"] not in {"runtime_search_next_config", "runtime_search_next_data", "runtime_search_next_docsize", "runtime_search_next_idx"}]
        objects.sort(key=lambda o: ({"table": 0, "index": 1, "trigger": 2, "view": 3}.get(o["type"], 4), o["name"]))
        for obj in objects:
            db.execute(obj["sql"])
        db.commit()
        db.close()

def case(name, mode, configure=None, extra=None):
    f = fixture.Radio("test_two_replies_share_all_text_and_stop")
    with patch("time.time", return_value=1000.0):
        f.setUp()
        try:
            for id in ("a", "b", "c", "d"):
                f.mutate_agent(id, name=id, status="idle", epoch=1, created=10.0, updated=10.0, provider="codex", model="gpt-6-astra", role="lead", depth=0, prompt="", worktree=False)
            with f.runtime.db() as db:
                db.execute("CREATE INDEX runtime_task_agent_created_id ON runtime_tasks(json_extract(record,'$.agent'),json_extract(record,'$.created'),id)")
                for table in ("work", "plans", "annotations", "complaints", "event_meta", "rules"):
                    db.execute(f"CREATE TABLE IF NOT EXISTS runtime_{table}(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
                db.execute("CREATE TABLE IF NOT EXISTS runtime_sidebar_order(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            if configure:
                configure(f)
            if mode == "convert":
                with f.runtime.db() as db:
                    for table, record in (
                        ("work", dict(id="w", owner="a", rootId="a", version=1, status="review", title="Work", results=[], decisions=[])),
                        ("plans", dict(id="a", rootId="a", text="Plan", version=1, updated=10.0, steps=[])),
                        ("annotations", dict(id="an", rootId="a", agent="a", text="Note")),
                        ("complaints", dict(id="co", leadId="a", author="a", text="Issue", status="open")),
                        ("event_meta", dict(id="em", rootId="a", leadId="a")),
                        ("rules", dict(id="ru", rootId="a", agent="a", inFlight=False)),
                    ):
                        f.runtime.put(db, table, record)
            if mode in ("project", "peer"):
                body = dict(extra or {})
                body.setdefault("path", f.path)
            else:
                body = dict(action="radio", radio_action=mode, path=f.path, team_id=f.team_id, request_id="p19-"+name, expected_revision=f.room()["radio"]["revision"])
                body.update(extra or {})
            drafts = []
            if mode == "create":
                Path(f.path).mkdir(parents=True)
                body = dict(action="radio", radio_action="create", path=f.path, name="Direct", request_id="p19-create", participants=[dict(account_key="default", model="gpt-6-astra")]*2)
                def prepare(spec, **kwargs):
                    with f.runtime.db() as db:
                        original = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='a'").fetchone()[0])
                    original.update(id=spec["id"], rootId=spec["id"], name=spec["name"], model=spec["model"], threadId=None)
                    drafts.append(original)
                    return original
                f.runtime.create = prepare
            if mode == "project-binding":
                f.runtime._project_server_id = "local-test"
                with f.runtime.db() as db:
                    project=json.loads(db.execute("SELECT record FROM runtime_projects").fetchone()[0])
                    project.update(homeServerId="local-test", locations=[dict(serverId="local-test",path=f.path,projectId=f.path)])
                    f.runtime.put(db,"projects",project)
            before = snapshot(f.runtime)
            error = False
            result = None
            try:
                if mode == "tick":
                    revision = f.room()["radio"]["revision"]
                    event_uuid = uuid.uuid5(uuid.NAMESPACE_URL, f.room()["id"] + ":" + str(revision))
                    with patch("codex_radio.uuid.uuid4", return_value=event_uuid):
                        f.tick()
                        result = list(f.interrupts)
                elif mode == "answer":
                    q = dict(id="q", method="agent/asyncQuestion", agent="a", epoch=1, turnId="turn")
                    body.update(question=q, text="answer", accepted=False)
                    with f.runtime.db() as db:
                        result = radio.route_question_answer(f.runtime, db, q, "answer")
                elif mode == "project-binding":
                    from codex_project_locations import chat_binding, settings_key
                    with f.runtime.db() as db:
                        binding=chat_binding(f.runtime,db,f.path,f.path,"local-test")
                        result=dict(binding=binding,settings=settings_key(f.runtime,db,f.path,binding))
                elif mode == "convert":
                    body = dict(action="convert", path=f.path, member="a", target="c", expected_revision=1, request_id="p19-convert")
                    f.runtime.agent = lambda key, db: json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])
                    f.runtime._put = f.runtime.put
                    f.runtime.changed = SimpleNamespace(set=lambda: None)
                    f.runtime.sync_room_entity = lambda *args, **kwargs: None
                    f.runtime._workspace_operations = lambda db, key: []
                    original_put = f.runtime.put
                    f.runtime.put = lambda db, table, record, **kwargs: original_put(db, table, record)
                    result = conversion.convert(f.runtime, body)
                elif mode == "bind":
                    a = next(json.loads(r["record"]) for r in before["agents"] if r["id"] == "a")
                    e = next(r for r in before["events"] if r["id"] == f.room()["radio"]["active"]["eventId"])
                    body.update(agent="a", event=e["id"])
                    with f.runtime.db() as db:
                        result = radio.validate_event(f.runtime, db, a, e)
                        error = result is not None
                elif mode == "project":
                    result = f.runtime.projects(body)
                elif mode == "peer":
                    from codex_peer_teams import manage
                    result = manage(f.runtime, body)
                else:
                    result = radio.manage(f.runtime, body)
            except ValueError:
                error = True
            after = snapshot(f.runtime)
            project_changed_events(before, after)
            return dict(name=name, mode=mode, body=body, before=before, after=after, result=result, error=error, drafts=drafts)
        finally:
            f.doCleanups()

def queued(f):
    f.action("send", text="question", request_id="seed-send")

def active(f):
    queued(f)
    f.tick()

def delivered(f):
    active(f)
    a = f.room()["radio"]["active"]
    f.mutate_agent("a", inFlight=True, status="running", turnId="turn", startAttempt=dict(events=[a["eventId"]], epoch=1, submitted=True, threadId="native-a", observedTurnId="turn"))
    with f.runtime.db() as db:
        db.execute("UPDATE runtime_events SET status='delivered',turn_id='turn'")

def question(f):
    delivered(f)
    with f.runtime.db() as db:
        db.execute("INSERT INTO runtime_completed_turns VALUES('a:turn')")
        f.runtime.put(db, "requests", dict(id="q", agent="a", epoch=1, turnId="turn", method="agent/asyncQuestion", status="pending", params={}, created=10.0))
    f.mutate_agent("a", inFlight=False, status="idle", lastCompletedTurn="turn", lastCompletedTurnStatus="completed")

def binding(f):
    f.mutate_agent("a", threadId=None)
    active(f)
    a=f.room()["radio"]["active"]
    f.mutate_agent("a", threadId="prepared", startAttempt=dict(events=[a["eventId"]],epoch=1,threadId="prepared"))

def uncertain(f):
    active(f)
    with f.runtime.db() as db:
        db.execute("UPDATE runtime_events SET status='uncertain'")

def stopped(f):
    delivered(f)
    f.action("stop")

root = Path(sys.argv[1])
template(root)

def peer_move_fixture(f):
    with f.runtime.db() as db:
        project = json.loads(db.execute("SELECT record FROM runtime_projects WHERE id=?", (f.path,)).fetchone()[0])
        project["peerTeams"] = [dict(id=f.team_id, name="Peers", members=["a", "b"]), dict(id="33333333-3333-4333-8333-333333333333", name="Second", members=["c", "d"])]
        project["peerTeamsRevision"] = 1
        f.runtime.put(db, "projects", project)

cases = [
    case("open", "open"), case("send", "send", extra=dict(text="hello", rounds=2)), case("pass", "pass", extra=dict(target="b")), case("pass-both", "pass"), case("bad-rounds", "send", extra=dict(text="x", rounds=3)), case("stop-idle", "stop"), case("stop-pending", "stop", active), case("stop-delivered", "stop", delivered), case("floor", "tick", queued), case("unknown", "tick", uncertain), case("interrupt", "tick", stopped), case("question-floor", "tick", question), case("answer", "answer", question), case("binding", "bind", binding), case("convert", "convert"), case("create", "create"), case("project-binding", "project-binding"),
    case("project-accounts", "project", extra=dict(action="set_accounts", account_keys=["default"], account_key="default", expected_revision=1)),
    case("project-base", "project", extra=dict(action="set_worker_base", base_ref="main", expected_revision=0)),
    case("project-folder", "project", extra=dict(action="add_folder", folder_id="22222222-2222-4222-8222-222222222222", name="Docs", expected_revision=0)),
    case("project-sidebar", "project", extra=dict(action="reorder", request_id="p19-sidebar", groups={"projects":["/project"]}, expected_revision=0)),
    case("peer-save", "peer", extra=dict(action="save", request_id="p19-save", expected_revision=0, team_id="33333333-3333-4333-8333-333333333333", name="Second", members=["c","d"])),
    case("peer-delete", "peer", extra=dict(action="delete", request_id="p19-delete", expected_revision=0, team_id="11111111-1111-4111-8111-111111111111")),
    case("peer-move", "peer", peer_move_fixture, extra=dict(action="move", request_id="p19-move", expected_revision=1, team_id="33333333-3333-4333-8333-333333333333", member="b")),
]
if "--full" in sys.argv:
    for target in ("a", "b", "both", "missing"):
        for rounds in (1, 2):
            cases.append(case(f"send-{target}-{rounds}", "send", extra=dict(text="Unicode λ", target=target, rounds=rounds)))
print(json.dumps(cases, ensure_ascii=False))
