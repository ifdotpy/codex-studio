"""FastAPI routes for agent lifecycle, recovery, and controls."""

from fastapi import APIRouter, Depends, Request
from starlette.responses import Response

from studio_api.context import ApiContext
from studio_api.sync.models import AgentEntityDto
from codex_sync_entities import project

from .models import (
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
    UsageResumeRequest,
    UsageResumeResponse,
)
from studio_api.models import ResponseModel


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
    runtime = context.runtime

    @router.post("/api/leads", response_model=AgentEntityDto)
    def create_lead(http_request: Request, body: CreateLeadRequest) -> Response:
        request = body.model_dump(mode="json", exclude_unset=True)
        created = runtime.new_lead(request)
        # Preserve the original receipt-before-snapshot behavior.
        with runtime.lock, runtime.db() as db:
            created = runtime.agent(created["id"], db)
            created["empty"] = runtime.empty_lead(db, created)
        return context.send(http_request, project("agent", created))

    @router.post("/api/agents", response_model=AgentEntityDto)
    def create_agent(http_request: Request, body: CreateAgentRequest) -> Response:
        request = body.model_dump(mode="json", exclude_unset=True)
        created = runtime.create(request, parent=request.get("parent"))
        return context.send(http_request, project("agent", created))

    @router.post("/api/conversation", response_model=AgentEntityDto)
    def conversation_settings(http_request: Request, body: ConversationRequest) -> Response:
        request = body.model_dump(mode="json", exclude_unset=True)
        try:
            result = runtime.conversation_settings(request["id"], request)
        except ValueError as error:
            if any(key in request for key in ("agent_mode", "expected_mode_revision", "subagent_concurrency")):
                return context.send(http_request, {"error": str(error), "outcome": "not_applied"}, status=400)
            raise
        return context.send(http_request, project("agent", result))

    @router.post("/api/configure", response_model=ConfigureResponse)
    def configure(http_request: Request, body: ConfigureRequest) -> Response:
        request = body.model_dump(mode="json", exclude_unset=True)
        return context.send(http_request, runtime.configure(request["id"], request))

    @router.post("/api/agents/account", response_model=AgentEntityDto)
    def select_account(http_request: Request, body: AccountSelectionRequest) -> Response:
        request = body.model_dump(mode="json", exclude_unset=True)
        selected = runtime.set_account(request["id"], request["account_key"], request.get("cwd"))
        with runtime.lock, runtime.db() as db:
            selected = runtime.agent(selected["id"], db)
            selected["empty"] = runtime.empty_lead(db, selected)
        return context.send(http_request, project("agent", selected))

    @router.post("/api/agents/account-transfer", response_model=TransferResponse)
    def account_transfer(http_request: Request, body: AccountTransferRequest) -> Response:
        from codex_account_transfer import transfer_store

        request = body.model_dump(mode="json", exclude_unset=True)
        transfers = transfer_store(runtime)
        if request.get("action"):
            return context.send(http_request, transfers.action(request["request_id"], request["action"]))
        return context.send(http_request, transfers.request(
            request.get("id"), request.get("account_key"), request["request_id"],
            request.get("scope", "team"),
        ))

    @router.post("/api/action", response_model=NativeActionResponse)
    def native_action(http_request: Request, body: NativeActionRequest) -> Response:
        request = body.model_dump(mode="json", exclude_unset=True)
        action = request["action"]
        if isinstance(action, dict):
            return context.send(http_request, runtime.native_action(request["id"], action))
        request_id = request.get("request_id")
        if not request_id:
            return context.send(http_request, {
                "error": "Reload Studio before Review or Compact. This client has no durable action request ID.",
                "outcome": "not_applied",
            }, status=400)
        context_data = request.get("context", {})
        if set(request) - {"id", "action", "request_id", "context"}:
            return context.send(http_request, {"error": "Invalid native action fields", "outcome": "not_applied"}, status=400)
        try:
            result = runtime.native_action(request["id"], action, request_id, context_data)
        except ValueError as error:
            return context.send(http_request, {"error": str(error), "outcome": "not_applied"}, status=400)
        return context.send(http_request, result)

    @router.post("/api/native-command", response_model=NativeCommandResponse | None)
    def native_command(http_request: Request, body: NativeCommandRequest) -> Response:
        return context.send(http_request, runtime.native_command_action(body.model_dump(mode="json", exclude_unset=True)))

    @router.post("/api/import", response_model=AgentEntityDto)
    def import_thread(http_request: Request, body: ImportRequest) -> Response:
        imported = runtime.import_thread(body.model_dump(mode="json", exclude_unset=True))
        return context.send(http_request, project("agent", imported))

    @router.post("/api/stop", response_model=StopResponse)
    def stop(http_request: Request, body: StopRequest) -> Response:
        request = body.model_dump(mode="json", exclude_unset=True)
        return context.send(http_request, runtime.stop(request["id"], request.get("descendants", True)))

    @router.post("/api/connection-recovery", response_model=RecoveryResponse)
    def connection_recovery(http_request: Request, body: IdRequest) -> Response:
        from codex_connection_recovery import recover

        return context.send(http_request, recover(runtime, body.id))

    @router.post("/api/context-repair", response_model=ContextRepairResponse)
    def context_repair(http_request: Request, body: IdRequest) -> Response:
        from codex_context_repair import repair_idle

        repaired = repair_idle(runtime, body.id)
        return context.send(http_request, {"id": repaired["id"], "repair": repaired.get("contextRepair")})

    @router.post("/api/capacity-retry", response_model=RetryResponse)
    def capacity_retry(http_request: Request, body: CapacityRetryRequest) -> Response:
        return context.send(http_request, runtime.capacity_retry(body.id, body.retry_id, body.action.value))

    @router.post("/api/usage-resume", response_model=UsageResumeResponse | None)
    def usage_resume(http_request: Request, body: UsageResumeRequest) -> Response:
        resumed = runtime.usage_resume_action(body.id, body.resume_id, body.enabled)
        return context.send(http_request, resumed)

    @router.post("/api/conversation/delete", response_model=DeletedResponse)
    def delete_conversation(http_request: Request, body: IdRequest) -> Response:
        return context.send(http_request, runtime.delete_conversation(body.id))

    @router.post("/api/rename", response_model=RenameResponse)
    def rename(http_request: Request, body: RenameRequest) -> Response:
        return context.send(http_request, runtime.rename(body.id, body.name))

    @router.get("/api/import", response_model=ImportListResponse)
    def import_list(request: Request, query: ImportListQuery = Depends()) -> Response:
        cursor = _first_query(request, "cursor")
        account_key = _first_query(request, "account_key") or "default"
        return context.send(request, runtime.import_list(cursor, account_key=account_key))

    @router.get("/api/capabilities", response_model=CapabilitiesResponse)
    def capabilities(request: Request, query: CapabilitiesQuery = Depends()) -> Response:
        agent = _first_query(request, "agent")
        return context.send(request, runtime.capabilities(agent), etag=True, weak_etag_fields=("at",))

    @router.get("/api/skills", response_model=SkillsResponse)
    def skills(request: Request, query: SkillsQuery = Depends()) -> Response:
        agent = _first_query(request, "agent")
        return context.send(request, runtime.skill_catalog(agent))

    return router


def _first_query(request: Request, key: str) -> str | None:
    """Match parse_qs defaults: first value, with empty values omitted."""
    values = request.query_params.getlist(key)
    return next((value for value in values if value != ""), None)
