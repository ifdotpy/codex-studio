//! Projections for persisted work, plan and queue reads.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use studio_runtime_store::{StoredQueueEvent, WorkItemRecord, WorkStatus};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkPolicyError {
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
}

pub struct PreparedQueueItem {
    event: StoredQueueEvent,
    metadata: studio_runtime_store::EventMetadataRecord,
}

pub struct PreparedQueue {
    items: Vec<PreparedQueueItem>,
    pub asset_ids: Vec<String>,
}

pub fn work_list(root: &str, records: &[WorkItemRecord]) -> Value {
    let statuses = work_statuses(records);
    let items = records
        .iter()
        .filter(|item| item.value()["rootId"] == root)
        .map(|item| work_item_view(item, &statuses))
        .collect::<Vec<_>>();
    json!({"items": items, "tasks": items})
}

pub fn work_statuses(records: &[WorkItemRecord]) -> BTreeMap<String, WorkStatus> {
    records
        .iter()
        .map(|item| (item.id().0.clone(), item.status()))
        .collect()
}

pub fn work_item_view(item: &WorkItemRecord, statuses: &BTreeMap<String, WorkStatus>) -> Value {
    let mut view = item.value().clone();
    if let Some(fields) = view.as_object_mut() {
        for private in ["archiveIntent", "vmSubmit", "vmAccept", "resultFailures"] {
            fields.remove(private);
        }
    }
    let blocked = item.blocked_by(statuses);
    let status = if !blocked.is_empty()
        && matches!(item.status(), WorkStatus::Ready | WorkStatus::Running)
    {
        WorkStatus::Blocked
    } else {
        item.status()
    };
    view["blockedBy"] = json!(blocked);
    view["displayStatus"] = json!(status);
    view
}

pub fn prepare_queue(events: Vec<StoredQueueEvent>) -> Result<PreparedQueue, WorkPolicyError> {
    let mut items = Vec::with_capacity(events.len());
    let mut asset_ids = Vec::new();
    for event in events {
        if !event.valid {
            return Err(invalid("runtime_events", &event.id));
        }
        let metadata = event.metadata.clone().unwrap_or_default();
        if let Some(assets) = &metadata.assets {
            for value in assets {
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
            let ids = metadata.assets.iter().flatten();
            let attachments = ids
                .map(|value| {
                    let id = value.as_str().expect("validated queue asset id");
                    asset_records
                        .get(id)
                        .map(|asset| (*asset).clone())
                        .ok_or_else(|| invalid("runtime_assets", id))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let delivery = match &metadata.delivery {
                studio_runtime_schema::queue_command::Field::Missing => json!("queue"),
                other => json!(other),
            };
            let requested = match &metadata.requested_delivery {
                studio_runtime_schema::queue_command::Field::Missing => delivery.clone(),
                other => json!(other),
            };
            let mut item = json!({
                "id": event.id, "kind": event.kind, "text": event.text,
                "status": event.status, "created": event.created,
                "error": event.error, "assets": attachments,
                "delivery": delivery,
                "requestedDelivery": requested,
            });
            if let Some(accepted) = metadata.extensions.get("acceptedAt") {
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
    use super::{prepare_queue, queue_items, work_list};
    use serde_json::json;
    use studio_runtime_store::StoredQueueEvent;

    #[test]
    fn work_public_projection_excludes_private_effect_authority() {
        let record=studio_runtime_store::WorkItemRecord::checked(json!({"id":"work","rootId":"root","status":"accepted","dependencies":[],"title":"Title","archiveIntent":{"publicAdmission":{"id":"private"}},"vmSubmit":{"publicAdmission":{"id":"private"}},"vmAccept":{"publicAdmission":{"id":"private"}},"resultFailures":{"errors":[]}})).expect("owner");
        let view = super::work_item_view(&record, &std::collections::BTreeMap::new());
        assert_eq!(view["title"], "Title");
        assert!(!view.to_string().contains("private"));
        assert!(view.get("archiveIntent").is_none());
        assert!(view.get("vmSubmit").is_none());
        assert!(view.get("vmAccept").is_none());
        assert!(view.get("resultFailures").is_none());
    }

    #[test]
    fn dependency_projection_keeps_record_fields_and_projects_blocking() {
        let _: fn(
            &[studio_runtime_store::WorkItemRecord],
        ) -> std::collections::BTreeMap<String, studio_runtime_store::WorkStatus> =
            super::work_statuses;
        let _: fn(
            &studio_runtime_store::Connection,
            &studio_runtime_store::WorkItemRecord,
        ) -> studio_runtime_store::StoreResult<serde_json::Value> =
            crate::work_write::work_item_view;
        let records = json!([
            {"id":"ready","rootId":"lead","status":"ready","dependencies":["accepted","missing"],"title":"Ready"},
            {"id":"accepted","rootId":"lead","status":"accepted","dependencies":[]},
            {"id":"private","rootId":"another-lead","status":"ready","dependencies":[]}
        ]);
        let result = work_list(
            "lead",
            &records
                .as_array()
                .expect("work records are an array")
                .iter()
                .cloned()
                .map(studio_runtime_store::WorkItemRecord::checked)
                .collect::<Result<Vec<_>, _>>()
                .expect("typed records"),
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
    fn queue_assets_refuse_dangling_assets() {
        let prepared = prepare_queue(vec![StoredQueueEvent {
            id: "event".into(),
            kind: "user".into(),
            text: "hello".into(),
            status: "pending".into(),
            created: 1.0,
            error: None,
            metadata: Some(
                studio_runtime_store::EventMetadataRecord::from_value(json!({"assets":["asset"]}))
                    .unwrap(),
            ),
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
