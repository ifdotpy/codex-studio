//! Opaque JSON evidence carried without interpreting its contents.
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::borrow::Cow;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawJson(pub Box<RawValue>);
impl PartialEq for RawJson {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}
impl schemars::JsonSchema for RawJson {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "RawJson".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({})
    }
}
