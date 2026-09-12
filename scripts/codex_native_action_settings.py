"""Apply selected settings before native actions that have no override fields."""
import concurrent.futures


def ensure(runtime, agent, attempt, server):
    from codex_runtime import PreparationPending
    futures = runtime.__dict__.setdefault("_action_settings_futures", {})
    with runtime.lock, runtime.db() as db:
        current = runtime.agent(agent["id"], db)
        active = current.get("startAttempt") or {}
        if (runtime.closed or active.get("id") != attempt["id"] or not current["autoWake"]
                or current["epoch"] != attempt["epoch"]):
            raise ValueError("Native action changed before settings submission")
        operation = active.get("modelSettings")
        if operation:
            _check(runtime, current, operation, server)
            if operation["status"] == "acknowledged":
                return
            future = futures.get(attempt["id"])
            if future is None:
                raise RuntimeError("Native action settings outcome unknown. Inspect the thread before another action")
        else:
            operation = {
                "id": attempt["id"], "epoch": current["epoch"],
                "accountKey": current.get("accountKey", "default"),
                "threadId": current["threadId"],
                "connectionId": runtime.connection_ids[current.get("accountKey", "default")],
                "settings": runtime.preparation_settings(current), "status": "submitted",
            }
            _check(runtime, current, operation, server)
            params = {"threadId": current["threadId"], "model": current["model"],
                      "serviceTier": "priority" if current.get("fastMode") else "default",
                      **runtime.turn_permissions(current)}
            effort = current.get("nativeEffort", current.get("effort"))
            if effort is not None:
                params["effort"] = effort
            active["modelSettings"] = operation
            runtime.put(db, "agents", current)
            db.commit()
            future = concurrent.futures.Future()
            futures[attempt["id"]] = future

            def complete(native_future):
                try:
                    native_future.result()
                    with runtime.lock, runtime.db() as receipt_db:
                        latest = runtime.agent(agent["id"], receipt_db)
                        _check(runtime, latest, operation, server)
                        latest["startAttempt"]["modelSettings"]["status"] = "acknowledged"
                        runtime.put(receipt_db, "agents", latest)
                    future.set_result(None)
                except BaseException as error:
                    future.set_exception(error)
                finally:
                    futures.pop(attempt["id"], None)

            try:
                submitted = runtime.submit_reserved(server, "thread/settings/update", params)
                server.on_result(submitted, complete)
            except BaseException as error:
                future.set_exception(error)
                # This attempt has no action submission. Its caller records failure.
                futures.pop(attempt["id"], None)
    try:
        future.result(getattr(runtime, "preparation_wait_seconds", 60))
    except concurrent.futures.TimeoutError:
        raise PreparationPending(future) from None


def _check(runtime, agent, operation, server):
    if (not runtime.operation_current(agent, operation)
            or not agent.get("autoWake") or agent.get("threadId") != operation["threadId"]
            or (agent.get("startAttempt") or {}).get("id") != operation["id"]
            or runtime.preparation_settings(agent) != operation["settings"]
            or runtime.servers.get(operation["accountKey"]) is not server):
        raise ValueError("Native action settings belong to an earlier agent state")
