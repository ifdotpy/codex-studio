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

pub(crate) fn schemas() -> [(&'static str, serde_json::Value); 1] {
    let mut generator = schemars::generate::SchemaSettings::openapi3()
        .for_serialize()
        .into_generator();
    [(
        "DiskSpaceStatus",
        DiskSpaceStatus::json_schema(&mut generator).to_value(),
    )]
}
