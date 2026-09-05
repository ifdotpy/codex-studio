"""Agent-owned requests for user action and durable completion notifications."""

import json
import time
import uuid
from codex_work import text_field


def user_task_tools(tool, text):
    return [
        tool(
            "orchestration_user_task",
            "Manage things the user must do for your work. Create a clear title and completion criteria. "
            "The user checks the task, which changes it to review and wakes you automatically, even after your final answer. "
            "Read the result, then accept it or return it with a concrete reason. You and your team lead can manage your tasks. "
            "list returns the team's tasks with versions. update, accept, return, and cancel require the current version. "
            "accept, return, and cancel also require a reason. "
            "Do not use the agent work board for user actions. Do not poll while waiting.",
            {
                "action": {
                    "type": "string",
                    "enum": ["list", "create", "update", "accept", "return", "cancel"],
                },
                "task_id": text,
                "title": text,
                "description": text,
                "criteria": text,
                "reason": text,
                "version": {"type": "integer", "minimum": 1},
            },
            ["action"],
        )
    ]


class UserTasksMixin:
    def setup_user_tasks(self, db):
        db.execute(
            "CREATE TABLE IF NOT EXISTS runtime_user_tasks (id TEXT PRIMARY KEY, record TEXT NOT NULL)"
        )
        db.execute(
            "CREATE INDEX IF NOT EXISTS runtime_user_task_owner ON runtime_user_tasks(json_extract(record,'$.agent'),json_extract(record,'$.status'))"
        )

    def user_task_view(self, db, task):
        owner = self.agent(task["agent"], db)
        return {
            **task,
            "agentName": owner["name"],
            "agentStopped": not owner["autoWake"],
        }

    def user_tasks(self, agent_id=None, db=None):
        if db is None:
            with self.lock, self.db() as connection:
                return self.user_tasks(agent_id, connection)
        owner = self.checked_actor(db, agent_id) if agent_id else None
        agents = {
            a["id"]: a for a in self.records(db, "agents") if not a.get("deletedAt")
        }
        return {
            "items": [
                self.user_task_view(db, t)
                for t in sorted(
                    self.records(db, "user_tasks"),
                    key=lambda t: t["updated"],
                    reverse=True,
                )
                if t["agent"] in agents
                and (not owner or t["rootId"] == owner["rootId"])
            ]
        }

    def user_task_action(self, actor_id, data, key=None, epoch=None):
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            if epoch is not None and actor["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            action = data.get("action")
            signature, prior = self.operation_receipt(
                db, key, {"actor": actor_id, "userTask": data}
            )
            if prior is not None:
                return prior
            if action == "list":
                return self.user_tasks(actor_id, db)
            now = time.time()
            if action == "create":
                task = {
                    "id": str(uuid.uuid4()),
                    "agent": actor_id,
                    "rootId": actor["rootId"],
                    "title": text_field(data.get("title"), "a task title", 160),
                    "description": text_field(
                        data.get("description", ""), "a description", 16000, empty=True
                    ),
                    "criteria": text_field(
                        data.get("criteria"), "completion criteria", 8000
                    ),
                    "status": "open",
                    "version": 1,
                    "created": now,
                    "updated": now,
                    "reason": "",
                    "completionNote": "",
                    "history": [
                        {"action": "create", "text": "", "actor": actor_id, "at": now}
                    ],
                }
            else:
                row = db.execute(
                    "SELECT record FROM runtime_user_tasks WHERE id=?",
                    (data.get("task_id"),),
                ).fetchone()
                if not row:
                    raise ValueError("Unknown user task")
                task = json.loads(row[0])
                self.checked_actor(db, task["agent"], actor_id)
                if actor_id not in {task["agent"], task["rootId"]}:
                    raise ValueError(
                        "Only the requesting agent or its lead can manage this task"
                    )
                self.check_user_task_version(task, data)
                reason = ""
                if action == "update":
                    if task["status"] != "open":
                        raise ValueError(
                            "Return this task before changing its instructions"
                        )
                    for name, maximum in [
                        ("title", 160),
                        ("description", 16000),
                        ("criteria", 8000),
                    ]:
                        if name in data:
                            task[name] = text_field(
                                data[name], name, maximum, empty=name == "description"
                            )
                elif action == "accept":
                    if task["status"] != "review":
                        raise ValueError(
                            "Wait for the user to check this task before accepting it"
                        )
                    reason = text_field(
                        data.get("reason"), "an acceptance reason", 16000
                    )
                    task.update(status="accepted", reason=reason, acceptedAt=now)
                elif action == "return":
                    if task["status"] not in {"review", "accepted"}:
                        raise ValueError(
                            "Only a completed or accepted task can be returned"
                        )
                    reason = text_field(
                        data.get("reason"), "a reason and the next user action", 16000
                    )
                    task.update(status="open", reason=reason, returnedAt=now)
                elif action == "cancel":
                    if task["status"] == "cancelled":
                        raise ValueError("This task is already cancelled")
                    reason = text_field(
                        data.get("reason"), "a cancellation reason", 16000
                    )
                    task.update(status="cancelled", reason=reason)
                else:
                    raise ValueError("Unknown user task action")
                task.update(version=task["version"] + 1, updated=now)
                task["history"].append(
                    {"action": action, "text": reason, "actor": actor_id, "at": now}
                )
            self.put(db, "user_tasks", task)
            self.touch_ui(task["agent"])
            return self.save_receipt(db, key, signature, self.user_task_view(db, task))

    @staticmethod
    def check_user_task_version(task, data):
        version = data.get("version")
        if (
            isinstance(version, bool)
            or not isinstance(version, int)
            or version != task["version"]
        ):
            raise ValueError(
                "This task changed. Read the current task before trying again"
            )

    def complete_user_task(self, data):
        request = text_field(data.get("id"), "a request id", 200)
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_user_tasks WHERE id=?",
                (data.get("task_id"),),
            ).fetchone()
            if not row:
                raise ValueError("Unknown user task")
            task = json.loads(row[0])
            owner = self.checked_actor(db, task["agent"])
            signature, prior = self.operation_receipt(
                db, request, {"userCompletion": data}
            )
            if prior is not None:
                return prior
            self.check_user_task_version(task, data)
            if task["status"] != "open":
                raise ValueError("This task is no longer awaiting your action")
            note = text_field(
                data.get("note", ""), "a completion note", 16000, empty=True
            )
            now = time.time()
            task.update(
                status="review",
                version=task["version"] + 1,
                updated=now,
                completedAt=now,
                completionNote=note,
            )
            task["history"].append(
                {"action": "complete", "text": note, "actor": "user", "at": now}
            )
            event = self.enqueue(
                db,
                owner,
                "user_task_completed",
                json.dumps(
                    {
                        "task_id": task["id"],
                        "title": task["title"],
                        "criteria": task["criteria"],
                        "completionNote": note,
                        "version": task["version"],
                        "instruction": "The user marked this task complete. Check the result. Use orchestration_user_task to accept it or return it with a reason and next action.",
                    },
                    ensure_ascii=False,
                ),
                "user-task-completed:" + task["id"] + ":" + str(task["version"]),
            )
            task["delivery"] = db.execute(
                "SELECT status FROM runtime_events WHERE id=?", (event,)
            ).fetchone()[0]
            self.put(db, "user_tasks", task)
            self.touch_ui(owner["id"])
            return self.save_receipt(
                db, request, signature, self.user_task_view(db, task)
            )

    def user_task_review_context(self, db, agent_id):
        pending = [
            t
            for t in self.records(db, "user_tasks")
            if t["agent"] == agent_id and t["status"] == "review"
        ]
        if not pending:
            return ""
        return (
            "\n\n[User tasks awaiting your review] The user checked these tasks. Read them with orchestration_user_task, then accept or return each with a reason: "
            + ", ".join(t["id"] for t in pending)
        )
