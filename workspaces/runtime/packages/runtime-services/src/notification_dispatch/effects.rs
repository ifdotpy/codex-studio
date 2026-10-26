//! Injection seam for effects owned by other notification subsystems.
use super::{DispatchOutcome, NotificationDispatchError, ProviderDeliveryIdentity};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use studio_runtime_store::Transaction;

/// JSON payload shared by immutable publication observations. Serialization is
/// transparent so durable journal records retain the same shape.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedJson(Arc<Value>);

impl SharedJson {
    pub fn new(value: Value) -> Self {
        Self(Arc::new(value))
    }

    pub fn agent_projection(agent: &Value) -> Self {
        let mut projection = serde_json::Map::new();
        for field in [
            "id",
            "name",
            "rootId",
            "parentId",
            "accountKey",
            "accountHistory",
            "provider",
            "threadId",
            "epoch",
            "turnId",
            "inFlight",
            "events",
            "tokensUsed",
            "tokenUsageAccounting",
            "model",
            "effort",
            "fastMode",
            "daybreakEnabled",
            "cyberAccessProgram",
            "cwd",
            "deletedAt",
        ] {
            if let Some(value) = agent.get(field) {
                projection.insert(field.to_owned(), value.clone());
            }
        }
        Self::new(Value::Object(projection))
    }

    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Deref for SharedJson {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SharedJson {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl Serialize for SharedJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SharedJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Value::deserialize(deserializer).map(Self::new)
    }
}

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
/// The effect kind plus `delivery` is the idempotency key; retain account and generation.
/// The caller stages these serializable values in its existing effect journal
/// before committing, and drains the journal after commit.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NotificationObservation {
    pub agent: Option<SharedJson>,
    pub method: String,
    pub params: SharedJson,
    pub delivery: ProviderDeliveryIdentity,
    pub observed_at: f64,
    pub provider_at: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum NotificationPostCommit {
    Analytics {
        observation: NotificationObservation,
        budget_capture_value: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage_conflict: Option<Box<studio_runtime_schema::usage::ledger::UsageReplayConflict>>,
        /// Budget migration epoch bound when this effect is durably logged.
        #[serde(
            default,
            skip_serializing_if = "studio_runtime_schema::usage::ledger::LedgerEpoch::is_legacy"
        )]
        ledger_since: studio_runtime_schema::usage::ledger::LedgerEpoch,
        #[serde(default)]
        usage_unknown: bool,
    },
    AnalyticsStream {
        observation: NotificationObservation,
        metrics: NotificationStreamMetrics,
    },
    /// `Analytics` after its rows were captured inside the journal commit. The
    /// sink only projects the token rate; the store observer owns state changes.
    AnalyticsCaptured(NotificationObservation),
    /// `AnalyticsStream` after capture inside the journal commit; the store
    /// observer already published its state changes.
    AnalyticsStreamCaptured(NotificationObservation),
    TokenRateStream(NotificationObservation),
    TokenRateAssociation(NotificationObservation),
    TokenRateUsage(NotificationObservation),
    /// Reconstructed from retained native history after a restart. This has a
    /// recovery identity rather than an invented provider delivery cursor.
    Recovered {
        identity: String,
        observation: RecoveredNotificationObservation,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RecoveredNotificationObservation {
    pub agent: Value,
    pub method: String,
    pub params: Value,
    pub observed_at: f64,
}

/// One committed publication unit. `sequence` is monotone per retained account
/// buffer; it is independent of original provider cursors. Persist this entire
/// vector with the updated buffer before commit. Recover batches in sequence
/// order, and effects in vector order. A sink deduplicates (account,sequence,index).
/// Never recover these values in a hashed effect-ID order.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct NotificationPublicationBatch {
    pub sequence: u64,
    pub effects: Vec<NotificationPostCommit>,
    pub analytics_overflow: u64,
}
impl NotificationPublicationBatch {
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.analytics_overflow == 0
    }
}

/// Aggregate matching Python StreamBuffer's analytics closure. The consumer
/// captures `observation` once, then corrects notifications per hour and stream
/// counters using these totals (without retaining each raw delta).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct NotificationStreamMetrics {
    pub hours: Vec<NotificationHourMetrics>,
    pub bytes: u64,
    pub characters: u64,
    pub lines: u64,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NotificationHourMetrics {
    pub hour: u64,
    pub count: u64,
    pub encoded_bytes: u64,
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
    ) -> Result<crate::budget::UsageAcceptance, NotificationDispatchError> {
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
