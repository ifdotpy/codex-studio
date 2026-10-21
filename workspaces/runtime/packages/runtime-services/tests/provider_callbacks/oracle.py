"""Real Python callback handlers, no runtime start or provider process."""
import os
from pathlib import Path
import sys

HERE = Path(__file__).resolve()
sys.path.insert(0, str(HERE.parent.parent / "turn_lifecycle"))
from oracle_guard import install_guard
_GUARD = install_guard()
SERVER = HERE.parents[4] / "apps/server"
if "--isolated" not in sys.argv:
    sys.path.insert(0, str(SERVER / "tests"))
    from http_backend_harness import BackendProcess
    scratch = Path(sys.argv[1]).resolve(strict=True)
    owner = BackendProcess(scratch)
    binary = owner._directory("disabled-provider") / "codex"
    binary.write_text("#!/bin/sh\nexit 99\n")
    binary.chmod(0o700)
    owner._provider_executable = binary
    os.execve(sys.executable, [sys.executable, "-B", str(HERE), "--isolated", str(scratch)], owner._environment())

import json
import sqlite3
import shutil
from unittest.mock import patch
sys.path.insert(0, str(SERVER / "src"))
from codex_runtime import Runtime
from agent_progress_contract import install_projection_contract
install_projection_contract(Runtime)
from codex_tool_requests import RequestMixin

class Quiet(Runtime):
    def schedule(self):
        pass
    def connect(self, *args, **kwargs):
        raise AssertionError("provider forbidden")
    connect_agent = connect
    def server_for(self, *args, **kwargs):
        return None
    def connection_current(self, *args, **kwargs):
        return True
    def reply(self, message, *args, **kwargs):
        self.replies.append(message)
    def start_review_archives(self):
        pass
    def resume_read_only_image_bases(self):
        pass

import concurrent.futures
import threading
import codex_accounts
import codex_runtime
from contextlib import ExitStack
stack = ExitStack()
# Fixture preparation has no durability contract; Rust opens every cloned case
# with FULL synchronous transactions. Avoid one fsync for each bootstrap DDL.
_connect = codex_runtime.sqlite_connect
def fixture_connect(*args, **kwargs):
    connection = _connect(*args, **kwargs)
    connection.execute("PRAGMA synchronous=OFF")
    return connection
stack.enter_context(patch.object(codex_runtime, "sqlite_connect", fixture_connect))
for substitution in (
    patch.object(codex_accounts, "codex_home", lambda: Path(os.environ["CODEX_HOME"])),
    patch.object(codex_accounts, "auth_metadata", lambda *_: {"status":"signedOut","accountId":None,"email":None,"plan":None}),
    patch.object(concurrent.futures.ThreadPoolExecutor, "submit", lambda *_a, **_kw: concurrent.futures.Future()),
    patch.object(threading.Thread, "start", lambda self: None),
    patch.object(threading.Thread, "join", lambda self, *a, **k: None),
    patch.object(codex_runtime, "_RuntimeWalKeeper", lambda *a: None),
):
    stack.enter_context(substitution)
root = Path(sys.argv[-1]) / "template"
root.mkdir()
runtime = Quiet(root, lambda *_args, **_kwargs: None)
runtime.replies = []
lead = runtime.create({"name": "Callback lead", "cwd": str(root), "prompt": "fixture"}, defer=True)
worker = runtime.create({"name": "Callback worker", "cwd": str(root), "prompt": "fixture"}, defer=True)
foreign = runtime.create({"name": "Other team", "cwd": str(root), "prompt": "fixture"}, defer=True)
worker.update(parentId=lead["id"], rootId=lead["id"], isLead=False)
for index, actor in enumerate([lead, worker, foreign]):
    actor.update(autoWake=True, status="running", accountKey="default", threadId=f"thread-{index}", turnId=f"turn-{index}", epoch=3, turnEpoch=3)
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", actor)

methods = ["item/commandExecution/requestApproval", "item/fileChange/requestApproval", "item/tool/requestUserInput", "item/permissions/requestApproval", "mcpServer/elicitation/request"]
answers = []
for method in methods:
    for decision in ["accept", "decline", "cancel"]:
        for secret in [False, True]:
            params = {"threadId":lead["threadId"], "turnId":lead["turnId"], "permissions":{"network":True},
                      "questions":[{"id":"q", "question":"Fixture?", "isSecret":secret}],
                      "requestedSchema":{"properties":{"q":{"title":"Fixture?", "writeOnly":secret}}}}
            data = {"decision":decision, "answers":{"q":{"answers":["different secret" if secret else "café 🦀"]}}, "content":{"q":"form value"}}
            key = f"answer-{len(answers)}"
            card = {"id":key,"rpcId":len(answers),"method":method,"params":params,"agent":lead["id"],"accountKey":"default","connectionId":"connection","epoch":3,"supervisor":{"generation":7},"status":"pending","createdAt":10.0}
            with runtime.lock, runtime.db() as db:
                runtime.put(db,"requests",card)
            with patch("time.time", return_value=20.0):
                runtime.answer(key,data)
            with runtime.db() as db:
                stored=json.loads(db.execute("SELECT record FROM runtime_requests WHERE id=?",(key,)).fetchone()[0])
            answers.append({"method":method,"params":params,"data":data,"expected":runtime.replies[-1]["result"],"history":stored["answerHistory"],"signature":stored["answerSignature"]})
# Exercise the real reservation path, then retain its Python-written receipts for
# cancellation/restart cases as well as complete identity/signature expectations.
reservations = []
for index, name in enumerate(["orchestration_task", "orchestration_spawn", "orchestration_send", "orchestration_review", "orchestration_servers", "orchestration_move", "orchestration_teleport"]):
    params = {"threadId":lead["threadId"], "turnId":lead["turnId"], "callId":f"alias-{index}", "tool":name,
              "arguments":{"request_id":f"request-{index}", "value":"café 🦀", "number":1e-8}}
    if index % 2:
        params["arguments"] = json.dumps(params["arguments"],ensure_ascii=False)
    message = {"id":index,"method":"item/tool/call","params":params}
    with patch("time.time", return_value=10.0):
        record = runtime.reserve_tool_request(message,"default","connection")
    reservations.append({"message":message,"record":record})
runtime.begin_tool_request(reservations[0]["record"]["id"])
chat_page = {"room":{"id":"room","kind":"private","rootId":lead["id"],"name":"Fixture","members":[lead["id"]]},
             "messages":[{"id":"message","seq":1,"text":"字"*1100,"deliveries":{lead["id"]:"queued"}}],"nextBefore":None}
chat_expected = runtime.model_chat_page(chat_page["room"],chat_page["messages"],False)
# The turn path delivers capability guidance before callbacks. Exercise that
# real Python transition with a read-only warmup; subsequent results are exact.
warmup = {"id":"warmup","method":"item/tool/call","params":{"threadId":lead["threadId"],"turnId":lead["turnId"],"callId":"warmup","tool":"orchestration_task","arguments":{"action":"list"}}}
with patch("time.time", return_value=19.0):
    runtime.dynamic(warmup,"default","connection")
dynamic_cases = []
for index, (tool, arguments) in enumerate([
    ("orchestration_task", {"action":"list"}),
    ("orchestration_context", {"topic":"plan"}),
    ("orchestration_title", {"title":"Unicode 🦀 title"}),
]):
    message = {"id":f"dynamic-{index}","method":"item/tool/call", "params":{
        "threadId":lead["threadId"],"turnId":lead["turnId"],"callId":f"dynamic-{index}","tool":tool,"arguments":arguments}}
    with patch("time.time", return_value=20.0):
        runtime.dynamic(message,"default","connection")
    dynamic_cases.append({"message":message,"expected":runtime.replies[-1]["result"]})
# Exercise the real visible-ID resolver against Python-written agents. Hidden
# prefix collisions must not affect a caller's scope; archived rows stay hidden.
resolver_agents = []
for source, identity, archived in [
    (worker, "aaaaaaaa-1111-4111-8111-111111111111", False),
    (worker, "aaaaaaaa-2222-4222-8222-222222222222", False),
    (foreign, "bbbbbbbb-1111-4111-8111-111111111111", False),
    (worker, "bbbbbbbb-2222-4222-8222-222222222222", False),
    (worker, "cccccccc-1111-4111-8111-111111111111", True),
]:
    actor = {**source, "id":identity, "deletedAt":15.0 if archived else None}
    if source is foreign:
        actor["rootId"] = identity
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", actor)
    resolver_agents.append(actor)
resolutions = []
for supplied in [worker["id"][:8], resolver_agents[0]["id"], "aaaaaaaa", "aaaaaaa", "ffffffff", foreign["id"][:8], "bbbbbbbb", "cccccccc"]:
    try:
        result = runtime.resolve_visible_agent_id(lead["id"], supplied)
        resolutions.append({"supplied":supplied,"result":result})
    except ValueError as error:
        resolutions.append({"supplied":supplied,"error":str(error)})
# Review regressions use real Python model-work and callback handlers.
import copy
review_cases = []
seed = runtime.work_action(lead["id"], {"action":"create","title":"Review fixture","description":"Details"}, actor=lead["id"], epoch=3)
with runtime.db() as db:
    base_work = json.loads(db.execute("SELECT record FROM runtime_work WHERE id=?", (seed["id"],)).fetchone()[0])
for args in [
    {"action":"list","title":"","version":"ignored"},
    {"action":"get","description":None},
    {"action":"history","result":None},
    {"action":"cancel","reason":"Unused description","description":None},
    {"action":"create","reason":""},
    {"action":"cancel","reason":"No longer needed"},
    {"action":"cancel","result":"Wrong field"},
    {"action":"cancel","reason":"x"*32001},
    {"action":"reject","result":"More evidence required"},
    {"action":"reject","result":"x"*32001},
    *[{"action":action, **{field:value}} for action in ["create","update"] for field,value in [
        ("title","x"*160),("title","x"*161),("description","x"*32001),("description",None),("status",None),
        ("dependencies",["x"]*101), ("title", " "), ("status","blocked"), ("description", "x"*32000)]],
]:
    before = copy.deepcopy(base_work)
    if args["action"] == "reject":
        before.update(status="review",owner=lead["id"],results=[{"id":"review-result","agent":lead["id"],"text":"Evidence","created":10.0}])
    args = {"title":"Valid title",**args,"task_id":before["id"]}
    with runtime.db() as db:
        db.execute("DELETE FROM runtime_work")
        runtime.put(db,"work",before)
        db.execute("DELETE FROM runtime_events")
    key = runtime.tool_request_key({"params":{"threadId":lead["threadId"],"callId":f"review-work-{len(review_cases)}"}})
    try:
        with patch("time.time", return_value=20.0):
            value = runtime.model_work(lead["id"],args,key,3)
        expected = {"success":True,"value":value}
    except ValueError as error:
        expected = {"success":False,"error":str(error)}
    with runtime.db() as db:
        after = json.loads(db.execute("SELECT record FROM runtime_work WHERE id=?",(before["id"],)).fetchone()[0])
        events = [dict(row) for row in db.execute("SELECT agent,kind,text FROM runtime_events")]
        created = runtime.work_by_id(db,value["id"],lead["id"]) if args["action"] == "create" and expected["success"] else None
        db.execute("DELETE FROM runtime_operation_receipts WHERE id=?",(key,))
    review_cases.append({"args":args,"before":before,"after":after,"expected":expected,"events":events,"created":created})
with runtime.db() as db:
    db.execute("DELETE FROM runtime_work")
    db.execute("DELETE FROM runtime_events")
# Exact model projection corpus, including an already paginated response.
projections = []
for raw, success in [("字"*12000,True),(json.dumps({"id":"large","description":"z"*32000,"status":"ready"},sort_keys=True,separators=(",",":")),True),("failure"*4000,False), (json.dumps({"outputRef":"saved","offset":0,"totalChars":32000,"text":"q"*20000},sort_keys=True,separators=(",",":")),True)]:
    result={"success":success,"contentItems":[{"type":"inputText","text":raw},{"type":"inputText","text":"[Time awareness] fixed"}]}
    projected=runtime.model_tool_result(lead["id"],"projection",copy.deepcopy(result))
    projections.append({"result":result,"expected":projected,"key":"projection","outcome":"unknown"})
preview = {"command":"printf review","cwd":"/fixture","changes":[{"path":"file.txt","kind":"update"}]}
item={"id":lead["id"]+":preview","agent":lead["id"],"type":"tool","title":"commandExecution","text":json.dumps(preview),"created":10.0}
with runtime.db() as db:
    runtime.item(db, lead["id"], "preview", "tool", json.dumps(preview), "commandExecution", index_search=False)
    item=json.loads(db.execute("SELECT record FROM runtime_items WHERE id=?",(lead["id"]+":preview",)).fetchone()[0])
preview_request={"id":"preview-rpc","method":methods[0],"params":{"threadId":lead["threadId"],"turnId":lead["turnId"],"itemId":"preview"}}
with patch("time.time",return_value=20.0):
    runtime.request(preview_request,"default","connection")
with runtime.db() as db:
    preview_expected=next(r for r in runtime.records(db,"requests") if r.get("rpcId")=="preview-rpc")["preview"]
from native_notifications.dispatch import consume_native_notification
resolved_cases=[]
for turn_state in ["active","completed","cleared"]:
    for status in ["pending","answering","uncertain","blocked"]:
        card={"id":"resolve-card","rpcId":"resolve-rpc","method":methods[0],"params":{"threadId":lead["threadId"]},"agent":lead["id"],"accountKey":"default","connectionId":"connection","epoch":3,"supervisor":{"generation":7},"status":status,"createdAt":10.0}
        actor={**lead,"status":"approval","inFlight":True}
        if turn_state != "active":
            actor.update(turnId=None,inFlight=False,status="completed" if turn_state=="completed" else "approval")
        with runtime.db() as db:
            db.execute("DELETE FROM runtime_requests")
            runtime.put(db,"requests",card)
            runtime.put(db,"agents",actor)
        with patch("time.time",return_value=20.0):
            consume_native_notification(runtime,{"method":"serverRequest/resolved","params":{"threadId":lead["threadId"],"requestId":"resolve-rpc"}},"default","connection")
        with runtime.db() as db:
            resolved_cases.append({"card":card,"expected":runtime.records(db,"requests")[0],"actor":actor,"expectedActor":runtime.agent(lead["id"],db),"turnState":turn_state})
with runtime.db() as db:
    runtime.put(db,"agents",lead)
# A chat message is no longer readable when its private-room member is archived.
room={"id":"private-review","kind":"private","updated":10.0,"members":[lead["id"],worker["id"]]}
with runtime.db() as db:
    runtime.put(db,"rooms",room)
    db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",("review-chat",room["id"],worker["id"],"Private text",10.0,"{}"))
chat_visibility=[]
for archived in [False,True]:
    with runtime.db() as db:
        runtime.put(db,"agents",{**worker,"deletedAt":15.0 if archived else None})
    try:
        value=runtime.model_read(lead["id"],{"output_ref":"chat-message:review-chat"})
        chat_visibility.append({"archived":archived,"success":True,"value":value})
    except ValueError as error:
        chat_visibility.append({"archived":archived,"success":False,"error":str(error)})
with runtime.db() as db:
    runtime.put(db,"agents",worker)
    db.execute("DELETE FROM runtime_rooms WHERE id=?",(room["id"],))
    db.execute("DELETE FROM runtime_chat_messages")
# Leave only the requested admission corpus in the cloned starting state.
with runtime.lock, runtime.db() as db:
    for case in [*dynamic_cases,{"message":warmup}]:
        key = runtime.tool_request_key(case["message"])
        db.execute("DELETE FROM runtime_tool_results WHERE id=?",(key,))
        db.execute("DELETE FROM runtime_tool_requests WHERE id=?",(key,))
        db.execute("DELETE FROM runtime_tool_request_aliases WHERE request=?",(key,))
# Python-written rows are the Rust snapshot source. No hand-built Rust snapshot.
with runtime.lock, runtime.db() as db:
    db.execute("DELETE FROM runtime_requests")
    legacy_card = {"id":"python-written-card", "rpcId":"legacy-rpc", "method":methods[0],
                   "params":{"threadId":lead["threadId"],"turnId":lead["turnId"]},
                   "agent":lead["id"],"accountKey":"default","connectionId":"connection", "epoch":3,
                   "supervisor":{"generation":7}, "status":"pending","createdAt":10.0}
    runtime.put(db,"requests",legacy_card)
    for actor in [lead,worker,foreign]:
        runtime.put(db,"agents",actor)
    db.commit()
with patch("codex_runtime.time.time", return_value=19.0), patch.object(Runtime, "launch_monitor", lambda *_args: None):
    managed_monitor = runtime.monitor(
        lead["id"], {"command":"printf fixture"}, key="managed-monitor", approved=True, epoch=3)
managed_monitor.update(status="running", operation={
    "agent":lead["id"],"epoch":3,"accountKey":"default","connectionId":"connection",
    "requestId":"managed-monitor"})
def sort_json(value):
    if isinstance(value, dict): return {key:sort_json(value[key]) for key in sorted(value)}
    if isinstance(value, list): return [sort_json(item) for item in value]
    return value
managed_monitor = sort_json(managed_monitor)
with runtime.lock, runtime.db() as db:
    runtime.put(db,"monitors",managed_monitor)
runtime.close()

# Managed host tool differential cases run through the real Python tool handler
# on a private clone. The original Python-written template stays the Rust input.
managed_root = Path(sys.argv[-1]) / "managed-tools"
shutil.copytree(root, managed_root)
managed = Quiet(managed_root, lambda *_args, **_kwargs: None)
managed.replies = []
with managed.lock, managed.db() as db:
    for actor in [lead, worker, foreign]:
        managed.put(db, "agents", actor)
    # The cloned Runtime performs Python restart recovery and expires native
    # pending requests. The Rust callback fixture uses the original live
    # Python-written pending card, so retain that original row in its snapshot.
    managed.put(db, "requests", legacy_card)
    stored_monitor = json.loads(db.execute(
        "SELECT record FROM runtime_monitors WHERE id=?", (managed_monitor["id"],)).fetchone()[0])
    managed_monitor = sort_json(stored_monitor)
    managed.put(db, "monitors", managed_monitor)
baseline_canvas = Path(sys.argv[-1]) / "managed-canvas-baseline.sqlite3"
with managed.db() as db:
    baseline = sqlite3.connect(baseline_canvas)
    db.backup(baseline)
    baseline.close()
managed_cases = []
managed_tables = {
    "orchestration_send": ["runtime_events", "runtime_event_meta", "runtime_rooms", "runtime_chat_messages", "runtime_agents", "runtime_complaints"],
    "orchestration_message": ["runtime_events", "runtime_rooms", "runtime_chat_messages", "runtime_complaints"],
    "orchestration_agent_manage": ["runtime_agents", "runtime_events", "runtime_event_meta"],
    "orchestration_complaint": ["runtime_events", "runtime_complaints", "runtime_agents"],
    "orchestration_search": [],
    "orchestration_context": ["runtime_monitors"],
    "orchestration_cancel_monitor": ["runtime_monitors", "runtime_events"],
}
def managed_snapshot(tool):
    names = [*managed_tables[tool], "sync_entities", "sync_documents", "sync_versions", "sync_entity_meta"]
    result = {}
    with managed.db() as db:
        for table in names:
            column_names = [column[1] for column in db.execute(f'PRAGMA table_info("{table}")')]
            columns = len(column_names)
            rows = db.execute(f'SELECT * FROM "{table}" ORDER BY ' + ','.join(str(i) for i in range(1, columns + 1))).fetchall()
            normalized = []
            for row in rows:
                values = []
                for column_name, value in zip(column_names, row):
                    if table == "sync_entities" and column_name == "seq":
                        value = 0
                    if isinstance(value, str):
                        try: value = json.loads(value)
                        except (ValueError, TypeError): pass
                    elif isinstance(value, bytes):
                        value = {"blobHex": value.hex()}
                    # These are model-context bookkeeping updates performed by
                    # Python's callback harness, outside the tool's domain write.
                    if table == "runtime_agents" and column_name == "record" and isinstance(value, dict):
                        value.pop("concurrency", None)
                        value.pop("deliveredMode", None)
                    # The callback's per-process timing and seeded backend
                    # identity are intentionally different across fixtures.
                    if table == "runtime_event_meta" and column_name == "record" and isinstance(value, dict):
                        value.pop("timing", None)
                    if table == "sync_entities" and column_name == "payload" and isinstance(value, dict):
                        if value.get("collection") == "agent" and isinstance(value.get("value"), dict):
                            value["value"].pop("concurrency", None)
                            value["value"].pop("deliveredMode", None)
                            value["value"].pop("serverId", None)
                    values.append(value)
                if table == "sync_entities" and values[0] == "agent":
                    # hash covers the omitted fixture-specific serverId field.
                    values[3] = "normalized-agent-projection-hash"
                normalized.append(values)
            result[table] = normalized
    return result
def managed_call(actor, tool, arguments, index, allow_failure=False):
    message = {"id":f"rpc-managed-{index}","method":"item/tool/call","params":{
        "threadId":actor["threadId"],"turnId":actor["turnId"],"callId":f"managed-{index}",
        "tool":tool,"arguments":arguments}}
    with patch("time.time", return_value=20.0):
        managed.dynamic(message,"default","connection")
    reply = managed.replies[-1]
    managed_cases.append({"actor":actor["id"],"tool":tool,"arguments":arguments,
                          "callId":message["params"]["callId"],"result":reply["result"],
                          "rows":managed_snapshot(tool),
                          "remoteOrigin":bool(actor.get("remoteOrigin"))})
    if not reply["result"]["success"] and not allow_failure:
        raise AssertionError(f"{tool} Python corpus case failed: {reply['result']}")
    return json.loads(reply["result"]["contentItems"][0]["text"]) if reply["result"]["success"] else None
managed_call(lead,"orchestration_send",{"agent_id":worker["id"],"text":"descendant input"},0)
managed_call(worker,"orchestration_send",{"agent_id":"parent","text":"parent alias"},1)
managed_call(worker,"orchestration_send",{"agent_id":lead["id"],"text":"parent id"},2)
managed_call(worker,"orchestration_send",{"agent_id":"broadcast","text":"team notice"},3)
managed_call(worker,"orchestration_send",{"agent_id":foreign["id"],"text":"peer refusal"},4,allow_failure=True)
complaint = managed_call(worker,"orchestration_complaint",{"action":"submit","text":"Need help"},5)
managed_call(lead,"orchestration_complaint",{"action":"read","complaint_id":complaint["id"]},6)
managed_call(lead,"orchestration_complaint",{"action":"respond","complaint_id":complaint["id"],"text":"Investigating","status":"in_progress"},7)
managed_call(lead,"orchestration_search",{"query":"fixture"},8)
managed_call(lead,"orchestration_context",{"topic":"monitor","id":managed_monitor["id"]},9)
managed_call(lead,"orchestration_cancel_monitor",{"monitor_id":managed_monitor["id"]},10)
# A lead's message to the user is a user-addressed message record. Python
# resolves an orchestration_send agent_id, so "user" is refused there.
managed_call(lead,"orchestration_send",{"agent_id":"user","text":"Report for the user"},13,allow_failure=True)
managed_call(lead,"orchestration_message",{"target":"user","text":"  Second report  "},14)
managed_call(worker,"orchestration_message",{"target":"user","text":"Worker report"},15,allow_failure=True)
# Event waits: park, list, emit once per event name, replay, and cancel.
parked = resolver_agents[0]["id"]
cancelled = resolver_agents[1]["id"]
managed_call(lead,"orchestration_agent_manage",{"action":"park","agent_id":parked,"event":"deploy-ready"},16)
managed_call(worker,"orchestration_agent_manage",{"action":"park","agent_id":worker["id"],"event":"deploy-ready"},17,allow_failure=True)
managed_call(lead,"orchestration_agent_manage",{"action":"list_parked"},18)
managed_call(lead,"orchestration_agent_manage",{"action":"park","agent_id":parked,"event":"deploy-ready"},19)
managed_call(worker,"orchestration_agent_manage",{"action":"emit_event","event":"deploy-ready","request_id":"emit-1"},20,allow_failure=True)
managed_call(lead,"orchestration_agent_manage",{"action":"emit_event","event":"deploy-ready","request_id":"emit-1"},21)
managed_call(lead,"orchestration_agent_manage",{"action":"emit_event","event":"deploy-ready","request_id":"emit-1"},22)
managed_call(lead,"orchestration_agent_manage",{"action":"emit_event","event":"deploy-ready","request_id":"emit-2"},23)
managed_call(lead,"orchestration_agent_manage",{"action":"park","agent_id":parked,"event":"deploy-ready"},24,allow_failure=True)
managed_call(lead,"orchestration_agent_manage",{"action":"park","agent_id":cancelled,"event":"second event"},25,allow_failure=True)
managed_call(lead,"orchestration_agent_manage",{"action":"park","agent_id":cancelled,"event":"second-event"},26)
managed_call(lead,"orchestration_agent_manage",{"action":"cancel_park","agent_id":cancelled},27)
managed_call(lead,"orchestration_agent_manage",{"action":"cancel_park","agent_id":cancelled},28)
managed_call(lead,"orchestration_agent_manage",{"action":"list"},29)
managed_call(lead,"orchestration_agent_manage",{"action":"list_archived"},30)
managed_call(worker,"orchestration_agent_manage",{"action":"list"},31,allow_failure=True)
managed_call(lead,"orchestration_agent_manage",{"action":"inspect","agent_id":parked,"unassign_work":True},32,allow_failure=True)
managed_call(lead,"orchestration_agent_manage",{"action":"explode"},33,allow_failure=True)
remote_worker = {**worker,"remoteOrigin":"source-server"}
with managed.lock, managed.db() as db:
    managed.put(db,"agents",remote_worker)
managed_call(remote_worker,"orchestration_search",{"query":"fixture"},11)
managed_call(remote_worker,"orchestration_context",{"topic":"monitor","id":managed_monitor["id"]},12)
with managed.lock, managed.db() as db:
    managed.put(db,"agents",worker)
deleted_lead = {**lead,"deletedAt":20.0}
with managed.lock, managed.db() as db:
    managed.put(db,"agents",deleted_lead)
    stored_deleted_lead = managed.agent(worker["rootId"], db)
    if not stored_deleted_lead.get("deletedAt"):
        raise AssertionError(f"deleted lead was not stored: {stored_deleted_lead!r}")
deleted_root_before = managed_snapshot("orchestration_complaint")
try:
    managed.complaint(worker["id"], {"action":"read"}, "deleted-root", worker["epoch"])
    deleted_root_result = {"success":True}
except ValueError as error:
    deleted_root_result = {"success":False,"error":str(error)}
deleted_root_after = managed_snapshot("orchestration_complaint")
if deleted_root_result["success"] or deleted_root_before != deleted_root_after:
    raise AssertionError(f"Python must refuse complaint access without writes: {deleted_root_result!r}")
managed_root_deleted_refusal = {"actor":worker["id"],"tool":"orchestration_complaint",
                               "callId":"deleted-root",
                               "arguments":{"action":"read"},"result":deleted_root_result,
                               "before":deleted_root_before,"rows":deleted_root_after}
managed.close()
shutil.copy2(baseline_canvas, managed_root / "canvas.sqlite3")
shutil.copy2(baseline_canvas, root / "canvas.sqlite3")

# orchestration_read and orchestration_context cases run on their own clone.
# Rust copies this clone's databases and payload blobs, so both read the same
# Python-written events, transcript items, receipts and blobs.
reads_root = Path(sys.argv[-1]) / "model-reads"
shutil.copytree(root, reads_root)
reads = Quiet(reads_root, lambda *_args, **_kwargs: None)
reads.replies = []
with reads.lock, reads.db() as db:
    for actor in [lead, worker, foreign]:
        reads.put(db, "agents", actor)
    for event_id, agent_id, kind, text, meta in [
        ("read-event-long", lead["id"], "monitor_exit", "first line\n" + "字" * 40000 + " needle tail",
         {"contextManifest": {"epoch": ["thread-0", 0]}, "source": "fixture"}),
        ("read-event-peer", lead["id"], "followup", "From another team", {"senderId": foreign["id"]}),
        ("read-event-team", lead["id"], "followup", "From my worker", {"senderId": worker["id"]}),
        ("read-event-foreign", foreign["id"], "monitor_exit", "Not yours", None),
    ]:
        db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id) VALUES(?,?,?,?,?,?,?,?)",
                   (event_id, agent_id, kind, text, "delivered", 12.5, 3, "turn-0"))
        if meta is not None:
            db.execute("INSERT INTO runtime_event_meta(id,record) VALUES(?,?)", (event_id, json.dumps(meta)))
    command = {"command": "printf read", "status": "completed", "exitCode": 0,
               "aggregatedOutput": "out\n" * 9000 + "done"}
    reads.item(db, lead["id"], "read-command", "tool", json.dumps(command), "commandExecution", index_search=False)
    reads.item(db, lead["id"], "read-command-lost", "tool", json.dumps(command), "commandExecution", index_search=False)
    db.execute("DELETE FROM runtime_item_fulltext WHERE id=?", (lead["id"] + ":read-command-lost",))
large_message = {"id": "read-large", "method": "item/tool/call", "params": {
    "threadId": lead["threadId"], "turnId": lead["turnId"], "callId": "read-large",
    "tool": "orchestration_task", "arguments": {"action": "list"}}}
with patch("time.time", return_value=20.0):
    large_key = reads.reserve_tool_request(large_message, "default", "connection")["id"]
reads.begin_tool_request(large_key)
large_result = {"success": True, "contentItems": [
    {"type": "inputText", "text": "large " + "x" * 70000 + " needle " + "y" * 5000}]}
from codex_payloads import externalize_result
with reads.lock, reads.db() as db:
    db.execute("INSERT OR IGNORE INTO runtime_tool_results VALUES (?,?)",
               (large_key, json.dumps(externalize_result(reads.root, db, large_result))))
    reads.finish_tool_request(large_key, large_result, outcome="applied", db=db)
    reads.item(db, lead["id"], "read-large", "tool", json.dumps({"id": "read-large"}), "dynamicToolCall",
               index_search=False)
    if "_payloadBlobs" not in json.loads(db.execute("SELECT result FROM runtime_tool_results WHERE id=?",
                                                    (large_key,)).fetchone()[0]):
        raise AssertionError("the large fixture result must be saved as a payload blob")
model_read_cases = []
for output_ref, extra in [
    ("event:read-event-long", {}),
    ("event:read-event-long", {"offset": 30000}),
    ("event:read-event-long", {"contains": "needle"}),
    ("event:read-event-long", {"contains": "absent"}),
    ("event:read-event-long", {"offset": 10 ** 6}),
    ("event:read-event-peer", {}),
    ("event:read-event-team", {}),
    ("event:read-event-foreign", {}),
    (lead["id"] + ":read-command", {}),
    (lead["id"] + ":read-command", {"contains": "done"}),
    (lead["id"] + ":read-command", {"contains": "absent"}),
    (lead["id"] + ":read-command-lost", {}),
    (large_key, {}),
    (large_key, {"offset": 30000}),
    (large_key, {"contains": "needle"}),
    (lead["id"] + ":read-large", {"offset": 60000}),
    (lead["id"] + ":missing", {}),
]:
    arguments = {"output_ref": output_ref, **extra}
    try:
        model_read_cases.append({"arguments": arguments, "success": True,
                                 "value": reads.model_read(lead["id"], arguments)})
    except ValueError as error:
        model_read_cases.append({"arguments": arguments, "success": False, "error": str(error)})
# Context rows are stored with sorted keys, so Python's insertion-order version
# digest and the Rust digest of the same value are equal.
submitted = reads.complaint(worker["id"], {"action": "submit", "text": "Need context"}, "context-complaint", worker["epoch"])
answered = reads.complaint(worker["id"], {"action": "submit", "text": "Already answered"}, "context-answered", worker["epoch"])
reads.complaint(lead["id"], {"action": "respond", "complaint_id": answered["id"], "text": "Done", "status": "in_progress"},
                "context-answer", lead["epoch"])
profile = reads.profiles({"name": "Context reviewer", "role": "reviewer", "model": "gpt-5.5", "effort": "high",
                          "instructions": "Check the diff"})
with reads.lock, reads.db() as db:
    for complaint_id in [submitted["id"], answered["id"]]:
        stored = json.loads(db.execute("SELECT record FROM runtime_complaints WHERE id=?", (complaint_id,)).fetchone()[0])
        reads.put(db, "complaints", sort_json(stored))
    db.execute("UPDATE runtime_profiles SET record=? WHERE id=?", (json.dumps(sort_json(profile)), profile["id"]))
    db.execute("DELETE FROM runtime_events WHERE kind IN ('complaint','complaint_response')")
context_cases = []
for actor, topic in [(lead, "complaints"), (worker, "complaints"), (lead, "profiles"), (lead, "tools"),
                     (worker, "tools"), (lead, "panel"), (lead, "background"), (worker, "panel"),
                     (worker, "background"), (lead, "unknown")]:
    try:
        context_cases.append({"actor": actor["id"], "topic": topic, "success": True,
                              "value": reads.model_context(actor["id"], {"topic": topic})})
    except ValueError as error:
        context_cases.append({"actor": actor["id"], "topic": topic, "success": False, "error": str(error)})
model_reads = {"template": str(reads_root), "reads": model_read_cases, "contexts": context_cases,
               "panelGuidance": reads.panel_guidance()}
reads.close()
print(json.dumps({"modelReads":model_reads,"agents":[lead,worker,foreign],"answers":answers,"reservations":reservations,"chat":{"page":chat_page,"expected":chat_expected},"dynamic":dynamic_cases,"resolutions":resolutions,"legacyCard":legacy_card,"reviewCases":review_cases,"baseWork":base_work,"projections":projections,"preview":{"item":item,"request":preview_request,"expected":preview_expected},"resolved":resolved_cases,"chatVisibility":chat_visibility,"managedToolCases":managed_cases,"managedRootDeletedRefusal":managed_root_deleted_refusal,"template":str(root)},ensure_ascii=False))
