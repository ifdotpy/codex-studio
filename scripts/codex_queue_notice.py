"""Tell an agent in a long turn that inputs wait for its next turn.

Queued inputs (agent messages, monitor results, child results) reach an agent
only when its turn ends. A turn can run for hours, and then its lead and peers
cannot reach it. The notice names what waits; it does not deliver the inputs
and does not interrupt the turn.
"""
import time

NOTICE_AFTER_SECONDS = 900
REPEAT_SECONDS = 1800
KIND_NAMES = {"agent_message": "agent messages", "monitor_exit": "monitor results",
              "child_result": "child results", "user": "user messages", "followup": "follow-up messages"}


def due_notices(rt, db, agents):
    """Reserve due notices under the runtime lock; the caller sends them."""
    now = time.time()
    waiting = {}
    for agent, epoch, kind, count, oldest in db.execute(
            "SELECT agent, epoch, kind, count(*), min(created) FROM runtime_events "
            "WHERE status='pending' GROUP BY agent, epoch, kind"):
        waiting.setdefault((agent, epoch), []).append((kind, count, oldest))
    notices = []
    for a in agents:
        kinds = waiting.get((a["id"], a["epoch"]))
        if (not kinds or not a.get("inFlight") or not a.get("turnId") or not a.get("autoWake")
                or a.get("deletedAt") or a.get("nativeReview") or a["status"] != "running"):
            continue
        oldest = min(k[2] for k in kinds)
        if now - oldest < NOTICE_AFTER_SECONDS:
            continue
        notice = a.get("queueNotice") or {}
        if notice.get("turnId") == a["turnId"] and now - notice.get("at", 0) < REPEAT_SECONDS:
            continue
        message_id = "queue-notice:" + a["id"] + ":" + a["turnId"] + ":" + str(int(now))
        a["queueNotice"] = {"turnId": a["turnId"], "at": now, "id": message_id}
        rt.put(db, "agents", a)
        parts = ", ".join(f"{count} {KIND_NAMES.get(kind, kind)}" for kind, count, _ in sorted(kinds))
        text = (f"Studio notice: {sum(k[1] for k in kinds)} inputs wait for your next turn ({parts}); "
                f"the oldest waits since {time.strftime('%H:%M', time.localtime(oldest))}. "
                "They cannot reach you during this turn. End this turn when your current step allows, "
                "so that you receive them. If this turn already ended, ignore this notice.")
        notices.append((a["id"], text, message_id))
    return notices


def send_notice(rt, agent_id, text, message_id):
    try:
        rt.send(agent_id, text, message_id, manual=False, delivery="steer")
    except ValueError:
        # The turn ended or the agent stopped; its queue is delivered or held.
        pass
