"""Strict HTTP contracts for account, project, and provider settings."""

from __future__ import annotations

from typing import Annotated, Literal

from pydantic import ConfigDict, Field, StrictBool, StrictInt, field_validator

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel, SafeInt
from studio_api.sync.models import RoomEntityDto, RoomRadio, RoomRadioActive, SidebarOrderDto
from studio_api.multi_server.models import ServerOperationResponse


RequestUUID = Annotated[
    str,
    Field(
        min_length=36,
        max_length=36,
        pattern=r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
    ),
]


class AccountStatus(ContractStrEnum):
    READY = "ready"
    SIGNED_OUT = "signedOut"
    PENDING = "pending"
    STARTING = "starting"
    ERROR = "error"
    CHANGED = "changed"
    DUPLICATE = "duplicate"


class ClaudeOptions(ContractModel):
    """Stored account options are returned through AccountStore._public."""

    binaryPath: str | None = None
    configDir: str | None = None
    launchArgs: str | None = None
    autoCompactWindow: SafeInt | None = None
    customModels: list[ClaudeCustomModel] | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ClaudeCustomModel(ContractModel):
    """Custom model records are nested in the stored account passthrough."""

    id: str
    label: str
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class Account(ContractModel):
    """Known profile fields; AccountStore._public passes public stored fields through."""

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
    canAttemptNativeProof: bool | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class CodexLoginReceipt(ContractModel):
    """Known fields; AccountStore.login_receipts() passes persisted receipts through."""

    requestId: str
    accountKey: str
    status: Literal["starting", "pending", "ready", "duplicate", "cancelled", "error", "uncertain"]
    createdAt: float | None = None
    loginId: str | None = None
    verificationUrl: str | None = None
    userCode: str | None = None
    error: str | None = None
    email: str | None = None
    resolvedAccountKey: str | None = None
    reauthAccountKey: str | None = None
    expiresAt: float | None = None
    expectedAccountId: str | None = None
    emailHint: str | None = None
    requestedLabel: str | None = None
    nativeCompleted: bool | None = None
    model_config = ConfigDict(extra="allow", strict=True)
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
    email: str | None = None
    createdAt: float | None = None
    expiresAt: float | None = None


class ClaudeLoginReceipt(ContractModel):
    requestId: str
    accountKey: str
    status: Literal["starting", "pending", "ready", "cancelled", "error"]
    email: str | None = None
    plan: str | None = None
    codeSubmitted: bool | None = None
    verificationUrl: str | None = None
    error: str | None = None
    chatsRefreshed: bool | None = None


class AccountsResponse(ResponseModel):
    accounts: list[Account]
    archivedAccounts: list[Account]
    defaultAccountKey: str
    logins: list[CodexLoginReceipt]
    supportsDisconnect: bool
    supportsDelete: bool


class ClaudeLoginResponse(ResponseModel):
    """Known receipt fields; LoginManager returns its saved receipt directly."""

    requestId: str
    accountKey: str
    status: Literal["starting", "pending", "ready", "cancelled", "error"]
    email: str | None = None
    plan: str | None = None
    codeSubmitted: bool | None = None
    verificationUrl: str | None = None
    error: str | None = None
    chatsRefreshed: bool | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ProjectFolder(ContractModel):
    """Folder records are nested in the stored project passthrough."""

    id: str
    name: str
    parentId: str | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class PeerTeam(ContractModel):
    """Peer-team records are nested in the stored project passthrough."""

    id: str
    name: str
    members: list[str]
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ProjectLocation(ContractModel):
    """Location records are nested in the stored project passthrough."""

    serverId: str
    path: str
    requestedPath: str | None = None
    projectId: str
    gitOrigin: str | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ProjectAlias(ContractModel):
    """Alias records are nested in the stored project passthrough."""

    serverId: str
    projectId: str
    name: str
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ProjectRegistration(ContractModel):
    """Registration records are nested in the stored project passthrough."""

    action: Literal["register"]
    project: str
    server: str
    path: str
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class Project(ContractModel):
    """Known project fields; records(db, \"projects\") passes stored records through."""

    homeServerId: str | None = None
    locations: list[ProjectLocation] | None = None
    locationsRevision: SafeInt | None = None
    projectAliases: list[ProjectAlias] | None = None
    id: str
    path: str
    name: str
    created: float
    updated: float | None = None
    accountKey: str | None = None
    accountKeys: list[str] | None = None
    accountRevision: SafeInt | None = None
    workerBaseRef: str | None = None
    workerBaseRevision: SafeInt | None = None
    workerEnvironment: Literal["host", "linux"] | None = None
    workerEnvironmentRevision: SafeInt | None = None
    organizationRevision: SafeInt | None = None
    peerTeamsRevision: SafeInt | None = None
    folders: list[ProjectFolder] | None = None
    peerTeams: list[PeerTeam] | None = None
    registration: ProjectRegistration | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ProjectMutationRecord(Project, ResponseModel):
    """A project record returned directly by successful project writes."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue]


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
    epoch: SafeInt
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
    usedPercent: SafeInt | float | None = None
    resetsAt: SafeInt | float | None = None
    windowDurationMins: SafeInt | float | None = None


class RateLimitIndividualLimit(ContractModel):
    remainingPercent: SafeInt | float | None = None


class RateLimitBucket(ContractModel):
    """Known rate-limit fields plus provider-owned fields passed through unchanged."""

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
    """A provider reset-credit record; native service fields remain extensible."""

    id: str
    status: str
    resetType: str
    grantedAt: SafeInt | float | None = None
    expiresAt: SafeInt | float | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class RateLimitResetCredits(ContractModel):
    """Provider-owned reset-credit summary with the known count and credit fields."""

    availableCount: SafeInt | None = None
    credits: list[RateLimitResetCredit] = Field(default_factory=list)
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class UsageLimitsData(ContractModel):
    """Known usage projection plus account-service fields passed through unchanged."""

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

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue]


class PeerConversionResponse(ResponseModel):
    id: str
    parentId: str
    rootId: str
    movedAgents: list[str]
    peerTeamsRevision: SafeInt


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


class AccountNameRequest(RequiredAccountKeyRequest):
    label: str = Field(max_length=32)
    request_id: RequestUUID

    @field_validator("label")
    @classmethod
    def require_unicode_scalar_label(cls, value: str) -> str:
        try:
            value.encode("utf-8")
        except UnicodeEncodeError as error:
            raise ValueError("Label must contain valid Unicode scalar values") from error
        return value


class LoginRequest(ContractModel):
    login_id: RequestUUID
    account_key: str | None = None
    email: str | None = Field(default=None, max_length=320, pattern=r"\S")
    label: str | None = Field(default=None, min_length=1, max_length=32, pattern=r"\S")


class ClaudeStartRequest(ContractModel):
    login_id: RequestUUID
    account_key: str


class ClaudeAddStartRequest(ContractModel):
    login_id: RequestUUID
    email: str | None = Field(default=None, max_length=320, pattern=r"\S")
    label: str | None = Field(default=None, min_length=1, max_length=32, pattern=r"\S")


class ClaudeCodeRequest(ContractModel):
    login_id: RequestUUID
    code: str = Field(min_length=1, max_length=4096)


class ClaudeCancelRequest(ContractModel):
    login_id: RequestUUID


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
    autoCompactWindow: SafeInt | None = Field(default=None, ge=100000, le=1000000)


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
    contextWindow: SafeInt | None = None
    totalTokens: SafeInt | None = None


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
    removedTurns: SafeInt


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
    cached: Literal["1"] | None = None


class ModelsQuery(ContractModel):
    account_key: str = "default"
    workers: Literal["1"] | None = None
    retry: Literal["1"] | None = None


class ClaudeLoginQuery(ContractModel):
    request_id: str | None = None


class PeerTeamSaveRequest(ContractModel):
    action: Literal["save"]
    path: str
    expected_revision: SafeInt
    team_id: str | None = None
    request_id: str
    name: str
    members: list[str]


class PeerTeamDeleteRequest(ContractModel):
    action: Literal["delete"]
    path: str
    expected_revision: SafeInt
    team_id: str
    request_id: str


class PeerTeamMoveRequest(ContractModel):
    action: Literal["move"]
    path: str
    expected_revision: SafeInt
    team_id: str | None = None
    member: str
    request_id: str


class PeerTeamRadioOpenRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["open"]
    path: str
    team_id: str
    request_id: str
    expected_revision: SafeInt | None = None


class PeerTeamRadioSendRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["send"]
    path: str
    team_id: str
    request_id: str
    expected_revision: SafeInt
    text: str
    target: str = "both"
    rounds: SafeInt = Field(default=1, ge=1, le=2)


class PeerTeamRadioPassRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["pass"]
    path: str
    team_id: str
    request_id: str
    expected_revision: SafeInt
    target: str
    rounds: SafeInt = Field(default=1, ge=1, le=2)


class PeerTeamRadioStopRequest(ContractModel):
    action: Literal["radio"]
    radio_action: Literal["stop"]
    path: str
    team_id: str
    request_id: str
    expected_revision: SafeInt


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
    expected_revision: SafeInt


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
    expected_revision: SafeInt | None = None
    base_ref: str | None = None


class ProjectOrganizationRequest(ContractModel):
    action: Literal["rename", "add_folder", "rename_folder", "remove_folder"]
    path: str
    expected_revision: SafeInt
    name: str | None = None
    folder_id: str | None = None
    parent_id: str | None = None


class ProjectAccountsRequest(ContractModel):
    action: Literal["set_accounts"]
    path: str
    account_key: str
    account_keys: list[str]
    expected_revision: SafeInt


class SidebarReorderRequest(ContractModel):
    action: Literal["reorder"]
    request_id: str = Field(min_length=1, max_length=255)
    expected_revision: SafeInt = Field(ge=0)
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
    expected_revision: SafeInt | None = None
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
    expected_revision: SafeInt


class ProjectWorkerBaseRequest(ProjectWriteRequest):
    action: Literal["set_worker_base"]
    expected_revision: SafeInt


class ProjectWorkerEnvironmentRequest(ProjectWriteRequest):
    action: Literal["set_worker_environment"]
    environment: Literal["host", "linux"]
    expected_revision: SafeInt
