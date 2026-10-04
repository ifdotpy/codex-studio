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


class AnalyticsCaptureErrors(ContractModel):
    count: int
    last: JsonValue


class AnalyticsCoverage(ContractModel):
    trackingSince: float
    captureErrors: AnalyticsCaptureErrors
    # Historical provider error records retain their upstream extensible fields.
    historyErrors: list[JsonValue]
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


class AnalyticsResponse(ResponseModel):
    version: Literal[1] | None = None
    generatedAt: float | None = None
    filters: AnalyticsFilters | None = None
    coverage: AnalyticsCoverage | None = None
    summary: AnalyticsSummary | None = None
    # These are serialized provider/protocol records with evolving fields, kept
    # as JSON values so known numbers/strings/containers still validate strictly.
    # Agent records are deliberately extensible and carry provider metadata.
    agents: list[JsonValue] | None = None
    agentTotals: list[JsonValue] | None = None
    tools: list[AnalyticsTool] | None = None
    modelTotals: list[AnalyticsGroupTotal] | None = None
    accountTotals: list[AnalyticsGroupTotal] | None = None
    operations: AnalyticsOperationCounts | None = None
    notifications: list[AnalyticsNotification] | None = None
    history: list[JsonValue] | None = None
    rateLimits: list[JsonValue] | None = None
    turns: list[JsonValue] | None = None
    timeline: list[JsonValue] | None = None
    chartBuckets: list[JsonValue] | None = None
    timelineTotal: int | None = None
    provisionalUsage: list[JsonValue] | None = None
    calls: list[JsonValue] | None = None
    items: list[AnalyticsItemCount] | None = None
    itemRecords: list[JsonValue] | None = None
    itemRecordsTotal: int | None = None
    itemBreakdown: list[AnalyticsItemBreakdown] | None = None
    compactions: list[JsonValue] | None = None
    compactionSnapshots: list[JsonValue] | None = None
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
    responseRate: JsonValue | None = None
    tokens: JsonValue | None = None

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
