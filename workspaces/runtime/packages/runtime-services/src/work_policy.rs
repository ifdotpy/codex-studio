//! Pure projections for persisted work, plan and queue read operations.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use studio_runtime_store::{StoredQueueEvent, StoredRecord};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkPolicyError {
    #[error("invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
}

pub fn work_list(root_id: &str, records: &[StoredRecord]) -> Result<Value, WorkPolicyError> {
    let mut works = Vec::new();
    for row in records {
        let item = parse_record("runtime_work", row)?;
        let Some(object) = item.as_object() else {
            return Err(invalid("runtime_work", &row.id));
        };
        if object.get("id").and_then(Value::as_str) != Some(row.id.as_str()) {
            return Err(invalid("runtime_work", &row.id));
        }
        if object.get("rootId").and_then(Value::as_str) == Some(root_id) {
            works.push(item);
        }
    }

    let statuses = works
        .iter()
        .filter_map(|item| {
            Some((
                item.get("id")?.as_str()?.to_owned(),
                item.get("status")?.as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut views = Vec::with_capacity(works.len());
    for mut item in works {
        let Some(object) = item.as_object_mut() else {
            return Err(invalid("runtime_work", "<record>"));
        };
        let Some(dependencies) = object.get("dependencies").and_then(Value::as_array) else {
            return Err(invalid(
                "runtime_work",
                object
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("<record>"),
            ));
        };
        let blocked_by = dependencies
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| statuses.get(*id).map(String::as_str) != Some("accepted"))
            .map(|id| Value::String(id.to_owned()))
            .collect::<Vec<_>>();
        let status = object
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("runtime_work", "<record>"))?
            .to_owned();
        object.insert("blockedBy".to_owned(), Value::Array(blocked_by.clone()));
        object.insert(
            "displayStatus".to_owned(),
            Value::String(
                if !blocked_by.is_empty() && matches!(status.as_str(), "ready" | "running") {
                    "blocked".to_owned()
                } else {
                    status
                },
            ),
        );
        views.push(item);
    }

    Ok(json!({"items": views, "tasks": views}))
}

pub fn plan_view(
    agent_id: &str,
    root_id: &str,
    stored: Option<&str>,
) -> Result<Value, WorkPolicyError> {
    let Some(stored) = stored else {
        return Ok(json!({
            "id": agent_id,
            "rootId": root_id,
            "text": "",
            "version": 0,
            "updated": null,
            "steps": [],
        }));
    };
    let mut record: Value =
        serde_json::from_str(stored).map_err(|_| invalid("runtime_plans", agent_id))?;
    let Some(object) = record.as_object_mut() else {
        return Err(invalid("runtime_plans", agent_id));
    };
    if object.get("id").and_then(Value::as_str) != Some(agent_id) {
        return Err(invalid("runtime_plans", agent_id));
    }
    Ok(record)
}

pub fn queue_asset_ids(events: &[StoredQueueEvent]) -> Result<Vec<String>, WorkPolicyError> {
    let mut ids = Vec::new();
    for event in events {
        let metadata = event_metadata(event)?;
        let Some(assets) = metadata.get("assets") else {
            continue;
        };
        let Some(assets) = assets.as_array() else {
            return Err(invalid("runtime_event_meta", &event.id));
        };
        for asset in assets {
            let Some(id) = asset.as_str() else {
                return Err(invalid("runtime_event_meta", &event.id));
            };
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

pub fn queue_items(
    events: &[StoredQueueEvent],
    assets: &[StoredRecord],
) -> Result<Vec<Value>, WorkPolicyError> {
    let mut asset_records = BTreeMap::new();
    for row in assets {
        let mut asset = parse_record("runtime_assets", row)?;
        let Some(object) = asset.as_object_mut() else {
            return Err(invalid("runtime_assets", &row.id));
        };
        if object.get("id").and_then(Value::as_str) != Some(row.id.as_str()) {
            return Err(invalid("runtime_assets", &row.id));
        }
        object.remove("path");
        asset_records.insert(row.id.as_str(), asset);
    }

    events
        .iter()
        .map(|event| {
            let metadata = event_metadata(event)?;
            let mut queue_item = Map::new();
            queue_item.insert("id".to_owned(), Value::String(event.id.clone()));
            queue_item.insert("kind".to_owned(), Value::String(event.kind.clone()));
            queue_item.insert("text".to_owned(), Value::String(event.text.clone()));
            queue_item.insert("status".to_owned(), Value::String(event.status.clone()));
            queue_item.insert("created".to_owned(), json!(event.created));
            queue_item.insert(
                "assets".to_owned(),
                Value::Array(
                    metadata
                        .get("assets")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|asset_id| {
                            let id = asset_id
                                .as_str()
                                .ok_or_else(|| invalid("runtime_event_meta", &event.id))?;
                            asset_records
                                .get(id)
                                .cloned()
                                .ok_or_else(|| invalid("runtime_assets", id))
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            );
            if let Some(error) = &event.error {
                queue_item.insert("error".to_owned(), Value::String(error.clone()));
            }
            let delivery = metadata
                .get("delivery")
                .cloned()
                .unwrap_or_else(|| Value::String("queue".to_owned()));
            let requested_delivery = metadata
                .get("requestedDelivery")
                .cloned()
                .unwrap_or_else(|| delivery.clone());
            queue_item.insert("delivery".to_owned(), delivery);
            queue_item.insert("requestedDelivery".to_owned(), requested_delivery);
            if let Some(accepted_at) = metadata.get("acceptedAt") {
                queue_item.insert("acceptedAt".to_owned(), accepted_at.clone());
            }
            Ok(Value::Object(queue_item))
        })
        .collect()
}

fn event_metadata(event: &StoredQueueEvent) -> Result<Value, WorkPolicyError> {
    let Some(raw) = &event.metadata else {
        return Ok(Value::Object(Map::new()));
    };
    let metadata: Value =
        serde_json::from_str(raw).map_err(|_| invalid("runtime_event_meta", &event.id))?;
    if !metadata.is_object() {
        return Err(invalid("runtime_event_meta", &event.id));
    }
    Ok(metadata)
}

fn parse_record(object: &'static str, row: &StoredRecord) -> Result<Value, WorkPolicyError> {
    serde_json::from_str(&row.record).map_err(|_| invalid(object, &row.id))
}

fn invalid(object: &'static str, id: &str) -> WorkPolicyError {
    WorkPolicyError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{plan_view, work_list};
    use serde_json::json;
    use studio_runtime_store::StoredRecord;

    #[test]
    fn dependency_projection_matches_python_work_view_and_keeps_record_fields() {
        let records = vec![
            StoredRecord {
                id: "ready".to_owned(),
                record: json!({
                    "id":"ready", "rootId":"lead", "status":"ready",
                    "dependencies":["accepted","missing"], "title":"Ready"
                })
                .to_string(),
            },
            StoredRecord {
                id: "accepted".to_owned(),
                record: json!({
                    "id":"accepted", "rootId":"lead", "status":"accepted",
                    "dependencies":[], "title":"Accepted"
                })
                .to_string(),
            },
            StoredRecord {
                id: "private".to_owned(),
                record: json!({
                    "id":"private", "rootId":"another-lead", "status":"ready",
                    "dependencies":[], "title":"Private"
                })
                .to_string(),
            },
        ];
        let result = work_list("lead", &records).expect("stored work projects");
        assert_eq!(result["items"].as_array().map(Vec::len), Some(2));
        assert_eq!(result["items"][0]["blockedBy"], json!(["missing"]));
        assert_eq!(result["items"][0]["displayStatus"], "blocked");
        assert_eq!(result["items"][1]["displayStatus"], "accepted");
        assert_eq!(result["items"][0]["title"], "Ready");
    }

    #[test]
    fn absent_plan_has_python_default_shape_and_invalid_storage_is_refused() {
        assert_eq!(
            plan_view("agent", "root", None).expect("missing plan defaults"),
            json!({"id":"agent","rootId":"root","text":"","version":0,"updated":null,"steps":[]})
        );
        let error = plan_view("agent", "root", Some("{}")).expect_err("invalid plan is refused");
        assert!(error.to_string().contains("runtime_plans/agent"));
    }
}
