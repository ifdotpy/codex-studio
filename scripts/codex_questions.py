"""Durable question history. Deferral changes presentation, never tool permission."""
from __future__ import annotations

import hashlib
import json
import time
from typing import TYPE_CHECKING, Protocol

from codex_records import RecordStore

if TYPE_CHECKING:
    import sqlite3
    from threading import Event
    from typing import ContextManager

    from codex_records import (
        AgentRecord,
        RequestAnswerDataRecord,
        RequestQuestionFieldRecord,
        RequestRecord,
    )


class QuestionsRuntime(RecordStore, Protocol):
    lock: "ContextManager[object]"
    changed: "Event"

    def db(self) -> "ContextManager[sqlite3.Connection]": ...
    def checked_actor(self, db: "sqlite3.Connection", actor: str) -> "AgentRecord": ...
    def answer(self, key: str, data: RequestAnswerDataRecord) -> object: ...
    def agent(self, key: str, db: "sqlite3.Connection | None" = None) -> "AgentRecord": ...
    def team_agents(self, db: "sqlite3.Connection", root_id: str, *, include_deleted: bool = False,
                    include_id: str | None = None) -> list["AgentRecord"]: ...


def is_question(record: "RequestRecord") -> bool:
    return record.get("method") in {"agent/asyncQuestion", "item/tool/requestUserInput"} or (
        record.get("method") == "mcpServer/elicitation/request"
        and record.get("params", {}).get("mode") == "form"
    )


def question_fields(record: "RequestRecord") -> list["RequestQuestionFieldRecord"]:
    params = record.get("params", {})
    if record.get("method") != "mcpServer/elicitation/request":
        return params.get("questions", [])  # type: ignore[return-value]  # typed-narrowing: Method discriminator guarantees question array
    return [
        {"id": key, "question": value.get("title", key),  # type: ignore[union-attr,typeddict-item]  # typed-narrowing: Provider schema title remains textual
         "isSecret": bool(value.get("isSecret") or value.get("writeOnly") or value.get("format") == "password")}  # type: ignore[union-attr]  # typed-narrowing: Provider properties remain object mappings
        for key, value in params.get("requestedSchema", {}).get("properties", {}).items()  # type: ignore[union-attr]  # typed-narrowing: Schema properties are object mappings
    ]


def answer_signature(data: "RequestAnswerDataRecord") -> str:
    # Never store secret values in receipts or the history projection.
    body = {key: data[key] for key in ("answers", "content", "decision") if key in data}
    return hashlib.sha256(json.dumps(body, sort_keys=True, ensure_ascii=False).encode()).hexdigest()


def record_answer(record: "RequestRecord", data: "RequestAnswerDataRecord") -> None:
    fields = question_fields(record)
    answers = data.get("answers", {})
    content = data.get("content") or {}
    record.update(answerSignature=answer_signature(data), answeredBy="user", answeredAt=time.time(),
                  decision=data.get("decision", "answer"), deferred=False)  # type: ignore[call-arg]  # typed-update
    record["answerHistory"] = [
        {"id": q["id"], "question": q.get("question", q["id"]), "isSecret": bool(q.get("isSecret")),
         "answer": "[redacted]" if q.get("isSecret") else (
             answers.get(q["id"], {}).get("answers", []) if "answers" in data else content.get(q["id"]))}
        for q in fields
    ]


class QuestionsMixin:
    def delete_question(self: "QuestionsRuntime", key: str) -> dict[str, str]:
        with self.lock, self.db() as db:
            row = db.execute("SELECT record FROM runtime_requests WHERE id=?", (key,)).fetchone()
            if not row:
                raise ValueError("Unknown question")
            record: "RequestRecord" = json.loads(row[0])
            if not is_question(record):
                raise ValueError("Only a question can be deleted")
            self.checked_actor(db, record["agent"])  # type: ignore[arg-type]  # typed-suspect: provider questions may lack an owning agent
            if record.get("deletedAt"):
                return {"id": key, "status": "deleted"}
            if record["status"] == "pending" and record["method"] != "agent/asyncQuestion":
                db.commit()
                # Resolve the native question without granting any permission.
                self.answer(key, {"decision": "cancel"} if record["method"] == "mcpServer/elicitation/request"
                            else {"answers": {}})
                record = json.loads(db.execute("SELECT record FROM runtime_requests WHERE id=?", (key,)).fetchone()[0])
            if record["status"] in {"answering", "uncertain"}:
                raise ValueError("Question delivery is uncertain; wait for its receipt")
            record.update(status="deleted", deletedAt=time.time(), deletedBy="user", deferred=False)  # type: ignore[call-arg]  # typed-update
            self.put(db, "requests", record)
            self.changed.set()
            return {"id": key, "status": "deleted"}

    def defer_question(self: "QuestionsRuntime", key: str, deferred: bool = True) -> dict[str, object]:
        if not isinstance(deferred, bool):
            raise ValueError("Deferred must be true or false")
        with self.lock, self.db() as db:
            row = db.execute("SELECT record FROM runtime_requests WHERE id=?", (key,)).fetchone()
            if not row:
                raise ValueError("Unknown question")
            record: "RequestRecord" = json.loads(row[0])
            if not is_question(record):
                raise ValueError("Only a question can be deferred")
            if record["status"] != "pending":
                raise ValueError("This question is no longer pending")
            if self.agent(record["agent"], db).get("deletedAt"):  # type: ignore[arg-type]  # typed-suspect: provider questions may lack an owning agent
                raise ValueError("This conversation was deleted")
            if record.get("deferred", False) != deferred:
                record.update(deferred=deferred, deferredAt=time.time(), deferredBy="user")  # type: ignore[call-arg]  # typed-update
                self.put(db, "requests", record)
            return {"id": key, "status": record["status"], "deferred": deferred}

    def question_history(self: "QuestionsRuntime", key: str) -> dict[str, list[dict[str, object]]]:
        if not key:
            raise ValueError("A conversation is required")
        with self.lock, self.db() as db:
            owner = self.checked_actor(db, key)
            ids = {a["id"] for a in self.team_agents(db, owner["rootId"])}
            records: list[dict[str, object]] = []
            for record in self.records(db, "requests"):
                if record.get("agent") not in ids or not is_question(record) or record.get("deletedAt"):
                    continue
                records.append({name: record.get(name) for name in (
                    "id", "agent", "method", "status", "createdAt", "answeredAt", "answeredBy", "decision",
                    "deferred", "deferredAt", "deferredBy", "answerHistory", "answerError")}
                    | {"questions": [{"id": q["id"], "question": q.get("question", q["id"]), "isSecret": bool(q.get("isSecret"))}
                                     for q in question_fields(record)]})
            records.sort(key=lambda record: record.get("answeredAt") or record.get("createdAt") or 0, reverse=True)  # type: ignore[arg-type,return-value]  # typed-suspect: stored timestamp may not be numeric
            return {"items": records}
