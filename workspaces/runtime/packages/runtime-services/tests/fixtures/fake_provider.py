import json
import os
import sys


def send(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


initialize = json.loads(sys.stdin.readline())
send({"id": initialize["id"], "result": {"protocolVersion": 1}})
mode = os.environ["FAKE_PROVIDER_MODE"]

for line in sys.stdin:
    request = json.loads(line)
    if request.get("method") == "initialized":
        continue
    if request.get("method") == "thread/resume":
        send({"id": request["id"], "result": {"thread": {"id": request["params"]["threadId"]}}})
        continue
    if request.get("method") == "turn/start":
        params = request["params"]
        turn_id = "turn-" + params["clientUserMessageId"]
        if mode == "turn-lost":
            sys.exit(0)
        send({"id": request["id"], "result": {"turn": {"id": turn_id}}})
        if mode == "turn-complete":
            send({"method": "turn/started", "params": {
                "threadId": "unknown-thread", "turn": {"id": "unowned-turn"}}})
        send({"method": "turn/started", "params": {
            "threadId": params["threadId"], "turn": {"id": turn_id}}})
        if mode == "turn-complete":
            send({"id": "fake-callback", "method": "thread/loaded/list", "params": {}})
            callback = json.loads(sys.stdin.readline())
            if (callback.get("id") != "fake-callback"
                    or callback.get("error", {}).get("code") != -32601):
                sys.exit(1)
            send({"method": "turn/completed", "params": {
                "threadId": params["threadId"],
                "turn": {"id": turn_id, "status": "completed"}}})
            sys.exit(0)
        if mode == "turn-exit-mid":
            sys.exit(0)
        if mode == "turn-hang":
            for _line in sys.stdin:
                pass
            sys.exit(0)
    if mode == "exit":
        sys.exit(0)
    if mode == "normal":
        send({"id": 9999, "result": {"orphan": True}})
        send({"method": "turn/started", "params": {"threadId": "fake-thread"}})
        send({"id": request["id"], "result": {"models": []}})
    elif mode == "late":
        send({"id": request["id"], "result": {"late": True}})
