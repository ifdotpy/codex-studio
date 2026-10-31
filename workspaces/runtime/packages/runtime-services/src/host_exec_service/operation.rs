//! Persisted host command status.
use super::HostExecError;
use serde::{Deserialize, Serialize};
use studio_runtime_schema::queue_command::Field;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Leased,
    Running,
    Exited,
    Released,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Op {
    pub(crate) operation_id: String,
    pub(crate) project_id: String,
    pub(crate) agent_id: String,
    pub(crate) line_path: String,
    pub(crate) slot_id: String,
    pub(crate) state: State,
    pub(crate) created_at: f64,
    pub(crate) slot_path: String,
    pub(crate) derived_data: String,
    pub(crate) package_cache: String,
    pub(crate) artifacts_path: String,
    pub(crate) disk_budget: u64,
    pub(crate) mac_only: Vec<String>,
    pub(crate) features: Vec<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub(crate) generation: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub(crate) generation_number: Field<u64>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub(crate) exit_code: Field<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pre_run_error: Option<HostExecError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) abandoned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) collected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) started_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exited_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) caches: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) layr_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) layr_tree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cancel_requested: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) orphaned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) changed_paths: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) artifact_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name_conflicts: Option<Vec<Vec<String>>>,
}
