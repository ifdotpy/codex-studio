use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_providers::{Frame, RequestId, ResponseBody};
use studio_runtime_schema::provider_errors::is_misalignment_policy_violation;
use studio_runtime_store::{StoreError, system_reads};
use thiserror::Error;

pub(super) const MAX_BYTES: usize = 1_000_000;
pub(super) const MAX_PENDING: usize = 128;
#[derive(Debug, Error)]
pub enum CallbackError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    InvalidResultSchema(String),
    #[error("Not implemented: requires {0}")]
    Dependency(&'static str),
    #[error("Callback identity or session changed")]
    Stale,
    #[error("Request identity already has different content")]
    Conflict,
    #[error("Callback delivery is uncertain; inspect the saved receipt, do not resend")]
    Unknown,
    #[error("Invalid stored callback at {0}")]
    Stored(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}
impl From<rusqlite::Error> for CallbackError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}
impl From<serde_json::Error> for CallbackError {
    fn from(_: serde_json::Error) -> Self {
        Self::Invalid("Invalid callback object")
    }
}
impl From<studio_runtime_store::outbox::OutboxError> for CallbackError {
    fn from(error: studio_runtime_store::outbox::OutboxError) -> Self {
        use studio_runtime_store::outbox::OutboxError;
        match error {
            OutboxError::Sql(error) => error.into(),
            OutboxError::Json(_) => Self::Stored("callback outbox payload".into()),
            OutboxError::Conflict(_) => Self::Conflict,
        }
    }
}
impl From<studio_runtime_store::request_results::RequestResultError> for CallbackError {
    fn from(error: studio_runtime_store::request_results::RequestResultError) -> Self {
        use studio_runtime_store::request_results::RequestResultError;
        match error {
            RequestResultError::Sql(error) => error.into(),
            RequestResultError::Json(_) => Self::Stored("callback result payload".into()),
            RequestResultError::Conflict { .. } => Self::Conflict,
            RequestResultError::Identity(_) => Self::Unknown,
        }
    }
}

/// Transport-owned values, never accepted from tool arguments. The account
/// worker must compare generation/connection to its live session before calling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackContext {
    pub agent: String,
    pub account: String,
    pub stream_id: String,
    pub connection: String,
    pub generation: u64,
    pub epoch: u64,
    pub thread: String,
    pub turn: String,
}
#[derive(Clone, Debug)]
pub struct CallbackSnapshot {
    actor: Value,
    context: CallbackContext,
}
impl CallbackSnapshot {
    /// The only snapshot constructor: validates raw Python-written records and
    /// binds them to the authenticated account worker, including active attempt.
    pub fn from_record(raw: &str, context: &CallbackContext) -> Result<Self, CallbackError> {
        let actor = system_reads::decode("runtime_agents", &context.agent, raw)?;
        if actor["epoch"].as_u64() != Some(context.epoch)
            || actor
                .get("accountKey")
                .map_or(Some("default"), Value::as_str)
                != Some(context.account.as_str())
            || actor["threadId"] != context.thread
            || actor["turnId"] != context.turn
            || actor
                .get("turnEpoch")
                .is_some_and(|epoch| epoch.as_u64() != Some(context.epoch))
            || context.connection.is_empty()
            || context.thread.is_empty()
            || context.turn.is_empty()
            || actor
                .pointer("/startAttempt/connectionGeneration")
                .and_then(Value::as_u64)
                .is_some_and(|generation| generation != context.generation)
            || actor
                .pointer("/startAttempt/connectionId")
                .and_then(Value::as_str)
                .is_some_and(|connection| connection != context.connection)
        {
            return Err(CallbackError::Stale);
        }
        Ok(Self {
            actor,
            context: context.clone(),
        })
    }
    pub(super) fn actor(&self) -> &Value {
        &self.actor
    }
    pub(super) fn context(&self) -> &CallbackContext {
        &self.context
    }
    pub(crate) fn active(&self) -> Result<(), CallbackError> {
        if self.actor["autoWake"] != true
            || studio_runtime_store::truthy(self.actor.get("deletedAt"))
        {
            return Err(CallbackError::Invalid("Agent is stopped or unknown"));
        }
        if self
            .actor
            .pointer("/nativeThreadBlock/threadId")
            .is_some_and(|id| id == &self.context.thread)
            || self
                .actor
                .get("error")
                .is_some_and(is_misalignment_policy_violation)
            || (self
                .actor
                .pointer("/nativeTurnError/turnId")
                .is_some_and(|id| id == &self.context.turn)
                && self
                    .actor
                    .pointer("/nativeTurnError/error")
                    .is_some_and(is_misalignment_policy_violation))
        {
            return Err(CallbackError::Invalid(
                "The native thread is blocked; inspect the agent before continuing",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyIdentity {
    pub context: CallbackContext,
    pub rpc_id: RequestId,
    /// Opaque prepared-body attempt; never reused after a proven NotSent.
    #[serde(default)]
    pub attempt: String,
}
impl ReplyIdentity {
    pub(super) fn key_context_matches(&self, context: &CallbackContext) -> bool {
        let saved = &self.context;
        saved.account == context.account
            && saved.stream_id == context.stream_id
            && saved.generation == context.generation
            && saved.agent == context.agent
            && saved.epoch == context.epoch
            && saved.thread == context.thread
            && saved.turn == context.turn
    }
    pub(super) fn new(context: &CallbackContext, rpc_id: &RequestId) -> Self {
        Self {
            context: context.clone(),
            rpc_id: rpc_id.clone(),
            attempt: String::new(),
        }
    }
    /// Retained transport identity; includes the attempt so late NotSent receipts
    /// cannot settle a replacement reply with the same provider RPC id.
    pub fn operation_id(&self) -> String {
        format!("{}:{}", self.key(), self.attempt)
    }
    pub(super) fn key(&self) -> String {
        format!(
            "provider-callback:{}",
            studio_runtime_store::sha256_hex(
                json!([
                    self.context.account,
                    self.context.generation,
                    self.context.stream_id,
                    self.context.agent,
                    self.context.epoch,
                    self.context.thread,
                    self.context.turn,
                    self.rpc_id
                ])
                .to_string()
                .as_bytes()
            )
        )
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub identity: ReplyIdentity,
    pub body: ResponseBody,
}
impl Reply {
    pub(super) fn new(
        mut identity: ReplyIdentity,
        body: ResponseBody,
    ) -> Result<Self, CallbackError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|_| CallbackError::Invalid("Reply entropy unavailable"))?;
        identity.attempt = uuid::Uuid::from_bytes(bytes).to_string();
        Ok(Self { identity, body })
    }
    pub fn frame(&self) -> Frame {
        Frame::Response {
            id: self.identity.rpc_id.clone(),
            body: self.body.clone(),
            fields: Default::default(),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum CallbackDecision {
    Answer(Reply),
    Pending(String),
    /// Durable delivery or resolution proof: acknowledge, never rebuild a reply.
    Settled(ReplyIdentity),
    Unsupported,
}
#[derive(Clone, Debug)]
pub(super) struct Input {
    pub id: RequestId,
    pub method: String,
    pub params: Value,
}
impl Input {
    pub fn from_frame(frame: &Frame) -> Result<Option<Self>, CallbackError> {
        let (id, method, params) = match frame {
            Frame::Request(request) => (
                request.id.clone(),
                request.method.as_str().to_owned(),
                request.params.clone(),
            ),
            Frame::UnknownMethod(request) if request.fields.contains_key("id") => (
                serde_json::from_value(request.fields["id"].clone())?,
                request.method.clone(),
                request.fields.get("params").cloned(),
            ),
            Frame::Notification(_) | Frame::UnknownMethod(_) | Frame::Response { .. } => {
                return Ok(None);
            }
        };
        let params = params.unwrap_or(json!({}));
        if !params.is_object() || params.to_string().len() > MAX_BYTES {
            return Err(CallbackError::Invalid(
                "Invalid or oversized callback params",
            ));
        }
        Ok(Some(Self { id, method, params }))
    }
    pub fn signature(&self) -> String {
        studio_runtime_store::sha256_hex(json!([self.method, self.params]).to_string().as_bytes())
    }
    pub fn check_context(&self, context: &CallbackContext) -> Result<(), CallbackError> {
        if self
            .params
            .get("threadId")
            .is_some_and(|v| v != &context.thread)
            || self
                .params
                .get("turnId")
                .is_some_and(|v| !v.is_null() && v != &context.turn)
        {
            return Err(CallbackError::Stale);
        }
        Ok(())
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;

    #[test]
    fn callback_origin_survives_reattach_and_fences_generation_and_agent() {
        let mut context = CallbackContext {
            agent: "agent".into(),
            account: "account".into(),
            stream_id: "account".into(),
            connection: "first-attachment".into(),
            generation: 7,
            epoch: 2,
            thread: "thread".into(),
            turn: "turn".into(),
        };
        let rpc = RequestId::String("opaque-rpc".into());
        let origin = ReplyIdentity::new(&context, &rpc).key();
        context.connection = "second-attachment".into();
        assert_eq!(origin, ReplyIdentity::new(&context, &rpc).key());
        context.generation += 1;
        assert_ne!(origin, ReplyIdentity::new(&context, &rpc).key());
        context.generation -= 1;
        context.agent = "another-agent".into();
        assert_ne!(origin, ReplyIdentity::new(&context, &rpc).key());
    }
}
