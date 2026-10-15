//! Runtime notification effects. The main writer captures budget and move cache;
//! analytics and volatile rates are immutable values for the caller after commit.
use crate::budget::CaptureSource;
use crate::notification_dispatch::{
    DispatchOutcome, NotificationDispatchError, NotificationEffects, NotificationObservation,
    NotificationPostCommit, ProviderDeliveryIdentity,
};
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_store::RuntimeStore;
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
            "item/agentMessage/delta" | "item/reasoning/textDelta"
        ) {
            result.push(NotificationPostCommit::TokenRateStream(observation(
                before.or(after),
            )));
        }
        if method == "provider/generationStarted" && matches!(outcome, DispatchOutcome::Written) {
            result.push(NotificationPostCommit::TokenRateStream(observation(
                before.or(after),
            )));
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
        if matches!(
            method,
            "turn/started" | "turn/completed" | "thread/tokenUsage/updated"
        ) {
            result.push(NotificationPostCommit::TokenRateUsage(observation(after)));
        }
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

    fn invalidate_state(&self, _request_id: &str) -> Result<(), NotificationDispatchError> {
        Ok(())
    }
}

pub struct UnavailablePostCommitSink;

impl NotificationPostCommitSink for UnavailablePostCommitSink {
    fn publish(&self, _effect: &NotificationPostCommit) -> Result<(), NotificationDispatchError> {
        // Non-host/test callers have no HTTP publication hubs attached.
        Ok(())
    }
}

/// Publishes committed notification effects to analytics and HTTP sync hubs.
pub struct RuntimeNotificationPostCommitSink {
    pub token_rates: std::sync::Arc<crate::token_rate::TokenRateHub>,
    pub resources: std::sync::Arc<crate::resource_notify::ResourceNotifications>,
    pub analytics_store: Arc<RuntimeStore>,
}

impl NotificationPostCommitSink for RuntimeNotificationPostCommitSink {
    fn publish(&self, effect: &NotificationPostCommit) -> Result<(), NotificationDispatchError> {
        if let NotificationPostCommit::Recovered { identity, .. } = effect {
            self.resources
                .publish_once(&serde_json::json!({
                    "requestId": format!("recovered-provider-publication:{identity}"),
                    "resources": [{"kind":"state"}],
                }))
                .map_err(|_| NotificationDispatchError::Dependency("sync/resources/publication"))?;
            return Ok(());
        }
        match effect {
            NotificationPostCommit::TokenRateStream(_)
            | NotificationPostCommit::TokenRateAssociation(_)
            | NotificationPostCommit::TokenRateUsage(_) => {
                self.project_token_rate(effect)?;
            }
            NotificationPostCommit::Analytics {
                observation,
                budget_capture_value: _,
            } => {
                self.capture_analytics(effect, observation, None)?;
                self.project_token_rate(effect)?;
            }
            NotificationPostCommit::AnalyticsStream {
                observation,
                metrics,
            } => {
                self.capture_analytics(effect, observation, Some(metrics))?;
            }
            NotificationPostCommit::Recovered { .. } => {
                return Err(NotificationDispatchError::InvalidNotification);
            }
        };
        let observation = match effect {
            NotificationPostCommit::Analytics { observation, .. }
            | NotificationPostCommit::AnalyticsStream { observation, .. }
            | NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation)
            | NotificationPostCommit::TokenRateUsage(observation) => observation,
            NotificationPostCommit::Recovered { .. } => {
                return Err(NotificationDispatchError::InvalidNotification);
            }
        };
        let delivery = &observation.delivery;
        let kind = match effect {
            NotificationPostCommit::Analytics { .. } => "analytics",
            NotificationPostCommit::AnalyticsStream { .. } => "analyticsStream",
            NotificationPostCommit::TokenRateStream(_) => "tokenRateStream",
            NotificationPostCommit::TokenRateAssociation(_) => "tokenRateAssociation",
            NotificationPostCommit::TokenRateUsage(_) => "tokenRateUsage",
            NotificationPostCommit::Recovered { .. } => {
                return Err(NotificationDispatchError::InvalidNotification);
            }
        };
        let request = serde_json::json!({
            "requestId": format!("provider-publication:{}:{}:{}:{kind}", delivery.provider_handle, delivery.generation, delivery.cursor),
            "resources": [{"kind":"state"}],
        });
        // A repeated full-state invalidation is a set operation: it asks clients
        // to re-read current state and cannot duplicate or mutate stored rows.
        self.resources
            .publish_once(&request)
            .map_err(|_| NotificationDispatchError::Dependency("sync/resources/publication"))?;
        Ok(())
    }

    fn invalidate_state(&self, request_id: &str) -> Result<(), NotificationDispatchError> {
        self.resources
            .publish_once(&serde_json::json!({
                "requestId": request_id,
                "resources": [{"kind":"state"}],
            }))
            .map_err(|_| NotificationDispatchError::Dependency("sync/resources/publication"))?;
        Ok(())
    }
}

impl RuntimeNotificationPostCommitSink {
    fn project_token_rate(
        &self,
        effect: &NotificationPostCommit,
    ) -> Result<(), NotificationDispatchError> {
        if !matches!(
            effect,
            NotificationPostCommit::Analytics { .. }
                | NotificationPostCommit::TokenRateStream(_)
                | NotificationPostCommit::TokenRateAssociation(_)
                | NotificationPostCommit::TokenRateUsage(_)
        ) {
            return Err(NotificationDispatchError::InvalidNotification);
        }
        self.token_rates
            .publish_observation(effect.clone())
            .map_err(|_| NotificationDispatchError::Dependency("sync/token-rates/publication"))?;
        Ok(())
    }

    fn capture_analytics(
        &self,
        effect: &NotificationPostCommit,
        observation: &NotificationObservation,
        stream: Option<&crate::notification_dispatch::NotificationStreamMetrics>,
    ) -> Result<bool, NotificationDispatchError> {
        let agent = observation
            .agent
            .as_ref()
            .ok_or(NotificationDispatchError::InvalidNotification)?;
        let prepared = crate::analytics_capture::PreparedCapture::event(
            agent,
            &observation.method,
            &observation.params,
            observation.observed_at,
            observation.observed_at,
            crate::analytics_capture::CaptureSource::Live,
        )
        .map_err(|_| NotificationDispatchError::Dependency("analytics/capture"))?;
        let kind = match effect {
            NotificationPostCommit::Analytics { .. } => "analytics",
            NotificationPostCommit::AnalyticsStream { .. } => "analyticsStream",
            _ => return Err(NotificationDispatchError::InvalidNotification),
        };
        crate::analytics_capture::capture_notification_effect(
            &self.analytics_store,
            &observation.delivery.provider_handle,
            observation.delivery.generation,
            observation.delivery.cursor,
            kind,
            &prepared,
            stream,
        )
        .map_err(|_| NotificationDispatchError::Dependency("analytics/capture"))
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
