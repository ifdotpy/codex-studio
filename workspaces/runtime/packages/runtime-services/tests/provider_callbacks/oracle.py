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
from unittest.mock import patch
sys.path.insert(0, str(SERVER / "src"))
from codex_runtime import Runtime
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
runtime.close()
print(json.dumps({"agents":[lead,worker,foreign],"answers":answers,"reservations":reservations,"chat":{"page":chat_page,"expected":chat_expected},"dynamic":dynamic_cases,"resolutions":resolutions,"legacyCard":legacy_card,"reviewCases":review_cases,"baseWork":base_work,"projections":projections,"preview":{"item":item,"request":preview_request,"expected":preview_expected},"resolved":resolved_cases,"chatVisibility":chat_visibility,"template":str(root)},ensure_ascii=False))
