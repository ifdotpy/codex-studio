"""Typed contracts for analytics and local estimate endpoints."""

from __future__ import annotations

from typing import Literal

from pydantic import ConfigDict, Field

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel, SafeInt, SafeQueryInt, SignedSafeInt


class AnalyticsScope(ContractStrEnum):
    AGENT = "agent"
    TEAM = "team"
    ALL = "all"


class LegacyQueryModel(ContractModel):
    """Strictly reject extras; the shared API boundary checks names first."""

    model_config = ConfigDict(extra="forbid")


class AnalyticsQuery(LegacyQueryModel):
    agent: str | None = None
    # One enum declaration drives both request validation and OpenAPI.
    scope: AnalyticsScope = AnalyticsScope.AGENT
    from_time: float | None = Field(default=None, strict=False, ge=0)
    to_time: float | None = Field(default=None, strict=False, ge=0)
    limit: SafeQueryInt | None = None
    offset: SafeQueryInt | None = None
    export: Literal["1"] | None = None
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
            mode="json", exclude_unset=True, exclude_none=True
        )
        for key in ("from_time", "to_time", "limit", "offset"):
            if key in options:
                options[key] = str(options[key])
        if "from_time" in options:
            options["from"] = options.pop("from_time")
        if "to_time" in options:
            options["to"] = options.pop("to_time")
        return options


class AnalyticsFilters(ContractModel):
    agent: str | None
    scope: AnalyticsScope
    from_time: float | None = Field(alias="from")
    to_time: float | None = Field(alias="to")
    tool: str | None


class AnalyticsTokens(ContractModel):
    inputTokens: SafeInt | float | None = None
    outputTokens: SafeInt | float | None = None
    cachedInputTokens: SafeInt | float | None = None
    cacheWriteInputTokens: SafeInt | float | None = None
    reasoningOutputTokens: SafeInt | float | None = None
    totalTokens: SafeInt | float | None = None


class AnalyticsProviderTokens(AnalyticsTokens):
    """Known token fields plus versioned keys from provider usage payloads."""

    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue]


class AnalyticsStoredRecord(ContractModel):
    """Stored analytics rows pass through; retain unrecognized JSON fields."""

    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue]


class AnalyticsPendingUsage(AnalyticsStoredRecord):
    responseId: str | None
    turnId: str | None
    usage: AnalyticsProviderTokens


class AnalyticsUsageAssociation(AnalyticsStoredRecord):
    responseId: str | None
    turnId: str | None
    total: AnalyticsProviderTokens
    last: AnalyticsProviderTokens


class AnalyticsSummary(ContractModel):
    agents: SafeInt
    turns: SafeInt
    usageSamples: SafeInt
    provisionalUsageSamples: SafeInt
    exactResponseSamples: SafeInt
    legacyUsageSamples: SafeInt
    modelToolCalls: SafeInt
    protocolToolCalls: SafeInt
    observedToolRows: SafeInt
    toolCalls: SafeInt
    failedToolCalls: SafeInt
    modelFailedToolCalls: SafeInt
    protocolFailedToolCalls: SafeInt
    compactions: SafeInt
    inputBytes: SafeInt | None
    outputBytes: SafeInt | None
    modelInputBytes: SafeInt | None
    modelOutputBytes: SafeInt | None
    protocolInputBytes: SafeInt | None
    protocolOutputBytes: SafeInt | None
    durationMs: SafeInt | float | None
    modelDurationMs: SafeInt | float | None
    protocolDurationMs: SafeInt | float | None
    tokens: AnalyticsTokens
    tokenObservations: dict[str, SafeInt]
    cacheHitRate: SafeInt | float | None
    cacheHitRateSamples: SafeInt
    cacheHitRateTotalSamples: SafeInt
    peakContextTokens: SafeInt | float | None
    peakContextPercent: SafeInt | float | None
    baselineMissingSamples: SafeInt


class AnalyticsPagination(ContractModel):
    limit: SafeInt
    offset: SafeInt
    total: SafeInt
    hasMore: bool


class AnalyticsPage(ContractModel):
    rateLimits: AnalyticsPagination
    turns: AnalyticsPagination


class AnalyticsCaptureErrorRecord(ContractModel):
    at: SafeInt | float
    operation: str
    error: str
    code: str | None = None
    errorType: str | None = None


class AnalyticsCaptureErrors(ContractModel):
    count: SafeInt
    last: AnalyticsCaptureErrorRecord | None


class AnalyticsCoverage(ContractModel):
    trackingSince: float
    captureErrors: AnalyticsCaptureErrors
    historyErrors: list[AnalyticsHistoryRecord]
    provisionalUsageSamples: SafeInt
    tokenAttribution: Literal["provider_usage_only"]
    payloadMeasurement: Literal["observed_protocol_payload"]
    history: Literal["live_and_stored_history"]
    notes: list[str]


class AnalyticsDuration(ContractModel):
    count: SafeInt
    min: SafeInt | float | None
    max: SafeInt | float | None
    mean: SafeInt | float | None
    p50: SafeInt | float | None
    p95: SafeInt | float | None


class AnalyticsAgentRecord(AnalyticsStoredRecord):
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
    deletedAt: SafeInt | float | None = None


class AnalyticsAgentTotal(AnalyticsAgentRecord):
    tokens: AnalyticsTokens
    usageSamples: SafeInt
    toolCalls: SafeInt
    modelToolCalls: SafeInt
    protocolToolCalls: SafeInt
    failedToolCalls: SafeInt
    protocolFailedToolCalls: SafeInt
    compactions: SafeInt
    duration: AnalyticsDuration
    protocolDuration: AnalyticsDuration


class AnalyticsEventMetadata(AnalyticsStoredRecord):
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
    at: SafeInt | float
    recordedAt: SafeInt | float
    source: str
    timestampSource: str
    last: AnalyticsProviderTokens
    total: AnalyticsProviderTokens
    delta: AnalyticsProviderTokens
    cumulativeDelta: AnalyticsProviderTokens
    # `raw` and the raw token record preserve provider-defined wire metadata.
    raw: JsonValue | None = None
    counterDomain: Literal["response", "nativeNotice"]
    modelContextWindow: SafeInt | float | None
    reset: bool | None
    baselineMissing: bool
    fingerprint: str | None = None
    responseId: str | None = None
    inputTokensAreUncached: bool | None = None
    rawTokenUsageRecord: JsonValue | None = None
    turnUsage: AnalyticsProviderTokens | None = None
    requestUsage: AnalyticsProviderTokens | None = None
    # A native usage notice may enrich an existing response row with these
    # fields; older and response-only records legitimately omit them.
    noticeAt: SafeInt | float | None = None
    noticeLast: AnalyticsProviderTokens | None = None
    noticeTotal: AnalyticsProviderTokens | None = None
    usageSource: str | None = None


class AnalyticsTurnRecord(AnalyticsEventMetadata):
    id: str | None = None
    at: SafeInt | float | None = None
    startedAt: SafeInt | float | None = None
    finishedAt: SafeInt | float | None = None
    firstOutputAt: SafeInt | float | None = None
    durationMs: SafeInt | float | None = None
    firstOutputDelayMs: SafeInt | float | None = None
    status: str
    source: str | None = None
    error: JsonValue | None = None
    terminalSource: str | None = None
    nativeDurationMs: SafeInt | float | None = None
    nativeTimeToFirstTokenMs: SafeInt | float | None = None


class AnalyticsPayloadImage(ContractModel):
    bytes: SafeInt | None
    width: SafeInt | None
    height: SafeInt | None


class AnalyticsPayloadMeasure(ContractModel):
    bytes: SafeInt
    chars: SafeInt
    lines: SafeInt
    imageCount: SafeInt
    imageBytes: SafeInt | None
    images: list[AnalyticsPayloadImage]
    format: Literal["text", "json"]


class AnalyticsStreamMeasure(ContractModel):
    bytes: SafeInt
    chars: SafeInt
    lines: SafeInt
    deltas: SafeInt


class AnalyticsItemRecord(AnalyticsEventMetadata):
    rootId: str
    accountKey: str
    accountHistory: list[JsonValue]
    id: str
    itemId: str | None = None
    at: SafeInt | float
    recordedAt: SafeInt | float
    firstRecordedAt: SafeInt | float | None = None
    firstSourceAt: SafeInt | float | None = None
    startedAt: SafeInt | float | None = None
    finishedAt: SafeInt | float | None = None
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
    exitCode: SignedSafeInt | None = None
    processId: str | SafeInt | None = None
    compactionMetadata: JsonValue | None = None
    error: str | None = None
    durationMs: SafeInt | float | None = None
    durationSource: str | None = None
    coverage: str | None = None
    payloadTruncated: bool | None = None
    stream: AnalyticsStreamMeasure | None = None


class AnalyticsRateLimitRecord(ContractModel):
    accountKey: str
    at: SafeInt | float
    # Raw provider allowance schemas vary by account/provider version.
    data: JsonValue


class AnalyticsHistoryContext(AnalyticsStoredRecord):
    threadId: str | None = None
    window: SafeInt | float | None = None
    turnId: str | None = None
    model: str | None = None
    effort: str | None = None
    allowedSourceThreadIds: list[str] | None = None
    pendingUsage: AnalyticsPendingUsage | None = None
    requestUsageAvailable: bool | None = None
    noticeAssociation: AnalyticsUsageAssociation | None = None


class AnalyticsFilesystemRemap(ContractModel):
    previous: list[SafeInt]
    current: list[SafeInt]
    offset: SafeInt
    at: SafeInt | float
    proof: Literal["samePathInodeHeaderAnchor"]


class AnalyticsHistoryRecord(AnalyticsStoredRecord):
    id: str | None = None
    agent: str | None = None
    accountKey: str | None = None
    threadId: str | None = None
    deletedAt: SafeInt | float | None = None
    offset: SafeInt | None = None
    importedRecords: SafeInt | None = None
    malformedLines: SafeInt | None = None
    status: str | None = None
    error: str | None = None
    updated: SafeInt | float | None = None
    path: str | None = None
    fileBytes: SafeInt | None = None
    identity: list[SafeInt] | None = None
    filesystemIdentity: list[SafeInt] | None = None
    filesystemRemap: AnalyticsFilesystemRemap | None = None
    filesystemRemapCount: SafeInt | None = None
    anchor: str | None = None
    validated: bool | None = None
    context: AnalyticsHistoryContext | None = None
    coverage: str | None = None
    wrongThreadRecord: SafeInt | None = None
    consecutiveFailures: SafeInt | None = None
    errorPersisted: bool | None = None
    errorPersistenceError: str | None = None


class AnalyticsTool(ContractModel):
    name: str
    type: str
    payloadBoundary: str
    calls: SafeInt
    failed: SafeInt
    inputBytes: SafeInt | None
    outputBytes: SafeInt | None
    modelInputBytes: SafeInt | None
    modelOutputBytes: SafeInt | None
    durationMs: SafeInt | float | None
    imageCount: SafeInt | None
    inputMeasurements: SafeInt
    outputMeasurements: SafeInt
    duration: AnalyticsDuration


class AnalyticsItemCount(ContractModel):
    type: str
    count: SafeInt
    bytes: SafeInt | None
    chars: SafeInt | None


class AnalyticsItemBreakdown(ContractModel):
    type: str
    payloadBoundary: str | None
    category: str | None
    role: str | None
    count: SafeInt
    inputBytes: SafeInt | None
    outputBytes: SafeInt | None
    inputMeasurements: SafeInt
    outputMeasurements: SafeInt


class AnalyticsGroupTotal(ContractModel):
    model: str | None = None
    accountKey: str | None = None
    samples: SafeInt
    tokens: AnalyticsTokens


class AnalyticsNotification(AnalyticsStoredRecord):
    id: str
    agent: str
    root: str | None = None
    method: str
    hour: SafeInt | float
    count: SafeInt
    bytes: SafeInt


class AnalyticsMonitor(ContractModel):
    id: str | None
    agent: str | None
    status: str | None
    created: SafeInt | float | None
    finished: SafeInt | float | None
    bytes: SafeInt | None
    exitCode: SignedSafeInt | None
    error: str | None
    timeout_ms: SafeInt | float | None


class AnalyticsOperationCounts(ContractModel):
    monitors: list[AnalyticsMonitor]
    eventCounts: dict[str, SafeInt]
    approvalCounts: dict[str, SafeInt]


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
    timelineTotal: SafeInt | None = None
    provisionalUsage: list[AnalyticsUsageRecord] | None = None
    calls: list[AnalyticsItemRecord] | None = None
    items: list[AnalyticsItemCount] | None = None
    itemRecords: list[AnalyticsItemRecord] | None = None
    itemRecordsTotal: SafeInt | None = None
    itemBreakdown: list[AnalyticsItemBreakdown] | None = None
    compactions: list[AnalyticsItemRecord] | None = None
    compactionSnapshots: list[AnalyticsItemRecord] | None = None
    pagination: AnalyticsPagination | None = None
    detailPagination: AnalyticsPage | None = None
    at: SafeInt | float | None = None
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
    turnDurationMs: SafeInt | float | None = None
    # MessageInfo returns the observed per-response rate as a scalar number.
    responseRate: SafeInt | float | None = None
    tokens: AnalyticsProviderTokens | None = None

class AccountCostData(ContractModel):
    source: str | None = None
    scope: str | None = None
    kind: str | None = None
    currency: str | None = None
    currencyCode: str | None = None
    billedUSD: SafeInt | float | None = None
    todayUSD: SafeInt | float | None = None
    last30DaysUSD: SafeInt | float | None = None
    todayTokens: SafeInt | float | None = None
    last30DaysTokens: SafeInt | float | None = None
    sourceUpdatedAt: str | float | None = None
    sourceDay: str | None = None
    # Coverage keys vary by provider scanner version; values remain JSON-only.
    coverage: JsonValue | None = None
    unknownModels: list[str] | None = None
    historyDays: SafeInt | None = None
    accountId: str | None = None
    note: str | None = None
    modelBreakdown: dict[str, SafeInt | float] | None = None


class AccountCostResponse(ResponseModel):
    at: SafeInt | float | None
    checkedAt: float | None = None
    sourceFingerprint: str | None = Field(default=None, exclude=True)
    error: str | None
    data: AccountCostData | None
    refreshing: bool
    stale: bool
    accountKey: str


class CostBreakdown(ContractModel):
    providers: dict[str, SafeInt | float]
    models: dict[str, SafeInt | float]


class SessionCostResponse(ResponseModel):
    rootId: str
    generation: SafeInt | None = None
    totalUSD: SafeInt | float | None
    pricedSamples: SafeInt
    breakdown: CostBreakdown
    unknownModels: list[str]
    estimated: bool
    pricingState: Literal["loading", "ready"]
    cacheAgeSeconds: SafeInt | float
    refreshing: bool
    claudeHistoryIncomplete: bool | None = None
    method: str | None = None


class AccountCostQuery(LegacyQueryModel):
    account_key: str = "default"


class SessionCostQuery(LegacyQueryModel):
    agent: str | None = None
