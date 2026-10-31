//! Persisted request envelope; method-specific payloads remain open JSON.
use crate::{persistence::RequestCardStatus, provider_input::RequestId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

/// Named envelope fields, also used to reject unknown legacy migration keys.
pub const FIELDS: &[&str] = &[
    "id",
    "agent",
    "method",
    "status",
    "createdAt",
    "params",
    "connectionId",
    "accountKey",
    "epoch",
    "turnId",
    "rpcId",
    "supervisor",
    "preview",
    "answerDecision",
    "answerError",
    "answerSignature",
    "answerHistory",
    "answeredAt",
    "answeredBy",
    "decision",
    "deferred",
    "deferredAt",
    "deferredBy",
    "deletedAt",
    "deletedBy",
    "resolvedAt",
    "closeReason",
];
pub const REQUIRED_FIELDS: &[&str] = &["id", "agent", "method", "status", "createdAt", "params"];

/// Methods that create durable request cards.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum RequestMethod {
    #[serde(rename = "agent/asyncQuestion")]
    AsyncQuestion,
    #[serde(rename = "browser/open")]
    BrowserOpen,
    #[serde(rename = "monitor/approve")]
    MonitorApprove,
    #[serde(rename = "item/commandExecution/requestApproval")]
    CommandApproval,
    #[serde(rename = "item/fileChange/requestApproval")]
    FileApproval,
    #[serde(rename = "item/permissions/requestApproval")]
    PermissionsApproval,
    #[serde(rename = "item/tool/requestUserInput")]
    UserInput,
    #[serde(rename = "mcpServer/elicitation/request")]
    Elicitation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RequestDecision {
    Accept,
    Decline,
    Cancel,
    Answer,
}
impl RequestDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
            Self::Answer => "answer",
        }
    }
}
impl std::str::FromStr for RequestDecision {
    type Err = serde::de::value::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::deserialize(serde::de::value::StrDeserializer::new(value))
    }
}

impl RequestMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AsyncQuestion => "agent/asyncQuestion",
            Self::BrowserOpen => "browser/open",
            Self::MonitorApprove => "monitor/approve",
            Self::CommandApproval => "item/commandExecution/requestApproval",
            Self::FileApproval => "item/fileChange/requestApproval",
            Self::PermissionsApproval => "item/permissions/requestApproval",
            Self::UserInput => "item/tool/requestUserInput",
            Self::Elicitation => "mcpServer/elicitation/request",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(rename = "PersistedRequestRecord")]
pub struct RequestRecord {
    pub id: String,
    pub agent: String,
    pub method: RequestMethod,
    pub status: RequestCardStatus,
    pub created_at: f64,
    #[serde(deserialize_with = "request_params")]
    #[schemars(with = "std::collections::BTreeMap<String, Value>")]
    pub params: Box<RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<RequestSupervisor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Value>")]
    pub preview: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_decision: Option<RequestDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_history: Option<Vec<RequestAnswer>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<RequestDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestSupervisor {
    pub generation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestAnswer {
    pub id: String,
    pub question: String,
    pub is_secret: bool,
    #[schemars(with = "Value")]
    pub answer: Box<RawValue>,
}

fn request_params<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Box<RawValue>, D::Error> {
    let raw = Box::<RawValue>::deserialize(deserializer)?;
    if !raw.get().trim_start().starts_with('{') {
        return Err(serde::de::Error::custom("request params must be an object"));
    }
    Ok(raw)
}

impl RequestRecord {
    pub fn id(&self) -> &String {
        &self.id
    }
    pub fn new(
        id: String,
        agent: String,
        method: RequestMethod,
        status: RequestCardStatus,
        created_at: f64,
        params: Box<RawValue>,
    ) -> Self {
        Self {
            id,
            agent,
            method,
            status,
            created_at,
            params,
            connection_id: None,
            account_key: None,
            epoch: None,
            turn_id: None,
            rpc_id: None,
            supervisor: None,
            preview: None,
            answer_decision: None,
            answer_error: None,
            answer_signature: None,
            answer_history: None,
            answered_at: None,
            answered_by: None,
            decision: None,
            deferred: None,
            deferred_at: None,
            deferred_by: None,
            deleted_at: None,
            deleted_by: None,
            resolved_at: None,
            close_reason: None,
        }
    }
}

pub(crate) fn schema() -> Value {
    schemars::schema_for!(RequestRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-request-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err(
            "persisted request schema must be generated from request_record::RequestRecord".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_request_rejects_sparse_rows_and_unknown_fields() {
        assert!(serde_json::from_str::<RequestRecord>(r#"{"id":"r"}"#).is_err());
        assert!(serde_json::from_str::<RequestRecord>(r#"{"id":"r","agent":"a","method":"agent/asyncQuestion","status":"pending","createdAt":1,"params":{},"unknown":1}"#).is_err());
        assert!(serde_json::from_str::<RequestRecord>(r#"{"id":"r","agent":"a","method":"agent/asyncQuestion","status":"pending","createdAt":1,"params":false}"#).is_err());
        assert!(serde_json::from_str::<RequestRecord>(r#"{"id":"r","agent":"a","method":"agent/asyncQuestion","status":"pending","createdAt":1,"params":{},"decision":"future"}"#).is_err());
    }

    #[test]
    fn request_preserves_opaque_payload_bytes() {
        let raw = r#"{"id":"r","agent":"a","method":"agent/asyncQuestion","status":"pending","createdAt":1,"params":{ "x": [1, 2] }}"#;
        let record: RequestRecord = serde_json::from_str(raw).unwrap();
        assert!(
            serde_json::to_string(&record)
                .unwrap()
                .contains(r#"{ "x": [1, 2] }"#)
        );
    }

    #[test]
    fn migration_keys_match_schema() {
        let properties = schema()["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let fields = FIELDS
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(properties, fields);
        let required = schema()["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = REQUIRED_FIELDS
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(required, expected);
        check_schema_parity().unwrap();
    }
}
