"""Emit archive files with the Python writer for the Rust read corpus."""

import contextlib
import json
import sqlite3
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard()

SERVER = Path(sys.argv[1])
ROOT = Path(sys.argv[2])
sys.path.insert(0, str(SERVER))

from codex_portable_history import export_history, history_context  # noqa: E402


class Runtime:
    def __init__(self, root):
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        with self.db() as db:
            db.executescript(
                """
                CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);
                CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY, body TEXT NOT NULL);
                CREATE TABLE runtime_events(
                  id TEXT PRIMARY KEY, agent TEXT, kind TEXT, text TEXT, status TEXT,
                  created REAL, epoch INTEGER, turn_id TEXT, error TEXT);
                """
            )

    @contextlib.contextmanager
    def db(self):
        connection = sqlite3.connect(self.root / "state.sqlite3")
        connection.row_factory = sqlite3.Row
        try:
            with connection:
                yield connection
        finally:
            connection.close()


def case(name, item=None, full_text=None, event=None):
    runtime = Runtime(ROOT / name)
    if item is not None:
        with runtime.db() as db:
            db.execute(
                "INSERT INTO runtime_items VALUES(?,?,?,?)",
                (item["id"], "agent-1", json.dumps(item, ensure_ascii=False), 1.25),
            )
            if full_text is not None:
                db.execute(
                    "INSERT INTO runtime_item_fulltext VALUES(?,?)", (item["id"], full_text)
                )
            if event is not None:
                db.execute(
                    "INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)",
                    (event["id"], "agent-1", event["kind"], event["text"], "delivered",
                     2.5, 7, None, None),
                )
    agent = {
        "id": "agent-1",
        "epoch": 7,
        "accountKey": "account-1",
        "threadId": None,
    }
    descriptor = export_history(runtime, agent, "transfer-1", None)
    return {
        "name": name,
        "root": str(runtime.root),
        "descriptor": descriptor,
        "context": history_context(runtime, descriptor),
    }


long_text = "older 😀 line\n" + ("tailé" * 5000)
cases = [
    case("empty"),
    case(
        "ordinary-and-input",
        {
            "id": "item-1",
            "role": "assistant",
            "text": "short fallback",
            "assets": [{"path": '/tmp/asset "quoted" 日本語.png'}],
            "inputs": [{"id": "event-1", "text": "placeholder", "truncated": True}],
        },
        event={"id": "event-1", "kind": "user", "text": "original input 😀"},
    ),
    case(
        "fulltext-and-context-bound",
        {"id": "item-2", "role": "assistant", "text": "stub", "truncated": True},
        full_text=long_text,
    ),
    case(
        "restored-item",
        {"id": "item-3", "role": "assistant", "text": "must not enter excerpt", "afterRestore": True},
    ),
]


class Native:
    def call(self, method, params, timeout=30):
        if method == "thread/read":
            return {"thread": {"id": params["threadId"], "turns": []}}
        if method == "thread/turns/list":
            return {
                "data": [{
                    "id": "turn-1",
                    "threadId": params["threadId"],
                    "itemsView": "full",
                    "items": [{
                        "id": "native-item-1",
                        "type": "agentMessage",
                        "threadId": params["threadId"],
                        "turnId": "turn-1",
                        "text": "Python-written native assistant text",
                    }],
                }],
                "nextCursor": None,
            }
        raise AssertionError(f"unexpected native method {method}")


native_runtime = Runtime(ROOT / "native-pages")
native_agent = {
    "id": "agent-native",
    "epoch": 8,
    "accountKey": "account-native",
    "threadId": "thread-native",
    "provider": "claude",
    "workerDefaults": {"model": "fixture-model"},
    "claudeOptions": {"thinking": "enabled"},
}
native_descriptor = export_history(
    native_runtime, native_agent, "transfer-native", Native()
)
cases.append(
    {
        "name": "native-pages",
        "root": str(native_runtime.root),
        "descriptor": native_descriptor,
        "context": history_context(native_runtime, native_descriptor),
    }
)
print(json.dumps(cases, ensure_ascii=False, separators=(",", ":")))
