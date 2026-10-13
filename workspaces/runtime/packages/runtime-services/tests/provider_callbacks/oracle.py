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
print(json.dumps({"agents":[lead,worker,foreign],"answers":answers,"reservations":reservations,"chat":{"page":chat_page,"expected":chat_expected},"dynamic":dynamic_cases,"resolutions":resolutions,"legacyCard":legacy_card,"template":str(root)},ensure_ascii=False))
