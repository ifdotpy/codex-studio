//! Projection assertions shared by service and HTTP frozen corpora.

use serde_json::Value;
use std::collections::BTreeMap;

/// Frozen Python snapshots predate the two view collections. Validate their
/// generated field ownership and values before comparing the legacy snapshot.
pub(crate) fn assert_agent_view_projections(db: &rusqlite::Connection) {
    let mut statement = db.prepare(
        "SELECT collection,id,payload,deleted FROM sync_entities WHERE collection IN ('agent','agentSummary','agentDetail')",
    ).expect("agent view rows");
    let rows = statement
        .query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                (row.get::<_, Option<String>>(2)?, row.get::<_, i64>(3)?),
            ))
        })
        .expect("view query")
        .collect::<Result<BTreeMap<_, _>, _>>()
        .expect("view rows");
    let schemas =
        &studio_runtime_services::sync_policy::openapi_document()["components"]["schemas"];
    for ((collection, id), (raw, deleted)) in &rows {
        if collection != "agentSummary" {
            continue;
        }
        let detail = rows
            .get(&("agentDetail".into(), id.clone()))
            .expect("detail accompanies summary");
        let legacy = rows
            .get(&("agent".into(), id.clone()))
            .expect("legacy accompanies summary");
        assert_eq!(*deleted, detail.1, "{id}: both views share deletion");
        assert_eq!(*deleted, legacy.1, "{id}: compatibility deletion");
        if *deleted != 0 {
            continue;
        }
        let summary: Value =
            serde_json::from_str(raw.as_deref().expect("summary payload")).expect("summary JSON");
        let detail: Value = serde_json::from_str(detail.0.as_deref().expect("detail payload"))
            .expect("detail JSON");
        let legacy: Value = serde_json::from_str(legacy.0.as_deref().expect("legacy payload"))
            .expect("legacy JSON");
        assert!(is_agent_view_collection(&summary["collection"]));
        let mut union = serde_json::Map::new();
        for (projection, schema) in [
            (&summary, "AgentSummaryEntityDto"),
            (&detail, "AgentDetailEntityDto"),
        ] {
            assert_eq!(projection["id"], *id);
            assert_eq!(projection["value"]["id"], *id);
            for (field, value) in projection["value"].as_object().expect("view object") {
                assert!(
                    schemas[schema]["properties"].get(field).is_some(),
                    "{schema}.{field} has one OpenAPI owner"
                );
                if let Some(expected) = legacy["value"].get(field) {
                    assert_eq!(value, expected, "{id}/{field}: view equals publication");
                }
                union.insert(field.clone(), value.clone());
            }
        }
        for (field, value) in legacy["value"].as_object().expect("legacy object") {
            assert_eq!(
                union.get(field),
                Some(value),
                "{id}/{field}: views cover legacy fields"
            );
        }
    }
}

pub(crate) fn is_agent_view_collection(collection: &Value) -> bool {
    collection == "agentSummary" || collection == "agentDetail"
}
