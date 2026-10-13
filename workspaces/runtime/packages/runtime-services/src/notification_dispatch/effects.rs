//! Injection seam for effects owned by other notification subsystems.
use super::NotificationDispatchError;
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

/// Transactional effects only: no provider call, blocking IO, or await here.
/// The worker supplies budget_service::capture(..., CaptureSource::Live) and
/// agent_move::capture_cache; both must commit with the notification receipt.
pub trait NotificationEffects {
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
