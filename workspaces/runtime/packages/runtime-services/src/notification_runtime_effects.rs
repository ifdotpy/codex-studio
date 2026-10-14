//! Runtime notification effects. The main writer captures budget and move cache;
//! analytics and volatile rates are immutable values for the caller after commit.
use crate::budget::CaptureSource;
use crate::notification_dispatch::{
    DispatchOutcome, NotificationDispatchError, NotificationEffects, NotificationObservation,
    NotificationPostCommit, ProviderDeliveryIdentity,
};
use serde_json::Value;
use studio_runtime_store::Transaction;

pub struct RuntimeNotificationEffects;

impl NotificationEffects for RuntimeNotificationEffects {
    fn capture_usage(
        &self,
        tx: &Transaction<'_>,
        agent: &mut Value,
        params: &Value,
        at: f64,
    ) -> Result<(), NotificationDispatchError> {
        crate::budget_service::capture(tx, agent, params, at, CaptureSource::Live, at)?;
        Ok(())
    }

    fn capture_move_cache(
        &self,
        tx: &Transaction<'_>,
        agent: &Value,
        params: &Value,
        at: f64,
    ) -> Result<(), NotificationDispatchError> {
        let agent_id = agent["id"]
            .as_str()
            .ok_or(NotificationDispatchError::InvalidNotification)?;
        crate::agent_move::capture_cache(tx, agent_id, params, at)?;
        Ok(())
    }

    fn account_notice(
        &self,
        tx: &Transaction<'_>,
        method: &str,
        params: &Value,
        identity: &ProviderDeliveryIdentity,
        at: f64,
    ) -> Result<(), NotificationDispatchError> {
        crate::notification_dispatch::notices::account_notice(tx, method, params, identity, at)
    }

    fn generation_started(&self) -> Result<(), NotificationDispatchError> {
        Ok(())
    }

    fn post_commit(
        &self,
        before: Option<&Value>,
        after: Option<&Value>,
        frame: &Value,
        identity: &ProviderDeliveryIdentity,
        at: f64,
        outcome: &DispatchOutcome,
    ) -> Vec<NotificationPostCommit> {
        let method = frame["method"].as_str().unwrap_or_default();
        let observation = |agent: Option<&Value>| NotificationObservation {
            agent: agent.cloned(),
            method: method.into(),
            params: frame["params"].clone(),
            delivery: identity.clone(),
            observed_at: at,
            provider_at: event_time(frame).filter(|time| *time != 0.0).unwrap_or(at),
        };
        let mut result = Vec::new();
        if matches!(
            method,
            "item/agentMessage/delta" | "item/reasoning/textDelta" | "provider/generationStarted"
        ) {
            result.push(NotificationPostCommit::TokenRateStream(observation(None)));
        }
        let Some(before) = before else {
            return result;
        };
        if matches!(outcome, DispatchOutcome::Ignored { reason } if reason == "closed-stream" || reason == "deleted-agent")
        {
            return result;
        }
        if matches!(
            method,
            "hook/started" | "hook/completed" | "serverRequest/resolved"
        ) {
            return result;
        }
        // Native notices skipped by turn affinity never reach Python analytics.
        if crate::notification_dispatch::notices::is_notice(method)
            && !crate::notification_dispatch::notices::captures_analytics(
                before,
                method,
                &frame["params"],
            )
        {
            return result;
        }
        let captured = (method == "thread/tokenUsage/updated")
            .then(|| after.and_then(|agent| agent["tokensUsed"].as_u64()))
            .flatten();
        // Python captures the budget before freezing analytics arguments.
        let mut analytics_agent = before.clone();
        if captured.is_some()
            && let Some(after) = after
        {
            analytics_agent["tokensUsed"] = after["tokensUsed"].clone();
            analytics_agent["tokenUsageAccounting"] = after["tokenUsageAccounting"].clone();
        }
        result.push(NotificationPostCommit::Analytics {
            observation: observation(Some(&analytics_agent)),
            budget_capture_value: captured,
        });
        if matches!(outcome, DispatchOutcome::Written)
            && matches!(
                method,
                "item/started" | "item/completed" | "thread/tokenUsage/updated"
            )
        {
            result.push(NotificationPostCommit::TokenRateAssociation(observation(
                after,
            )));
        }
        result
    }
}

/// The B7 consumer implements this port outside the SQLite transaction. It must
/// deduplicate observations by delivery identity before publishing or capturing.
/// The worker never calls it for an uncommitted result or a receipt replay.
/// Implementations deduplicate by the effect's `observation.delivery`; a
/// process may crash after publication and before the durable row is deleted.
pub trait NotificationPostCommitSink: Send + Sync {
    fn publish(&self, effect: &NotificationPostCommit) -> Result<(), NotificationDispatchError>;
}

pub struct UnavailablePostCommitSink;

impl NotificationPostCommitSink for UnavailablePostCommitSink {
    fn publish(&self, _effect: &NotificationPostCommit) -> Result<(), NotificationDispatchError> {
        // Non-host/test callers have no HTTP publication hubs attached.
        Ok(())
    }
}

/// Publishes committed notification effects to the process-local sync hubs.
/// Analytics persistence has no Rust consumer yet, so those variants are an
/// explicit successful no-op; the receipt is nevertheless drained.
pub struct RuntimeNotificationPostCommitSink {
    pub token_rates: std::sync::Arc<crate::token_rate::TokenRateHub>,
    pub resources: std::sync::Arc<crate::resource_notify::ResourceNotifications>,
}

impl NotificationPostCommitSink for RuntimeNotificationPostCommitSink {
    fn publish(&self, effect: &NotificationPostCommit) -> Result<(), NotificationDispatchError> {
        match effect {
            NotificationPostCommit::TokenRateStream(_) => {
                self.token_rates
                    .publish_observation(effect.clone())
                    .map_err(|_| {
                        NotificationDispatchError::Dependency("sync/token-rates/publication")
                    })?;
            }
            NotificationPostCommit::TokenRateAssociation(_) => {
                // Python associates rollout response usage with a live rate
                // sample; Rust has no response-rate association cache yet.
            }
            NotificationPostCommit::Analytics { .. }
            | NotificationPostCommit::AnalyticsStream { .. } => {
                // Python schedules analytics capture after commit. Rust has no
                // analytics stream writer; keep this a named no-op until it lands.
            }
        }
        let observation = match effect {
            NotificationPostCommit::Analytics { observation, .. }
            | NotificationPostCommit::AnalyticsStream { observation, .. }
            | NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation) => observation,
        };
        let delivery = &observation.delivery;
        let request = serde_json::json!({
            "requestId": format!("provider-publication:{}:{}:{}", delivery.provider_handle, delivery.generation, delivery.cursor),
            "resources": [{"kind":"state"}],
        });
        self.resources
            .publish_once(&request)
            .map_err(|_| NotificationDispatchError::Dependency("sync/resources/publication"))?;
        Ok(())
    }
}

/// Match the provider's event-time precedence; clocks are supplied, never read.
const MILLISECOND_TIMESTAMP_THRESHOLD: f64 = 100_000_000_000.0;

fn event_time(frame: &Value) -> Option<f64> {
    let method = frame["method"].as_str().unwrap_or_default();
    let first = if matches!(method, "item/started" | "turn/started") {
        "startedAt"
    } else {
        "completedAt"
    };
    let params = &frame["params"];
    let time = |value: &Value| {
        value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|n| {
                if n > MILLISECOND_TIMESTAMP_THRESHOLD {
                    n / 1000.0
                } else {
                    n
                }
            })
    };
    for source in [params, &params["item"], &params["turn"]] {
        for field in [first, "timestamp", "time", "createdAt"] {
            if let Some(at) = time(&source[field]) {
                return Some(at);
            }
        }
    }
    for field in [
        "_studioSupervisorReceivedAt",
        "_studioSupervisorTime",
        "_studioSequenceAt",
        "_studioReceivedAt",
    ] {
        if let Some(at) = time(&frame[field]) {
            return Some(at);
        }
    }
    None
}
