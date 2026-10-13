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
    if mode == "exit":
        sys.exit(0)
    if mode == "normal":
        send({"id": 9999, "result": {"orphan": True}})
        send({"method": "turn/started", "params": {"threadId": "fake-thread"}})
        send({"id": request["id"], "result": {"models": []}})
    elif mode == "late":
        send({"id": request["id"], "result": {"late": True}})
