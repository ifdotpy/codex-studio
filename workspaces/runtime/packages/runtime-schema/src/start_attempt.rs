//! Typed fields shared by stored execution and recovery attempts.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartAttemptView {
    pub id: Option<String>,
    pub epoch: Option<i64>,
    pub account_key: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub observed_turn_id: Option<String>,
    pub events: Option<Vec<String>>,
    pub submitted: Option<bool>,
    pub active_at_reservation: Option<bool>,
    pub created: Option<f64>,
    #[serde(default, deserialize_with = "start_attempt_action")]
    #[schemars(with = "StartAttemptAction")]
    pub action: Option<StartAttemptAction>,
    pub execution_outcome: Option<crate::persistence::ExecutionStatus>,
    pub request_ticket: Option<String>,
    pub connection_id: Option<String>,
    pub connection_generation: Option<u64>,
    pub native_operation_id: Option<String>,
    pub root_request_id: Option<String>,
    pub interrupt: Option<StartInterruptView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StartAttemptAction {
    Compact,
    Review,
    Capacity,
    Safety,
}

fn start_attempt_action<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<StartAttemptAction>, D::Error> {
    StartAttemptAction::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InterruptOutcome {
    Unknown,
    Unsent,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartInterruptView {
    pub request_ticket: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub generation: Option<u64>,
    pub outcome: Option<InterruptOutcome>,
}
