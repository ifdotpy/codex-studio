"""Keep unsubmitted turn input queued while a workspace reservation exists."""

def defer_workspace_start(runtime, agent_id, attempt_id, error, *, unknown=False):
    if (unknown or not isinstance(error, ValueError)
            or not str(error).startswith("A workspace operation is active in this directory")):
        return False
    with runtime.lock, runtime.db() as db:
        agent = runtime.agent(agent_id, db)
        attempt = agent.get("startAttempt") or {}
        if (attempt.get("id") != attempt_id or attempt.get("submitted")
                or attempt.get("epoch") != agent["epoch"] or not agent["autoWake"]
                or agent.get("deletedAt")):
            return False
        if any(b["operation"] not in {"checkpoint", "capture"}
               for b in runtime.workspace_blockers(db, agent)):
            return False
        # The original local reservation check failed before native submission.
        # It may have settled since then. Dispatch checks it again before starting.
        for event_id in attempt.get("events", []):
            db.execute("UPDATE runtime_events SET status='pending',error=NULL "
                       "WHERE id=? AND agent=? AND epoch=? AND status IN ('reserved','dispatching')",
                       (event_id, agent_id, agent["epoch"]))
        agent.update(status="queued", inFlight=False, error=None)
        agent.pop("startAttempt", None)
        runtime.put(db, "agents", agent)
    runtime.changed.set()
    return True
