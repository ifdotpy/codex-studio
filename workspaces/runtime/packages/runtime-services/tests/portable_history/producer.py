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
    case(
        "restored-empty-list",
        {"id": "item-4", "role": "assistant", "text": "empty list stays in excerpt", "afterRestore": []},
    ),
    case(
        "restored-empty-object",
        {"id": "item-5", "role": "assistant", "text": "empty object stays in excerpt", "afterRestore": {}},
    ),
]


class Native:
    def __init__(self, *, item_page=False, unmaterialized=False):
        self.item_page = item_page
        self.unmaterialized = unmaterialized

    def call(self, method, params, timeout=30):
        if method == "thread/read":
            return {"thread": {"id": params["threadId"], "turns": []}}
        if method == "thread/turns/list":
            if self.unmaterialized:
                from codex_native_errors import NativeRpcError

                thread_id = params["threadId"]
                raise NativeRpcError({
                    "code": -32600,
                    "message": f"thread {thread_id} is not materialized yet; thread/turns/list is unavailable before first user message",
                })
            return {
                "data": [{
                    "id": "turn-1",
                    "threadId": params["threadId"],
                    "itemsView": "summary" if self.item_page else "full",
                    "items": [] if self.item_page else [{
                        "id": "native-item-1",
                        "type": "agentMessage",
                        "threadId": params["threadId"],
                        "turnId": "turn-1",
                        "text": "Python-written native assistant text",
                    }],
                }],
                "nextCursor": None,
            }
        if method == "thread/turns/items/list" and self.item_page:
            return {
                "data": [{
                    "turnId": "turn-1",
                    "item": {
                        "id": "native-item-page-1",
                        "type": "agentMessage",
                        "threadId": params["threadId"],
                        "turnId": "turn-1",
                        "text": "Python-written native item-page text",
                    },
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

item_page_runtime = Runtime(ROOT / "native-item-page")
item_page_agent = {
    **native_agent,
    "id": "agent-item-page",
    "accountKey": "account-item-page",
    "threadId": "thread-item-page",
}
item_page_descriptor = export_history(
    item_page_runtime, item_page_agent, "transfer-item-page", Native(item_page=True)
)
cases.append({
    "name": "native-item-page",
    "root": str(item_page_runtime.root),
    "descriptor": item_page_descriptor,
    "context": history_context(item_page_runtime, item_page_descriptor),
})

unmaterialized_runtime = Runtime(ROOT / "native-unmaterialized")
unmaterialized_agent = {
    **native_agent,
    "id": "agent-unmaterialized",
    "accountKey": "account-unmaterialized",
    "threadId": "thread-unmaterialized",
}
unmaterialized_descriptor = export_history(
    unmaterialized_runtime,
    unmaterialized_agent,
    "transfer-unmaterialized",
    Native(unmaterialized=True),
)
cases.append({
    "name": "native-unmaterialized",
    "root": str(unmaterialized_runtime.root),
    "descriptor": unmaterialized_descriptor,
    "context": history_context(unmaterialized_runtime, unmaterialized_descriptor),
})

ancestor_runtime = Runtime(ROOT / "ancestor-chain")
ancestor_agent = {"id": "agent-chain", "epoch": 9, "accountKey": "account-chain", "threadId": None}
ancestor_base = export_history(ancestor_runtime, ancestor_agent, "transfer-base", None)
ancestor_agent["portableHistory"] = ancestor_base
ancestor_child = export_history(ancestor_runtime, ancestor_agent, "transfer-child", None)
ancestor_agent["portableHistory"] = ancestor_child
ancestor_grandchild = export_history(ancestor_runtime, ancestor_agent, "transfer-grandchild", None)
cases.append({
    "name": "ancestor-chain",
    "root": str(ancestor_runtime.root),
    "descriptor": ancestor_grandchild,
    "context": history_context(ancestor_runtime, ancestor_grandchild),
})
print(json.dumps(cases, ensure_ascii=False, separators=(",", ":")))
