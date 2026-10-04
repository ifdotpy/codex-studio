"""FastAPI routes for agent lifecycle, recovery, and controls."""

from __future__ import annotations

import sqlite3
from collections.abc import Callable
from types import TracebackType
from typing import ContextManager, Protocol, cast

from fastapi import APIRouter, Depends, HTTPException, Request
from starlette.responses import Response

from studio_api.context import ApiContext
from studio_api.request_helpers import body_data, first_nonempty_query
from codex_sync_entities import project

from .models import (
    AgentResponse,
    AgentMode,
    AccountSelectionRequest,
    AccountTransferRequest,
    CapacityRetryRequest,
    CapabilitiesResponse,
    ConfigureRequest,
    ContextRepairResponse,
    ConversationRequest,
    CreateAgentRequest,
    CreateLeadRequest,
    DeletedResponse,
    IdRequest,
    ImportListResponse,
    ImportRequest,
    NativeActionRequest,
    NativeActionResponse,
    NativeCommandRequest,
    NativeCommandResponse,
    RecoveryResponse,
    RenameRequest,
    RenameResponse,
    RetryResponse,
    StopRequest,
    SkillsResponse,
    ImportListQuery,
    CapabilitiesQuery,
    SkillsQuery,
    TransferResponse,
    TransferActionRequest,
    TransferScope,
    TransferMemberPhase,
    UsageResumeRequest,
    UsageResumeResponse,
)
from studio_api.models import JsonValue, ResponseModel

AgentRecord = dict[str, JsonValue]


class RuntimeLock(Protocol):
    """Existing runtime lock surface used to refresh create receipts."""

    def __enter__(self) -> object: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> bool | None: ...


class AgentRuntime(Protocol):
    lock: RuntimeLock

    def db(self) -> ContextManager[sqlite3.Connection]: ...
    def new_lead(self, data: dict[str, JsonValue]) -> AgentRecord: ...
    def agent(self, key: str, db: sqlite3.Connection) -> AgentRecord: ...
    def empty_lead(self, db: sqlite3.Connection, agent: AgentRecord) -> bool: ...
    def create(self, data: dict[str, JsonValue], parent: str | None = None) -> AgentRecord: ...
    def conversation_settings(self, key: str, data: dict[str, JsonValue]) -> AgentRecord: ...
    def configure(self, key: str, data: dict[str, JsonValue]) -> dict[str, JsonValue]: ...
    def set_account(self, key: str, account_key: str, cwd: str | None = None) -> AgentRecord: ...
    def native_action(
        self,
        key: str,
        action: str | dict[str, JsonValue],
        request_id: str | None = None,
        context: dict[str, JsonValue] | None = None,
    ) -> dict[str, JsonValue]: ...
    def native_command_action(self, data: dict[str, JsonValue]) -> dict[str, JsonValue] | None: ...
    def import_thread(self, data: dict[str, JsonValue]) -> AgentRecord: ...
    def stop(self, key: str, descendants: bool = True) -> dict[str, JsonValue]: ...
    def capacity_retry(self, key: str, retry_id: str, action: str) -> dict[str, JsonValue]: ...
    def usage_resume_action(self, key: str, resume_id: str, enabled: bool) -> dict[str, JsonValue] | None: ...
    def delete_conversation(self, key: str) -> dict[str, JsonValue]: ...
    def rename(self, key: str, name: str | None, request_id: str | None = None) -> dict[str, JsonValue]: ...
    def import_list(
        self, cursor: str | None = None, *, account_key: str = "default"
    ) -> dict[str, JsonValue]: ...
    def capabilities(self, key: str | None) -> dict[str, JsonValue]: ...
    def skill_catalog(self, key: str | None) -> dict[str, JsonValue]: ...


class TransferStore(Protocol):
    def action(self, request_id: str, action: str) -> dict[str, JsonValue]: ...
    def request(
        self,
        key: str | None,
        account_key: str | None,
        request_id: str,
        scope: str,
    ) -> dict[str, JsonValue]: ...


class StopResponse(ResponseModel):
    stopped: list[str]


class ConfigureResponse(ResponseModel):
    id: str
    concurrency: int
    agentMode: AgentMode
    agentModeRevision: int
    maxAgents: int
    tokenBudget: int | None = None


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    def current_runtime() -> AgentRuntime:
        runtime = context.runtime
        if runtime is None:
            raise HTTPException(status_code=404, detail="Not found")
        return cast(AgentRuntime, runtime)

    @router.post("/api/leads", response_model=AgentResponse)
    def create_lead(http_request: Request, body: CreateLeadRequest) -> Response:
        runtime = current_runtime()
        request = body_data(body)
        created = runtime.new_lead(request)
        # Preserve the original receipt-before-snapshot behavior.
        with runtime.lock, runtime.db() as db:
            created = runtime.agent(cast(str, created["id"]), db)
            created["empty"] = runtime.empty_lead(db, created)
        return context.send(http_request, project("agent", created))

    @router.post("/api/agents", response_model=AgentResponse)
    def create_agent(http_request: Request, body: CreateAgentRequest) -> Response:
        runtime = current_runtime()
        request = body_data(body)
        created = runtime.create(request, parent=body.parent)
        return context.send(http_request, project("agent", created))

    @router.post("/api/conversation", response_model=AgentResponse)
    def conversation_settings(http_request: Request, body: ConversationRequest) -> Response:
        runtime = current_runtime()
        request = body_data(body)
        try:
            result = runtime.conversation_settings(body.id, request)
        except ValueError as error:
            if any(key in request for key in ("agent_mode", "expected_mode_revision", "subagent_concurrency")):
                return context.send(http_request, {"error": str(error), "outcome": "not_applied"}, status=400)
            raise
        return context.send(http_request, project("agent", result))

    @router.post("/api/configure", response_model=ConfigureResponse)
    def configure(http_request: Request, body: ConfigureRequest) -> Response:
        runtime = current_runtime()
        request = body_data(body)
        return context.send(http_request, runtime.configure(body.id, request))

    @router.post("/api/agents/account", response_model=AgentResponse)
    def select_account(http_request: Request, body: AccountSelectionRequest) -> Response:
        runtime = current_runtime()
        request = body_data(body)
        selected = runtime.set_account(body.id, body.account_key, body.cwd)
        with runtime.lock, runtime.db() as db:
            selected = runtime.agent(cast(str, selected["id"]), db)
            selected["empty"] = runtime.empty_lead(db, selected)
        return context.send(http_request, project("agent", selected))

    @router.post("/api/agents/account-transfer", response_model=TransferResponse)
    def account_transfer(http_request: Request, body: AccountTransferRequest) -> Response:
        runtime = current_runtime()
        from codex_account_transfer import transfer_store

        transfer_factory = cast(Callable[[AgentRuntime], TransferStore], transfer_store)
        transfers = transfer_factory(runtime)
        if isinstance(body, TransferActionRequest):
            operation = transfers.action(body.request_id, body.action.value)
        else:
            operation = transfers.request(
                body.id,
                body.account_key,
                body.request_id,
                body.scope.value,
            )
        return context.send(http_request, _public_transfer(operation))

    @router.post("/api/action", response_model=NativeActionResponse)
    def native_action(http_request: Request, body: NativeActionRequest) -> Response:
        runtime = current_runtime()
        request = body_data(body)
        if not isinstance(body.action, str):
            safety_action = body_data(body.action)
            return context.send(http_request, runtime.native_action(body.id, safety_action))
        action = body.action.value
        request_id = body.request_id
        if not request_id:
            return context.send(http_request, {
                "error": "Reload Studio before Review or Compact. This client has no durable action request ID.",
                "outcome": "not_applied",
            }, status=400)
        context_data = (
            cast(dict[str, JsonValue], request["context"])
            if isinstance(request.get("context"), dict)
            else None if "context" in request else {}
        )
        if set(request) - {"id", "action", "request_id", "context"}:
            return context.send(http_request, {"error": "Invalid native action fields", "outcome": "not_applied"}, status=400)
        try:
            result = runtime.native_action(body.id, action, request_id, context_data)
        except ValueError as error:
            return context.send(http_request, {"error": str(error), "outcome": "not_applied"}, status=400)
        return context.send(http_request, result)

    @router.post("/api/native-command", response_model=NativeCommandResponse | None)
    def native_command(http_request: Request, body: NativeCommandRequest) -> Response:
        runtime = current_runtime()
        request = body_data(body)
        return context.send(http_request, runtime.native_command_action(request))

    @router.post("/api/import", response_model=AgentResponse)
    def import_thread(http_request: Request, body: ImportRequest) -> Response:
        runtime = current_runtime()
        request = body_data(body)
        imported = runtime.import_thread(request)
        return context.send(http_request, project("agent", imported))

    @router.post("/api/stop", response_model=StopResponse)
    def stop(http_request: Request, body: StopRequest) -> Response:
        runtime = current_runtime()
        return context.send(http_request, runtime.stop(body.id, body.descendants))

    @router.post("/api/connection-recovery", response_model=RecoveryResponse)
    def connection_recovery(http_request: Request, body: IdRequest) -> Response:
        runtime = current_runtime()
        from codex_connection_recovery import recover

        recovery = cast(Callable[[AgentRuntime, str], dict[str, JsonValue]], recover)
        return context.send(http_request, recovery(runtime, body.id))

    @router.post("/api/context-repair", response_model=ContextRepairResponse)
    def context_repair(http_request: Request, body: IdRequest) -> Response:
        runtime = current_runtime()
        from codex_context_repair import repair_idle

        repair = cast(Callable[[AgentRuntime, str], dict[str, JsonValue]], repair_idle)
        repaired = repair(runtime, body.id)
        return context.send(http_request, {"id": repaired["id"], "repair": repaired.get("contextRepair")})

    @router.post("/api/capacity-retry", response_model=RetryResponse)
    def capacity_retry(http_request: Request, body: CapacityRetryRequest) -> Response:
        runtime = current_runtime()
        return context.send(http_request, runtime.capacity_retry(body.id, body.retry_id, body.action.value))

    @router.post("/api/usage-resume", response_model=UsageResumeResponse | None)
    def usage_resume(http_request: Request, body: UsageResumeRequest) -> Response:
        runtime = current_runtime()
        resumed = runtime.usage_resume_action(body.id, body.resume_id, body.enabled)
        return context.send(http_request, resumed)

    @router.post("/api/conversation/delete", response_model=DeletedResponse)
    def delete_conversation(http_request: Request, body: IdRequest) -> Response:
        runtime = current_runtime()
        return context.send(http_request, runtime.delete_conversation(body.id))

    @router.post("/api/rename", response_model=RenameResponse)
    def rename(http_request: Request, body: RenameRequest) -> Response:
        runtime = current_runtime()
        return context.send(http_request, runtime.rename(body.id, body.name, body.request_id))

    @router.get("/api/import", response_model=ImportListResponse)
    def import_list(request: Request, query: ImportListQuery = Depends()) -> Response:
        runtime = current_runtime()
        cursor = first_nonempty_query(request, "cursor")
        account_key = first_nonempty_query(request, "account_key") or "default"
        return context.send(request, runtime.import_list(cursor, account_key=account_key))

    @router.get("/api/capabilities", response_model=CapabilitiesResponse)
    def capabilities(request: Request, query: CapabilitiesQuery = Depends()) -> Response:
        runtime = current_runtime()
        agent = first_nonempty_query(request, "agent")
        return context.send(request, runtime.capabilities(agent), etag=True, weak_etag_fields=("at",))

    @router.get("/api/skills", response_model=SkillsResponse)
    def skills(request: Request, query: SkillsQuery = Depends()) -> Response:
        runtime = current_runtime()
        agent = first_nonempty_query(request, "agent")
        return context.send(request, runtime.skill_catalog(agent))

    return router


_TRANSFER_FIELDS = (
    "id", "leadId", "targetAccountKey", "status", "created", "updated",
    "scope", "targetProvider", "finishHistory", "requests",
)
_TRANSFER_MEMBER_FIELDS = (
    "phase", "sourceAccountKey", "sourceThreadId", "targetThreadId", "provider",
    "name", "reason", "error", "waiting", "lazy", "interruptReason",
    "interruptOutcome", "continueAfterTransfer",
)
_MEMBER_TERMINAL_PHASES = {TransferMemberPhase.COMPLETED, TransferMemberPhase.LEFT}
_MEMBER_MOVING_PHASES = {
    TransferMemberPhase.READING,
    TransferMemberPhase.SUBMITTED,
    TransferMemberPhase.LAZY_SUBMITTED,
    TransferMemberPhase.INTERRUPTING,
}
_MEMBER_WAIT_EXCLUDED_PHASES = {
    TransferMemberPhase.BLOCKED,
    TransferMemberPhase.UNKNOWN,
    TransferMemberPhase.LAZY,
    TransferMemberPhase.LAZY_SUBMITTED,
}


def _public_transfer(operation: dict[str, JsonValue]) -> TransferResponse:
    """Project an internal transfer operation to fields used by the UI."""
    projected: dict[str, JsonValue] = {
        key: operation[key] for key in _TRANSFER_FIELDS if key in operation
    }
    raw_members = operation.get("members")
    members: dict[str, JsonValue] = {}
    can_retry = False
    if isinstance(raw_members, dict):
        for agent_id, raw_member in raw_members.items():
            if isinstance(raw_member, dict):
                if (
                    raw_member.get("phase") == TransferMemberPhase.BLOCKED.value
                    and raw_member.get("archiveInvalidated") is not True
                ):
                    can_retry = True
                members[agent_id] = {
                    key: raw_member[key]
                    for key in _TRANSFER_MEMBER_FIELDS
                    if key in raw_member
                }
    projected["members"] = members

    phase_members: list[tuple[TransferMemberPhase, dict[str, JsonValue]]] = []
    for member in members.values():
        if not isinstance(member, dict):
            continue
        phase_value = member.get("phase")
        if isinstance(phase_value, str):
            phase_members.append((TransferMemberPhase(phase_value), member))
    phases = [phase for phase, _member in phase_members]
    projected.update({
        "total": len(phases),
        "completed": sum(phase in _MEMBER_TERMINAL_PHASES for phase in phases),
        "moved": sum(
            phase is TransferMemberPhase.COMPLETED or member.get("lazy") is True
            for phase, member in phase_members
        ),
        "nativeHistoryPending": sum(
            isinstance(member, dict)
            and member.get("lazy") is True
            and member.get("phase") != TransferMemberPhase.COMPLETED.value
            for member in members.values()
        ),
        "movingNow": sum(phase in _MEMBER_MOVING_PHASES for phase in phases),
        "canFinishHistory": False,
        "interrupted": _transfer_notices(members, "interruptReason"),
        "leftOnSource": _transfer_notices(members, "phase", "left"),
        "blocked": _transfer_notices(members, "phase", "blocked", "unknown"),
        "waitingCount": sum(
            phase not in _MEMBER_TERMINAL_PHASES
            and phase not in _MEMBER_WAIT_EXCLUDED_PHASES
            for phase in phases
        ),
        "needsAttention": any(
            phase in {TransferMemberPhase.BLOCKED, TransferMemberPhase.UNKNOWN}
            for phase in phases
        ),
        "canRetry": can_retry,
    })
    return TransferResponse.model_validate(projected)


def _transfer_notices(
    members: dict[str, JsonValue],
    condition_key: str,
    *condition_values: str,
) -> list[JsonValue]:
    notices: list[JsonValue] = []
    for agent_id, member in members.items():
        if not isinstance(member, dict):
            continue
        condition = member.get(condition_key)
        if not condition:
            continue
        if condition_values and condition not in condition_values:
            continue
        notice: dict[str, JsonValue] = {"id": agent_id}
        for key in ("name", "provider"):
            value = member.get(key)
            if isinstance(value, str):
                notice[key] = value
        reason_key = "interruptReason" if condition_key == "interruptReason" else (
            "error" if condition_key == "phase" and condition in {"blocked", "unknown"}
            else "reason"
        )
        reason = member.get(reason_key)
        if isinstance(reason, str):
            notice["reason"] = reason
        notices.append(notice)
    return notices
