use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_providers::{Frame, RequestId, ResponseBody};
use studio_runtime_store::{StoreError, system_reads};
use thiserror::Error;

pub(super) const MAX_BYTES: usize = 1_000_000;
pub(super) const MAX_PENDING: usize = 128;
#[derive(Debug, Error)]
pub enum CallbackError {
    #[error("{0}")]
    Invalid(&'static str),
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

/// Transport-owned values, never accepted from tool arguments. The account
/// worker must compare generation/connection to its live session before calling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackContext {
    pub agent: String,
    pub account: String,
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
            || actor["accountKey"].as_str().unwrap_or("default") != context.account
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
    pub(super) fn active(&self) -> Result<(), CallbackError> {
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
                .pointer("/error/codexErrorInfo")
                .is_some_and(|e| e == "misalignmentPolicyViolation")
            || (self
                .actor
                .pointer("/nativeTurnError/turnId")
                .is_some_and(|id| id == &self.context.turn)
                && self
                    .actor
                    .pointer("/nativeTurnError/error/codexErrorInfo")
                    .is_some_and(|e| e == "misalignmentPolicyViolation"))
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
}
impl ReplyIdentity {
    pub(super) fn new(context: &CallbackContext, rpc_id: &RequestId) -> Self {
        Self {
            context: context.clone(),
            rpc_id: rpc_id.clone(),
        }
    }
    pub(super) fn key(&self) -> String {
        format!(
            "provider-callback:{}",
            studio_runtime_store::sha256_hex(
                json!([
                    self.context.account,
                    self.context.generation,
                    self.context.connection,
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
    pub(super) fn new(identity: ReplyIdentity, body: ResponseBody) -> Self {
        Self { identity, body }
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
            _ => return Ok(None),
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
