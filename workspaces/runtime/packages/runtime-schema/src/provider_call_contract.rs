//! Exact retained-provider payload and fingerprint contract shared by runtime and migration.
use crate::provider_input::RequestId;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConnectionScope {
    pub account: String,
    pub connection: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestActor {
    pub agent: String,
    pub epoch: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransferAuthorization {
    pub transfer_id: String,
    pub member: String,
    pub source_account: String,
    pub thread_id: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RequestResult {
    Reply(Value),
    Rejected(Value),
    NotSent,
    Unknown,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ProviderCallIntent<M, A, S = ConnectionScope> {
    pub root_request_id: String,
    pub admitted_request: Option<A>,
    pub scope: S,
    pub operation: String,
    pub actor: Option<RequestActor>,
    pub transfer: Option<TransferAuthorization>,
    pub deleted_actor_session: Option<String>,
    pub method: M,
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TransportRequest<M> {
    pub id: RequestId,
    pub generation: u64,
    pub method: M,
    pub operation: String,
    pub thread: Option<String>,
    pub lightweight: bool,
    pub deadline: u64,
    pub unknown: bool,
    #[serde(rename = "remoteId")]
    pub remote_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetainedOutcome {
    pub result: RequestResult,
    pub cursor: u64,
    /// Exact committed journal evidence, retained even when the domain owner is malformed.
    #[serde(default)]
    pub committed_ack: Option<CommittedAck>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetainedCall<M, A, S = ConnectionScope> {
    pub call: ProviderCallIntent<M, A, S>,
    pub request: Option<TransportRequest<M>>,
    pub admitted: A,
    pub written: bool,
    pub outcome: Option<RetainedOutcome>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommittedAck {
    pub stream_id: String,
    pub generation: u64,
    pub cursor: u64,
}

impl CommittedAck {
    pub fn matches(&self, stream: &str, generation: u64, cursor: u64) -> bool {
        self.stream_id == stream && self.generation == generation && self.cursor == cursor
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RetainedReply<M, A, S = ConnectionScope> {
    pub call: ProviderCallIntent<M, A, S>,
    pub request: TransportRequest<M>,
    pub written: bool,
    pub result: RequestResult,
    pub ack: CommittedAck,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ThreadProvenance<S = ConnectionScope> {
    pub scope: S,
    pub actor: RequestActor,
    pub operation: String,
}

impl<M, A, S> ProviderCallIntent<M, A, S> {
    pub fn fingerprint(&self, account: &str, method: &str) -> String {
        let value = json!((
            account,
            &self.operation,
            &self.actor,
            &self.transfer,
            &self.deleted_actor_session,
            method,
            &self.params
        ));
        data_encoding::HEXLOWER.encode(&Sha256::digest(
            crate::json_compat::python_json(&value).as_bytes(),
        ))
    }
    pub fn effect_discriminator(&self, account: &str) -> [String; 4] {
        [
            account.into(),
            self.actor
                .as_ref()
                .map(|actor| actor.agent.clone())
                .unwrap_or_default(),
            self.actor
                .as_ref()
                .map(|actor| actor.epoch.to_string())
                .unwrap_or_default(),
            self.operation.clone(),
        ]
    }
}
pub fn transport_key(account: &str, operation: &str) -> String {
    format!(
        "retained:{account}:{}",
        data_encoding::HEXLOWER.encode(&Sha256::digest(operation.as_bytes()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprint_and_transport_key_bind_the_original_call_content() {
        let call: ProviderCallIntent<(), (), ()> = ProviderCallIntent {
            root_request_id: String::new(),
            admitted_request: None,
            scope: (),
            operation: "operation".into(),
            actor: Some(RequestActor {
                agent: "agent".into(),
                epoch: 7,
            }),
            transfer: None,
            deleted_actor_session: None,
            method: (),
            params: json!({"text":"é","n":1}),
        };
        assert_eq!(
            call.fingerprint("account", "thread/start"),
            "dd5b140961cfc9a96dbd06b5b15c9670999e6d8643ca8c1212d88c5a98e95f84"
        );
        assert_ne!(
            call.fingerprint("account", "thread/start"),
            call.fingerprint("other", "thread/start")
        );
        assert_eq!(
            call.effect_discriminator("account"),
            ["account", "agent", "7", "operation"].map(str::to_owned)
        );
        assert_ne!(
            transport_key("account", "operation"),
            transport_key("account", "other")
        );
    }
}
