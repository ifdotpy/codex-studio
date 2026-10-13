"""Narrow, open storage contracts for analytics capture and rollout checkpoints."""
from typing import Annotated

from pydantic import BaseModel, ConfigDict, Field, StrictBool, StrictInt, StrictStr

Nonnegative = Annotated[float, Field(ge=0)]
Counter = Annotated[StrictInt, Field(ge=0, le=9007199254740991)]


class AnalyticsObject(BaseModel):
    model_config = ConfigDict(extra="allow", strict=True)


class AnalyticsAgent(AnalyticsObject):
    id: StrictStr
    epoch: Counter | None = None
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    accountKey: StrictStr | None = None
    rootId: StrictStr | None = None
    model: StrictStr | None = None
    effort: StrictStr | None = None
    accountHistory: list[object] | None = None


class AnalyticsTurn(AnalyticsObject):
    id: StrictStr
    agentId: StrictStr
    at: Nonnegative
    startedAt: Nonnegative | None
    finishedAt: Nonnegative | None
    firstOutputAt: Nonnegative | None
    status: StrictStr
    terminalSource: StrictStr | None = None


class AnalyticsSize(AnalyticsObject):
    bytes: Counter
    chars: Counter
    lines: Counter
    deltas: Counter | None = None


class AnalyticsItem(AnalyticsObject):
    id: StrictStr
    agentId: StrictStr
    at: Nonnegative
    startedAt: Nonnegative | None = None
    finishedAt: Nonnegative | None = None
    type: StrictStr
    name: StrictStr
    isTool: StrictBool
    stream: AnalyticsSize | None = None
    input: AnalyticsSize | None = None
    output: AnalyticsSize | None = None
    durationSource: StrictStr | None = None
    source: StrictStr | None = None


class AnalyticsTokens(AnalyticsObject):
    inputTokens: Nonnegative | None = None
    cachedInputTokens: Nonnegative | None = None
    cacheWriteInputTokens: Nonnegative | None = None
    outputTokens: Nonnegative | None = None
    reasoningOutputTokens: Nonnegative | None = None
    totalTokens: Nonnegative | None = None


class AnalyticsTokenEnvelope(AnalyticsObject):
    total: AnalyticsTokens | None = None
    last: AnalyticsTokens | None = None
    modelContextWindow: Nonnegative | None = None


class AnalyticsUsage(AnalyticsObject):
    id: StrictStr
    agentId: StrictStr
    at: Nonnegative
    total: AnalyticsTokens
    last: AnalyticsTokens
    responseId: StrictStr | None = None
    counterDomain: StrictStr | None = None
    raw: AnalyticsTokenEnvelope | None = None
    modelContextWindow: Nonnegative | None = None


class AnalyticsContext(AnalyticsObject):
    threadId: StrictStr
    turnId: StrictStr | None = None
    model: StrictStr | None = None
    effort: StrictStr | None = None
    window: Nonnegative | None = None
    allowedSourceThreadIds: list[StrictStr] = Field(default_factory=list, max_length=1024)
    pendingUsage: dict[str, object] | None = None
    noticeAssociation: dict[str, object] | None = None


class AnalyticsHistory(AnalyticsObject):
    id: StrictStr
    agent: StrictStr
    accountKey: StrictStr
    threadId: StrictStr
    offset: Counter
    importedRecords: Counter
    malformedLines: Counter
    context: AnalyticsContext
    identity: tuple[Counter, Counter] | None = None
    filesystemIdentity: tuple[Counter, Counter] | None = None
    anchor: Annotated[StrictStr, Field(pattern="^[0-9a-f]{64}$")] | None = None


class AnalyticsNativeItem(AnalyticsObject):
    id: StrictStr | None = None
    type: StrictStr | None = None
    name: StrictStr | None = None
    tool: StrictStr | None = None
    durationMs: Nonnegative | None = None


class AnalyticsNotice(AnalyticsObject):
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    itemId: StrictStr | None = None
    responseId: StrictStr | None = None
    item: AnalyticsNativeItem | None = None
    turn: dict[str, object] | None = None
    tokenUsage: AnalyticsTokenEnvelope | None = None
    requestUsage: AnalyticsTokens | None = None
    rawTokenUsageRecord: dict[str, object] | None = None
    startedAt: Nonnegative | None = None
    completedAt: Nonnegative | None = None


ANALYTICS_SCHEMA_MODELS = {
    "analytics-agent.schema.json": AnalyticsAgent,
    "analytics-context.schema.json": AnalyticsContext,
    "analytics-history.schema.json": AnalyticsHistory,
    "analytics-item.schema.json": AnalyticsItem,
    "analytics-notice.schema.json": AnalyticsNotice,
    "analytics-turn.schema.json": AnalyticsTurn,
    "analytics-usage.schema.json": AnalyticsUsage,
}
