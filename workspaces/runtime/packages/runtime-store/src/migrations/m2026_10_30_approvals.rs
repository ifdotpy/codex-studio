//! Copies every `runtime_requests` row into the typed `approvals` table in one
//! transaction. The legacy table stays untouched and read-only (nothing is
//! deleted). A legacy row that never recorded its answer becomes
//! `Closed { reason: Legacy }`: nothing is inferred.
use rusqlite::Transaction;
use serde_json::Value;
use studio_runtime_schema::approvals::{
    AgentId, Answer, Approval, ApprovalId, ApprovalState, CloseReason, CommandId, Decision,
    Principal, Question, QuestionAnswer, QuestionOption, Request, Timestamp, TurnId,
};

use crate::approvals;
use crate::{StoreError, StoreResult};

fn bad(id: &str, what: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("approvals migration: request {id}: {what}"))
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn questions(params: &Value) -> Vec<Question> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(index, q)| Question {
                    id: text(q, "id").unwrap_or_else(|| index.to_string()),
                    question: text(q, "question").unwrap_or_default(),
                    header: text(q, "header"),
                    options: q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .map(|o| QuestionOption {
                                    label: text(o, "label").unwrap_or_default(),
                                    description: text(o, "description"),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    is_secret: q.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn request(id: &str, record: &Value) -> StoreResult<Request> {
    let params = record.get("params").unwrap_or(&Value::Null);
    let preview = record.get("preview").unwrap_or(&Value::Null);
    let method = text(record, "method").ok_or_else(|| bad(id, "no method"))?;
    Ok(match method.as_str() {
        "item/commandExecution/requestApproval" => Request::Exec {
            command: text(params, "command")
                .or_else(|| text(preview, "command"))
                .unwrap_or_default(),
            cwd: text(params, "cwd")
                .or_else(|| text(preview, "cwd"))
                .unwrap_or_default(),
            reason: text(params, "reason"),
        },
        "item/fileChange/requestApproval" => Request::Patch {
            item: text(params, "itemId").unwrap_or_default(),
            files: preview
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| changes.iter().filter_map(|c| text(c, "path")).collect())
                .unwrap_or_default(),
            reason: text(params, "reason"),
        },
        "item/permissions/requestApproval" => Request::Permissions {
            requested: params
                .get("permissions")
                .and_then(Value::as_object)
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default(),
        },
        "mcpServer/elicitation/request" => Request::Elicitation {
            schema: params
                .get("requestedSchema")
                .cloned()
                .unwrap_or(Value::Null),
        },
        "item/tool/requestUserInput" => Request::Question {
            questions: questions(params),
        },
        "agent/asyncQuestion" => Request::AgentQuestion {
            questions: questions(params),
        },
        "monitor/approve" => Request::MonitorStart {
            monitor: text(params, "monitorId").unwrap_or_default(),
            command: text(params, "command").unwrap_or_default(),
        },
        "browser/open" => Request::BrowserOpen {
            url: text(params, "url").unwrap_or_default(),
        },
        other => return Err(bad(id, &format!("unmapped method {other}"))),
    })
}

fn answer(record: &Value) -> Option<Answer> {
    match record.get("decision").and_then(Value::as_str)? {
        "accept" => Some(Answer::Decision {
            decision: Decision::Accept,
        }),
        "decline" => Some(Answer::Decision {
            decision: Decision::Decline,
        }),
        "cancel" => Some(Answer::Decision {
            decision: Decision::Cancel,
        }),
        "answer" => {
            let history = record.get("answerHistory")?.as_array()?;
            let answers: Vec<_> = history
                .iter()
                .filter_map(|entry| {
                    Some(QuestionAnswer {
                        id: text(entry, "id")?,
                        answer: entry
                            .get("answer")?
                            .as_array()?
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect(),
                    })
                })
                .collect();
            (!answers.is_empty()).then_some(Answer::Answers { answers })
        }
        _ => None,
    }
}

fn time(record: &Value, key: &str) -> Option<f64> {
    record.get(key).and_then(Value::as_f64)
}

fn convert(id: &str, record: &Value) -> StoreResult<Approval> {
    let request = request(id, record)?;
    let created = time(record, "createdAt").unwrap_or(0.0);
    let when = |keys: &[&str]| {
        Timestamp(
            keys.iter()
                .find_map(|key| time(record, key))
                .unwrap_or(created),
        )
    };
    let status = text(record, "status").ok_or_else(|| bad(id, "no status"))?;
    // An answer is kept only when it was recorded and fits the request.
    let given = answer(record).filter(|a| request.accepts(a).is_ok());
    let legacy = ApprovalState::Closed {
        reason: CloseReason::Legacy,
        at: when(&["resolvedAt", "answeredAt", "deletedAt"]),
    };
    let state = match (status.as_str(), given) {
        ("pending", _) => ApprovalState::Pending,
        ("answering", Some(answer)) => ApprovalState::Answering {
            answer,
            command: CommandId(id.to_owned()),
        },
        ("answered" | "deleted", Some(answer))
            if record.get("answeredBy").and_then(Value::as_str) == Some("user") =>
        {
            ApprovalState::Answered {
                answer,
                by: Principal::Owner,
                at: when(&["answeredAt"]),
            }
        }
        ("uncertain", Some(answer)) => ApprovalState::Unknown {
            answer,
            since: when(&["answeredAt"]),
        },
        ("expired", _) => ApprovalState::Closed {
            reason: CloseReason::Expired,
            at: when(&["resolvedAt", "deferredAt", "deletedAt"]),
        },
        ("resolved", _) => ApprovalState::Closed {
            reason: CloseReason::ProviderResolved,
            at: when(&["resolvedAt", "answeredAt"]),
        },
        ("answering" | "answered" | "deleted" | "uncertain" | "blocked", _) => legacy,
        (other, _) => return Err(bad(id, &format!("unknown status {other}"))),
    };
    let turn = text(record, "turnId")
        .or_else(|| text(record.get("params").unwrap_or(&Value::Null), "turnId"))
        .unwrap_or_default();
    Ok(Approval {
        id: ApprovalId(id.to_owned()),
        agent: AgentId(text(record, "agent").unwrap_or_default()),
        turn: TurnId(turn),
        created: Timestamp(created),
        request,
        state,
        fire_at: None,
        hidden: status == "deleted" || record.get("deletedAt").is_some_and(|v| !v.is_null()),
    })
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
        let record: Value = serde_json::from_str(&record)
            .map_err(|error| bad(&id, &format!("invalid JSON: {error}")))?;
        let row = convert(&id, &record)?;
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
            json!({"id":"q2","method":"item/tool/requestUserInput","agent":"a","status":"deleted","createdAt":1.0,"deletedAt":9.0,
                "params":{"turnId":"t","questions":[]}}),
            json!({"id":"x1","method":"item/commandExecution/requestApproval","agent":"a","status":"expired","createdAt":1.0,"params":{}}),
            json!({"id":"m1","method":"monitor/approve","agent":"a","status":"answered","params":{"monitorId":"m","command":"c"}}),
            json!({"id":"b1","method":"browser/open","agent":"a","status":"expired","closeReason":"stale","params":{"url":"https://x"}}),
            json!({"id":"f1","method":"item/fileChange/requestApproval","agent":"a","status":"resolved","params":{"itemId":"i"},"preview":{"changes":[{"path":"/p"}]}}),
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
        assert!(state("q2").hidden);
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
    fn an_unknown_legacy_method_fails_the_migration() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_requests VALUES('z','{\"method\":\"new/thing\",\"status\":\"pending\"}')").unwrap();
        let tx = db.transaction().unwrap();
        assert!(apply(&tx).is_err());
    }
}
