//! Wire owner of the one command path: `POST /api/commands`.
//!
//! Every write action of a person, an agent, or a peer server is one `CommandRequest`.
//! `Command` is internally tagged by `kind`; the OpenAPI schema `CommandRequest` mirrors it
//! and the web client consumes the generated TypeScript form.
//! Tracking: https://github.com/ifdotpy/layr-studio/issues/630

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::DeliveryMoment;

/// Maximum length of a client id. Unit: characters.
pub const COMMAND_CLIENT_ID_MAX_CHARS: usize = 128;
/// Largest response row payload. Larger rows are left to the stream. Unit: bytes.
pub const COMMAND_ROW_MAX_BYTES: usize = 262_144;
/// Maximum attachments of one message. Unit: attachments.
pub const COMMAND_SEND_MAX_ASSETS: usize = 8;

/// One command. `id` is a UUIDv7 made by the caller. `client` names the numbering client
/// (a device install, an agent, a peer link) and `number` is that client's command counter,
/// starting at 1. The server keeps the highest applied number for each client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandRequest {
    pub id: String,
    pub client: String,
    pub number: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    /// Create a user message. The command id is the message id.
    Send { target: String, body: SendBody },
    /// Delete a queued message and return its text. `target` is the chat; `body.message`
    /// is the message id.
    Withdraw { target: String, body: WithdrawBody },
    /// Stop the turn of a chat. `target` is the chat.
    Stop { target: String, body: StopBody },
}

impl Command {
    /// The wire name of the command, also the operation part of its request identity.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Send { .. } => "send",
            Self::Withdraw { .. } => "withdraw",
            Self::Stop { .. } => "stop",
        }
    }

    /// The chat that the command acts on.
    pub fn target(&self) -> &str {
        match self {
            Self::Send { target, .. }
            | Self::Withdraw { target, .. }
            | Self::Stop { target, .. } => target,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendBody {
    pub text: String,
    #[serde(default)]
    pub assets: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::delivery::compatibility::option"
    )]
    #[schemars(with = "Option<String>")]
    pub delivery: Option<DeliveryMoment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WithdrawBody {
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopBody {
    #[serde(default = "default_true")]
    pub descendants: bool,
}

fn default_true() -> bool {
    true
}

/// A rejected command is an HTTP error. Only an applied command has a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Applied,
}

/// The one response form, on the first run and on a replay. `ref_*` and `rev` are exact.
/// `rows` are read when the response is built and can be newer than after the first run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommandResponse {
    pub status: CommandStatus,
    pub ref_kind: String,
    pub ref_id: String,
    pub rev: i64,
    /// Highest `seq` among the rows of the entities that the command changed. A client that
    /// holds a row with `seq` at or above this value needs no pending layer.
    pub seq: i64,
    pub rows: Vec<CommandRow>,
}

/// One entity row with its stream `seq`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommandRow {
    pub collection: String,
    pub id: String,
    pub seq: i64,
    pub payload: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_flat_and_tagged_by_kind() {
        let wire = serde_json::json!({
            "id": "0192f3a0-0000-7000-8000-000000000001",
            "client": "web-1",
            "number": 3,
            "kind": "withdraw",
            "target": "chat",
            "body": {"message": "m"}
        });
        let request: CommandRequest = serde_json::from_value(wire.clone()).expect("parse");
        assert_eq!(request.command.name(), "withdraw");
        assert_eq!(request.command.target(), "chat");
        assert_eq!(serde_json::to_value(&request).expect("encode"), wire);
    }

    #[test]
    fn unknown_kind_and_unknown_body_field_are_refused() {
        let base = serde_json::json!({"id":"i","client":"c","number":1,"target":"t"});
        let mut unknown_kind = base.clone();
        unknown_kind["kind"] = "delete_everything".into();
        unknown_kind["body"] = serde_json::json!({});
        assert!(serde_json::from_value::<CommandRequest>(unknown_kind).is_err());
        let mut extra = base;
        extra["kind"] = "stop".into();
        extra["body"] = serde_json::json!({"extra": true});
        assert!(serde_json::from_value::<CommandRequest>(extra).is_err());
    }

    #[test]
    fn stop_descendants_default_to_true() {
        let request: CommandRequest = serde_json::from_value(serde_json::json!(
            {"id":"i","client":"c","number":1,"kind":"stop","target":"t","body":{}}
        ))
        .expect("parse");
        assert_eq!(
            request.command,
            Command::Stop {
                target: "t".into(),
                body: StopBody { descendants: true }
            }
        );
    }
}
