//! Supervisor health responses and handshake acknowledgement wire contracts.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorHandshakeV2Result {
    #[serde(default)]
    pub ok: bool,
    pub protocol: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct HealthResult {
    pub protocol: u64,
    #[serde(rename = "stateDir")]
    pub state_dir: String,
    pub handles: Vec<HealthHandle>,
    #[serde(rename = "journalLimitBytes")]
    pub journal_limit_bytes: u64,
    #[serde(rename = "outputLimitBytesPerHandle")]
    pub output_limit_bytes_per_handle: u64,
    pub durability: Durability,
    pub recovery: RecoveryStatus,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthHandle {
    pub id: String,
    pub pid: Option<u32>,
    pub signature: String,
    #[serde(rename = "startTime")]
    pub start_time: Option<String>,
    #[serde(rename = "jobIdentity")]
    pub job_identity: Option<String>,
    pub sequence: u64,
    pub acknowledged: u64,
    #[serde(rename = "bufferedBytes")]
    pub buffered_bytes: u64,
    pub backpressure: bool,
    #[serde(rename = "stdoutReaderAlive")]
    pub stdout_reader_alive: bool,
    #[serde(rename = "stdoutReaderError")]
    pub stdout_reader_error: Option<String>,
    #[serde(rename = "persistenceErrors")]
    pub persistence_errors: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Durability {
    SqliteFullSyncPerRequest,
}
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryStatus {
    pub degraded: bool,
    #[serde(rename = "fallbackReady")]
    pub fallback_ready: bool,
    pub blocked: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
