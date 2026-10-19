//! Runtime notification effects. The main writer captures budget and move cache;
//! analytics and volatile rates are immutable values for the caller after commit.
use crate::budget::CaptureSource;
use crate::notification_dispatch::{
    DispatchOutcome, NotificationDispatchError, NotificationEffects, NotificationObservation,
    NotificationPostCommit, ProviderDeliveryIdentity, SharedJson,
};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
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
        let params = SharedJson::new(frame["params"].clone());
        let before_observation = before.map(SharedJson::agent_projection);
        let after_observation = after.map(SharedJson::agent_projection);
        let observation = |agent: Option<&SharedJson>| NotificationObservation {
            agent: agent.cloned(),
            method: method.into(),
            params: params.clone(),
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
                before_observation.as_ref().or(after_observation.as_ref()),
            )));
        }
        if method == "provider/generationStarted" && matches!(outcome, DispatchOutcome::Written) {
            result.push(NotificationPostCommit::TokenRateStream(observation(
                before_observation.as_ref().or(after_observation.as_ref()),
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
        let mut analytics_agent = SharedJson::agent_projection(before);
        if captured.is_some()
            && let Some(after) = after
        {
            for field in ["tokensUsed", "tokenUsageAccounting"] {
                if let Some(value) = after.get(field) {
                    analytics_agent[field] = value.clone();
                }
            }
        }
        result.push(NotificationPostCommit::Analytics {
            observation: observation(Some(&analytics_agent)),
            budget_capture_value: captured,
        });
        if matches!(
            method,
            "turn/started" | "turn/completed" | "thread/tokenUsage/updated"
        ) {
            result.push(NotificationPostCommit::TokenRateUsage(observation(
                after_observation.as_ref(),
            )));
        }
        if matches!(outcome, DispatchOutcome::Written)
            && matches!(
                method,
                "item/started" | "item/completed" | "thread/tokenUsage/updated"
            )
        {
            result.push(NotificationPostCommit::TokenRateAssociation(observation(
                after_observation.as_ref(),
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
/// An analytics effect whose agent identity was superseded returns `Ok` and
/// writes nothing; every other error keeps the receipt for retry.
pub trait NotificationPostCommitSink: Send + Sync {
    fn publish<'a>(
        &'a self,
        effect: &'a NotificationPostCommit,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotificationDispatchError>> + Send + 'a>>;

    fn invalidate_state<'a>(
        &'a self,
        _request_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotificationDispatchError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

pub struct UnavailablePostCommitSink;

impl NotificationPostCommitSink for UnavailablePostCommitSink {
    fn publish<'a>(
        &'a self,
        _effect: &'a NotificationPostCommit,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotificationDispatchError>> + Send + 'a>> {
        Box::pin(async {
            // Non-host/test callers have no HTTP publication hubs attached.
            Ok(())
        })
    }
}

/// Publishes committed notification effects to analytics and HTTP sync hubs.
pub struct RuntimeNotificationPostCommitSink {
    pub token_rates: std::sync::Arc<crate::token_rate::TokenRateHub>,
    pub resources: std::sync::Arc<crate::resource_notify::ResourceNotifications>,
    pub analytics_store: Arc<RuntimeStore>,
}

impl NotificationPostCommitSink for RuntimeNotificationPostCommitSink {
    fn publish<'a>(
        &'a self,
        effect: &'a NotificationPostCommit,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotificationDispatchError>> + Send + 'a>> {
        Box::pin(async move { self.publish_effect(effect).await })
    }

    fn invalidate_state<'a>(
        &'a self,
        request_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotificationDispatchError>> + Send + 'a>> {
        Box::pin(async move {
            self.resources
                .publish_once(&serde_json::json!({
                    "requestId": request_id,
                    "resources": [{"kind":"state"}],
                }))
                .map_err(|_| NotificationDispatchError::Dependency("sync/resources/publication"))?;
            Ok(())
        })
    }
}

/// Whether an analytics effect still belongs to the authoritative agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnalyticsDisposition {
    /// Captured now, or an exact replay. Replays still project volatile token
    /// rates and state, because that projection is not durable across restart.
    Current,
    /// The agent identity was replaced or deleted. The effect settles without
    /// analytics, token-rate, or state publication.
    Stale,
}

impl RuntimeNotificationPostCommitSink {
    async fn publish_effect(
        &self,
        effect: &NotificationPostCommit,
    ) -> Result<(), NotificationDispatchError> {
        let (observation, kind) = match effect {
            NotificationPostCommit::Recovered { identity, .. } => {
                self.resources
                    .publish_once(&serde_json::json!({
                        "requestId": format!("recovered-provider-publication:{identity}"),
                        "resources": [{"kind":"state"}],
                    }))
                    .map_err(|_| {
                        NotificationDispatchError::Dependency("sync/resources/publication")
                    })?;
                return Ok(());
            }
            NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation)
            | NotificationPostCommit::TokenRateUsage(observation) => {
                // Unmanaged provider threads have no agent to project. Retire
                // their committed observation instead of retrying it forever.
                if observation.agent.is_none() {
                    return Ok(());
                }
                self.project_token_rate(effect)?;
                // The token-rate hub owns this volatile update and its SSE frame.
                // No durable state changed here, so a state reset would only
                // make clients reread the same entity sequence.
                return Ok(());
            }
            NotificationPostCommit::Analytics {
                observation,
                budget_capture_value: _,
            } => {
                let disposition = self.capture_analytics(effect, observation, None).await?;
                if disposition == AnalyticsDisposition::Stale {
                    return Ok(());
                }
                self.project_token_rate(effect)?;
                (observation, "analytics")
            }
            NotificationPostCommit::AnalyticsStream {
                observation,
                metrics,
            } => {
                let disposition = self
                    .capture_analytics(effect, observation, Some(metrics.clone()))
                    .await?;
                if disposition == AnalyticsDisposition::Stale {
                    return Ok(());
                }
                (observation, "analyticsStream")
            }
        };
        let delivery = &observation.delivery;
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

    async fn capture_analytics(
        &self,
        effect: &NotificationPostCommit,
        observation: &NotificationObservation,
        stream: Option<crate::notification_dispatch::NotificationStreamMetrics>,
    ) -> Result<AnalyticsDisposition, NotificationDispatchError> {
        let agent = observation
            .agent
            .as_ref()
            .ok_or(NotificationDispatchError::InvalidNotification)?;
        // Input validation is deterministic: a retry fails the same way, so a
        // refused input settles like a stale one (see below).
        let prepared = match crate::analytics_capture::PreparedCapture::notification_event(
            agent,
            &observation.method,
            &observation.params,
            observation.observed_at,
            observation.observed_at,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                eprintln!(
                    "analytics capture dropped {} {}:{}:{}: {error}",
                    observation.method,
                    observation.delivery.provider_handle,
                    observation.delivery.generation,
                    observation.delivery.cursor
                );
                return Ok(AnalyticsDisposition::Stale);
            }
        };
        let kind = match effect {
            NotificationPostCommit::Analytics { .. } => "analytics".to_owned(),
            NotificationPostCommit::AnalyticsStream { .. } => "analyticsStream".to_owned(),
            _ => return Err(NotificationDispatchError::InvalidNotification),
        };
        match crate::analytics_capture::capture_notification_effect(
            &self.analytics_store,
            observation.delivery.provider_handle.clone(),
            observation.delivery.generation,
            observation.delivery.cursor,
            kind,
            prepared,
            stream,
        )
        .await
        {
            // `true` captured now; `false` is an exact or legacy replay whose
            // rows already exist.
            Ok(_) => Ok(AnalyticsDisposition::Current),
            // The identity check rolls back the effect receipt, so a retry
            // would fail the same way and block every later publication.
            Err(crate::analytics_capture::CaptureError::StaleIdentity) => {
                Ok(AnalyticsDisposition::Stale)
            }
            // An invalid input fails the same way on every retry. Python's
            // lossy capture worker records the failure and continues; holding
            // the receipt would block every later publication of the account.
            Err(crate::analytics_capture::CaptureError::Invalid(location)) => {
                eprintln!(
                    "analytics capture dropped {} {}:{}:{}: invalid input at {location}",
                    observation.method,
                    observation.delivery.provider_handle,
                    observation.delivery.generation,
                    observation.delivery.cursor
                );
                Ok(AnalyticsDisposition::Stale)
            }
            Err(_) => Err(NotificationDispatchError::Dependency("analytics/capture")),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_project_agents_and_share_immutable_params() {
        let agent = serde_json::json!({
            "id":"agent", "name":"Agent", "rootId":"root", "accountKey":"default",
            "accountHistory":[], "provider":"codex", "threadId":"thread", "epoch":4,
            "turnId":"turn", "inFlight":false, "model":"model", "effort":"high",
            "fastMode":false, "daybreakEnabled":false, "cyberAccessProgram":false,
            "cwd":"/workspace", "deletedAt":null, "unrelatedTranscript": "x".repeat(64 * 1024)
        });
        let frame = serde_json::json!({
            "method":"turn/completed",
            "params":{"threadId":"thread","turnId":"turn"}
        });
        let identity = ProviderDeliveryIdentity {
            provider_handle: "account:default".into(),
            generation: 4,
            cursor: 9,
        };

        let effects = RuntimeNotificationEffects.post_commit(
            Some(&agent),
            Some(&agent),
            &frame,
            &identity,
            12.0,
            &DispatchOutcome::Written,
        );
        let observations = effects
            .iter()
            .map(|effect| match effect {
                NotificationPostCommit::Analytics { observation, .. }
                | NotificationPostCommit::TokenRateUsage(observation) => observation,
                _ => panic!("unexpected effect"),
            })
            .collect::<Vec<_>>();
        assert_eq!(observations.len(), 2);
        assert!(
            observations[0]
                .params
                .shares_storage_with(&observations[1].params)
        );
        let analytics_agent = observations[0].agent.as_ref().expect("agent projection");
        assert_eq!(analytics_agent["id"], "agent");
        assert!(
            !analytics_agent
                .as_object()
                .expect("analytics agent is an object")
                .contains_key("unrelatedTranscript")
        );
        assert!(
            serde_json::to_value(&effects[0])
                .expect("journal serialization")
                .pointer("/Analytics/observation/params/threadId")
                .is_some()
        );
    }
}

#[cfg(test)]
mod capture_refusal_tests {
    use super::*;

    #[tokio::test]
    async fn an_invalid_analytics_input_settles_instead_of_holding_the_queue() {
        // Live cutover 4: one capture that failed the same way on every retry
        // held the account's publication head until the queue reached its
        // bound and the account worker stopped.
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store opens"),
        );
        let changes = Arc::new(crate::change_notifications::ChangeNotificationHub::new());
        let sink = RuntimeNotificationPostCommitSink {
            token_rates: Arc::new(crate::token_rate::TokenRateHub::with_change_hub(
                Arc::clone(&changes),
            )),
            resources: Arc::new(crate::resource_notify::ResourceNotifications::new(
                Arc::clone(&changes),
            )),
            analytics_store: Arc::clone(&store),
        };
        let effect = NotificationPostCommit::Analytics {
            observation: NotificationObservation {
                agent: Some(crate::notification_dispatch::SharedJson::new(
                    serde_json::json!({
                        "id":"agent","name":"Agent","rootId":"root","accountKey":"default",
                        "accountHistory":[],"provider":"codex","threadId":"thread","turnId":"turn",
                        "model":"gpt-5","effort":"high","fastMode":false,"daybreakEnabled":false,
                        "cyberAccessProgram":false,"cwd":"/disposable","deletedAt":null,
                        "epoch":0,"status":"running"
                    }),
                )),
                method: "item/started".into(),
                params: crate::notification_dispatch::SharedJson::new(serde_json::json!([
                    "not an object"
                ])),
                delivery: ProviderDeliveryIdentity {
                    provider_handle: "account:default".into(),
                    generation: 7,
                    cursor: 50,
                },
                observed_at: 60.0,
                provider_at: 60.0,
            },
            budget_capture_value: None,
        };
        sink.publish(&effect)
            .await
            .expect("a deterministic capture refusal settles the publication");
    }
}
