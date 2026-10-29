//! Persisted process-action ownership and its replay result.
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use studio_runtime_schema::process_controls::{ProcessActionResult, ProcessEntry};

use super::request_results::AdmittedRequest;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessActionPhase {
    Dispatching,
    Uncertain,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessActionKind {
    StopSelected,
    StopOwned,
    StopResponse,
    BackgroundCommand,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessActionRecord {
    pub request_id: String,
    pub root_request_id: String,
    pub admitted_request: AdmittedRequest,
    pub signature: String,
    pub phase: ProcessActionPhase,
    pub scope_agent: String,
    pub scope_account_key: String,
    pub scope_epoch: Number,
    pub action: ProcessActionKind,
    /// Retain the wire identity's numeric spelling for exact owner comparison.
    pub response_target: Value,
    pub processes: Vec<ProcessEntry>,
    pub created: f64,
    pub result: ProcessActionResult,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_action_record_round_trips_the_persisted_shape() {
        let raw = r#"{"requestId":"request","rootRequestId":"root","admittedRequest":{"id":"token","signature":"signature","created":1},"signature":"signature","phase":"dispatching","scopeAgent":"agent","scopeAccountKey":"account","scopeEpoch":1.0,"action":"stop_owned","responseTarget":null,"processes":[],"created":1.0,"result":{"requestId":"request","status":"uncertain","processes":[]}}"#;
        let record: ProcessActionRecord = serde_json::from_str(raw).expect("stored record");
        let encoded = serde_json::to_string(&record).expect("serialize record");
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).expect("encoded JSON"),
            serde_json::from_str::<Value>(raw).expect("original JSON")
        );
        assert_eq!(record.admitted_request.id(), "token");
    }
}
