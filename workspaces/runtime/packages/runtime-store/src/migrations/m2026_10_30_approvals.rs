//! Copies every `runtime_requests` row into the typed `approvals` table in one
//! transaction. The legacy table stays untouched and read-only (nothing is
//! deleted). The converter places every legacy field in a typed column or
//! struct field. A field it cannot place fails the migration (the legacy record
//! type denies unknown fields). A row that never recorded its answer becomes
//! `Closed { reason: Legacy }`: nothing is inferred.
use rusqlite::Transaction;
use serde::Deserialize;
use serde_json::{Map, Value};
use studio_runtime_schema::approvals::{
    AgentId, Answer, Approval, ApprovalId, ApprovalState, CloseReason, CommandId, Decision,
    Deferral, Delivery, Detail, Hidden, Principal, ProviderObject, Question, QuestionAnswer,
    QuestionOption, Request, RequestKind, RpcId, Supervisor, Timestamp, TurnId,
};

use crate::approvals;
use crate::{StoreError, StoreResult};

fn bad(id: &str, what: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("approvals migration: request {id}: {what}"))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyRpcId {
    Number(i64),
    Text(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacySupervisor {
    generation: u64,
    state_dir: Option<String>,
    handle: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyHistory {
    id: String,
    #[allow(dead_code)]
    question: Option<String>,
    #[serde(rename = "isSecret")]
    #[allow(dead_code)]
    is_secret: Option<bool>,
    answer: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyOption {
    label: String,
    description: Option<String>,
    preview: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyQuestion {
    id: Option<String>,
    question: Option<String>,
    header: Option<String>,
    options: Option<Vec<LegacyOption>>,
    is_secret: Option<bool>,
    multi_select: Option<bool>,
    is_other: Option<bool>,
}

/// Every key a `runtime_requests` record has. An unknown key fails the migration.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Legacy {
    #[allow(dead_code)]
    id: String,
    agent: Option<String>,
    method: String,
    params: Option<Map<String, Value>>,
    status: String,
    created_at: Option<f64>,
    turn_id: Option<String>,
    account_key: Option<String>,
    connection_id: Option<String>,
    epoch: Option<u64>,
    supervisor: Option<LegacySupervisor>,
    rpc_id: Option<LegacyRpcId>,
    preview: Option<Map<String, Value>>,
    decision: Option<String>,
    /// Monitor approvals write the decision twice. It must equal `decision`.
    answer_decision: Option<String>,
    answered_by: Option<String>,
    answered_at: Option<f64>,
    /// A hash of the answer. Replay is now decided by comparing answers.
    #[allow(dead_code)]
    answer_signature: Option<String>,
    answer_history: Option<Vec<LegacyHistory>>,
    answer_error: Option<String>,
    deferred: Option<bool>,
    deferred_at: Option<f64>,
    deferred_by: Option<String>,
    deleted_at: Option<f64>,
    deleted_by: Option<String>,
    close_reason: Option<String>,
    resolved_at: Option<f64>,
}

fn principal(id: &str, who: &str, close_reason: Option<&str>) -> StoreResult<Principal> {
    match (who, close_reason) {
        ("user", _) => Ok(Principal::Owner),
        // The launcher of this device ended a browser sign-in: the owner's action.
        ("browser", Some("local")) => Ok(Principal::Owner),
        (other, _) => Err(bad(id, &format!("unknown actor {other}"))),
    }
}

fn questions(id: &str, params: &mut Map<String, Value>) -> StoreResult<Vec<Question>> {
    let Some(list) = params.remove("questions") else {
        return Ok(Vec::new());
    };
    let list: Vec<LegacyQuestion> =
        serde_json::from_value(list).map_err(|e| bad(id, &format!("questions: {e}")))?;
    Ok(list
        .into_iter()
        .enumerate()
        .map(|(index, q)| Question {
            id: q.id.unwrap_or_else(|| index.to_string()),
            question: q.question.unwrap_or_default(),
            header: q.header,
            options: q
                .options
                .unwrap_or_default()
                .into_iter()
                .map(|o| QuestionOption {
                    label: o.label,
                    description: o.description,
                    preview: o.preview,
                })
                .collect(),
            is_secret: q.is_secret.unwrap_or(false),
            multi_select: q.multi_select,
            is_other: q.is_other,
        })
        .collect())
}

fn take_text(params: &mut Map<String, Value>, key: &str) -> Option<String> {
    match params.remove(key) {
        Some(Value::String(text)) => Some(text),
        Some(other) => {
            params.insert(key.to_owned(), other);
            None
        }
        None => None,
    }
}

/// Moves the typed fields out of `params`; the rest stays in `Detail.params`.
fn request(id: &str, legacy: &Legacy, params: &mut Map<String, Value>) -> StoreResult<Request> {
    let preview_text = |key: &str| {
        legacy
            .preview
            .as_ref()
            .and_then(|p| p.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    Ok(match legacy.method.as_str() {
        "item/commandExecution/requestApproval" => Request::Exec {
            command: take_text(params, "command")
                .or_else(|| preview_text("command"))
                .unwrap_or_default(),
            cwd: take_text(params, "cwd")
                .or_else(|| preview_text("cwd"))
                .unwrap_or_default(),
            reason: take_text(params, "reason"),
        },
        "item/fileChange/requestApproval" => Request::Patch {
            item: take_text(params, "itemId").unwrap_or_default(),
            files: legacy
                .preview
                .as_ref()
                .and_then(|p| p.get("changes"))
                .and_then(Value::as_array)
                .map(|changes| {
                    changes
                        .iter()
                        .filter_map(|c| c.get("path").and_then(Value::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            reason: take_text(params, "reason"),
        },
        "item/permissions/requestApproval" => Request::Permissions {
            requested: params
                .get("permissions")
                .and_then(Value::as_object)
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default(),
        },
        "mcpServer/elicitation/request" => Request::Elicitation {
            schema: params.remove("requestedSchema").unwrap_or(Value::Null),
        },
        "item/tool/requestUserInput" => Request::Question {
            questions: questions(id, params)?,
        },
        "agent/asyncQuestion" => Request::AgentQuestion {
            questions: questions(id, params)?,
        },
        "monitor/approve" => Request::MonitorStart {
            monitor: take_text(params, "monitorId").unwrap_or_default(),
            command: take_text(params, "command").unwrap_or_default(),
        },
        "browser/open" => Request::BrowserOpen {
            url: take_text(params, "url").unwrap_or_default(),
        },
        other => return Err(bad(id, &format!("unmapped method {other}"))),
    })
}

fn answer(id: &str, legacy: &Legacy) -> StoreResult<Option<Answer>> {
    let Some(decision) = legacy.decision.as_deref() else {
        return Ok(None);
    };
    Ok(Some(match decision {
        "accept" => Answer::Decision {
            decision: Decision::Accept,
        },
        "decline" => Answer::Decision {
            decision: Decision::Decline,
        },
        "cancel" => Answer::Decision {
            decision: Decision::Cancel,
        },
        "answer" => {
            let history = legacy
                .answer_history
                .as_ref()
                .filter(|history| !history.is_empty())
                .ok_or_else(|| bad(id, "answer decision without answer history"))?;
            Answer::Answers {
                answers: history
                    .iter()
                    .map(|entry| QuestionAnswer {
                        id: entry.id.clone(),
                        answer: entry.answer.clone(),
                    })
                    .collect(),
            }
        }
        other => return Err(bad(id, &format!("unknown decision {other}"))),
    }))
}

fn convert(id: &str, legacy: Legacy) -> StoreResult<Approval> {
    let mut params = legacy.params.clone().unwrap_or_default();
    let request = request(id, &legacy, &mut params)?;
    let created = legacy.created_at.unwrap_or(0.0);
    let when = |keys: &[Option<f64>]| Timestamp(keys.iter().find_map(|k| *k).unwrap_or(created));
    if legacy.answer_decision.is_some() && legacy.answer_decision != legacy.decision {
        return Err(bad(id, "answerDecision differs from decision"));
    }
    let given = answer(id, &legacy)?;
    if let Some(answer) = &given {
        request
            .accepts(answer)
            .map_err(|e| bad(id, &format!("recorded answer does not fit: {e}")))?;
    }
    let by = legacy
        .answered_by
        .as_deref()
        .map(|who| principal(id, who, legacy.close_reason.as_deref()))
        .transpose()?;
    let state = match (legacy.status.as_str(), given, by) {
        ("pending", ..) => ApprovalState::Pending,
        ("answering", Some(answer), _) => ApprovalState::Answering {
            answer,
            command: CommandId(id.to_owned()),
        },
        ("answered" | "deleted", Some(answer), Some(by)) => ApprovalState::Answered {
            answer,
            by,
            at: when(&[legacy.answered_at]),
        },
        ("uncertain", Some(answer), _) => ApprovalState::Unknown {
            answer,
            since: when(&[legacy.answered_at]),
        },
        ("expired", ..) => ApprovalState::Closed {
            reason: CloseReason::Expired,
            at: when(&[legacy.resolved_at, legacy.deferred_at, legacy.deleted_at]),
        },
        ("resolved", ..) => ApprovalState::Closed {
            reason: CloseReason::ProviderResolved,
            at: when(&[legacy.resolved_at, legacy.answered_at]),
        },
        ("answering" | "answered" | "deleted" | "uncertain", ..) => ApprovalState::Closed {
            reason: CloseReason::Legacy,
            at: when(&[legacy.resolved_at, legacy.answered_at, legacy.deleted_at]),
        },
        (other, ..) => return Err(bad(id, &format!("unknown status {other}"))),
    };
    // Live data: only agent-level kinds (questions, monitor starts, browser
    // opens) have rows without a turn. A provider-turn kind without one is
    // corrupt and fails the migration.
    let params_turn = take_text(&mut params, "turnId");
    if let (Some(a), Some(b)) = (&legacy.turn_id, &params_turn)
        && a != b
    {
        return Err(bad(id, "turnId differs between record and params"));
    }
    let turn = legacy.turn_id.clone().or(params_turn).map(TurnId);
    if turn.is_none() && !RequestKind::TURN_OPTIONAL.contains(&RequestKind::from(&request)) {
        return Err(bad(id, "a provider-turn request has no turn"));
    }
    let delivery = match (legacy.rpc_id, legacy.account_key, legacy.connection_id) {
        (Some(rpc), Some(account), Some(connection)) => Some(Delivery {
            rpc_id: match rpc {
                LegacyRpcId::Number(value) => RpcId::Number { value },
                LegacyRpcId::Text(value) => RpcId::Text { value },
            },
            account,
            connection,
            supervisor: legacy.supervisor.map(|s| Supervisor {
                generation: s.generation,
                state_dir: s.state_dir,
                handle: s.handle,
            }),
        }),
        (None, None, None) if legacy.supervisor.is_none() => None,
        _ => return Err(bad(id, "incomplete delivery fields")),
    };
    let deferral = match (legacy.deferred_at, legacy.deferred_by.as_deref()) {
        (Some(at), Some(by)) => Some(Deferral {
            active: legacy.deferred.unwrap_or(false),
            at: Timestamp(at),
            by: principal(id, by, None)?,
        }),
        (None, None) if legacy.deferred != Some(true) => None,
        _ => return Err(bad(id, "incomplete deferral fields")),
    };
    let hidden = match (legacy.deleted_at, legacy.deleted_by.as_deref()) {
        (Some(at), Some(by)) => Some(Hidden {
            at: Timestamp(at),
            by: principal(id, by, None)?,
        }),
        (None, None) if legacy.status != "deleted" => None,
        _ => return Err(bad(id, "incomplete deletion fields")),
    };
    Ok(Approval {
        id: ApprovalId(id.to_owned()),
        agent: AgentId(legacy.agent.unwrap_or_default()),
        turn,
        created: Timestamp(created),
        epoch: legacy.epoch,
        request,
        delivery,
        detail: Detail {
            params: ProviderObject(params),
            preview: legacy.preview.map(ProviderObject),
            answer_error: legacy.answer_error,
            host_close_reason: legacy.close_reason,
        },
        state,
        deferral,
        fire_at: None,
        hidden,
    })
}

/// Convert one legacy record. Tests seed their fixtures with it.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn approval_from_record(id: &str, record: &str) -> StoreResult<Approval> {
    let legacy: Legacy = serde_json::from_str(record)
        .map_err(|error| bad(id, &format!("cannot place a field: {error}")))?;
    convert(id, legacy)
}

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    approvals::create_schema(tx)?;
    if !crate::schema::table_exists(tx, "runtime_requests")? {
        return Ok(());
    }
    let rows = {
        let mut statement = tx.prepare(
            "SELECT id,record FROM runtime_requests WHERE id NOT IN (SELECT id FROM approvals) ORDER BY id",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (id, record) in rows {
        let legacy: Legacy = serde_json::from_str(&record)
            .map_err(|error| bad(&id, &format!("cannot place a field: {error}")))?;
        let row = convert(&id, legacy)?;
        approvals::insert(tx, &row).map_err(|error| bad(&id, &error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;

    #[test]
    fn every_legacy_row_is_kept_and_nothing_is_inferred() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL)",
        )
        .unwrap();
        let rows = [
            json!({"id":"e1","method":"item/commandExecution/requestApproval","agent":"a","status":"answered","createdAt":1.0,
                "params":{"turnId":"t","command":"ls","cwd":"/","reason":"why"},"decision":"accept","answeredBy":"user","answeredAt":2.0}),
            json!({"id":"e2","method":"item/commandExecution/requestApproval","agent":"a","status":"answered","createdAt":1.0,"params":{"turnId":"t"}}),
            json!({"id":"e3","method":"item/commandExecution/requestApproval","agent":"a","status":"uncertain","createdAt":1.0,
                "params":{"turnId":"t"},"decision":"accept","answeredBy":"user","answeredAt":3.0}),
            json!({"id":"q1","method":"agent/asyncQuestion","agent":"a","status":"answered","createdAt":1.0,
                "params":{"questions":[{"id":"0","question":"?","options":[{"label":"x"}],"isSecret":false}]},
                "decision":"answer","answeredBy":"user","answeredAt":2.0,"answerHistory":[{"id":"0","answer":["x"]}]}),
            json!({"id":"q2","method":"item/tool/requestUserInput","agent":"a","status":"deleted","createdAt":1.0,"deletedAt":9.0,"deletedBy":"user",
                "params":{"turnId":"t","questions":[]}}),
            json!({"id":"x1","method":"item/commandExecution/requestApproval","agent":"a","status":"expired","createdAt":1.0,"params":{"turnId":"t"}}),
            json!({"id":"m1","method":"monitor/approve","agent":"a","status":"answered","params":{"monitorId":"m","command":"c"}}),
            json!({"id":"b1","method":"browser/open","agent":"a","status":"expired","closeReason":"stale","params":{"url":"https://x"}}),
            json!({"id":"f1","method":"item/fileChange/requestApproval","agent":"a","status":"resolved","params":{"itemId":"i","turnId":"t"},"preview":{"changes":[{"path":"/p"}]}}),
        ];
        for row in &rows {
            db.execute(
                "INSERT INTO runtime_requests VALUES (?1,?2)",
                [row["id"].as_str().unwrap(), &row.to_string()],
            )
            .unwrap();
        }
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        apply(&tx).unwrap();
        let count = |sql: &str| tx.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT count(*) FROM approvals"), rows.len() as i64);
        assert_eq!(
            count("SELECT count(*) FROM runtime_requests"),
            rows.len() as i64
        );
        let state = |id: &str| {
            approvals::get(&tx, &ApprovalId(id.into()))
                .unwrap()
                .unwrap()
        };
        assert!(matches!(
            state("e1").state,
            ApprovalState::Answered {
                by: Principal::Owner,
                ..
            }
        ));
        assert!(matches!(
            state("e2").state,
            ApprovalState::Closed {
                reason: CloseReason::Legacy,
                ..
            }
        ));
        assert!(matches!(state("e3").state, ApprovalState::Unknown { .. }));
        assert!(matches!(
            state("q1").state,
            ApprovalState::Answered {
                answer: Answer::Answers { .. },
                ..
            }
        ));
        assert!(state("q2").hidden.is_some());
        assert!(matches!(
            state("x1").state,
            ApprovalState::Closed {
                reason: CloseReason::Expired,
                ..
            }
        ));
        assert!(matches!(state("b1").request, Request::BrowserOpen { .. }));
        assert!(
            matches!(state("f1").request, Request::Patch { ref files, .. } if files == &["/p"])
        );
    }

    #[test]
    fn agent_level_rows_keep_no_turn_and_a_turnless_exec_fails() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_requests VALUES('m','{\"id\":\"m\",\"method\":\"monitor/approve\",\"agent\":\"a\",\"status\":\"pending\",\"params\":{}}')").unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        assert_eq!(
            approvals::get(&tx, &ApprovalId("m".into()))
                .unwrap()
                .unwrap()
                .turn,
            None
        );
        drop(tx);
        db.execute_batch("INSERT INTO runtime_requests VALUES('e','{\"id\":\"e\",\"method\":\"item/commandExecution/requestApproval\",\"agent\":\"a\",\"status\":\"pending\",\"params\":{}}')").unwrap();
        let tx = db.transaction().unwrap();
        assert!(apply(&tx).is_err());
    }

    #[test]
    fn a_field_the_converter_cannot_place_fails_the_migration() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_requests VALUES('m','{\"id\":\"m\",\"method\":\"monitor/approve\",\"agent\":\"a\",\"status\":\"pending\",\"params\":{},\"surprise\":1}')").unwrap();
        let tx = db.transaction().unwrap();
        assert!(apply(&tx).is_err());
    }

    #[test]
    fn delivery_details_and_display_content_survive() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL)",
        )
        .unwrap();
        let row = json!({"id":"d","method":"item/commandExecution/requestApproval","agent":"a","status":"pending",
            "createdAt":5.0,"rpcId":22,"accountKey":"acc","connectionId":"conn","epoch":3,
            "supervisor":{"stateDir":"/s","handle":"account:x","generation":9},
            "params":{"threadId":"th","turnId":"t","itemId":"i","command":"ls","cwd":"/","availableDecisions":["accept","cancel"]},
            "preview":{"type":"commandExecution","aggregatedOutput":"out"}});
        db.execute(
            "INSERT INTO runtime_requests VALUES ('d',?1)",
            [row.to_string()],
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        let got = approvals::get(&tx, &ApprovalId("d".into()))
            .unwrap()
            .unwrap();
        assert_eq!(got.epoch, Some(3));
        let delivery = got.delivery.unwrap();
        assert_eq!(delivery.rpc_id, RpcId::Number { value: 22 });
        assert_eq!(delivery.supervisor.unwrap().generation, 9);
        assert_eq!(
            got.detail.params.0["availableDecisions"],
            json!(["accept", "cancel"])
        );
        assert_eq!(got.detail.params.0["threadId"], json!("th"));
        assert!(!got.detail.params.0.contains_key("command"));
        assert_eq!(
            got.detail.preview.unwrap().0["aggregatedOutput"],
            json!("out")
        );
        assert_eq!(got.turn, Some(TurnId("t".into())));
    }

    #[test]
    fn an_unknown_legacy_method_fails_the_migration() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_requests VALUES('z','{\"id\":\"z\",\"method\":\"new/thing\",\"status\":\"pending\"}')").unwrap();
        let tx = db.transaction().unwrap();
        assert!(apply(&tx).is_err());
    }
}
