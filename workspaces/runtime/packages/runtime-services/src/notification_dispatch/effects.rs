//! Injection seam for effects owned by other notification subsystems.
use super::{DispatchOutcome, NotificationDispatchError, ProviderDeliveryIdentity};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use studio_runtime_store::Transaction;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NotificationDependency {
    #[serde(rename = "budget/capture")]
    BudgetCapture,
    #[serde(rename = "agent-move/cache-capture")]
    MoveCache,
    #[serde(rename = "accounts/login-completed")]
    AccountLogin,
    #[serde(rename = "accounts/refresh")]
    AccountRefresh,
    #[serde(rename = "accounts/rate-limits")]
    RateLimits,
    #[serde(rename = "analytics/token-rate")]
    TokenRate,
    #[serde(rename = "voice/realtime")]
    Realtime,
    #[serde(rename = "monitors/output")]
    MonitorOutput,
    #[serde(rename = "turn-recovery/thread-unloaded")]
    ThreadUnloaded,
    #[serde(rename = "native-notices/account")]
    AccountNotice,
    #[serde(rename = "native-limits/refresh")]
    RefreshLimits,
    #[serde(rename = "scheduler/turn-lifecycle")]
    TurnLifecycle,
    #[serde(rename = "scheduler/user-message-receipt")]
    UserMessageReceipt,
}
impl NotificationDependency {
    pub fn name(self) -> &'static str {
        match self {
            Self::BudgetCapture => "budget/capture",
            Self::MoveCache => "agent-move/cache-capture",
            Self::AccountLogin => "accounts/login-completed",
            Self::AccountRefresh => "accounts/refresh",
            Self::RateLimits => "accounts/rate-limits",
            Self::TokenRate => "analytics/token-rate",
            Self::Realtime => "voice/realtime",
            Self::MonitorOutput => "monitors/output",
            Self::ThreadUnloaded => "turn-recovery/thread-unloaded",
            Self::AccountNotice => "native-notices/account",
            Self::RefreshLimits => "native-limits/refresh",
            Self::TurnLifecycle => "scheduler/turn-lifecycle",
            Self::UserMessageReceipt => "scheduler/user-message-receipt",
        }
    }
}

/// Immutable observation handed to analytics/token-rate owners after commit.
/// `delivery` is the idempotency key; consumers must retain its account and generation.
/// A caller requiring crash-durable telemetry stages these serializable values in
/// its existing effect journal before committing, and drains it after commit.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NotificationObservation {
    pub agent: Option<Value>,
    pub method: String,
    pub params: Value,
    pub delivery: ProviderDeliveryIdentity,
    pub observed_at: f64,
    pub provider_at: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum NotificationPostCommit {
    Analytics {
        observation: NotificationObservation,
        budget_capture_value: Option<u64>,
    },
    TokenRateStream(NotificationObservation),
    TokenRateAssociation(NotificationObservation),
}

/// Transactional effects only: no provider call, blocking IO, or await here.
/// The worker supplies budget_service::capture(..., CaptureSource::Live) and
/// agent_move::capture_cache; both must commit with the notification receipt.
pub trait NotificationEffects {
    /// Pure staging only. A receipt replay never calls this method.
    fn post_commit(
        &self,
        _before: Option<&Value>,
        _after: Option<&Value>,
        _frame: &Value,
        _identity: &ProviderDeliveryIdentity,
        _at: f64,
        _outcome: &DispatchOutcome,
    ) -> Vec<NotificationPostCommit> {
        Vec::new()
    }

    fn account_notice(
        &self,
        _tx: &Transaction<'_>,
        _method: &str,
        _params: &Value,
        _identity: &ProviderDeliveryIdentity,
        _at: f64,
    ) -> Result<(), NotificationDispatchError> {
        Err(NotificationDispatchError::Dependency(
            NotificationDependency::AccountNotice.name(),
        ))
    }

    fn generation_started(&self) -> Result<(), NotificationDispatchError> {
        Err(NotificationDispatchError::Dependency(
            NotificationDependency::TokenRate.name(),
        ))
    }

    fn capture_usage(
        &self,
        _tx: &Transaction<'_>,
        _agent: &mut Value,
        _params: &Value,
        _at: f64,
    ) -> Result<(), NotificationDispatchError> {
        Err(NotificationDispatchError::Dependency(
            NotificationDependency::BudgetCapture.name(),
        ))
    }
    fn capture_move_cache(
        &self,
        _tx: &Transaction<'_>,
        _agent: &Value,
        _params: &Value,
        _at: f64,
    ) -> Result<(), NotificationDispatchError> {
        Err(NotificationDispatchError::Dependency(
            NotificationDependency::MoveCache.name(),
        ))
    }
}
#[derive(Default)]
pub struct UnavailableNotificationEffects;
impl NotificationEffects for UnavailableNotificationEffects {}
