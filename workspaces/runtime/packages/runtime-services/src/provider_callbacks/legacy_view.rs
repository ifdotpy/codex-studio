//! Test support: an approval as the retired `runtime_requests` record showed
//! it, and the way back. Fixtures that were written as such records seed the
//! typed table through [`insert`], and assertions read the card through [`view`].
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use studio_runtime_schema::approvals::{
    Answer, Approval, ApprovalId, ApprovalState, Principal, Request, RpcId,
};
use studio_runtime_store::approvals;

/// Seed one legacy-shaped card as a typed approval.
/// A request of a provider turn without a turn id gets the fixture turn.
pub fn insert(tx: &Connection, card: &Value) -> Result<(), studio_runtime_store::StoreError> {
    let mut card = card.clone();
    let agent_level = matches!(
        card["method"].as_str(),
        Some("agent/asyncQuestion" | "monitor/approve" | "browser/open")
    );
    if !agent_level && card["turnId"].is_null() && card["params"]["turnId"].is_null() {
        card["params"]["turnId"] = json!("turn-0");
    }
    let id = card["id"].as_str().unwrap_or_default().to_owned();
    approvals::put(tx, &approvals::from_legacy_record(&id, &card)?)?;
    Ok(())
}

/// Every approval, oldest first, as legacy records.
pub fn views(tx: &Connection) -> Vec<Value> {
    approvals::all(tx)
        .expect("approvals")
        .iter()
        .map(view)
        .collect()
}

/// One approval as a legacy record, or none.
pub fn stored(tx: &Connection, id: &str) -> Option<Value> {
    approvals::get(tx, &ApprovalId(id.to_owned()))
        .expect("approval")
        .as_ref()
        .map(view)
}

fn questions(list: &[studio_runtime_schema::approvals::Question]) -> Value {
    Value::Array(
        list.iter()
            .map(|q| {
                let mut out = Map::new();
                out.insert("id".into(), json!(q.id));
                out.insert("question".into(), json!(q.question));
                if let Some(header) = &q.header {
                    out.insert("header".into(), json!(header));
                }
                out.insert(
                    "options".into(),
                    json!(
                        q.options
                            .iter()
                            .map(|o| json!({"label":o.label,"description":o.description}))
                            .collect::<Vec<_>>()
                    ),
                );
                out.insert("isSecret".into(), json!(q.is_secret));
                if let Some(multi) = q.multi_select {
                    out.insert("multiSelect".into(), json!(multi));
                }
                if let Some(other) = q.is_other {
                    out.insert("isOther".into(), json!(other));
                }
                Value::Object(out)
            })
            .collect(),
    )
}

/// The record a legacy reader would have seen.
pub fn view(row: &Approval) -> Value {
    let mut params = row.detail.params.0.clone();
    let method = match &row.request {
        Request::Exec {
            command,
            cwd,
            reason,
        } => {
            params.insert("command".into(), json!(command));
            params.insert("cwd".into(), json!(cwd));
            if let Some(reason) = reason {
                params.insert("reason".into(), json!(reason));
            }
            "item/commandExecution/requestApproval"
        }
        Request::Patch { item, reason, .. } => {
            params.insert("itemId".into(), json!(item));
            if let Some(reason) = reason {
                params.insert("reason".into(), json!(reason));
            }
            "item/fileChange/requestApproval"
        }
        Request::Permissions { .. } => "item/permissions/requestApproval",
        Request::Elicitation { schema } => {
            params.insert("requestedSchema".into(), schema.clone());
            "mcpServer/elicitation/request"
        }
        Request::Question { questions: list } => {
            params.insert("questions".into(), questions(list));
            "item/tool/requestUserInput"
        }
        Request::AgentQuestion { questions: list } => {
            params.insert("questions".into(), questions(list));
            "agent/asyncQuestion"
        }
        Request::MonitorStart { monitor, command } => {
            params.insert("monitorId".into(), json!(monitor));
            params.insert("command".into(), json!(command));
            "monitor/approve"
        }
        Request::BrowserOpen(card) => {
            params.insert("url".into(), json!(card.url));
            "browser/open"
        }
        Request::ToolUse { .. } => "tool/use",
    };
    if let Some(turn) = &row.turn {
        params.insert("turnId".into(), json!(turn.as_str()));
    }
    let mut card = Map::new();
    card.insert("id".into(), json!(row.id.as_str()));
    card.insert("agent".into(), json!(row.agent.as_str()));
    card.insert("method".into(), json!(method));
    card.insert("params".into(), Value::Object(params));
    card.insert("status".into(), json!(super::cards::effect_status(row)));
    card.insert("createdAt".into(), json!(row.created.0));
    if let Some(epoch) = row.epoch {
        card.insert("epoch".into(), json!(epoch));
    }
    if let Some(delivery) = &row.delivery {
        card.insert("accountKey".into(), json!(delivery.account));
        card.insert("connectionId".into(), json!(delivery.connection));
        card.insert(
            "rpcId".into(),
            match &delivery.rpc_id {
                RpcId::Number { value } => json!(value),
                RpcId::Text { value } => json!(value),
            },
        );
        if let Some(supervisor) = &delivery.supervisor {
            card.insert(
                "supervisor".into(),
                json!({"generation":supervisor.generation}),
            );
        }
    }
    if let Some(preview) = &row.detail.preview {
        card.insert("preview".into(), Value::Object(preview.0.clone()));
    }
    if let Some(error) = &row.detail.answer_error {
        card.insert("answerError".into(), json!(error));
    }
    let given = match &row.state {
        ApprovalState::Answering { answer, .. }
        | ApprovalState::Answered { answer, .. }
        | ApprovalState::Unknown { answer, .. } => Some(answer),
        ApprovalState::Pending | ApprovalState::Closed { .. } => None,
    };
    match given {
        Some(Answer::Decision { decision }) => {
            let decision = serde_json::to_value(decision).unwrap_or(Value::Null);
            card.insert("decision".into(), decision);
        }
        Some(Answer::Elicitation { action, .. }) => {
            card.insert(
                "decision".into(),
                serde_json::to_value(action).unwrap_or(Value::Null),
            );
        }
        Some(Answer::Answers { answers }) => {
            card.insert("decision".into(), json!("answer"));
            card.insert(
                "answerHistory".into(),
                json!(
                    answers
                        .iter()
                        .map(|a| json!({"id":a.id,"answer":a.answer}))
                        .collect::<Vec<_>>()
                ),
            );
        }
        None => {}
    }
    if let ApprovalState::Answered { by, at, .. } = &row.state {
        if matches!(by, Principal::Owner) {
            card.insert("answeredBy".into(), json!("user"));
        }
        card.insert("answeredAt".into(), json!(at.0));
    }
    if let Some(deferral) = &row.deferral {
        card.insert("deferred".into(), json!(deferral.active));
    }
    if let Some(hidden) = &row.hidden {
        card.insert("deletedAt".into(), json!(hidden.at.0));
        card.insert("deletedBy".into(), json!("user"));
    }
    Value::Object(card)
}
