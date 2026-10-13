use super::*;
use crate::work_write::{self, CreateWork, WorkMutation, WorkWrite};
pub(super) fn execute(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
    id: &str,
) -> Result<Value, CallbackError> {
    if args
        .get("version")
        .is_some_and(|v| !v.is_null() && v.as_i64().is_none())
    {
        return Err(CallbackError::Invalid("Task version must be an integer"));
    }
    let actor = snapshot.actor();
    let root = text(actor, "rootId")?;
    let who = &snapshot.context().agent;
    let records = WorkItemsRepository::work(tx)?
        .into_iter()
        .filter_map(|r| r.record)
        .map(|raw| {
            serde_json::from_str::<Value>(&raw).map_err(studio_runtime_store::StoreError::from)
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|r| r["rootId"] == root)
        .collect::<Vec<_>>();
    let statuses = work_policy::work_statuses(&records);
    let action = args["action"].as_str().unwrap_or("list");
    if action == "list" {
        let mut rows = records
            .iter()
            .filter(|r| {
                args.get("owner")
                    .is_none_or(|v| !studio_runtime_store::truthy(Some(v)) || v == &r["owner"])
                    && args
                        .get("state")
                        .is_none_or(|v| !studio_runtime_store::truthy(Some(v)) || v == &r["status"])
            })
            .map(|r| brief(&work_policy::work_item_view(r, &statuses)))
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        return page(rows, args, json!([root, args["owner"], args["state"]]));
    }
    let work = if action != "create" {
        Some(
            records
                .iter()
                .find(|r| Some(&r["id"]) == args.get("task_id"))
                .ok_or(CallbackError::Invalid("Unknown task in this team"))?,
        )
    } else {
        None
    };
    if let Some(work) = work {
        if action == "get" {
            let mut result = brief(&work_policy::work_item_view(work, &statuses));
            result["description"] = work["description"].clone();
            result["dependencies"] = work["dependencies"].clone();
            result["latestResult"] = work["results"]
                .as_array()
                .and_then(|v| v.last())
                .cloned()
                .unwrap_or(Value::Null);
            result["latestDecision"] = work["decisions"]
                .as_array()
                .and_then(|v| v.last())
                .cloned()
                .unwrap_or(Value::Null);
            result["history"] = json!({"results":work["results"].as_array().map_or(0,Vec::len),"decisions":work["decisions"].as_array().map_or(0,Vec::len)});
            return Ok(result);
        }
        if action == "history" {
            let mut rows = Vec::new();
            for (field, kind) in [("results", "result"), ("decisions", "decision")] {
                for mut row in work[field].as_array().cloned().unwrap_or_default() {
                    row["kind"] = json!(kind);
                    rows.push(row);
                }
            }
            rows.sort_by(|a, b| {
                a["created"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .total_cmp(&b["created"].as_f64().unwrap_or(0.0))
            });
            return page(rows, args, json!([root, work["id"], "history"]));
        }
    }
    let lead = actor["isLead"] == true;
    if !lead && !(action == "cancel" && work.is_some_and(|w| w["createdBy"] == *who)) {
        return Err(CallbackError::Invalid(
            "Only the lead can create, assign, or review work",
        ));
    }
    let optional = |key: &str| {
        args.get(key)
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or(CallbackError::Invalid("Invalid task text"))
            })
            .transpose()
    };
    let deps = || -> Result<Vec<String>, CallbackError> {
        Ok(serde_json::from_value(
            args.get("dependencies").cloned().unwrap_or(json!([])),
        )?)
    };
    let command = match action {
        "create" => WorkWrite::Create(CreateWork {
            actor_id: who.clone(),
            title: text(args, "title")?.into(),
            description: optional("description")?.unwrap_or_default(),
            owner: optional("owner")?,
            status: optional("status")?,
            dependencies: deps()?,
        }),
        "update" => WorkWrite::Mutate(WorkMutation::Update {
            task_id: text(args, "task_id")?.into(),
            version: args["version"].as_i64(),
            title: optional("title")?,
            description: optional("description")?,
            owner: args.get("owner").map(|_| optional("owner")).transpose()?,
            status: optional("status")?,
            dependencies: args.get("dependencies").map(|_| deps()).transpose()?,
        }),
        "cancel" => WorkWrite::Mutate(WorkMutation::Cancel {
            task_id: text(args, "task_id")?.into(),
            version: args["version"].as_i64(),
            reason: text(args, "result")?.into(),
        }),
        "reject" => WorkWrite::Mutate(WorkMutation::Reject {
            task_id: text(args, "task_id")?.into(),
            version: args["version"].as_i64(),
            reason: text(args, "result")?.into(),
        }),
        "claim" | "submit" | "accept" => {
            return Err(CallbackError::Dependency(
                "work-board/assignment-and-result-driver",
            ));
        }
        _ => return Err(CallbackError::Invalid("Unknown task action")),
    };
    let result = work_write::write(
        tx,
        who,
        Some(id),
        &crate::event_write::request_signature(args),
        command,
    )?
    .record;
    let mut receipt = brief(&result);
    receipt["requestId"] = json!(id);
    receipt["detail"] = json!({"tool":"orchestration_task","action":"get","task_id":result["id"]});
    Ok(receipt)
}
