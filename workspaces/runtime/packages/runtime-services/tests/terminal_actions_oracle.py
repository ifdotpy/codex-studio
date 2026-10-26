"""Compare terminal actions using the production Python manager and SQLite rows."""
import concurrent.futures
import itertools
import json
import uuid
from pathlib import Path
import shutil
import sqlite3
import sys
import threading
from types import SimpleNamespace

source, root, rust_root = map(Path, sys.argv[1:4])
sys.path.insert(0, str(source))
from codex_canvas import Canvas
from codex_terminals import TerminalManager
import codex_terminals


class Runtime:
    def __init__(self, canvas):
        self.canvas = canvas
        self.lock = threading.RLock()

    def db(self):
        return self.canvas.connect()

    @staticmethod
    def checked_actor(_db, agent):
        if agent != "agent":
            raise ValueError("unknown agent")
        return {"id": "agent", "cwd": str(root)}


class Server:
    def __init__(self):
        self.closed = False
        self.transport_error = None
        self.proc = SimpleNamespace(poll=lambda: None)
        self.calls = []

    def submit(self, method, params, operation_id=None):
        future = concurrent.futures.Future()
        future.set_result(None)
        result = (method, params, future)
        self.calls.append({"method": method, "params": params, "operationId": operation_id})
        return result

    @staticmethod
    def on_result(submitted, callback):
        callback(submitted[2])

    @staticmethod
    def wait(submitted, timeout=None):
        submitted[2].result(timeout=timeout)

    def call(self, method, params, timeout=None):
        self.calls.append({"method": method, "params": params, "timeout": timeout})


def tables(manager):
    result = {}
    with manager.db() as db:
        for name in (
            "user_terminals", "user_terminal_receipts", "user_terminal_output",
            "user_terminal_event_cursor", "user_terminal_decoder_state",
        ):
            result[name] = [list(row) for row in db.execute(f'SELECT * FROM "{name}" ORDER BY rowid')]
    return result


def backup(source_root, destination):
    destination.mkdir(parents=True, exist_ok=True)
    for name in ("canvas.sqlite3", "analytics.sqlite3"):
        with sqlite3.connect(source_root / name) as source_db:
            with sqlite3.connect(destination / name) as destination_db:
                source_db.backup(destination_db)


def main():
    clock_ticks = itertools.count()
    codex_terminals.time.time = lambda: 1700000000.25 + next(clock_ticks)
    uuid.uuid4 = lambda: uuid.UUID("f48d7c00-0000-4000-8000-000000000001")
    canvas = Canvas(root)
    manager = TerminalManager(root)
    server = Server()
    manager.server = server
    manager.connection = object()
    manager.connect = lambda: server
    manager._publish = lambda *_resources: None
    runtime = Runtime(canvas)

    create_request = {"id": "action-create", "agent": "agent", "cols": 91, "rows": 33}
    backup(root, rust_root)
    results = [{"action": "create", "result": manager.create(runtime, create_request), "tables": tables(manager)}]
    terminal_id = results[0]["result"]["id"]
    for label, action, data in (
        ("rename", "rename", {"id": terminal_id, "title": "  Shell  "}),
        ("input", "input", {"id": terminal_id, "text": "hi🙂", "request_id": "actions-input"}),
        ("input-replay", "input", {"id": terminal_id, "text": "hi🙂", "request_id": "actions-input"}),
        ("resize", "resize", {"id": terminal_id, "cols": 91, "rows": 33}),
        ("close", "close", {"id": terminal_id}),
    ):
        results.append({"action": label, "result": manager.action(action, data), "tables": tables(manager)})
    changed = {"id": terminal_id, "text": "different", "request_id": "actions-input"}
    try:
        result = manager.action("input", changed)
        error = None
    except ValueError as exception:
        result = None
        error = "receipt_conflict" if str(exception) == "This request id has different content" else str(exception)
    results.append({"action": "input-reject", "result": result, "error": error, "tables": tables(manager)})
    print(json.dumps({"actions": results, "wire": server.calls}, ensure_ascii=False, sort_keys=True))


if __name__ == "__main__":
    main()
