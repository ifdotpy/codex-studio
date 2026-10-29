use crate::request_results::AdmittedRequest;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicRequest {
    pub public_request_id: String,
    pub public_admission: AdmittedRequest,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Submission {
    pub version: i64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Acceptance {
    pub result_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Owner<I> {
    pub request_id: String,
    pub owner: String,
    pub revision: String,
    #[serde(flatten)]
    pub id: I,
    #[serde(flatten)]
    pub public: Option<PublicRequest>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Evidence {
    pub layr_project_id: String,
    pub layr_line: String,
    pub layr_state_id: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubmissionResult {
    pub evidence: Evidence,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Intent<I, R> {
    Pending(Owner<I>),
    NotSent(Owner<I>),
    Unknown(Owner<I>),
    Completed {
        #[serde(flatten)]
        owner: Owner<I>,
        #[serde(flatten)]
        result: R,
    },
    UnknownUnattributed(PublicRequest),
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_submit: Option<Intent<Submission, SubmissionResult>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_accept: Option<Intent<Acceptance, ()>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_completion_round_trips_without_result_field() {
        let stored = r#"{"status":"completed","requestId":"rpc","owner":"a","revision":"rev","resultId":"r"}"#;
        let intent: Intent<Acceptance, ()> =
            serde_json::from_str(stored).expect("stored completion");
        assert_eq!(serde_json::to_string(&intent).expect("serialized"), stored);
    }
}
