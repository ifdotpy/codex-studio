//! Provider stdio timing journal records. Timestamps use Unix nanoseconds.
use crate::provider_input::RequestId;
use serde::Serialize;

/// Prefix preceding the JSON record of a physical provider stdin flush.
pub const STDIN_TIMING_DIAGNOSTIC_PREFIX: &str = "studio_provider_stdin_write ";
/// Prefix preceding the JSON record of a reply to a witnessed request.
pub const REPLY_TIMING_DIAGNOSTIC_PREFIX: &str = "studio_provider_reply ";

#[derive(Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StdinTimingDiagnostic<'a> {
    pub provider_handle: &'a str,
    pub generation: u64,
    pub operation_id: &'a str,
    pub request_id: RequestId,
    pub method: &'static str,
    pub stdin_flushed_at_ns: u128,
}

#[derive(Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplyTimingDiagnostic<'a> {
    pub provider_handle: &'a str,
    pub generation: u64,
    pub operation_id: &'a str,
    pub request_id: &'a RequestId,
    pub method: &'static str,
    pub reply_at_ns: u128,
    /// `result` or `error`.
    pub outcome: &'static str,
}
