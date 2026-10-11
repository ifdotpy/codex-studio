#!/usr/bin/env python3
"""Small JSON-RPC provider which replays fixture frames over stdio."""
import json
import os
from pathlib import Path
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent / "server"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
from provider_replay_dispatch import dispatch_rpc_lines
from rpc_replay_contract import EMPTY_RESULT_METHODS

fixture = json.load(open(sys.argv[1], encoding="utf-8"))
reply_frames = {**json.load(open(sys.argv[2], encoding="utf-8")), **fixture.get("rpcReplies", {})}
thread = None
thread_count = 0
turn_number = 0
lock = threading.Lock()
child_finished = threading.Event()
tool_responses = {}

def send(message):
    with lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def event_frame(frame, thread_id, turn_id):
    value = json.loads(json.dumps(frame))
    value = json.dumps(value).replace("$threadId", thread_id).replace("$turnId", turn_id)
    message = json.loads(value)
    if message.get("method") == "item/tool/call" and message.get("id") is not None:
        tool_responses[str(message["id"])] = threading.Event()
    send(message)


def recorded_reply(method, request, thread_id=None, turn_id=None):
    recorded = reply_frames.get(method)
    if not recorded or recorded.get("direction") != "in" or recorded.get("responseFor") != method:
        raise AssertionError(f"Missing recorded provider reply for {method}")
    value = json.dumps(recorded["message"])
    value = value.replace("$threadId", thread_id or "replay-thread-1")
    value = value.replace("$turnId", turn_id or "replay-turn-1")
    value = value.replace("$model", request.get("params", {}).get("model", "fixture-model"))
    message = json.loads(value)
    return {"id": request["id"], **message}


def handle(line):
    global thread, thread_count, turn_number
    request = json.loads(line)
    method = request.get("method")
    if method is None and ("result" in request or "error" in request):
        # The app-server replies to fixture-injected JSON-RPC requests on this
        # same stdio stream; those replies are observations, not new RPC calls.
        response = tool_responses.get(str(request.get("id")))
        if response is not None:
            response.set()
        return
    if method == "initialized":
        return
    if "id" not in request:
        raise AssertionError(f"Unexpected provider RPC notification in replay: {method!r}")
    params = request.get("params") or {}
    if method in {"initialize", "config/read", "skills/extraRoots/set", "model/list",
              "account/rateLimits/read", "thread/loaded/list", "thread/resume", "thread/read",
                  "thread/turns/list"}:
        send(recorded_reply(method, request, thread))
        return
    if method == "thread/start":
        with lock:
            thread_count += 1
            thread_id = f"replay-thread-{thread_count}"
            thread = thread_id
        send(recorded_reply(method, request, thread_id))
        return
    if method == "turn/start":
        with lock:
            turn_number += 1
            turn_id = f"replay-turn-{turn_number}"
        thread_id = params.get("threadId") or thread or "replay-thread-1"
        send({"method": "turn/started", "params": {"threadId": thread_id,
              "turn": {"id": turn_id, "status": "inProgress"}}})
        if fixture.get("supervisorReplay"):
            from pathlib import Path
            Path(os.environ["PROVIDER_REPLAY_PHASE"]).touch()
            release = Path(os.environ["PROVIDER_REPLAY_RELEASE"])
            deadline = time.monotonic() + 30
            while not release.exists() and time.monotonic() < deadline:
                time.sleep(.01)
        if fixture.get("parentChildOrder") and thread_id.endswith("-1"):
            child_finished.wait(8)
        selected_events = fixture.get("eventsByThread", {}).get(
            thread_id, fixture.get("events", []),
        )
        if fixture.get("parentChildOrder") and thread_id.endswith("-2"):
            selected_events = fixture.get("childEvents", selected_events)
        deferred_completion = []
        for frame in selected_events:
            if (fixture.get("waitForToolResponse")
                    and frame.get("method") == "turn/completed"):
                deferred_completion.append(frame)
                continue
            event_frame(frame, thread_id, turn_id)
        if fixture.get("parentChildOrder") and thread_id.endswith("-2"):
            child_finished.set()
        if fixture.get("parentChildOrder") and thread_id.endswith("-1"):
            selected_parent_events = fixture.get("parentEvents", [])
            for frame in selected_parent_events:
                event_frame(frame, thread_id, turn_id)
        if fixture.get("waitForToolResponse"):
            for frame in selected_events:
                if frame.get("method") == "item/tool/call" and frame.get("id") is not None:
                    response = tool_responses[str(frame["id"])]
                    if not response.wait(3):
                        raise AssertionError(f"App-server did not answer fixture tool call {frame['id']}")
        for frame in deferred_completion:
            event_frame(frame, thread_id, turn_id)
        if fixture.get("lostStartReply"):
            return
        send(recorded_reply(method, request, thread, turn_id))
        return
    if method in EMPTY_RESULT_METHODS:
        send({"id": request["id"], "result": {}})
        return
    message = f"Unexpected provider RPC method in replay: {method!r}"
    send({"id": request["id"], "error": {"code": -32601, "message": message}})
    raise AssertionError(message)


def capture_received(line):
    received = os.environ.get("CODEX_FAKE_RECEIVED")
    if received:
        with Path(received).open("a", encoding="utf-8") as capture:
            capture.write(line)
            capture.flush()


# turn/start handlers emit fixture notifications while other requests arrive:
# the parent/child replay completes the child first, the supervisor replay
# keeps a turn pending across reattach, and tool-call fixtures wait for the
# app-server's reply. Ordinary provider RPCs keep stdin order.
dispatch_rpc_lines(sys.stdin, handle, {"turn/start"}, capture_received)
