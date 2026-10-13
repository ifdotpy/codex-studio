#!/usr/bin/python3
"""Bounded scripted provider: retain stdin and report only turn/start to its owner."""
import json
import os
import pathlib
import socket
import sys

root = pathlib.Path(__file__).resolve().parent
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if "id" not in request:
        continue
    if method == "thread/resume":
        result = {"thread": {"id": request["params"]["threadId"]}}
    elif method == "turn/start":
        with socket.socket(socket.AF_UNIX) as observer:
            observer.connect(str(root / "turn.sock"))
            observer.sendall((json.dumps({"pid": os.getpid(), "request": request}) + "\n").encode())
        result = {"turn": {"id": "fixture-turn"}}
    elif method == "model/list":
        result = {"data": [{"id": "fake-model", "model": "fake-model", "displayName": "Fixture",
                            "supportedReasoningEfforts": [], "defaultReasoningEffort": "medium"}]}
    else:
        result = {}
    print(json.dumps({"id": request["id"], "result": result}), flush=True)
