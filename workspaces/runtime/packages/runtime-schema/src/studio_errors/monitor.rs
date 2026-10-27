//! Safe monitor evidence. Native transport extensions and command text stay with their private owners.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(rename = "MonitorState")]
pub enum MonitorStatus {
    Approval,
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

impl MonitorStatus {
    pub const ALL: &'static [Self] = &[
        Self::Approval,
        Self::Starting,
        Self::Running,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
        Self::Lost,
    ];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
        }
    }
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Lost
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MonitorRecoveryStatus {
    Running,
    NotReattachable,
}

/// Studio-owned recovery proof codes. A retained native reason outside this set remains unclassified.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MonitorRecoveryReason {
    NoOperation,
    InvalidProviderObservation,
    ProviderRejected,
    OperationNotAccepted,
    InvalidTerminalReceipt,
    TerminationUnconfirmed,
    RetainedExitUnavailable,
    ReattachWindowExpired,
}

impl MonitorRecoveryReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::NoOperation => "no_operation",
            Self::InvalidProviderObservation => "invalid_provider_observation",
            Self::ProviderRejected => "provider_rejected",
            Self::OperationNotAccepted => "operation_not_accepted",
            Self::InvalidTerminalReceipt => "invalid_terminal_receipt",
            Self::TerminationUnconfirmed => "termination_unconfirmed",
            Self::RetainedExitUnavailable => "retained_exit_unavailable",
            Self::ReattachWindowExpired => "reattach_window_expired",
        }
    }
}

/// Unclassified text stays private in the retained record; its digest permits exact correlation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "classification", rename_all = "snake_case", deny_unknown_fields)]
pub enum MonitorRecoveryProof {
    Recognized { reason: MonitorRecoveryReason },
    Unclassified { sha256: String },
}

/// Public projection of the native operation identity, without open transport evidence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MonitorOperationIdentity {
    pub agent: String,
    pub epoch: i64,
    pub account_key: String,
    pub connection_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MonitorRecoveryEvidence {
    pub status: MonitorRecoveryStatus,
    /// Unix seconds.
    pub started_at: f64,
    pub proof: Option<MonitorRecoveryProof>,
}
