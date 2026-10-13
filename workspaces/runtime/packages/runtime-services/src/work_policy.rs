//! Projections for persisted work, plan and queue reads.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use studio_runtime_store::StoredQueueEvent;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkPolicyError {
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
}

pub struct PreparedQueueItem {
    event: StoredQueueEvent,
    metadata: Value,
}

pub struct PreparedQueue {
    items: Vec<PreparedQueueItem>,
    pub asset_ids: Vec<String>,
}

pub fn work_list(root: &str, records: &[Value]) -> Value {
    let statuses = records
        .iter()
        .map(|item| {
            (
                item["id"]
                    .as_str()
                    .expect("schema-validated work id")
                    .to_owned(),
                item["status"]
                    .as_str()
                    .expect("schema-validated work status")
                    .to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let items = records
        .iter()
        .filter(|item| item["rootId"] == root)
        .map(|item| work_item_view(item, &statuses))
        .collect::<Vec<_>>();
    json!({"items": items, "tasks": items})
}

pub fn work_item_view(item: &Value, statuses: &BTreeMap<String, String>) -> Value {
    let mut view = item.clone();
    let object = view.as_object_mut().expect("schema-validated work item");
    let blocked = item["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| statuses.get(*id).map(String::as_str) != Some("accepted"))
        .map(|id| Value::String(id.to_owned()))
        .collect::<Vec<_>>();
    let status = item["status"]
        .as_str()
        .expect("schema-validated work status");
    object.insert("blockedBy".into(), json!(blocked));
    object.insert(
        "displayStatus".into(),
        json!(
            if !object["blockedBy"].as_array().is_some_and(Vec::is_empty)
                && matches!(status, "ready" | "running")
            {
                "blocked"
            } else {
                status
            }
        ),
    );
    view
}

pub fn plan_view(agent_id: &str, root_id: &str, stored: Option<Value>) -> Value {
    stored.unwrap_or_else(|| json!({
        "id": agent_id, "rootId": root_id, "text": "", "version": 0, "updated": null, "steps": []
    }))
}

pub fn prepare_queue(events: Vec<StoredQueueEvent>) -> Result<PreparedQueue, WorkPolicyError> {
    let mut items = Vec::with_capacity(events.len());
    let mut asset_ids = Vec::new();
    for event in events {
        if !event.valid {
            return Err(invalid("runtime_events", &event.id));
        }
        let metadata = event.metadata.clone().unwrap_or_else(|| json!({}));
        let Some(metadata_object) = metadata.as_object() else {
            return Err(invalid("runtime_event_meta", &event.id));
        };
        if let Some(assets) = metadata_object.get("assets") {
            for value in assets
                .as_array()
                .ok_or_else(|| invalid("runtime_event_meta", &event.id))?
            {
                asset_ids.push(
                    value
                        .as_str()
                        .ok_or_else(|| invalid("runtime_event_meta", &event.id))?
                        .to_owned(),
                );
            }
        }
        items.push(PreparedQueueItem { event, metadata });
    }
    Ok(PreparedQueue { items, asset_ids })
}

pub fn queue_items(
    prepared: PreparedQueue,
    assets: &[Value],
) -> Result<Vec<Value>, WorkPolicyError> {
    let asset_records = assets
        .iter()
        .filter_map(|asset| Some((asset.get("id")?.as_str()?, asset)))
        .collect::<BTreeMap<_, _>>();
    prepared
        .items
        .into_iter()
        .map(|prepared| {
            let event = prepared.event;
            let metadata = prepared.metadata;
            let ids = metadata
                .get("assets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            let attachments = ids
                .map(|value| {
                    let id = value.as_str().expect("validated queue asset id");
                    asset_records
                        .get(id)
                        .map(|asset| (*asset).clone())
                        .ok_or_else(|| invalid("runtime_assets", id))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let delivery = metadata
                .get("delivery")
                .cloned()
                .unwrap_or_else(|| json!("queue"));
            let mut item = json!({
                "id": event.id, "kind": event.kind, "text": event.text,
                "status": event.status, "created": event.created,
                "error": event.error, "assets": attachments,
                "delivery": delivery,
                "requestedDelivery": metadata.get("requestedDelivery").cloned().unwrap_or(delivery),
            });
            if let Some(accepted) = metadata.get("acceptedAt") {
                item["acceptedAt"] = accepted.clone();
            }
            Ok(item)
        })
        .collect()
}

pub fn invalid(object: &'static str, id: &str) -> WorkPolicyError {
    WorkPolicyError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{plan_view, prepare_queue, queue_items, work_list};
    use serde_json::json;
    use studio_runtime_store::StoredQueueEvent;

    #[test]
    fn dependency_projection_keeps_record_fields_and_projects_blocking() {
        let records = json!([
            {"id":"ready","rootId":"lead","status":"ready","dependencies":["accepted","missing"],"title":"Ready"},
            {"id":"accepted","rootId":"lead","status":"accepted","dependencies":[]},
            {"id":"private","rootId":"another-lead","status":"ready","dependencies":[]}
        ]);
        let result = work_list(
            "lead",
            records.as_array().expect("work records are an array"),
        );
        assert_eq!(
            result["items"]
                .as_array()
                .expect("projected work items are an array")
                .len(),
            2
        );
        assert_eq!(result["items"][0]["blockedBy"], json!(["missing"]));
        assert_eq!(result["items"][0]["displayStatus"], "blocked");
        assert_eq!(result["items"][0]["title"], "Ready");
    }

    #[test]
    fn missing_plan_defaults_and_queue_assets_refuse_dangling_assets() {
        assert_eq!(plan_view("agent", "root", None)["text"], "");
        let prepared = prepare_queue(vec![StoredQueueEvent {
            id: "event".into(),
            kind: "user".into(),
            text: "hello".into(),
            status: "pending".into(),
            created: 1.0,
            error: None,
            metadata: Some(json!({"assets":["asset"]})),
            valid: true,
        }])
        .expect("queue event is valid");
        assert!(
            queue_items(prepared, &[])
                .expect_err("dangling asset must be refused")
                .to_string()
                .contains("runtime_assets/asset")
        );
    }
}
