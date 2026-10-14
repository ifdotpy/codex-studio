//! Retained-session voice request boundary.
//!
//! Each request is identified and durably reserved by the voice service before
//! this port is called. A caller must never retry an Unknown receipt.

use std::{future::Future, pin::Pin, time::Instant};

use serde_json::Value;
pub use studio_runtime_providers::Method;

pub type VoiceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceConnectionScope {
    pub agent_id: String,
    pub epoch: i64,
    pub account_key: String,
    pub thread_id: String,
    pub connection_id: String,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceWriteCertainty {
    Written,
    NotSent,
    Unknown,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoiceRequestReceipt {
    pub certainty: VoiceWriteCertainty,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceRpcId {
    Number(i64),
    String(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceReply {
    pub rpc_id: VoiceRpcId,
    pub result: Value,
}

/// Port to one retained provider connection. `prepare` resolves the current
/// account connection using a session-scoped durable operation; `request` must
/// preserve the supplied identity and deadline and never resend an Unknown.
pub trait VoiceRequestPort: Send + Sync {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        session_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> VoiceFuture<'a, Result<VoiceConnectionScope, VoiceWriteCertainty>>;

    fn request<'a>(
        &'a self,
        scope: VoiceConnectionScope,
        operation_id: String,
        method: Method,
        params: Value,
        deadline: Instant,
    ) -> VoiceFuture<'a, VoiceRequestReceipt>;

    fn reply<'a>(
        &'a self,
        scope: VoiceConnectionScope,
        operation_id: String,
        reply: VoiceReply,
        deadline: Instant,
    ) -> VoiceFuture<'a, VoiceRequestReceipt>;
}
