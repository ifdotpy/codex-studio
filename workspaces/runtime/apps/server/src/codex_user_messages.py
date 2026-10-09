"""Durable user messages and preservation of former user requests."""

import json

from codex_work import text_field


_STATUS = {"open": "open", "review": "in_progress", "accepted": "resolved", "cancelled": "declined"}
_ACTION_STATUS = {"complete": "in_progress", "accept": "resolved", "return": "open", "cancel": "declined", "update": "open"}
_ACTION_LABEL = {"complete": "User marked complete", "accept": "Accepted", "return": "Returned", "cancel": "Cancelled", "update": "Instructions updated"}


def _message(task):
    text = "\n\n".join(part for part in (
        task.get("title", ""), task.get("description", ""),
        "Completion criteria:\n" + task["criteria"] if task.get("criteria") else "",
    ) if part)
    responses = []
    for index, entry in enumerate(task.get("history", [])):
        action = entry["action"]
        if action == "create":
            continue
        label = _ACTION_LABEL.get(action, action)
        body = entry.get("text", "")
        responses.append({
            "id": "user-task-history:" + task["id"] + ":" + str(index),
            "author": "user" if action == "complete" else entry["actor"],
            "text": label + (": " + body if body else ""),
            "at": entry["at"], "status": _ACTION_STATUS.get(action, "open"),
        })
    return {
        "id": task["id"], "sourceType": "user_task", "leadId": task["rootId"],
        "author": task["agent"], "recipient": "user", "text": text,
        "status": _STATUS[task["status"]], "version": max(1, task.get("version", 1)),
        "created": task["created"], "updated": task["updated"],
        "readAt": next((r["at"] for r in responses if r["author"] == "user"), None),
        "responses": responses,
    }


def migrate(runtime, db):
    """Use the caller's transaction. Keep the original task records unchanged."""
    if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_user_tasks'").fetchone():
        return 0
    additions = []
    for (raw,) in db.execute("SELECT record FROM runtime_user_tasks"):
        task = json.loads(raw)
        existing = db.execute("SELECT record FROM runtime_complaints WHERE id=?", (task["id"],)).fetchone()
        if existing:
            prior = json.loads(existing[0])
            if (prior.get("sourceType"), prior.get("author"), prior.get("leadId")) != ("user_task", task["agent"], task["rootId"]):
                raise ValueError("User message migration found an existing message with the same id: " + task["id"])
            continue
        additions.append(_message(task))
    # Check and prepare every record before writes, including the queued events.
    events = []
    for event in db.execute("SELECT id,text FROM runtime_events WHERE kind='user_task_completed' AND status='pending'"):
        payload = json.loads(event["text"])
        payload["instruction"] = (
            "The user replied that this request is complete. Read the completionNote and check the result. "
            "If you need to reply, use orchestration_message with target=user. "
            "This is a normal message; no task acceptance or return operation is required."
        )
        events.append((json.dumps(payload, ensure_ascii=False), event["id"]))
    for message in additions:
        runtime.put(db, "complaints", message)
    db.executemany("UPDATE runtime_events SET text=? WHERE id=? AND status='pending'", events)
    return len(additions)


def send_to_user(runtime, sender_id, text, key, epoch=None):
    text = text_field(text, "a message", 12000)
    key = text_field(key, "a request id", 200)
    with runtime.lock, runtime.db() as db:
        db.execute("BEGIN IMMEDIATE")
        sender = runtime.agent(sender_id, db)
        if sender.get("deletedAt") or not sender.get("autoWake") or (epoch is not None and sender["epoch"] != epoch):
            raise ValueError("Sender was stopped")
        if not sender.get("isLead") or sender["rootId"] != sender_id or sender.get("parentId"):
            raise ValueError("Only the lead can contact the user. Send your message to your lead.")
        signature, saved = runtime.operation_receipt(db, key, {"sender": sender_id, "target": "user", "text": text})
        if saved is not None:
            return saved
        message = runtime.submit_complaint(db, sender, sender, text, key)
        message["sourceType"] = "message"
        runtime.put(db, "complaints", message)
        result = {"id": message["id"], "room": "user:" + sender_id, "recipient": "user"}
        runtime.save_receipt(db, key, signature, result)
    runtime.touch_ui(sender_id)
    return result
