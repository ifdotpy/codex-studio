//! Shared admission and acceptance ports of native compact/review actions.
//! The HTTP action route and the native review child driver use these owners.
use crate::message_admission::MessageAdmission;
use crate::native_actions::{
    Acceptance, Action, Admission, Delivery, NativeError, NativeRequest, Result,
};
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, Transaction};

/// Native action admission over the message-ingress workspace and budget owner.
/// Review also applies the global and team concurrency limits of Python's dispatcher.
pub struct MessageNativeAdmission(Arc<dyn MessageAdmission>);
impl MessageNativeAdmission {
    pub fn new(admission: Arc<dyn MessageAdmission>) -> Self {
        Self(admission)
    }
}
impl Admission for MessageNativeAdmission {
    fn check(&self, tx: &Transaction<'_>, value: &Value, action: Action) -> Result<()> {
        let actor =
            AgentRecord::from_value(value.clone()).map_err(|_| NativeError::Invalid("agent"))?;
        let root = AgentRepository::get(tx, &actor.root_id)
            .map_err(|_| NativeError::Unavailable("budget-policy/native-action"))?
            .ok_or(NativeError::Changed)?;
        let account = value
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        if root
            .extra
            .get("excludedAccountKeys")
            .and_then(Value::as_array)
            .is_some_and(|keys| keys.iter().any(|key| key.as_str() == Some(account)))
        {
            return Err(NativeError::Invalid("account-excluded"));
        }
        let thread_id = value.get("threadId").and_then(Value::as_str);
        let native_thread_blocked = value
            .get("nativeThreadBlock")
            .and_then(Value::as_object)
            .is_some_and(|block| {
                block
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| Some(id) == thread_id)
            })
            || (thread_id.is_some()
                && value
                    .pointer("/error/codexErrorInfo")
                    .and_then(Value::as_str)
                    == Some("misalignmentPolicyViolation"))
            || (thread_id.is_some()
                && value.get("nativeTurnError").is_some_and(|previous| {
                    previous.get("turnId") == value.get("turnId")
                        && previous
                            .pointer("/error/codexErrorInfo")
                            .and_then(Value::as_str)
                            == Some("misalignmentPolicyViolation")
                }));
        if native_thread_blocked {
            return Err(NativeError::Invalid("native-thread/precaution-stop"));
        }
        if action == Action::Review {
            if value.get("provider").and_then(Value::as_str) != Some("codex") {
                return Err(NativeError::Invalid("review/provider"));
            }
            if value.get("daybreakEnabled") == Some(&serde_json::json!(true)) {
                return Err(NativeError::Invalid("review/daybreak"));
            }
            let root_value = root.value();
            let mode_version = root_value
                .get("subagentConcurrencyVersion")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let mode = root_value.get("agentMode").and_then(Value::as_str);
            let team_limit = if mode_version < 2 && mode == Some("single") {
                0
            } else {
                root_value
                    .get("concurrency")
                    .and_then(Value::as_i64)
                    .unwrap_or(if mode == Some("single") { 0 } else { 32 })
            };
            if actor.root_id != actor.id && team_limit == 0 {
                return Err(NativeError::Invalid("review/single-agent-mode"));
            }
            let global_limit = std::env::var("CODEX_CANVAS_CONCURRENCY")
                .ok()
                .map(|value| value.parse::<i64>())
                .transpose()
                .map_err(|_| NativeError::Invalid("review/global-concurrency"))?
                .unwrap_or(
                    studio_runtime_schema::agent_limits_generated::DEFAULT_GLOBAL_CONCURRENCY,
                )
                .max(studio_runtime_schema::agent_limits_generated::MIN_GLOBAL_CONCURRENCY);
            let global_limit = usize::try_from(global_limit)
                .map_err(|_| NativeError::Invalid("review/global-concurrency"))?;
            let active = tx
                .prepare(
                    "SELECT id, json_extract(record,'$.rootId') FROM runtime_agents \
                     WHERE json_extract(record,'$.inFlight')=1 \
                        OR json_extract(record,'$.status') IN ('running','starting','approval') \
                        OR (json_extract(record,'$.startAttempt.activeAtReservation')=1 AND EXISTS (\
                            SELECT 1 FROM json_each(runtime_agents.record,'$.startAttempt.events') AS input \
                            JOIN runtime_events AS event ON event.id=input.value \
                            WHERE event.agent=runtime_agents.id \
                              AND event.epoch=json_extract(runtime_agents.record,'$.startAttempt.epoch') \
                              AND event.status IN ('reserved','dispatching','uncertain'))) \
                     ORDER BY id",
                )?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let active = active
                .iter()
                .filter(|(id, _)| id != &actor.id.0)
                .collect::<Vec<_>>();
            if active.len() >= global_limit {
                return Err(NativeError::Invalid("review/global-capacity"));
            }
            if actor.root_id != actor.id
                && active
                    .iter()
                    .filter(|(id, root_id)| {
                        id != &actor.root_id.0
                            && root_id.as_deref() == Some(actor.root_id.0.as_str())
                    })
                    .count()
                    >= usize::try_from(team_limit)
                        .map_err(|_| NativeError::Invalid("review/team-concurrency"))?
            {
                return Err(NativeError::Invalid("review/team-capacity"));
            }
        }
        let blockers = self.0.workspace(tx, &actor).map_err(admission_error)?;
        if !blockers.is_empty() {
            return Err(NativeError::Invalid("workspace"));
        }
        self.0.budget(tx, &actor, &root).map_err(admission_error)
    }
}

/// Provider-owned agent effects of an accepted or rejected compact/review hand-off.
pub struct NativeActionAcceptance;
impl Acceptance for NativeActionAcceptance {
    fn apply(
        &self,
        tx: &Transaction<'_>,
        request: &NativeRequest,
        delivery: &Delivery,
    ) -> Result<()> {
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(request.ticket.agent.as_str()))
            .map_err(|_| NativeError::Changed)?
        else {
            return Err(NativeError::Changed);
        };
        let mut value = agent.value();
        if value["startAttempt"]["id"] != request.ticket.operation
            || value["startAttempt"]["actionRequestId"] != request.ticket.request
            || value["epoch"].as_u64() != Some(request.ticket.epoch)
        {
            return Err(NativeError::Changed);
        }
        match delivery {
            Delivery::Written(result) if request.method == "review/start" => {
                let turn = result["turn"]["id"]
                    .as_str()
                    .filter(|turn| !turn.is_empty())
                    .ok_or(NativeError::Unknown)?;
                value["turnId"] = serde_json::json!(turn);
                value["inFlight"] = serde_json::json!(true);
                value["status"] = serde_json::json!("running");
                value["error"] = Value::Null;
            }
            Delivery::Written(_) if request.method == "thread/compact/start" => {
                if value
                    .get("error")
                    .is_some_and(|error| error == &value["startAttempt"]["responseError"])
                {
                    value["error"] = Value::Null;
                }
            }
            Delivery::Rejected(_) => {
                value["status"] = serde_json::json!("failed");
                value["inFlight"] = serde_json::json!(false);
                value["error"] = serde_json::json!("The provider rejected the native action.");
            }
            Delivery::NotSent | Delivery::Unknown | Delivery::Written(_) => return Ok(()),
        }
        let review_turn = match delivery {
            Delivery::Written(result) if request.method == "review/start" => {
                result["turn"]["id"].as_str().map(str::to_owned)
            }
            _ => None,
        };
        AgentRepository::put(tx, &AgentRecord::from_value(value)?)
            .map_err(|_| NativeError::Changed)?;
        if let Some(turn) = review_turn {
            // Python `native_action_accepted` accepts a review reply with the
            // ordinary `start_accepted`, which binds `startAttempt.turnId`. The
            // turn's completion settles only a bound attempt.
            let epoch = i64::try_from(request.ticket.epoch).map_err(|_| NativeError::Changed)?;
            studio_runtime_store::SchedulerRepository::settle_started(
                tx,
                &request.ticket.agent,
                epoch,
                &request.ticket.operation,
                &turn,
            )
            .map_err(|_| NativeError::Changed)?;
        }
        Ok(())
    }
}

fn admission_error(error: crate::coordination::CoordinationError) -> NativeError {
    use crate::coordination::CoordinationError;
    match error {
        CoordinationError::RequiresDependency(name) => NativeError::Unavailable(name),
        CoordinationError::AdmissionDenied(_) => NativeError::Invalid("budget"),
        _ => NativeError::Invalid("admission"),
    }
}
