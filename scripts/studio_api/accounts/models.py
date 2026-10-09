"""Strict HTTP contracts for account, project, and provider settings."""

from __future__ import annotations

from typing import Annotated, Literal
from uuid import UUID

from pydantic import AfterValidator, ConfigDict, Field, StrictBool, StrictInt

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel
from studio_api.sync.models import RoomEntityDto, RoomRadio, RoomRadioActive, SidebarOrderDto
from studio_api.multi_server.models import ServerOperationResponse


def _uuid_string(value: str) -> str:
    try:
        UUID(value)
    except ValueError as error:
        raise ValueError("Supply a UUID request_id") from error
    return value


RequestUUID = Annotated[str, AfterValidator(_uuid_string)]


class AccountStatus(ContractStrEnum):
    READY = "ready"
    SIGNED_OUT = "signedOut"
    PENDING = "pending"
    STARTING = "starting"
    ERROR = "error"
    CHANGED = "changed"
    DUPLICATE = "duplicate"


class ClaudeOptions(ContractModel):
    binaryPath: str | None = None
    configDir: str | None = None
    launchArgs: str | None = None
    autoCompactWindow: int | None = None
    customModels: list[ClaudeCustomModel] | None = None


class ClaudeCustomModel(ContractModel):
    id: str
    label: str


class Account(ContractModel):
    id: str
    home: str
    label: str
    source: str
    provider: Literal["codex", "claude"] | None = None
    accountId: str | None = None
    email: str | None = None
    plan: str | None = None
    status: AccountStatus
    error: str | None = None
    disconnected: bool | None = None
    deleted: bool | None = None
    duplicateOf: str | None = None
    claudeOptions: ClaudeOptions | None = None
    authenticationRecovery: str | None = None


class CodexLoginReceipt(ContractModel):
    requestId: str
    accountKey: str
    status: Literal["starting", "pending", "ready", "duplicate", "cancelled", "error", "uncertain"]
    loginId: str | None = None
    verificationUrl: str | None = None
    userCode: str | None = None
    error: str | None = None
    resolvedAccountKey: str | None = None
    createdAt: float | None = None
    reauthAccountKey: str | None = None
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class AccountLoginResponse(ResponseModel):
    requestId: str
    accountKey: str
    status: Literal["starting", "pending", "ready", "duplicate", "cancelled", "error", "uncertain"]
    loginId: str | None = None
    verificationUrl: str | None = None
    userCode: str | None = None
    error: str | None = None
    resolvedAccountKey: str | None = None
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ClaudeLoginReceipt(ContractModel):
    requestId: str
    accountKey: str
    status: Literal["starting", "pending", "ready", "cancelled", "error"]
    email: str | None = None
    verificationUrl: str | None = None
    error: str | None = None
    chatsRefreshed: bool | None = None


class AccountsResponse(ResponseModel):
    accounts: list[Account]
    archivedAccounts: list[Account] = Field(default_factory=list)
    defaultAccountKey: str
    logins: list[CodexLoginReceipt] = Field(default_factory=list)
    supportsDisconnect: bool | None = None
    supportsDelete: bool | None = None


class ClaudeLoginResponse(ResponseModel):
    requestId: str
    accountKey: str
    status: Literal["starting", "pending", "ready", "cancelled", "error"]
    email: str | None = None
    verificationUrl: str | None = None
    error: str | None = None
    chatsRefreshed: bool | None = None


class ProjectFolder(ContractModel):
    id: str
    name: str
    parentId: str | None = None


class PeerTeam(ContractModel):
    id: str
    name: str
    members: list[str]


class ProjectLocation(ContractModel):
    serverId: str
    path: str
    projectId: str
    gitOrigin: str | None = None


class ProjectAlias(ContractModel):
    serverId: str
    projectId: str
    name: str


class Project(ContractModel):
    homeServerId: str | None = None
    locations: list[ProjectLocation] | None = None
    locationsRevision: int | None = None
    projectAliases: list[ProjectAlias] | None = None
    id: str
    path: str
    name: str
    created: float
    updated: float | None = None
    accountKey: str | None = None
    accountKeys: list[str] | None = None
    accountRevision: int | None = None
    workerBaseRef: str | None = None
    workerBaseRevision: int | None = None
    workerEnvironment: Literal["host", "linux"] | None = None
    workerEnvironmentRevision: int | None = None
    organizationRevision: int | None = None
    peerTeamsRevision: int | None = None
    folders: list[ProjectFolder] | None = None
    peerTeams: list[PeerTeam] | None = None
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ProjectMutationRecord(Project, ResponseModel):
    """A project record returned directly by successful project writes."""


class ProjectRemovalResponse(ResponseModel):
    id: str
    removed: bool


class SidebarOrderResponse(SidebarOrderDto, ResponseModel):
    pass


class RemoteProjectRegistrationResponse(ServerOperationResponse):
    projectId: str


ProjectMutationResponse = ProjectMutationRecord | ProjectRemovalResponse | SidebarOrderResponse | RemoteProjectRegistrationResponse | ServerOperationResponse


class ProjectReadResponse(ResponseModel):
    items: list[Project]


class PeerRadioActive(RoomRadioActive):
    epoch: int
    interruptRequested: bool | None = None
    questionContinuationPlanned: bool | None = None


class PeerRoomRadio(RoomRadio):
    active: PeerRadioActive | None


class PeerRoomResponse(RoomEntityDto):
    members: list[str]
    customName: str | None = None
    created: float | None = None
    radio: PeerRoomRadio


class ProviderModel(ContractModel):
    """One extensible provider catalog entry; names remain provider-owned strings."""

    model: str
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class UnavailableAccount(ContractModel):
    accountKey: str
    error: str
    catalogPending: bool | None = None


class ModelCatalogResponse(ResponseModel):
    data: list[ProviderModel] | None = None
    nextCursor: str | None = None
    error: str | None = None
    catalogPending: bool | None = None
    accountKey: str | None = None
    unavailableAccounts: list[UnavailableAccount] = Field(default_factory=list)


class RateLimitWindow(ContractModel):
    usedPercent: int | float | None = None
    resetsAt: int | float | None = None
    windowDurationMins: int | float | None = None


class RateLimitIndividualLimit(ContractModel):
    remainingPercent: int | float | None = None


class RateLimitBucket(ContractModel):
    limitId: str | None = None
    limitName: str | None = None
    primary: RateLimitWindow | None = None
    secondary: RateLimitWindow | None = None
    spendControlReached: bool | None = None
    individualLimit: RateLimitIndividualLimit | None = None
    rateLimitReachedType: str | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class RateLimitResetCredit(ContractModel):
    id: str
    status: str
    resetType: str
    grantedAt: int | float | None = None
    expiresAt: int | float | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class RateLimitResetCredits(ContractModel):
    availableCount: int | None = None
    credits: list[RateLimitResetCredit] = Field(default_factory=list)
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class UsageLimitsData(ContractModel):
    accountId: str | None = None
    rateLimits: RateLimitBucket | None = None
    rateLimitsByLimitId: dict[str, RateLimitBucket] | None = None
    rateLimitResetCredits: RateLimitResetCredits | None = None
    ordinaryUsageAllowed: bool | None = None
    status: str | None = None
    signedIn: bool | None = None
    source: str | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class UsageLimitsResponse(ResponseModel):
    accountKey: str
    data: UsageLimitsData | None = None
    at: float | None = None
    error: str | None = None
    readAt: float | None = None
    checkedAt: float | None = None
    processedAt: float | None = None


class LimitResetResponse(ResponseModel):
    outcome: Literal["reset", "nothingToReset", "noCredit", "alreadyRedeemed", "uncertain"]
    request_id: str
    credit_id: str
    account_id: str
    account_key: str
    limits: UsageLimitsResponse
    error: str | None = None


class ResetRequest(ContractModel):
    account_key: str = "default"
    account_id: str = Field(min_length=1, max_length=1024)
    credit_id: str = Field(min_length=1, max_length=1024)
    request_id: RequestUUID


class PeerProjectResponse(Project, ResponseModel):
    """Project record returned by peer team save/delete/move operations."""


class PeerConversionResponse(ResponseModel):
    id: str
    parentId: str
    rootId: str
    movedAgents: list[str]
    peerTeamsRevision: int


class PeerRadioResponse(ResponseModel):
    room: PeerRoomResponse


PeerTeamsResponse = PeerProjectResponse | PeerConversionResponse | PeerRadioResponse


class AccountKeyRequest(ContractModel):
    account_key: str | None = None


class RequiredAccountKeyRequest(ContractModel):
    account_key: str


class RegisterAccountRequest(ContractModel):
    home: str = Field(min_length=1)


class DeleteAccountRequest(RequiredAccountKeyRequest):
    request_id: RequestUUID


def _account_name(value: str) -> str:
    if len(value.strip()) > 32:
        raise ValueError("The account name must contain at most 32 characters")
    return value


class AccountNameRequest(RequiredAccountKeyRequest):
    label: Annotated[str, AfterValidator(_account_name)]
    request_id: RequestUUID


class LoginRequest(ContractModel):
    request_id: RequestUUID
    account_key: str | None = None


class ClaudeStartRequest(ContractModel):
    request_id: RequestUUID
    account_key: str


class ClaudeCodeRequest(ContractModel):
    request_id: RequestUUID
    code: str = Field(min_length=1, max_length=4096)


class ClaudeCancelRequest(ContractModel):
    request_id: RequestUUID


class ClaudeProfileRequest(ContractModel):
    account_key: str | None = None
    options: ClaudeOptions | None = None
    label: str | None = None


class ClaudeSessionStateRequest(ContractModel):
    action: Literal["state", "commands"]
    id: str


class ClaudeSessionCommandRequest(ContractModel):
    action: Literal["command"]
    id: str
    command: str
    request_id: str = Field(min_length=1)


class ClaudeSessionSettingsRequest(ContractModel):
    action: Literal["settings"]
    id: str
    settings: ClaudeSettings
    request_id: str | None = Field(default=None, min_length=1)


class ClaudeSessionRollbackRequest(ContractModel):
    action: Literal["rollback"]
    id: str
    turn_id: str
    request_id: str = Field(min_length=1)


class ClaudeSettings(ContractModel):
    permissionMode: Literal["default", "acceptEdits", "auto", "plan", "bypassPermissions"] | None = None
    thinking: StrictBool | None = None
    autoCompactWindow: StrictInt | None = Field(default=None, ge=100000, le=1000000)


class ClaudeSessionStopTaskRequest(ContractModel):
    action: Literal["stop_task"]
    id: str
    task_id: str


class AccountDiscoverRequest(ContractModel):
    """The legacy discover operation accepts an empty JSON object or no body."""


class ClaudeSessionSettings(ClaudeSettings):
    binaryPath: str | None = None
    configDir: str | None = None
    launchArgs: str | None = None
    customModels: list[ClaudeCustomModel] | None = None


class ClaudeControlOperation(ContractModel):
    requestId: str
    turnId: str | None = None
    phase: Literal["provider_pending", "provider_ready", "completed", "failed"]
    error: str | None = None


class ClaudeTurn(ContractModel):
    id: str
    status: Literal["inProgress", "completed", "failed", "interrupted"]
    text: str


class ClaudeBackgroundTask(ContractModel):
    task_id: str
    description: str | None = None
    status: str | None = None
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ClaudeSessionStateResponse(ResponseModel):
    settings: ClaudeSessionSettings = Field(default_factory=ClaudeSessionSettings)
    turns: list[ClaudeTurn]
    tasks: list[ClaudeBackgroundTask] = Field(default_factory=list)
    controlOperation: ClaudeControlOperation | None = None
    version: str | None = None
    nativeId: str | None = None
    contextWindow: int | None = None
    totalTokens: int | None = None


class ClaudeCommand(ContractModel):
    name: str
    description: str
    argumentHint: str
    builtin: bool
    aliases: list[str]


class ClaudeDeliveryResponse(ResponseModel):
    id: str
    status: Literal[
        "pending", "queued", "reserved", "dispatching", "delivered",
        "uncertain", "stored_only", "cancelled", "failed",
    ]
    error: str | None = None


class ClaudeSettingsResponse(ResponseModel):
    settings: ClaudeSessionSettings


class ClaudeRollbackResponse(ResponseModel):
    threadId: str
    nativeId: str
    removedTurns: int


class ClaudeStopTaskResponse(ResponseModel):
    pass


ClaudeSessionResponse = (
    ClaudeSessionStateResponse
    | list[ClaudeCommand]
    | ClaudeDeliveryResponse
    | ClaudeSettingsResponse
    | ClaudeRollbackResponse
    | ClaudeStopTaskResponse
)


class LimitsQuery(ContractModel):
    account_key: str = "default"
    cached: str | None = None


class ModelsQuery(ContractModel):
    account_key: str = "default"
    workers: str | None = None
    retry: Literal["1"] | None = None


class ClaudeLoginQuery(ContractModel):
    request_id: str | None = None


class PeerTeamSaveRequest(ContractModel):
    action: Literal["save"]
    path: str
    expected_revision: int
    team_id: str | None = None
    request_id: str
    name: str
    members: list[str]


class PeerTeamDeleteRequest(ContractModel):
    action: Literal["delete"]
    path: str
    expected_revision: int
    team_id: str
    request_id: str


class PeerTeamMoveRequest(ContractModel):
    action: Literal["move"]
    path: str
    expected_revision: int
    team_id: str | None = None
    member: str
    request_id: str


class PeerTeamRadioOpenRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["open"]
    path: str
    team_id: str
    request_id: str
    expected_revision: int | None = None


class PeerTeamRadioSendRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["send"]
    path: str
    team_id: str
    request_id: str
    expected_revision: int
    text: str
    target: str = "both"
    rounds: Literal[1, 2] = 1


class PeerTeamRadioPassRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["pass"]
    path: str
    team_id: str
    request_id: str
    expected_revision: int
    target: str
    rounds: Literal[1, 2] = 1


class PeerTeamRadioStopRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["stop"]
    path: str
    team_id: str
    request_id: str
    expected_revision: int


class SharedParticipantRequest(ContractModel):
    account_key: str = Field(min_length=1, max_length=255)
    model: str = Field(min_length=1, max_length=255)
    effort: str | None = None


class PeerTeamRadioCreateRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["create"]
    path: str = Field(min_length=1, max_length=4096)
    request_id: str = Field(min_length=1, max_length=255)
    name: str = Field(min_length=1, max_length=80)
    participants: list[SharedParticipantRequest] = Field(min_length=2, max_length=2)


class PeerTeamConvertRequest(ContractModel):
    action: Literal["convert"]
    path: str
    member: str
    target: str
    request_id: str
    expected_revision: int


class ProjectRegisterRequest(ContractModel):
    path: str
    name: str | None = None
    account_key: str | None = None
    action: Literal["register"] = "register"


class ProjectPathActionRequest(ContractModel):
    action: Literal["remove", "set_account", "set_worker_base"]
    path: str
    name: str | None = None
    account_key: str | None = None
    expected_revision: int | None = None
    base_ref: str | None = None


class ProjectOrganizationRequest(ContractModel):
    action: Literal["rename", "add_folder", "rename_folder", "remove_folder"]
    path: str
    expected_revision: int
    name: str | None = None
    folder_id: str | None = None
    parent_id: str | None = None


class ProjectAccountsRequest(ContractModel):
    action: Literal["set_accounts"]
    path: str
    account_key: str
    account_keys: list[str]
    expected_revision: int


class SidebarReorderRequest(ContractModel):
    action: Literal["reorder"]
    request_id: str = Field(min_length=1, max_length=255)
    expected_revision: int = Field(ge=0)
    groups: dict[str, list[str]]
    migration: bool | None = None


class ProjectLocationQuery(ContractModel):
    action: Literal["info", "matches"] = "info"
    project: str = Field(min_length=1, max_length=4096)
    server: str = Field(min_length=1, max_length=128)
    path: str | None = Field(default=None, max_length=4096)
    request_id: str | None = Field(default=None, min_length=1, max_length=128)


class ProjectLocationMatch(ContractModel):
    projectId: str
    path: str
    name: str


class ProjectLocationQueryResponse(ResponseModel):
    gitHead: str | None = None
    gitOrigin: str | None = None
    matches: list[ProjectLocationMatch] = Field(default_factory=list)


class ProjectLocationRequest(ContractModel):
    action: Literal["add_location", "remove_location"]
    project: str = Field(min_length=1, max_length=4096)
    server: str = Field(min_length=1, max_length=128)
    path: str | None = Field(default=None, max_length=4096)
    request_id: str = Field(min_length=1, max_length=128)


class ProjectWriteRequest(ContractModel):
    server: str | None = Field(default=None, max_length=128)
    request_id: str | None = Field(default=None, min_length=1, max_length=128)
    action: Literal["register", "remove", "set_account", "set_worker_base", "set_worker_environment", "set_accounts",
                    "rename", "add_folder", "rename_folder", "remove_folder"] | None = None
    path: str = Field(min_length=1)
    name: str | None = None
    account_key: str | None = None
    account_keys: list[str] | None = None
    expected_revision: int | None = None
    base_ref: str | None = None
    environment: Literal["host", "linux"] | None = None
    folder_id: str | None = None
    parent_id: str | None = None


PeerTeamRadioRequestUnion = Annotated[
    PeerTeamRadioOpenRequest
    | PeerTeamRadioSendRequest
    | PeerTeamRadioPassRequest
    | PeerTeamRadioStopRequest
    | PeerTeamRadioCreateRequest,
    Field(discriminator="radio_action"),
]


PeerTeamRequest = (
    PeerTeamSaveRequest
    | PeerTeamDeleteRequest
    | PeerTeamMoveRequest
    | PeerTeamConvertRequest
    | PeerTeamRadioRequestUnion
)


class ProjectAccountSetRequest(ProjectWriteRequest):
    action: Literal["set_accounts"]
    account_key: str
    account_keys: list[str]
    expected_revision: int


class ProjectWorkerBaseRequest(ProjectWriteRequest):
    action: Literal["set_worker_base"]
    expected_revision: int


class ProjectWorkerEnvironmentRequest(ProjectWriteRequest):
    action: Literal["set_worker_environment"]
    environment: Literal["host", "linux"]
    expected_revision: int
