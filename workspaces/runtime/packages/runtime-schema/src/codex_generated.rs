// Generated from Codex CLI 0.164.0 selected JSON Schema; do not edit.
// Source: provider-contract-sources.ts; SHA-256: 08fb3c6d9b4f22d2ac9e58b1bd2b6e0803d08dca361d7a504e04dbebfaaf8b53

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(rename = "TurnStatus")]
pub enum CodexTurnStatus {
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "inProgress")]
    InProgress,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "type")]
#[schemars(rename = "ThreadStatus")]
pub enum CodexThreadStatus {
    #[serde(rename = "notLoaded")]
    NotLoaded,
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "systemError")]
    SystemError,
    #[serde(rename = "active")]
    Active {
        #[serde(rename = "activeFlags")]
        active_flags: Vec<CodexThreadActiveFlag>,
    },
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(rename = "ThreadActiveFlag")]
pub enum CodexThreadActiveFlag {
    #[serde(rename = "waitingOnApproval")]
    WaitingOnApproval,
    #[serde(rename = "waitingOnUserInput")]
    WaitingOnUserInput,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(rename = "TurnItemsView")]
pub enum CodexTurnItemsView {
    #[serde(rename = "notLoaded")]
    NotLoaded,
    #[serde(rename = "summary")]
    Summary,
    #[serde(rename = "full")]
    Full,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "type")]
#[schemars(rename = "ThreadActivity")]
pub enum CodexThreadActivity {
    #[serde(rename = "notLoaded")]
    NotLoaded,
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "systemError")]
    SystemError,
    #[serde(rename = "active")]
    Active,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(rename = "ThreadTokenUsage")]
pub struct CodexThreadTokenUsage {
    #[serde(rename = "last")]
    pub last: CodexTokenUsageBreakdown,
    #[serde(rename = "modelContextWindow")]
    pub model_context_window: Option<i64>,
    #[serde(rename = "total")]
    pub total: CodexTokenUsageBreakdown,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(rename = "TokenUsageBreakdown")]
pub struct CodexTokenUsageBreakdown {
    #[serde(default)]
    #[serde(rename = "cacheWriteInputTokens")]
    pub cache_write_input_tokens: i64,
    #[serde(rename = "cachedInputTokens")]
    pub cached_input_tokens: i64,
    #[serde(rename = "inputTokens")]
    pub input_tokens: i64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: i64,
    #[serde(rename = "reasoningOutputTokens")]
    pub reasoning_output_tokens: i64,
    #[serde(rename = "totalTokens")]
    pub total_tokens: i64,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[schemars(rename = "ThreadTokenUsageUpdatedNotification")]
pub struct CodexThreadTokenUsageUpdatedNotification {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "tokenUsage")]
    pub token_usage: CodexThreadTokenUsage,
    #[serde(rename = "turnId")]
    pub turn_id: String,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum KnownCodexErrorKind {
    #[serde(rename = "contextWindowExceeded")]
    ContextWindowExceeded,
    #[serde(rename = "sessionBudgetExceeded")]
    SessionBudgetExceeded,
    #[serde(rename = "usageLimitExceeded")]
    UsageLimitExceeded,
    #[serde(rename = "rateLimitExceeded")]
    RateLimitExceeded,
    #[serde(rename = "flexUnavailable")]
    FlexUnavailable,
    #[serde(rename = "serverOverloaded")]
    ServerOverloaded,
    #[serde(rename = "cyberPolicy")]
    CyberPolicy,
    #[serde(rename = "misalignmentPolicyViolation")]
    MisalignmentPolicyViolation,
    #[serde(rename = "tooManyDenials")]
    TooManyDenials,
    #[serde(rename = "internalServerError")]
    InternalServerError,
    #[serde(rename = "unauthorized")]
    Unauthorized,
    #[serde(rename = "badRequest")]
    BadRequest,
    #[serde(rename = "threadRollbackFailed")]
    ThreadRollbackFailed,
    #[serde(rename = "sandboxError")]
    SandboxError,
    #[serde(rename = "other")]
    Other,
    #[serde(rename = "httpConnectionFailed")]
    HttpConnectionFailed,
    #[serde(rename = "responseStreamConnectionFailed")]
    ResponseStreamConnectionFailed,
    #[serde(rename = "responseStreamDisconnected")]
    ResponseStreamDisconnected,
    #[serde(rename = "responseTooManyFailedAttempts")]
    ResponseTooManyFailedAttempts,
    #[serde(rename = "activeTurnNotSteerable")]
    ActiveTurnNotSteerable,
}
