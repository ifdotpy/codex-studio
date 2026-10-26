//! Operational work states and workspace modes. OpenAPI and its generated
//! consumers are rendered from these enums; persisted records reference them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Ready,
    Blocked,
    Running,
    Review,
    Accepted,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    Layr,
    Image,
    Worktree,
    Shared,
}

const WORK_STATUS_FIELDS: &[&str] = &[
    "/components/schemas/WorkItem/properties/status",
    "/components/schemas/WorkEntityDto/properties/status/anyOf/0",
    "/components/schemas/WorkViewRecordResponse/properties/status",
    "/components/schemas/WorkViewRecordResponse/properties/displayStatus/anyOf/1",
];
const WORKSPACE_MODE_FIELDS: &[&str] = &[
    "/components/schemas/AgentDetailEntityDto/properties/workspaceMode/anyOf/0",
    "/components/schemas/AgentEntityDto/properties/workspaceMode/anyOf/0",
    "/components/schemas/AgentRecordResponse/properties/workspaceMode/anyOf/0",
    "/components/schemas/AgentResponse/properties/workspaceMode/anyOf/0",
    "/components/schemas/BranchResponse/properties/workspaceMode/anyOf/0",
    "/components/schemas/CreateAgentRequest/properties/workspaceMode",
];
const LEAD_WORKSPACE_MODE: &str = "/components/schemas/CreateLeadRequest/properties/workspaceMode";

fn schema<T: JsonSchema>() -> Value {
    T::json_schema(&mut schemars::generate::SchemaSettings::openapi3().into_generator()).to_value()
}

pub(crate) fn refresh_openapi(openapi: &mut Value) -> Result<(), String> {
    openapi["components"]["schemas"]["WorkStatus"] = schema::<WorkStatus>();
    openapi["components"]["schemas"]["WorkspaceMode"] = schema::<WorkspaceMode>();
    for (paths, name) in [
        (WORK_STATUS_FIELDS, "WorkStatus"),
        (WORKSPACE_MODE_FIELDS, "WorkspaceMode"),
    ] {
        for path in paths {
            let field = openapi
                .pointer_mut(path)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| format!("OpenAPI is missing record field {path}"))?;
            field.remove("enum");
            field.remove("type");
            field.insert("$ref".into(), json!(format!("#/components/schemas/{name}")));
        }
    }
    // Lead creation intentionally permits the existing three-mode subset.
    // Generate it from the same owner rather than maintaining another list.
    let mut lead = schema::<WorkspaceMode>();
    let shared = serde_json::to_value(WorkspaceMode::Shared).map_err(|error| error.to_string())?;
    lead["enum"]
        .as_array_mut()
        .ok_or("WorkspaceMode must be an enum")?
        .retain(|mode| mode != &shared);
    let field = openapi
        .pointer_mut(LEAD_WORKSPACE_MODE)
        .and_then(Value::as_object_mut)
        .ok_or("OpenAPI is missing lead workspace mode")?;
    field.insert("enum".into(), lead["enum"].clone());
    Ok(())
}

pub(crate) fn check_schema_parity(openapi: &Value) -> Result<(), String> {
    let mut generated = openapi.clone();
    refresh_openapi(&mut generated)?;
    for path in WORK_STATUS_FIELDS
        .iter()
        .chain(WORKSPACE_MODE_FIELDS)
        .copied()
        .chain([
            LEAD_WORKSPACE_MODE,
            "/components/schemas/WorkStatus",
            "/components/schemas/WorkspaceMode",
        ])
    {
        if openapi.pointer(path) != generated.pointer(path) {
            return Err(format!(
                "{path} must be generated from runtime-schema/src/records.rs"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_work_boundary_record_domains_have_one_generated_owner() {
        let openapi = serde_json::from_str(include_str!("../../runtime-http/openapi.json"))
            .expect("OpenAPI document");
        check_schema_parity(&openapi)
            .expect("all record domain consumers are generated from this owner");
        for value in schema::<WorkStatus>()["enum"]
            .as_array()
            .expect("work states")
        {
            let parsed: WorkStatus =
                serde_json::from_value(value.clone()).expect("owner value parses");
            assert_eq!(json!(parsed), *value);
        }
        for value in schema::<WorkspaceMode>()["enum"]
            .as_array()
            .expect("workspace modes")
        {
            let parsed: WorkspaceMode =
                serde_json::from_value(value.clone()).expect("owner value parses");
            assert_eq!(json!(parsed), *value);
        }
        assert!(serde_json::from_value::<WorkStatus>(json!("unknown")).is_err());
        assert!(serde_json::from_value::<WorkspaceMode>(json!(7)).is_err());
    }
}
