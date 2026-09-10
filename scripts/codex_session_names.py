"""Copy managed names to native threads without model turns or scheduler waits."""
import threading
import time


def identity(agent):
    return {"accountKey": agent.get("accountKey", "default"),
            "threadId": agent.get("threadId"), "name": agent["name"]}


def session_names(runtime):
    with runtime.lock:
        if not hasattr(runtime, "_session_names"):
            runtime._session_names = SessionNames(runtime)
        return runtime._session_names


class SessionNames:
    def __init__(self, runtime):
        self.runtime = runtime
        self.pending = {}
        self.next_scan = 0

    def tick(self):
        rt = self.runtime
        with rt.lock:
            if rt.closed or time.monotonic() < self.next_scan:
                return
            self.next_scan = time.monotonic() + 1
            with rt.db() as db:
                for agent in sorted(rt.records(db, "agents"), key=lambda a: not bool(a.get("nativeNameSynced"))):
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

    def submit(self, key, wanted):
        rt = self.runtime
        try:
            server = rt.connect(wanted["accountKey"])
            with rt.lock:
                if rt.closed or identity(rt.agent(key)) != wanted:
                    self.pending.pop(key, None)
                    return
            try:
                submitted = server.submit("thread/name/set", {
                    "threadId": wanted["threadId"], "name": wanted["name"],
                })
            except Exception as error:
                submitted = getattr(error, "submitted", None)
                if submitted is None:
                    raise
            server.on_result(submitted, lambda future: self.complete(key, wanted, future))
        except Exception as error:
            self.complete(key, wanted, error=error)

    def complete(self, key, wanted, future=None, error=None):
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
                    agent["nativeNameSynced"] = wanted
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
