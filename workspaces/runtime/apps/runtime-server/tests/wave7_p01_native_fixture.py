#!/usr/bin/env python3
"""Bounded scripted provider: retain stdin and report only turn/start to its owner."""
import json
import os
import pathlib
import signal
import socket
import sys

root = pathlib.Path(__file__).resolve().parent
turn_count = 0
(root / "startup-cwd").write_text(os.getcwd(), encoding="utf-8")

def report(name, value):
    path = root / f"{name}.sock"
    if path.exists():
        with socket.socket(socket.AF_UNIX) as observer:
            observer.connect(str(path))
            observer.sendall((json.dumps(value) + "\n").encode())

if observation_socket := os.environ.get("FIXTURE_CWD_SOCKET"):
    with socket.socket(socket.AF_UNIX) as observer:
        observer.connect(observation_socket)
        observer.sendall(
            (json.dumps({"pid": os.getpid(), "cwd": os.getcwd()}) + "\n").encode()
        )
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    client = request.get("params", {}).get("clientInfo", {}).get("name")
    account_provider = client == "codex_agents_canvas"
    if method == "initialize" and account_provider:
        launch_count_path = root / "account-launch-count"
        launch_count = int(launch_count_path.read_text(encoding="utf-8")) + 1 if launch_count_path.exists() else 1
        launch_count_path.write_text(str(launch_count), encoding="utf-8")
        report("launch", {"pid": os.getpid(), "count": launch_count})
        if launch_count == 1 and (root / "crash-first-initialize").exists():
            os._exit(17)
    if "id" not in request:
        continue
    if method == "thread/resume":
        result = {"thread": {"id": request["params"]["threadId"]}}
    elif method == "turn/start":
        turn_count_path = root / "turn-count"
        turn_count = int(turn_count_path.read_text(encoding="utf-8")) + 1 if turn_count_path.exists() else 1
        (root / "turn-count").write_text(str(turn_count), encoding="utf-8")
        report("turn", {"pid": os.getpid(), "cwd": os.getcwd(), "count": turn_count,
                         "priority": os.getpriority(os.PRIO_PROCESS, 0), "request": request})
        if turn_count == 1 and (root / "hang-first-turn").exists():
            signal.pause()
        result = {"turn": {"id": "fixture-turn"}}
    elif method == "fixture/barrier":
        with socket.socket(socket.AF_UNIX) as observer:
            observer.connect(str(root / "turn.sock"))
            observer.sendall((json.dumps({"method": method, "count": turn_count}) + "\n").encode())
        result = {"count": turn_count}
    elif method == "model/list":
        result = {"data": [{"id": "fake-model", "model": "fake-model", "displayName": "Fixture",
                            "supportedReasoningEfforts": [], "defaultReasoningEffort": "medium"}]}
    else:
        result = {}
    print(json.dumps({"id": request["id"], "result": result}), flush=True)
    if method == "initialize" and account_provider and (root / "emit-generation-notification").exists():
        print(json.dumps({"method": "fixture/generation", "params": {"pid": os.getpid()}}), flush=True)
    if method == "turn/start" and os.environ.get("STUDIO_W7_P01_COMPLETE_TURN") == "1":
        params = request["params"]
        thread_id = params["threadId"]
        turn_id = "fixture-turn"
        item_id = "fixture-assistant-item"
        for event in (
            {"method": "turn/started", "params": {"threadId": thread_id,
             "turn": {"id": turn_id, "status": "inProgress"}}},
            {"method": "item/started", "params": {"threadId": thread_id,
             "turnId": turn_id, "item": {"id": item_id, "type": "agentMessage", "text": ""}}},
            {"method": "item/agentMessage/delta", "params": {"threadId": thread_id,
             "turnId": turn_id, "itemId": item_id, "delta": "fixture assistant response"}},
            {"method": "item/completed", "params": {"threadId": thread_id,
             "turnId": turn_id, "item": {"id": item_id, "type": "agentMessage",
             "text": "fixture assistant response"}}},
            {"method": "turn/completed", "params": {"threadId": thread_id,
             "turn": {"id": turn_id, "status": "completed"}}},
        ):
            print(json.dumps(event), flush=True)
