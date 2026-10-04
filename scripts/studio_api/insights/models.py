"""Typed contracts for analytics and local estimate endpoints."""

from __future__ import annotations

from typing import Literal

from pydantic import ConfigDict, Field

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel


class AnalyticsScope(ContractStrEnum):
    AGENT = "agent"
    TEAM = "team"
    ALL = "all"


class DiskMeasure(ContractStrEnum):
    PRIVATE_ON_APFS = "private on APFS"
    ALLOCATED_BLOCKS = "allocated blocks"
    MIXED_MEASURES = "mixed measures"
    UNMEASURED = "unmeasured"


class DiskWorkerState(ContractStrEnum):
    READY = "ready"
    MISSING = "missing"
    UNAVAILABLE = "unavailable"
    UNMEASURED = "unmeasured"


class LegacyQueryModel(ContractModel):
    """Strict declared query fields while retaining the old ignored-extra behavior."""

    model_config = ConfigDict(extra="ignore")


class AnalyticsQuery(LegacyQueryModel):
    agent: str | None = None
    # The handler converts this schema-enumerated string to AnalyticsScope
    # itself so invalid values retain the legacy "Unknown analytics scope" 400.
    scope: str = Field(
        default=AnalyticsScope.AGENT.value,
        json_schema_extra={"enum": [scope.value for scope in AnalyticsScope]},
    )
    from_time: str | None = Field(default=None, alias="from")
    to_time: str | None = Field(default=None, alias="to")
    limit: str | None = None
    offset: str | None = None
    export: str = "0"
    tool: str | None = None
    timing: str | None = None
    view: str | None = None
    detail: str | None = None
    item: str | None = None
    turn: str | None = None
    thread: str | None = None
    turns: str | None = None

    def service_options(self) -> dict[str, str]:
        """Return only supplied legacy keys, preserving string query values."""
        options = self.model_dump(
            mode="json", by_alias=True, exclude_unset=True, exclude_none=True
        )
        if "scope" in options:
            try:
                options["scope"] = AnalyticsScope(options["scope"]).value
            except ValueError:
                raise ValueError("Unknown analytics scope") from None
        return options


class AnalyticsFilters(ContractModel):
    agent: str | None
    scope: AnalyticsScope
    from_time: float | None = Field(alias="from")
    to_time: float | None = Field(alias="to")
    tool: str | None


class AnalyticsTokens(ContractModel):
    inputTokens: int | float | None = None
    outputTokens: int | float | None = None
    cachedInputTokens: int | float | None = None
    cacheWriteInputTokens: int | float | None = None
    reasoningOutputTokens: int | float | None = None
    totalTokens: int | float | None = None


class AnalyticsProviderTokens(AnalyticsTokens):
    """Known token fields plus versioned keys from provider usage payloads."""

    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue]


class AnalyticsPendingUsage(ContractModel):
    responseId: str | None
    turnId: str | None
    usage: AnalyticsProviderTokens


class AnalyticsUsageAssociation(ContractModel):
    responseId: str | None
    turnId: str | None
    total: AnalyticsProviderTokens
    last: AnalyticsProviderTokens


class AnalyticsSummary(ContractModel):
    agents: int
    turns: int
    usageSamples: int
    provisionalUsageSamples: int
    exactResponseSamples: int
    legacyUsageSamples: int
    modelToolCalls: int
    protocolToolCalls: int
    observedToolRows: int
    toolCalls: int
    failedToolCalls: int
    modelFailedToolCalls: int
    protocolFailedToolCalls: int
    compactions: int
    inputBytes: int | None
    outputBytes: int | None
    modelInputBytes: int | None
    modelOutputBytes: int | None
    protocolInputBytes: int | None
    protocolOutputBytes: int | None
    durationMs: int | float | None
    modelDurationMs: int | float | None
    protocolDurationMs: int | float | None
    tokens: AnalyticsTokens
    tokenObservations: dict[str, int]
    cacheHitRate: int | float | None
    cacheHitRateSamples: int
    cacheHitRateTotalSamples: int
    peakContextTokens: int | float | None
    peakContextPercent: int | float | None
    baselineMissingSamples: int


class AnalyticsPagination(ContractModel):
    limit: int
    offset: int
    total: int
    hasMore: bool


class AnalyticsPage(ContractModel):
    rateLimits: AnalyticsPagination
    turns: AnalyticsPagination


class AnalyticsCaptureErrorRecord(ContractModel):
    at: int | float
    operation: str
    error: str
    code: str | None = None
    errorType: str | None = None


class AnalyticsCaptureErrors(ContractModel):
    count: int
    last: AnalyticsCaptureErrorRecord | None


class AnalyticsCoverage(ContractModel):
    trackingSince: float
    captureErrors: AnalyticsCaptureErrors
    historyErrors: list[AnalyticsHistoryRecord]
    provisionalUsageSamples: int
    tokenAttribution: Literal["provider_usage_only"]
    payloadMeasurement: Literal["observed_protocol_payload"]
    history: Literal["live_and_stored_history"]
    notes: list[str]


class AnalyticsDuration(ContractModel):
    count: int
    min: int | float | None
    max: int | float | None
    mean: int | float | None
    p50: int | float | None
    p95: int | float | None


class AnalyticsAgentRecord(ContractModel):
    id: str
    name: str | None = None
    rootId: str | None = None
    parentId: str | None = None
    accountKey: str | None = None
    # Account history is provider/profile metadata with versioned fields.
    accountHistory: list[JsonValue] | None = None
    provider: str | None = None
    threadId: str | None = None
    model: str | None = None
    effort: str | None = None
    fastMode: bool | None = None
    daybreakEnabled: bool | None = None
    cyberAccessProgram: str | None = None
    cwd: str | None = None
    deletedAt: int | float | None = None


class AnalyticsAgentTotal(AnalyticsAgentRecord):
    tokens: AnalyticsTokens
    usageSamples: int
    toolCalls: int
    modelToolCalls: int
    protocolToolCalls: int
    failedToolCalls: int
    protocolFailedToolCalls: int
    compactions: int
    duration: AnalyticsDuration
    protocolDuration: AnalyticsDuration


class AnalyticsEventMetadata(ContractModel):
    agentId: str
    agentName: str | None = None
    rootId: str | None = None
    accountKey: str | None = None
    # Historical profile metadata has provider-specific additions.
    accountHistory: list[JsonValue] | None = None
    provider: str | None = None
    threadId: str | None = None
    turnId: str | None = None
    model: str | None = None
    effort: str | None = None
    fastMode: bool | None = None
    daybreakEnabled: bool | None = None
    cyberAccessProgram: str | None = None


class AnalyticsUsageRecord(AnalyticsEventMetadata):
    rootId: str
    accountKey: str
    accountHistory: list[JsonValue]
    id: str
    at: int | float
    recordedAt: int | float
    source: str
    timestampSource: str
    last: AnalyticsProviderTokens
    total: AnalyticsProviderTokens
    delta: AnalyticsProviderTokens
    cumulativeDelta: AnalyticsProviderTokens
    # `raw` and the raw token record preserve provider-defined wire metadata.
    raw: JsonValue | None = None
    counterDomain: Literal["response", "nativeNotice"]
    modelContextWindow: int | float | None
    reset: bool | None
    baselineMissing: bool
    fingerprint: str | None = None
    responseId: str | None = None
    inputTokensAreUncached: bool | None = None
    rawTokenUsageRecord: JsonValue | None = None
    turnUsage: AnalyticsProviderTokens | None = None
    requestUsage: AnalyticsProviderTokens | None = None
    usageSource: str | None = None


class AnalyticsTurnRecord(AnalyticsEventMetadata):
    id: str | None = None
    at: int | float | None = None
    startedAt: int | float | None = None
    finishedAt: int | float | None = None
    firstOutputAt: int | float | None = None
    durationMs: int | float | None = None
    firstOutputDelayMs: int | float | None = None
    status: str
    source: str | None = None
    error: JsonValue | None = None
    terminalSource: str | None = None
    nativeDurationMs: int | float | None = None
    nativeTimeToFirstTokenMs: int | float | None = None


class AnalyticsPayloadImage(ContractModel):
    bytes: int | None
    width: int | None
    height: int | None


class AnalyticsPayloadMeasure(ContractModel):
    bytes: int
    chars: int
    lines: int
    imageCount: int
    imageBytes: int | None
    images: list[AnalyticsPayloadImage]
    format: Literal["text", "json"]


class AnalyticsStreamMeasure(ContractModel):
    bytes: int
    chars: int
    lines: int
    deltas: int


class AnalyticsItemRecord(AnalyticsEventMetadata):
    rootId: str
    accountKey: str
    accountHistory: list[JsonValue]
    id: str
    itemId: str | None = None
    at: int | float
    recordedAt: int | float
    firstRecordedAt: int | float | None = None
    firstSourceAt: int | float | None = None
    startedAt: int | float | None = None
    finishedAt: int | float | None = None
    source: str
    timestampSource: str
    type: str
    name: str
    isTool: bool
    status: str
    input: AnalyticsPayloadMeasure | None = None
    output: AnalyticsPayloadMeasure | None = None
    modelInput: AnalyticsPayloadMeasure | None = None
    modelOutput: AnalyticsPayloadMeasure | None = None
    payloadBoundary: Literal["model", "protocol"]
    category: str | None = None
    role: str | None = None
    callId: str | None = None
    snapshotItemId: str | None = None
    namespace: str | None = None
    command: str | None = None
    cwd: str | None = None
    server: str | None = None
    exitCode: int | None = None
    processId: str | int | None = None
    compactionMetadata: JsonValue | None = None
    error: str | None = None
    durationMs: int | float | None = None
    durationSource: str | None = None
    coverage: str | None = None
    payloadTruncated: bool | None = None
    stream: AnalyticsStreamMeasure | None = None


class AnalyticsRateLimitRecord(ContractModel):
    accountKey: str
    at: int | float
    # Raw provider allowance schemas vary by account/provider version.
    data: JsonValue


class AnalyticsHistoryContext(ContractModel):
    threadId: str | None = None
    window: int | float | None = None
    turnId: str | None = None
    model: str | None = None
    effort: str | None = None
    allowedSourceThreadIds: list[str] | None = None
    pendingUsage: AnalyticsPendingUsage | None = None
    requestUsageAvailable: bool | None = None
    noticeAssociation: AnalyticsUsageAssociation | None = None


class AnalyticsFilesystemRemap(ContractModel):
    previous: list[int]
    current: list[int]
    offset: int
    at: int | float
    proof: Literal["samePathInodeHeaderAnchor"]


class AnalyticsHistoryRecord(ContractModel):
    id: str | None = None
    agent: str | None = None
    accountKey: str | None = None
    threadId: str | None = None
    deletedAt: int | float | None = None
    offset: int | None = None
    importedRecords: int | None = None
    malformedLines: int | None = None
    status: str | None = None
    error: str | None = None
    updated: int | float | None = None
    path: str | None = None
    fileBytes: int | None = None
    identity: list[int] | None = None
    filesystemIdentity: list[int] | None = None
    filesystemRemap: AnalyticsFilesystemRemap | None = None
    filesystemRemapCount: int | None = None
    anchor: str | None = None
    validated: bool | None = None
    context: AnalyticsHistoryContext | None = None
    coverage: str | None = None
    wrongThreadRecord: int | None = None
    consecutiveFailures: int | None = None
    errorPersisted: bool | None = None
    errorPersistenceError: str | None = None


class AnalyticsTool(ContractModel):
    name: str
    type: str
    payloadBoundary: str
    calls: int
    failed: int
    inputBytes: int | None
    outputBytes: int | None
    modelInputBytes: int | None
    modelOutputBytes: int | None
    durationMs: int | float | None
    imageCount: int | None
    inputMeasurements: int
    outputMeasurements: int
    duration: AnalyticsDuration


class AnalyticsItemCount(ContractModel):
    type: str
    count: int
    bytes: int | None
    chars: int | None


class AnalyticsItemBreakdown(ContractModel):
    type: str
    payloadBoundary: str | None
    category: str | None
    role: str | None
    count: int
    inputBytes: int | None
    outputBytes: int | None
    inputMeasurements: int
    outputMeasurements: int


class AnalyticsGroupTotal(ContractModel):
    model: str | None = None
    accountKey: str | None = None
    samples: int
    tokens: AnalyticsTokens


class AnalyticsNotification(ContractModel):
    id: str
    agent: str
    root: str | None = None
    method: str
    hour: int | float
    count: int
    bytes: int


class AnalyticsMonitor(ContractModel):
    id: str | None
    agent: str | None
    status: str | None
    created: int | float | None
    finished: int | float | None
    bytes: int | None
    exitCode: int | None
    error: str | None
    timeout_ms: int | float | None


class AnalyticsOperationCounts(ContractModel):
    monitors: list[AnalyticsMonitor]
    eventCounts: dict[str, int]
    approvalCounts: dict[str, int]


class AnalyticsResponseRate(ContractModel):
    rate: int | float
    outputTokens: int | float
    durationSeconds: int | float


class AnalyticsResponse(ResponseModel):
    version: Literal[1] | None = None
    generatedAt: float | None = None
    filters: AnalyticsFilters | None = None
    coverage: AnalyticsCoverage | None = None
    summary: AnalyticsSummary | None = None
    agents: list[AnalyticsAgentRecord] | None = None
    agentTotals: list[AnalyticsAgentTotal] | None = None
    tools: list[AnalyticsTool] | None = None
    modelTotals: list[AnalyticsGroupTotal] | None = None
    accountTotals: list[AnalyticsGroupTotal] | None = None
    operations: AnalyticsOperationCounts | None = None
    notifications: list[AnalyticsNotification] | None = None
    history: list[AnalyticsHistoryRecord] | None = None
    rateLimits: list[AnalyticsRateLimitRecord] | None = None
    turns: list[AnalyticsTurnRecord] | None = None
    timeline: list[AnalyticsUsageRecord] | None = None
    chartBuckets: list[AnalyticsUsageRecord] | None = None
    timelineTotal: int | None = None
    provisionalUsage: list[AnalyticsUsageRecord] | None = None
    calls: list[AnalyticsItemRecord] | None = None
    items: list[AnalyticsItemCount] | None = None
    itemRecords: list[AnalyticsItemRecord] | None = None
    itemRecordsTotal: int | None = None
    itemBreakdown: list[AnalyticsItemBreakdown] | None = None
    compactions: list[AnalyticsItemRecord] | None = None
    compactionSnapshots: list[AnalyticsItemRecord] | None = None
    pagination: AnalyticsPagination | None = None
    detailPagination: AnalyticsPage | None = None
    at: int | float | None = None
    id: str | None = None
    runId: str | None = None
    attemptId: str | None = None
    agentId: str | None = None
    threadId: str | None = None
    model: str | None = None
    turnId: str | None = None
    effort: str | None = None
    accountKey: str | None = None
    accountLabel: str | None = None
    provider: str | None = None
    turnDurationMs: int | float | None = None
    responseRate: AnalyticsResponseRate | None = None
    tokens: AnalyticsProviderTokens | None = None

class AccountCostData(ContractModel):
    source: str | None = None
    scope: str | None = None
    kind: str | None = None
    currency: str | None = None
    currencyCode: str | None = None
    billedUSD: int | float | None = None
    todayUSD: int | float | None = None
    last30DaysUSD: int | float | None = None
    todayTokens: int | float | None = None
    last30DaysTokens: int | float | None = None
    sourceUpdatedAt: str | float | None = None
    sourceDay: str | None = None
    # Coverage keys vary by provider scanner version; values remain JSON-only.
    coverage: JsonValue | None = None
    unknownModels: list[str] | None = None
    historyDays: int | None = None
    accountId: str | None = None
    note: str | None = None
    modelBreakdown: dict[str, int | float] | None = None


class AccountCostResponse(ResponseModel):
    at: int | float | None
    checkedAt: float | None = None
    error: str | None
    data: AccountCostData | None
    refreshing: bool
    stale: bool
    accountKey: str


class CostBreakdown(ContractModel):
    providers: dict[str, int | float]
    models: dict[str, int | float]


class SessionCostResponse(ResponseModel):
    rootId: str
    generation: int | None = None
    totalUSD: int | float | None
    pricedSamples: int
    breakdown: CostBreakdown
    unknownModels: list[str]
    estimated: bool
    pricingState: Literal["loading", "ready"]
    cacheAgeSeconds: int | float
    refreshing: bool
    claudeHistoryIncomplete: bool | None = None
    method: str | None = None


class WorktreeDiskWorker(ContractModel):
    state: DiskWorkerState
    bytes: int | None = None
    scannedAt: int | float | None = None
    measure: DiskMeasure | None = None
    error: str | None = None


class WorktreeDiskResponse(ResponseModel):
    workers: dict[str, WorktreeDiskWorker]
    totalBytes: int
    limitBytes: int
    warning: bool
    scanning: bool
    error: str | None
    measure: DiskMeasure


class WorktreeDiskQuery(LegacyQueryModel):
    workers: str = ""


class AccountCostQuery(LegacyQueryModel):
    account_key: str = "default"


class SessionCostQuery(LegacyQueryModel):
    agent: str | None = None
