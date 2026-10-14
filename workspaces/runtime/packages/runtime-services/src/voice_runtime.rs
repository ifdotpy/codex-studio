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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceWriteCertainty {
    Written,
    NotSent,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoiceRequestReceipt {
    pub certainty: VoiceWriteCertainty,
}

/// Port to one retained provider connection. `prepare` resolves and validates
/// the current account connection; `request` must preserve the supplied
/// identity and absolute deadline, and report uncertainty without resending.
pub trait VoiceRequestPort: Send + Sync {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
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
}
