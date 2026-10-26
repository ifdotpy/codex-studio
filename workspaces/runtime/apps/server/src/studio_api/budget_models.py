"""Narrow persisted and observation contracts consumed by Rust team budgets."""
from typing import Annotated, Literal
from pydantic import ConfigDict, Field, StrictBool, StrictInt, StrictStr
from studio_api.models import ContractModel, JsonValue, MAX_SAFE_INTEGER

Tokens = Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
Timestamp = Annotated[float, Field(allow_inf_nan=False)]

class BudgetObject(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

class PersistedBudgetState(BudgetObject):
    cutoff: Timestamp
    floor: Tokens
    spent: Tokens
    before: Tokens
    after: Tokens
    historicalNotices: Tokens
    noticeSpent: Tokens
    counter: Tokens | None
    thread: StrictStr | None
    noticeAt: Timestamp
    created: Timestamp
    lastAmount: Tokens | None
    ambiguousNotices: list[StrictStr]
    faults: list[StrictStr]

class PersistedBudgetUsage(BudgetObject):
    id: StrictStr
    agent: StrictStr
    kind: Literal["response", "notice"]
    tokens: Tokens
    at: Timestamp

class BudgetAttempt(BudgetObject):
    id: StrictStr | None = None
    epoch: Tokens | None = None
    accountKey: StrictStr | None = None
    threadId: StrictStr | None = None
    submitted: StrictBool | None = None
    turnId: StrictStr | None = None
    observedTurnId: StrictStr | None = None
    notSubmittedReason: StrictStr | None = None
    events: list[StrictStr] | None = None
    action: StrictStr | StrictBool | None = None
    actionRequestId: StrictStr | None = None
    actionIdentity: dict[StrictStr, JsonValue] | None = None

class BudgetWait(BudgetObject):
    attemptId: StrictStr | None = None
    agentId: StrictStr | None = None
    accountKey: StrictStr | None = None
    epoch: Tokens | None = None
    threadId: StrictStr | None = None
    events: list[StrictStr] | None = None
    action: StrictStr | StrictBool | None = None
    actionRequestId: StrictStr | None = None
    actionIdentity: dict[StrictStr, JsonValue] | None = None
    error: StrictStr | None = None
    at: Timestamp | None = None
    admission: dict[StrictStr, JsonValue] | None = None

class PersistedBudgetAgent(BudgetObject):
    id: Annotated[StrictStr, Field(min_length=1)]
    rootId: StrictStr | None = None
    accountKey: StrictStr = "default"
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    lastCompletedTurn: StrictStr | None = None
    lastCompletedTurnStatus: StrictStr | None = None
    tokensUsed: Tokens | None = None
    tokenBudget: Tokens | None = None
    created: Timestamp = 0
    epoch: Tokens | None = None
    autoWake: StrictBool | None = None
    inFlight: StrictBool | None = None
    deletedAt: Timestamp | None = None
    startAttempt: BudgetAttempt | None = None
    budgetStartWait: BudgetWait | None = None
    budgetActionWait: BudgetWait | None = None
    contextRepair: dict[StrictStr, JsonValue] | None = None

class TokenAmount(BudgetObject):
    totalTokens: Tokens | None = None
    inputTokens: Tokens | None = None
    cachedInputTokens: Tokens | None = None
    cacheWriteInputTokens: Tokens | None = None
    outputTokens: Tokens | None = None

class TokenUsage(BudgetObject):
    total: TokenAmount | None = None
    last: TokenAmount | None = None

class BudgetCapture(BudgetObject):
    tokenUsage: TokenUsage | None = None
    requestUsage: TokenAmount | None = None
    responseId: StrictStr | None = None
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    usageSource: StrictStr | None = None
    rawTokenUsageRecord: dict[StrictStr, JsonValue] | None = None

class BudgetHistory(BudgetObject):
    status: StrictStr | None = None
    coverage: StrictStr | None = None
    identity: list[Tokens] | None = None
    offset: Tokens = 0
    context: dict[StrictStr, JsonValue] | None = None

BUDGET_SCHEMA_MODELS = {
    "persisted-budget-state.schema.json": PersistedBudgetState,
    "persisted-budget-usage.schema.json": PersistedBudgetUsage,
    "persisted-budget-agent.schema.json": PersistedBudgetAgent,
    "budget-capture.schema.json": BudgetCapture,
    "budget-history.schema.json": BudgetHistory,
}
