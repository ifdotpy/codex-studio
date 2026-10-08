"""Copy managed names to native threads without model turns or scheduler waits."""
import json
import threading
import time
import sqlite3
import threading
from concurrent.futures import Future
from typing import TYPE_CHECKING, Callable, ContextManager, Protocol, TypedDict

from codex_records import AgentRecord, RecordStore

if TYPE_CHECKING:
    from codex_runtime import Runtime


class NameIdentity(TypedDict):
    accountKey: str
    threadId: str | None
    name: str


class NativeNameServer(Protocol):
    def submit(self, method: str, params: dict[str, object]) -> Future[object]: ...
    def on_result(self, future: object, callback: Callable[[Future[object]], None]) -> None: ...


class _Lock(Protocol):
    def __enter__(self) -> object: ...
    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None: ...


class SessionNamesHost(RecordStore, Protocol):
    lock: _Lock
    closed: bool
    changed: threading.Event
    _session_names: "SessionNames"

    def db(self) -> ContextManager[sqlite3.Connection]: ...
    def connect(self, account_key: str) -> NativeNameServer: ...
    def connect_agent(self, agent: AgentRecord) -> NativeNameServer: ...


def identity(agent: AgentRecord) -> NameIdentity:
    return {"accountKey": agent.get("accountKey", "default"),
            "threadId": agent.get("threadId"), "name": agent["name"]}


def session_names(runtime: "Runtime") -> "SessionNames":
    with runtime.lock:
        if not hasattr(runtime, "_session_names"):
            runtime._session_names = SessionNames(runtime)  # type: ignore[attr-defined,arg-type]
        return runtime._session_names  # type: ignore[attr-defined,no-any-return]


class SessionNames:
    def __init__(self, runtime: SessionNamesHost) -> None:
        self.runtime = runtime
        self.pending: dict[str, NameIdentity] = {}
        self.next_scan: float = 0

    def tick(self) -> None:
        rt = self.runtime
        with rt.lock:
            if rt.closed or time.monotonic() < self.next_scan:
                return
            self.next_scan = time.monotonic() + 1
            with rt.db() as db:
                # Decode only agents whose native name may differ; this runs every second.
                candidates = [json.loads(row[0]) for row in db.execute(
                    "SELECT record FROM runtime_agents WHERE json_extract(record,'$.deletedAt') IS NULL "
                    "AND json_extract(record,'$.threadId') IS NOT NULL AND ("
                    "json_extract(record,'$.nativeNameSynced') IS NULL "
                    "OR json_extract(record,'$.nativeNameSynced.name') IS NOT json_extract(record,'$.name') "
                    "OR json_extract(record,'$.nativeNameSynced.threadId') IS NOT json_extract(record,'$.threadId') "
                    "OR json_extract(record,'$.nativeNameSynced.accountKey') IS NOT "
                    "COALESCE(json_extract(record,'$.accountKey'),'default'))")]
                for agent in sorted(candidates, key=lambda a: not bool(a.get("nativeNameSynced"))):
                    if len(self.pending) >= 2:
                        break
                    wanted = identity(agent)
                    if (not wanted["threadId"] or agent.get("deletedAt")
                            or agent.get("accountTransferId") or agent["id"] in self.pending
                            or agent.get("nativeNameSynced") == wanted):
                        continue
                    failure = agent.get("nativeNameFailure", {})
                    if failure.get("identity") == wanted and failure.get("retryAt", 0) > time.time():
                        continue
                    self.pending[agent["id"]] = wanted
                    threading.Thread(target=self.submit, args=(agent["id"], wanted),
                                     name="studio-session-name", daemon=True).start()

    def submit(self, key: str, wanted: NameIdentity) -> None:
        rt = self.runtime
        try:
            server = rt.connect_agent(rt.agent(key))
            with rt.lock:
                if rt.closed or identity(rt.agent(key)) != wanted:
                    self.pending.pop(key, None)
                    return
            try:
                submitted: object = server.submit("thread/name/set", {
                    "threadId": wanted["threadId"], "name": wanted["name"],
                })
            except Exception as error:
                submitted = getattr(error, "submitted", None)
                if submitted is None:
                    raise
            server.on_result(submitted, lambda future: self.complete(key, wanted, future))
        except Exception as error:
            self.complete(key, wanted, error=error)

    def complete(self, key: str, wanted: NameIdentity, future: Future[object] | None = None, error: Exception | None = None) -> None:
        rt = self.runtime
        if future is not None:
            try:
                future.result()
            except Exception as failure:
                error = failure
        with rt.lock:
            if self.pending.get(key) != wanted:
                return
            self.pending.pop(key, None)
            if rt.closed:
                return
            with rt.db() as db:
                agent = rt.agent(key, db)
                if agent.get("deletedAt") or identity(agent) != wanted:
                    rt.changed.set()
                    return
                if error is None:
                    agent["nativeNameSynced"] = wanted  # type: ignore[typeddict-item]
                    agent.pop("nativeNameFailure", None)
                else:
                    previous = agent.get("nativeNameFailure", {})
                    attempts = previous.get("attempts", 0) + 1 if previous.get("identity") == wanted else 1
                    agent["nativeNameFailure"] = {
                        "identity": wanted, "error": str(error), "attempts": attempts,
                        "retryAt": time.time() + min(300, 15 * 2 ** min(attempts - 1, 5)),
                    }
                rt.put(db, "agents", agent)
            rt.changed.set()
