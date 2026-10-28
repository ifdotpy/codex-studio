//! Desktop disk notices. Exact byte counts belong to the admission gate, not this wire status.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Native server implementation identifier emitted by the desktop metadata route.
pub const BACKEND_KIND: &str = "rust";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum DiskSpaceLevel {
    Warning,
    Critical,
    Full,
}

/// The visible disk notice returned by `/api/desktop`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiskSpaceStatus {
    pub level: Option<DiskSpaceLevel>,
    pub message: Option<String>,
}

/// The build stamp in Contents/Resources/studio-build.json (docs/peer-updates.md).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PeerUpdateBuild {
    /// Format version of the stamp. Always 1.
    #[schemars(range(min = 1, max = 1))]
    pub version: u8,
    /// Packaging time on the build Mac, in seconds since the epoch.
    pub built_at: f64,
    pub commit: String,
    /// Commit time, in seconds since the epoch.
    pub commit_time: u64,
    pub branch: String,
}

/// The last pull or install attempt of the update puller.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeerUpdateEvent {
    /// Time of the pull or install attempt, in seconds since the epoch.
    pub at: Option<f64>,
    /// The paired server that offered the build.
    pub server: Option<String>,
    pub build: Option<PeerUpdateBuild>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
}

/// The staged update that the desktop installer takes next.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeerUpdatePending {
    pub build: PeerUpdateBuild,
    pub server: String,
}

/// The update state from a paired server, as `/api/desktop` shows it (`peerUpdate`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PeerUpdateStatus {
    /// The build of the installed app; null for a source checkout.
    pub build: Option<PeerUpdateBuild>,
    /// Time of the last check, in seconds since the epoch.
    pub checked_at: Option<f64>,
    pub last_pull: Option<PeerUpdateEvent>,
    pub pending: Option<PeerUpdatePending>,
    pub last_install: Option<PeerUpdateEvent>,
}

pub(crate) fn schemas() -> [(&'static str, serde_json::Value); 5] {
    let mut generator = schemars::generate::SchemaSettings::openapi3()
        .for_serialize()
        .into_generator();
    [
        (
            "DiskSpaceStatus",
            DiskSpaceStatus::json_schema(&mut generator).to_value(),
        ),
        (
            "PeerUpdateStatus",
            PeerUpdateStatus::json_schema(&mut generator).to_value(),
        ),
        (
            "PeerUpdateBuild",
            PeerUpdateBuild::json_schema(&mut generator).to_value(),
        ),
        (
            "PeerUpdateEvent",
            PeerUpdateEvent::json_schema(&mut generator).to_value(),
        ),
        (
            "PeerUpdatePending",
            PeerUpdatePending::json_schema(&mut generator).to_value(),
        ),
    ]
}
