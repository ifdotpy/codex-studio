use super::*;
use crate::work_write::{self, WorkActorWrite, WorkWrite};
pub(super) fn execute(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
    id: &str,
) -> Result<Value, CallbackError> {
    use studio_runtime_schema::task_command::TaskCommand;
    let action = args["action"].as_str().unwrap_or("list");
    let command = if matches!(action, "list" | "get" | "history") {
        None
    } else {
        Some(
            TaskCommand::parse(args)
                .map_err(|_| CallbackError::Invalid("Invalid task command field or bound"))?,
        )
    };
    let actor = snapshot.actor();
    let root = text(actor, "rootId")?;
    let who = &snapshot.context().agent;
    if let Some(TaskCommand::SetDefaultGate { gate }) = &command {
        return Ok(crate::task_gate::set_default(
            tx,
            who,
            &gate.clone().map_or(
                Value::Null,
                studio_runtime_schema::task_gate::GateInput::into_value,
            ),
            Some(id),
            &crate::event_write::request_signature(args),
        )?);
    }
    let records = WorkItemsRepository::work(tx)?
        .into_iter()
        .filter(|record| record.value()["rootId"] == root)
        .collect::<Vec<_>>();
    let statuses = work_policy::work_statuses(&records);
    if action == "list" {
        let mut rows = records
            .iter()
            .filter(|r| {
                args.get("owner").is_none_or(|v| {
                    !studio_runtime_store::truthy(Some(v))
                        || v.as_str().is_some_and(|owner| r.owner() == Some(owner))
                }) && args.get("state").is_none_or(|v| {
                    !studio_runtime_store::truthy(Some(v)) || v == &json!(r.status())
                })
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
                .find(|r| Some(r.id().as_ref()) == args.get("task_id").and_then(Value::as_str))
                .ok_or(CallbackError::Invalid("Unknown task in this team"))?,
        )
    } else {
        None
    };
    if let Some(work) = work {
        if action == "get" {
            let mut result = brief(&work_policy::work_item_view(work, &statuses));
            result["gate"] = work.get("gate").cloned().unwrap_or(Value::Null);
            result["description"] = work.value()["description"].clone();
            if let Some(schema) = work.get("resultSchema") {
                result["resultSchema"] = schema.clone();
            }
            result["dependencies"] = json!(work.dependencies());
            result["latestResult"] = work.value()["results"]
                .as_array()
                .and_then(|v| v.last())
                .cloned()
                .unwrap_or(Value::Null);
            result["latestDecision"] = work.value()["decisions"]
                .as_array()
                .and_then(|v| v.last())
                .cloned()
                .unwrap_or(Value::Null);
            result["history"] = json!({"results":work.value()["results"].as_array().map_or(0,Vec::len),"decisions":work.value()["decisions"].as_array().map_or(0,Vec::len)});
            return Ok(result);
        }
        if action == "history" {
            let mut rows = Vec::new();
            for (field, kind) in [("results", "result"), ("decisions", "decision")] {
                for mut row in work.value()[field].as_array().cloned().unwrap_or_default() {
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
            return page(rows, args, json!([root, work.id(), "history"]));
        }
    }
    let lead = actor["isLead"] == true;
    if !lead && !(action == "cancel" && work.is_some_and(|w| w.value()["createdBy"] == *who)) {
        return Err(CallbackError::Invalid(
            "Only the lead can create, assign, or review work",
        ));
    }
    let command = command.ok_or(CallbackError::Invalid("Unknown task action"))?;
    let command = match work_write::task_command(command, who.clone(), std::path::PathBuf::new())? {
        WorkWrite::Create(input) => WorkActorWrite::Create(input),
        WorkWrite::Mutate(input) => WorkActorWrite::Mutate(input),
        WorkWrite::Claim(_) | WorkWrite::Submit(_) | WorkWrite::Accept(_) => {
            return Err(CallbackError::Dependency(
                "work-board/assignment-and-result-driver",
            ));
        }
    };
    let result = work_write::write_as_agent(
        tx,
        who,
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

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn task_transport_accepts_canonical_description_and_integral_decimal_version() {
        assert!(
            studio_runtime_schema::task_command::TaskCommand::parse(
                &json!({"action":"create", "title":"Work", "description":"x".repeat(64000)})
            )
            .is_ok()
        );
        assert!(
            studio_runtime_schema::task_command::TaskCommand::parse(
                &json!({"action":"update", "task_id":"task", "version":2.0})
            )
            .is_ok()
        );
    }
}
