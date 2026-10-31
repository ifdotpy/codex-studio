//! Complete checked storage shape for work items; operation evidence is opaque.
use crate::{
    persistence::WorkStatus, queue_command::Field, rule_record::RuleInteger, task_record::TaskId,
};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

/// Persisted work version, bounded to the JSON safe integer range.
pub type WorkItemVersion = RuleInteger<0, 9007199254740991>;

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn status<'de, D: serde::Deserializer<'de>>(d: D) -> Result<WorkStatus, D::Error> {
    let text = String::deserialize(d)?;
    text.parse()
        .map_err(|()| serde::de::Error::custom("invalid work status"))
}
fn status_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"$ref":"https://codex-studio.invalid/openapi.json#/components/schemas/WorkItem/properties/status"})
}
fn dependencies_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"$ref":"https://codex-studio.invalid/openapi.json#/components/schemas/WorkItem/properties/dependencies"})
}
fn updated_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"$ref":"https://codex-studio.invalid/openapi.json#/components/schemas/WorkItem/properties/updated"})
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[schemars(rename = "PersistedWorkItemRecord")]
pub struct WorkItem {
    id: TaskId,
    #[serde(deserialize_with = "status")]
    #[schemars(schema_with = "status_schema")]
    status: WorkStatus,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(schema_with = "dependencies_schema")]
    dependencies: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    owner: Field<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(schema_with = "updated_schema")]
    updated: Option<Number>,
    #[serde(rename = "rootId", default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    root_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    #[schemars(with = "Option<String>")]
    title: Field<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "WorkItemVersion")]
    version: Option<WorkItemVersion>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Vec<serde_json::Map<String, Value>>")]
    results: Option<Vec<serde_json::Map<String, Value>>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Vec<serde_json::Map<String, Value>>")]
    decisions: Option<Vec<serde_json::Map<String, Value>>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Vec<Value>")]
    releases: Option<Vec<Value>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl WorkItem {
    pub fn id(&self) -> &TaskId {
        &self.id
    }
    pub fn status(&self) -> WorkStatus {
        self.status
    }
    pub fn dependencies(&self) -> &[String] {
        self.dependencies.as_deref().unwrap_or_default()
    }
    pub fn owner(&self) -> Option<&str> {
        self.owner.value().map(String::as_str)
    }
    pub fn root_id(&self) -> Option<&str> {
        self.root_id.value().map(String::as_str)
    }
    pub fn title(&self) -> Option<&str> {
        self.title.value().map(String::as_str)
    }
    pub fn updated(&self) -> f64 {
        self.updated
            .as_ref()
            .and_then(Number::as_f64)
            .unwrap_or_default()
    }
    pub fn version(&self) -> Option<&Number> {
        self.version.as_ref().map(RuleInteger::number)
    }
    pub fn results(&self) -> Option<&[serde_json::Map<String, Value>]> {
        self.results.as_deref()
    }
    pub fn decisions(&self) -> Option<&[serde_json::Map<String, Value>]> {
        self.decisions.as_deref()
    }
    pub fn results_mut(&mut self) -> Option<&mut Vec<serde_json::Map<String, Value>>> {
        self.results.as_mut()
    }
    pub fn decisions_mut(&mut self) -> Option<&mut Vec<serde_json::Map<String, Value>>> {
        self.decisions.as_mut()
    }
    pub fn extra(&self, key: &str) -> Option<&Value> {
        self.extra.get(key)
    }
    pub fn set_status(&mut self, value: WorkStatus) {
        self.status = value;
    }
    pub fn set(&mut self, key: impl Into<String>, value: Value) -> Result<(), serde_json::Error> {
        let key = key.into();
        match key.as_str() {
            "id" => {
                let id = TaskId::deserialize(value)?;
                if id != self.id {
                    return Err(serde::de::Error::custom("work record id is immutable"));
                }
            }
            "status" => self.status = status(value)?,
            "dependencies" => self.dependencies = Some(Vec::<String>::deserialize(value)?),
            "owner" => self.owner = Field::deserialize(value)?,
            "updated" => self.updated = Some(Number::deserialize(value)?),
            "rootId" => self.root_id = Field::deserialize(value)?,
            "title" => self.title = Field::deserialize(value)?,
            "version" => self.version = Some(RuleInteger::deserialize(value)?),
            "results" => {
                self.results = Some(Vec::<serde_json::Map<String, Value>>::deserialize(value)?)
            }
            "decisions" => {
                self.decisions = Some(Vec::<serde_json::Map<String, Value>>::deserialize(value)?)
            }
            "releases" => self.releases = Some(Vec::<Value>::deserialize(value)?),
            _ => {
                self.extra.insert(key, value);
            }
        }
        Ok(())
    }
    pub fn value(&self) -> Value {
        self.serialize(serde_json::value::Serializer)
            .expect("work item fields serialize")
    }
    pub fn into_value(self) -> Value {
        let mut fields: serde_json::Map<String, Value> = self.extra.into_iter().collect();
        fields.insert("id".into(), Value::String(self.id.0));
        fields.insert("status".into(), Value::String(self.status.as_str().into()));
        if let Some(v) = self.dependencies {
            fields.insert(
                "dependencies".into(),
                Value::Array(v.into_iter().map(Value::String).collect()),
            );
        }
        insert_field(&mut fields, "owner", self.owner, Value::String);
        if let Some(v) = self.updated {
            fields.insert("updated".into(), Value::Number(v));
        }
        insert_field(&mut fields, "rootId", self.root_id, Value::String);
        insert_field(&mut fields, "title", self.title, Value::String);
        if let Some(v) = self.version {
            fields.insert("version".into(), Value::Number(v.number().clone()));
        }
        if let Some(v) = self.results {
            fields.insert(
                "results".into(),
                Value::Array(v.into_iter().map(Value::Object).collect()),
            );
        }
        if let Some(v) = self.decisions {
            fields.insert(
                "decisions".into(),
                Value::Array(v.into_iter().map(Value::Object).collect()),
            );
        }
        if let Some(v) = self.releases {
            fields.insert("releases".into(), Value::Array(v));
        }
        Value::Object(fields)
    }
}
fn insert_field<T>(
    fields: &mut serde_json::Map<String, Value>,
    key: &str,
    field: Field<T>,
    project: impl FnOnce(T) -> Value,
) {
    match field {
        Field::Missing => {}
        Field::Null => {
            fields.insert(key.into(), Value::Null);
        }
        Field::Value(v) => {
            fields.insert(key.into(), project(v));
        }
    }
}
pub(crate) fn schema() -> Value {
    schemars::schema_for!(WorkItem).to_value()
}
pub(crate) fn check_schema_parity() -> Result<(), String> {
    let stored: Value = serde_json::from_slice(include_bytes!(
        "schemas/persisted-work-item-record.schema.json"
    ))
    .map_err(|e| e.to_string())?;
    if stored != schema() {
        return Err(
            "persisted work schema must be generated from work_item_record::WorkItem".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn work_schema_matches_typed_fields_and_preserves_json() {
        check_schema_parity().unwrap();
        let fields = [
            "id",
            "status",
            "dependencies",
            "owner",
            "updated",
            "rootId",
            "title",
            "version",
            "results",
            "decisions",
            "releases",
        ];
        assert_eq!(
            schema()["properties"].as_object().unwrap().len(),
            fields.len()
        );
        for field in fields {
            for candidate in [
                Value::Null,
                json!("ready"),
                json!(true),
                json!(1),
                json!(1.0),
                json!(-1),
                json!(9007199254740991_u64),
                json!(9007199254740992_u64),
                json!({}),
                json!([]),
                json!([{}]),
                json!([1]),
            ] {
                let mut value = json!({"id":"task", "status":"ready", "extra":{"nested":[1]}});
                value[field] = candidate;
                let parsed = WorkItem::deserialize(value.clone());
                assert_eq!(
                    parsed.is_ok(),
                    crate::validate("PersistedWorkItemRecord", &value).unwrap(),
                    "{field}: {value}"
                );
                if let Ok(parsed) = parsed {
                    assert_eq!(parsed.into_value(), value);
                }
            }
        }
        assert!(WorkItem::deserialize(json!({"id":"task"})).is_err());
        assert!(WorkItem::deserialize(json!({"status":"ready"})).is_err());
    }
}
