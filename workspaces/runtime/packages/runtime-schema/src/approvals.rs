//! The typed approval entity: one row per provider question or permission
//! request, a child of a turn. The Rust types own the shape. SQLite checks and
//! the TypeScript contract are generated from them.
//!
//! Times are Unix seconds (`Timestamp`). Every enum is internally tagged.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::VariantNames;

macro_rules! id_newtype {
    ($($(#[$meta:meta])* $name:ident),+) => {$(
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn as_str(&self) -> &str { &self.0 }
        }
    )+};
}
id_newtype!(
    /// Identity of one approval. Equals the legacy request id after migration.
    ApprovalId,
    /// Identity of the agent that owns the turn.
    AgentId,
    /// Provider turn identifier. A domain Turn table replaces this later.
    TurnId,
    /// Identity of the outbox command that delivers an answer.
    CommandId
);

/// Unix seconds.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Timestamp(pub f64);

/// Who acts. Local copy of the permissions work (`feat/principal-permissions`);
/// the second change to land unifies the two.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    Owner,
    Device {
        id: String,
        grant: Grant,
    },
    Peer {
        server: String,
    },
    Agent {
        id: String,
        epoch: u64,
    },
    Federated {
        peer: String,
    },
    /// Automatic answers, such as full access mode.
    Policy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    Full,
    Observe,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
    /// Markdown shown next to the option.
    pub preview: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    pub id: String,
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    pub is_secret: bool,
    /// Absent when the provider did not send the flag.
    pub multi_select: Option<bool>,
    pub is_other: Option<bool>,
}

/// Fixed at creation. Decides which answers are allowed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema, strum::EnumDiscriminants)]
#[strum_discriminants(
    name(RequestKind),
    derive(VariantNames, Serialize, Deserialize, JsonSchema)
)]
#[strum_discriminants(serde(rename_all = "snake_case"), strum(serialize_all = "snake_case"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Exec {
        command: String,
        cwd: String,
        reason: Option<String>,
    },
    Patch {
        item: String,
        files: Vec<String>,
        reason: Option<String>,
    },
    Permissions {
        requested: Vec<String>,
    },
    Elicitation {
        /// JSON schema chosen by the provider; shown as is.
        schema: serde_json::Value,
    },
    Question {
        questions: Vec<Question>,
    },
    /// Claude tool permission.
    ToolUse {
        tool: String,
        input: serde_json::Value,
    },
    /// Today `agent/asyncQuestion`.
    AgentQuestion {
        questions: Vec<Question>,
    },
    /// Today `monitor/approve`.
    MonitorStart {
        monitor: String,
        command: String,
    },
    /// Today `browser/open`.
    BrowserOpen {
        url: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QuestionAnswer {
    pub id: String,
    pub answer: Vec<String>,
}

/// The answer a person or policy gives. Its kind must fit the request kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    Decision {
        decision: Decision,
    },
    Answers {
        answers: Vec<QuestionAnswer>,
    },
    Elicitation {
        action: ElicitationAction,
        content: Option<serde_json::Value>,
    },
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CloseReason {
    TurnEnded,
    ProviderResolved,
    SessionLost,
    ChatDeleted,
    Expired,
    /// Migrated row whose answer was never recorded. Nothing is inferred.
    Legacy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema, strum::EnumDiscriminants)]
#[strum_discriminants(name(ApprovalStateKind), derive(VariantNames))]
#[strum_discriminants(strum(serialize_all = "snake_case"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    /// The answer is stored and its delivery command exists.
    Answering {
        answer: Answer,
        command: CommandId,
    },
    Answered {
        answer: Answer,
        by: Principal,
        at: Timestamp,
    },
    /// Delivery was lost. Nobody knows whether the provider got the answer.
    Unknown {
        answer: Answer,
        since: Timestamp,
    },
    Closed {
        reason: CloseReason,
        at: Timestamp,
    },
}

/// JSON-RPC id of a provider request. The reply must use the same form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RpcId {
    Number { value: i64 },
    Text { value: String },
}

/// The provider process generation that owns a request, under the supervisor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Supervisor {
    pub generation: u64,
    pub state_dir: Option<String>,
    pub handle: Option<String>,
}

/// What the server needs to send the answer back to the provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Delivery {
    pub rpc_id: RpcId,
    pub account: String,
    pub connection: String,
    /// Absent on rows written before supervisor generations were recorded.
    pub supervisor: Option<Supervisor>,
}

/// Provider-owned content that Studio stores and shows as is (tolerant at the
/// provider boundary). Typed request fields are removed from `params`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ProviderObject(pub serde_json::Map<String, serde_json::Value>);

/// Display content and provider fields that no logic reads by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Detail {
    /// Provider request params without the fields typed in `Request`.
    pub params: ProviderObject,
    /// Item body shown on the card (command run, file changes).
    pub preview: Option<ProviderObject>,
    /// Human text of a lost delivery.
    pub answer_error: Option<String>,
    /// Text the browser host gave when it closed a browser request.
    pub host_close_reason: Option<String>,
}

/// The user moved the question aside. `active` is false after it came back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Deferral {
    pub active: bool,
    pub at: Timestamp,
    pub by: Principal,
}

/// History stays: a hidden row is not shown in the default list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Hidden {
    pub at: Timestamp,
    pub by: Principal,
}

/// A row. `fire_at` is the durable expiry timer of a pending approval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Approval {
    pub id: ApprovalId,
    pub agent: AgentId,
    /// None only for kinds in `RequestKind::TURN_OPTIONAL`: requests of the
    /// agent itself (not of a provider turn).
    pub turn: Option<TurnId>,
    pub created: Timestamp,
    /// Agent epoch when the request was made. Absent on old rows.
    pub epoch: Option<u64>,
    pub request: Request,
    /// Present for provider requests that Studio answers over a connection.
    pub delivery: Option<Delivery>,
    pub detail: Detail,
    pub state: ApprovalState,
    pub deferral: Option<Deferral>,
    pub fire_at: Option<Timestamp>,
    pub hidden: Option<Hidden>,
}

/// Definitions of `Approval` and every type it names, for the OpenAPI document.
/// The sync entities `request` and `questionHistory` carry this shape.
pub fn schemas() -> Vec<(String, serde_json::Value)> {
    let mut generator = schemars::generate::SchemaSettings::draft2020_12()
        .for_serialize()
        .with(|settings| settings.definitions_path = "/components/schemas".into())
        .into_generator();
    generator.subschema_for::<Approval>();
    generator.take_definitions(true).into_iter().collect()
}

/// The committed OpenAPI document holds the Rust definitions, not a copy by hand.
pub(crate) fn check_schema_parity(openapi: &serde_json::Value) -> Result<(), String> {
    for (name, schema) in schemas() {
        if openapi.pointer(&format!("/components/schemas/{name}")) != Some(&schema) {
            return Err(format!("{name} must match runtime-schema/src/approvals.rs"));
        }
    }
    for payload in [
        "SyncRequestEntityPayload",
        "SyncQuestionHistoryEntityPayload",
    ] {
        let value = openapi.pointer(&format!(
            "/components/schemas/{payload}/properties/value/$ref"
        ));
        if value != Some(&serde_json::json!("#/components/schemas/Approval")) {
            return Err(format!("{payload} must carry the Approval schema"));
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AnswerMismatch {
    #[error("The answer does not fit the {0} request")]
    Kind(&'static str),
    #[error("The answer to question {0} does not match a question of this request")]
    UnknownQuestion(String),
    #[error("Every question needs exactly one answer")]
    Incomplete,
}

impl Request {
    /// An answer fits when its kind matches and its content refers to the request.
    pub fn accepts(&self, answer: &Answer) -> Result<(), AnswerMismatch> {
        match (self, answer) {
            (
                Self::Exec { .. }
                | Self::Patch { .. }
                | Self::Permissions { .. }
                | Self::ToolUse { .. }
                | Self::MonitorStart { .. }
                | Self::BrowserOpen { .. },
                Answer::Decision { .. },
            ) => Ok(()),
            (Self::Elicitation { .. }, Answer::Elicitation { .. }) => Ok(()),
            (
                Self::Question { questions } | Self::AgentQuestion { questions },
                Answer::Answers { answers },
            ) => {
                let mut seen = std::collections::BTreeSet::new();
                for given in answers {
                    if !questions.iter().any(|q| q.id == given.id) {
                        return Err(AnswerMismatch::UnknownQuestion(given.id.clone()));
                    }
                    if !seen.insert(given.id.as_str()) {
                        return Err(AnswerMismatch::Incomplete);
                    }
                }
                if seen.len() == questions.len() {
                    Ok(())
                } else {
                    Err(AnswerMismatch::Incomplete)
                }
            }
            _ => Err(AnswerMismatch::Kind(RequestKind::from(self).into())),
        }
    }
}

impl RequestKind {
    /// Kinds that may exist without a provider turn: agent questions, monitor
    /// starts and browser opens come from the agent, not from a turn.
    pub const TURN_OPTIONAL: [RequestKind; 3] = [
        RequestKind::AgentQuestion,
        RequestKind::MonitorStart,
        RequestKind::BrowserOpen,
    ];
    pub fn as_str(self) -> &'static str {
        RequestKind::VARIANTS[self as usize]
    }
}

impl From<RequestKind> for &'static str {
    fn from(kind: RequestKind) -> Self {
        RequestKind::VARIANTS[kind as usize]
    }
}
