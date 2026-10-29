//! Typed annotation command response and its current sync row.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::persistence::{RequestReferenceKind, RequestResultStatus};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnnotationData {
    pub id: String,
    pub agent: String,
    pub root_id: String,
    pub path: String,
    pub line: i64,
    pub text: String,
    pub created: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum AnnotationRowOperation {
    #[serde(rename = "upsert")]
    Upsert,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AnnotationEntityRow {
    pub collection: RequestReferenceKind,
    pub id: String,
    pub op: AnnotationRowOperation,
    pub seq: i64,
    pub value: AnnotationData,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AnnotationCommandResponse {
    pub status: RequestResultStatus,
    pub ref_kind: RequestReferenceKind,
    pub ref_id: String,
    pub rev: i64,
    pub rows: Vec<AnnotationEntityRow>,
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use strum::VariantNames;

    #[derive(Deserialize)]
    struct Document {
        components: Components,
    }
    #[derive(Deserialize)]
    struct Components {
        schemas: Schemas,
    }
    #[derive(Deserialize)]
    struct Schemas {
        #[serde(rename = "AnnotationCommandResponse")]
        annotation: Schema,
    }
    #[derive(Deserialize)]
    struct Schema {
        properties: Properties,
    }
    #[derive(Deserialize)]
    struct Properties {
        status: EnumValues,
        ref_kind: EnumValues,
    }
    #[derive(Deserialize)]
    struct EnumValues {
        #[serde(rename = "enum")]
        values: Vec<String>,
    }

    #[test]
    fn annotation_command_enums_match_the_rust_owners() {
        let doc: Document =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).unwrap();
        assert_eq!(
            doc.components.schemas.annotation.properties.status.values,
            super::RequestResultStatus::VARIANTS
        );
        assert_eq!(
            doc.components.schemas.annotation.properties.ref_kind.values,
            super::RequestReferenceKind::VARIANTS
        );
    }
}
