//! Closed stored project records used by project and radio admission.
use crate::persistence::{AdmittedRequest, ProjectRemovalReason, WorkEffectStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAccountPreference {
    pub account_key: Option<String>,
    #[serde(default)]
    pub account_groups: Vec<String>,
    #[serde(default)]
    pub account_keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "recordKind", rename_all = "lowercase")]
pub enum ProjectRecord {
    Live(Box<ProjectLive>),
    Removal(ProjectTombstone),
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectLive {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub peer_teams: Vec<ProjectPeerTeam>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ProjectLocation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_aliases: Vec<ProjectAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_groups_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_server_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_teams_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<ProjectFolder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_base_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_environment_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_policy_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub location_intents: BTreeMap<String, ProjectLocationIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<ProjectRegistration>,
}
impl ProjectLive {
    pub fn preference(&self) -> ProjectAccountPreference {
        ProjectAccountPreference {
            account_key: self.account_key.clone(),
            account_groups: self.account_groups.clone(),
            account_keys: self.account_keys.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectTombstone {
    pub id: String,
    pub removal: ProjectRemoval,
    #[serde(default)]
    pub peer_teams: Vec<ProjectPeerTeam>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectRemoval {
    pub reason: ProjectRemovalReason,
    pub sequence: i64,
    pub request_id: String,
    pub deleted_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectLocation {
    pub server_id: String,
    pub path: String,
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectAlias {
    pub server_id: String,
    pub project_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectPeerTeam {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectFolder {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectLocationIntent {
    pub request: AdmittedRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectRegistration {
    pub server: String,
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<WorkEffectStatus>,
}

pub(crate) fn schema() -> serde_json::Value {
    schemars::schema_for!(ProjectRecord).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: serde_json::Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-project-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err(
            "persisted project schema must be generated from project_record::ProjectRecord".into(),
        );
    }
    Ok(())
}
