"""Read-only sidebar and notification projection for unloaded UI frames."""
from __future__ import annotations

import json
import sqlite3
from collections.abc import Callable
from typing import ContextManager

from studio_api.models import ContractModel


class SummaryProject(ContractModel):
    path: str
    name: str


class SummaryChat(ContractModel):
    id: str
    name: str
    path: str
    archived: bool
    status: str
    unread: bool


class SummaryTarget(ContractModel):
    agentId: str
    section: str = "messages"
    itemId: str | None = None


class SummaryAlert(ContractModel):
    id: str
    title: str
    body: str
    target: SummaryTarget


class UiSummaryResponse(ContractModel):
    ready: bool
    busy: bool
    projects: list[SummaryProject]
    chats: list[SummaryChat]
    alerts: list[SummaryAlert]


def read_summary(connect: Callable[[], ContextManager[sqlite3.Connection]]) -> UiSummaryResponse:
    result = UiSummaryResponse(ready=False, busy=False, projects=[], chats=[], alerts=[])
    # Use the durable entity projection. Do not pull, initialize stores, or write read receipts.
    with connect() as db:
        db.execute("PRAGMA query_only=ON")
        db.execute("BEGIN")
        if not db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_entities'").fetchone():
            return result
        rows = db.execute("SELECT collection,payload FROM sync_entities WHERE deleted=0 AND collection IN ('agent','project','room','request','complaint') ORDER BY collection,id").fetchall()
        values: dict[str, list[dict[str, object]]] = {}
        for collection, payload in rows:
            try:
                value = json.loads(payload)["value"]
                if isinstance(value, dict):
                    values.setdefault(collection, []).append(value)
            except (ValueError, KeyError, TypeError):
                continue
    result.ready = True
    projects = {str(row["path"]): SummaryProject(path=str(row["path"]), name=str(row.get("name") or row["path"]))
                for row in values.get("project", []) if row.get("path")}
    agents = {str(row["id"]): row for row in values.get("agent", []) if row.get("id") and not row.get("deletedAt")}
    unread: set[str] = set()
    for agent_id, row in agents.items():
        result.busy = result.busy or bool(row.get("inFlight"))
        read = row.get("readState")
        if row.get("threadId") and row.get("lastCompletedTurn") and row.get("lastCompletedTurnStatus") == "completed" and not (
            isinstance(read, dict) and read.get("read") is True and read.get("threadId") == row.get("threadId") and read.get("turnId") == row.get("lastCompletedTurn")
        ):
            unread.add(agent_id)
        if row.get("source") != "managed" or not row.get("isLead") or row.get("sharedRoomId") or row.get("remoteAnchor"):
            continue
        path = str(row.get("cwd") or "")
        projects.setdefault(path, SummaryProject(path=path, name=path or "Other chats"))
        result.chats.append(SummaryChat(id=agent_id, name=str(row.get("name") or "Chat"), path=path,
                                        archived=bool(row.get("archived")), status=str(row.get("status") or ""), unread=agent_id in unread))

    def add(alert_id: str, agent_id: object, title: str, body: object, item_id: object = None) -> None:
        agent = agents.get(str(agent_id))
        if agent is None or agent.get("remoteAnchor"):
            return
        result.alerts.append(SummaryAlert(id=alert_id, title=f"{agent.get('name') or 'Chat'}: {title}"[:160], body=str(body)[:2000],
                            target=SummaryTarget(agentId=str(agent_id), itemId=str(item_id) if item_id else None)))

    for agent_id, row in agents.items():
        if not row.get("isLead") or row.get("inFlight"):
            continue
        turn = row.get("lastCompletedTurn")
        state = row.get("status")
        if state == "completed" and turn and row.get("lastCompletedTurnStatus") == "completed":
            add(f"completed:{agent_id}:{turn}", agent_id, "Reply ready", row.get("tail") or "Open the chat to read the reply.")
        if state in {"failed", "interrupted"}:
            error = row.get("error")
            message = error.get("message") if isinstance(error, dict) else error
            add(f"failed:{agent_id}:{turn or row.get('turnId') or state}", agent_id, "Work stopped", message or "Open the chat to check the error.")
    for row in values.get("request", []):
        if row.get("status") != "pending" or row.get("deferred"):
            continue
        params = row.get("params")
        questions = params.get("questions") if isinstance(params, dict) else None
        question = questions[0].get("question") if isinstance(questions, list) and questions and isinstance(questions[0], dict) else None
        add(f"request:{row.get('id')}", row.get("agent"), "Question for you" if question else "Your approval is needed", question or "Open the request to review it.", row.get("id"))
    for row in values.get("complaint", []):
        if row.get("needsUserResponse"):
            add(f"complaint:{row.get('id')}:{row.get('version') or 0}", row.get("leadId"), "Message for you", row.get("title") or "Open the message to read it.", row.get("id"))
    for row in values.get("room", []):
        if not row.get("radio") or row.get("userHidden"):
            continue
        path = str(row.get("projectPath") or "")
        projects.setdefault(path, SummaryProject(path=path, name=path or "Other chats"))
        members = row.get("members")
        result.chats.append(SummaryChat(id=str(row["id"]), name=str(row.get("name") or "Shared chat"), path=path,
                                       archived=False, status="", unread=isinstance(members, list) and any(str(member) in unread for member in members)))
    result.projects = list(projects.values())
    return result
