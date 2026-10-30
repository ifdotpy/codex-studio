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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    pub id: String,
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    pub is_secret: bool,
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

/// A row. `fire_at` is the durable expiry timer of a pending approval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Approval {
    pub id: ApprovalId,
    pub agent: AgentId,
    /// None only for kinds in `RequestKind::TURN_OPTIONAL`: requests of the
    /// agent itself (not of a provider turn).
    pub turn: Option<TurnId>,
    pub created: Timestamp,
    pub request: Request,
    pub state: ApprovalState,
    pub fire_at: Option<Timestamp>,
    /// History stays. Hidden rows are not shown in the default list.
    pub hidden: bool,
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
