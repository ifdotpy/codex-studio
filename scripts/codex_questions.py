"""Durable question history. Deferral changes presentation, never tool permission."""
import hashlib
import json
import time


def is_question(record):
    return record.get("method") in {"agent/asyncQuestion", "item/tool/requestUserInput"} or (
        record.get("method") == "mcpServer/elicitation/request"
        and record.get("params", {}).get("mode") == "form"
    )


def question_fields(record):
    params = record.get("params", {})
    if record.get("method") != "mcpServer/elicitation/request":
        return params.get("questions", [])
    return [
        {"id": key, "question": value.get("title", key),
         "isSecret": bool(value.get("isSecret") or value.get("writeOnly") or value.get("format") == "password")}
        for key, value in params.get("requestedSchema", {}).get("properties", {}).items()
    ]


def answer_signature(data):
    # Never store secret values in receipts or the history projection.
    body = {key: data[key] for key in ("answers", "content", "decision") if key in data}
    return hashlib.sha256(json.dumps(body, sort_keys=True, ensure_ascii=False).encode()).hexdigest()


def record_answer(record, data):
    fields = question_fields(record)
    answers = data.get("answers", {})
    content = data.get("content") or {}
    record.update(answerSignature=answer_signature(data), answeredBy="user", answeredAt=time.time(),
                  decision=data.get("decision", "answer"), deferred=False)
    record["answerHistory"] = [
        {"id": q["id"], "question": q.get("question", q["id"]), "isSecret": bool(q.get("isSecret")),
         "answer": "[redacted]" if q.get("isSecret") else (
             answers.get(q["id"], {}).get("answers", []) if "answers" in data else content.get(q["id"]))}
        for q in fields
    ]


class QuestionsMixin:
    def defer_question(self, key, deferred=True):
        if not isinstance(deferred, bool):
            raise ValueError("Deferred must be true or false")
        with self.lock, self.db() as db:
            row = db.execute("SELECT record FROM runtime_requests WHERE id=?", (key,)).fetchone()
            if not row:
                raise ValueError("Unknown question")
            record = json.loads(row[0])
            if not is_question(record):
                raise ValueError("Only a question can be deferred")
            if record["status"] != "pending":
                raise ValueError("This question is no longer pending")
            if self.agent(record["agent"], db).get("deletedAt"):
                raise ValueError("This conversation was deleted")
            if record.get("deferred", False) != deferred:
                record.update(deferred=deferred, deferredAt=time.time(), deferredBy="user")
                self.put(db, "requests", record)
            return {"id": key, "status": record["status"], "deferred": deferred}

    def question_history(self, key):
        if not key:
            raise ValueError("A conversation is required")
        with self.lock, self.db() as db:
            owner = self.checked_actor(db, key)
            ids = {a["id"] for a in self.records(db, "agents")
                   if not a.get("deletedAt") and a["rootId"] == owner["rootId"]}
            records = []
            for record in self.records(db, "requests"):
                if record.get("agent") not in ids or not is_question(record):
                    continue
                records.append({name: record.get(name) for name in (
                    "id", "agent", "method", "status", "createdAt", "answeredAt", "answeredBy", "decision",
                    "deferred", "deferredAt", "deferredBy", "answerHistory", "answerError")}
                    | {"questions": [{"id": q["id"], "question": q.get("question", q["id"]), "isSecret": bool(q.get("isSecret"))}
                                     for q in question_fields(record)]})
            records.sort(key=lambda record: record.get("answeredAt") or record.get("createdAt") or 0, reverse=True)
            return {"items": records}
