//! Host-saved VM share observations, independent of the OS mount table.
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(d)?.as_str().map(str::to_owned))
}
fn number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(Value::deserialize(d)?.as_u64())
}
#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedMount {
    #[serde(
        default,
        deserialize_with = "text",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<String>,
    #[serde(
        default,
        deserialize_with = "text",
        skip_serializing_if = "Option::is_none"
    )]
    pub path: Option<String>,
    #[serde(
        default,
        deserialize_with = "number",
        skip_serializing_if = "Option::is_none"
    )]
    pub port: Option<u64>,
    #[serde(default)]
    pub project_id: Value,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedMountFailure {
    #[serde(default)]
    pub at: Value,
    #[serde(default)]
    pub error: Value,
    #[serde(default)]
    pub project_id: Value,
}
impl SavedMount {
    pub fn from_value(value: Value) -> Self {
        if value.is_object() {
            Self::deserialize(value).unwrap_or_default()
        } else {
            Self::default()
        }
    }
}
impl SavedMountFailure {
    pub fn from_value(value: Value) -> Self {
        if value.is_object() {
            Self::deserialize(value).unwrap_or_default()
        } else {
            Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn saved_mount_writer_matches_historical_json_bytes() {
        let saved = SavedMount {
            project_id: json!("p"),
            name: Some("app".into()),
            path: Some("/view".into()),
            port: Some(50123),
        };
        let failure = SavedMountFailure {
            project_id: json!(null),
            error: json!("failed"),
            at: json!(12.5),
        };
        assert_eq!(
            serde_json::to_string(&saved).unwrap(),
            json!({"projectId":"p","name":"app","path":"/view","port":50123}).to_string()
        );
        assert_eq!(
            serde_json::to_string(&failure).unwrap(),
            json!({"projectId":null,"error":"failed","at":12.5}).to_string()
        );
    }
}
