//! Execution evidence shares the lifecycle transaction; no commits or provider retries here.
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{AgentRecord, StoreError, StoreResult};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_execution_attempts",
        columns: &["id", "run", "record"],
    },
    TableRequirement {
        name: "runtime_execution_inputs",
        columns: &["run", "event"],
    },
    TableRequirement {
        name: "runtime_execution_runs",
        columns: &[
            "id", "agent", "account", "epoch", "thread", "turn", "created", "record",
        ],
    },
];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[ConditionalTableRequirement {
        table_exists: "runtime_execution_runs",
        tables: TABLES,
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
pub struct ExecutionHistoryRepository;
impl ExecutionHistoryRepository {
    /// Immutable lifecycle results live with the attempt whose identity they settle.
    pub fn replay(
        tx: &Transaction<'_>,
        id: &str,
        kind: &str,
        input: &Value,
    ) -> StoreResult<Option<Value>> {
        let Some(saved) = Self::attempt(tx, id)? else {
            return Ok(None);
        };
        let Some(receipt) = saved.get("receipts").and_then(|r| r.get(kind)) else {
            return Ok(None);
        };
        if receipt.get("input") != Some(input) || receipt.get("result").is_none() {
            return Err(Self::invalid("runtime_execution_attempts", id));
        }
        Ok(receipt.get("result").cloned())
    }
    pub fn save_receipt(
        tx: &Transaction<'_>,
        id: &str,
        kind: &str,
        input: Value,
        result: Value,
    ) -> StoreResult<()> {
        let path = format!("$.receipts.{kind}");
        let changed = tx.execute(
            "UPDATE runtime_execution_attempts SET record=json_set(record,?1,json(?2)) WHERE id=?3",
            params![path, json!({"input":input,"result":result}).to_string(), id],
        )?;
        if changed != 1 {
            return Err(Self::invalid("runtime_execution_attempts", id));
        }
        Ok(())
    }
    pub fn attempt(tx: &Transaction<'_>, id: &str) -> StoreResult<Option<Value>> {
        tx.query_row(
            "SELECT record FROM runtime_execution_attempts WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|s| decode("runtime_execution_attempts", id, &s))
        .transpose()
    }
    pub(crate) fn invalid(table: &str, id: &str) -> StoreError {
        StoreError::InvalidSyncObject {
            scope: table.into(),
            id: id.into(),
        }
    }
    pub fn save_claim(
        tx: &Transaction<'_>,
        id: &str,
        identity: &Value,
        result: &Value,
    ) -> StoreResult<()> {
        tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.inputIdentity',json(?1),'$.claimResult',json(?2)) WHERE id=?3",params![identity.to_string(),result.to_string(),id])?;
        Ok(())
    }
    /// Reflect a claimed/submitted/accepted attempt, including Python-written existing runs.
    pub fn record(tx: &Transaction<'_>, agent: &AgentRecord) -> StoreResult<()> {
        let Some(attempt) = agent
            .extra
            .get("startAttempt")
            .filter(|a| a["id"].is_string())
        else {
            return Ok(());
        };
        let id = attempt["id"].as_str().ok_or_else(|| invalid(&agent.id.0))?;
        let account = attempt
            .get("accountKey")
            .or_else(|| agent.extra.get("accountKey"))
            .and_then(Value::as_str)
            .unwrap_or("default");
        let thread = attempt
            .get("threadId")
            .or_else(|| agent.extra.get("threadId"))
            .cloned()
            .unwrap_or(Value::Null);
        let run: Option<Value> = tx.query_row(
            "SELECT r.id,r.record FROM runtime_execution_runs r WHERE r.id=(SELECT run FROM runtime_execution_attempts WHERE id=?1) OR (r.id IN (SELECT run FROM runtime_execution_inputs WHERE event=?2) AND r.agent=?3 AND r.account=?4 AND r.epoch=?5 AND r.turn IS NULL) ORDER BY (r.id=(SELECT run FROM runtime_execution_attempts WHERE id=?1)) DESC, r.created DESC LIMIT 1",
            params![id,attempt["events"][0].as_str(),agent.id.0,account,attempt["epoch"].as_i64()],
            |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?.map(|(id,raw)|decode("runtime_execution_runs",&id,&raw)).transpose()?;
        let mut run = run.unwrap_or_else(||json!({"id":format!("run:{id}"),"agent":agent.id.0,"accountKey":account,
            "epoch":attempt["epoch"],"threadId":thread,"turnId":null,"created":attempt.get("created").cloned().unwrap_or_else(||json!(now())),
            "status":"pending","firstAttemptId":null,"rootAttemptId":null,"latestAttemptId":null}));
        if run["agent"] != agent.id.0 || run["accountKey"] != account || !run["id"].is_string() {
            return Err(invalid(run["id"].as_str().unwrap_or(id)));
        }
        let turn = attempt
            .get("turnId")
            .filter(|v| v.is_string())
            .or_else(|| attempt.get("observedTurnId").filter(|v| v.is_string()));
        let evidence = if attempt["turnId"].is_string() {
            "accepted"
        } else if turn.is_some() {
            "observed"
        } else {
            attempt["executionOutcome"]
                .as_str()
                .unwrap_or(if attempt["submitted"] == true {
                    "unknown"
                } else {
                    "unsent"
                })
        };
        if let Some(turn) = turn {
            run["turnId"] = turn.clone();
            if run["rootAttemptId"].is_null() {
                run["rootAttemptId"] = json!(id);
            }
        }
        run["threadId"] = thread;
        if run["firstAttemptId"].is_null() {
            run["firstAttemptId"] = json!(id);
        }
        run["latestAttemptId"] = json!(id);
        if !terminal(&run["status"]) {
            run["status"] = json!(if turn.is_some() { "running" } else { evidence });
        }
        for key in [
            "disconnectRecovery",
            "restartRecovery",
            "connectionRecovery",
        ] {
            if let Some(marker) = agent.extra.get(key)
                && marker
                    .get("turnId")
                    .is_none_or(|t| t.is_null() || *t == run["turnId"])
                && marker
                    .get("accountKey")
                    .is_none_or(|v| *v == run["accountKey"])
                && marker.get("threadId").is_none_or(|v| *v == run["threadId"])
            {
                run[key] = marker.clone();
            }
        }
        let mut saved = attempt.clone();
        if let Some(previous) = Self::attempt(tx, id)? {
            for key in ["inputIdentity", "claimResult", "receipts"] {
                if let Some(value) = previous.get(key) {
                    saved[key] = value.clone();
                }
            }
        }
        saved["runId"] = run["id"].clone();
        saved["submission"] = json!(evidence);
        saved["prepareAttemptId"] = agent
            .extra
            .get("prepareAttempt")
            .cloned()
            .unwrap_or(Value::Null);
        saved["turnStatus"] = if terminal(&run["status"]) {
            run["status"].clone()
        } else {
            Value::Null
        };
        saved["resultRunId"] = if terminal(&run["status"]) {
            run["id"].clone()
        } else {
            Value::Null
        };
        save_run(tx, &run)?;
        tx.execute("INSERT INTO runtime_execution_attempts(id,run,record) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET run=excluded.run,record=excluded.record",
            params![id,run["id"].as_str(),saved.to_string()])?;
        tx.execute("INSERT OR IGNORE INTO runtime_execution_inputs(run,event) SELECT ?1,value FROM json_each(?2)",
            params![run["id"].as_str(),attempt["events"].to_string()])?;
        Ok(())
    }
    /// Native outcome is retained even when missing transcript support makes the agent fail.
    pub fn terminal(
        tx: &Transaction<'_>,
        agent_id: &str,
        turn_id: &str,
        status: &str,
        error: Option<&Value>,
    ) -> StoreResult<()> {
        let mut stmt = tx.prepare("SELECT r.id,r.record FROM runtime_execution_runs r JOIN runtime_agents a ON a.id=r.agent WHERE r.agent=?1 AND r.turn=?2 AND r.account=COALESCE(json_extract(a.record,'$.accountKey'),'default') AND r.thread IS json_extract(a.record,'$.threadId') LIMIT 2")?;
        let rows = stmt
            .query_map(params![agent_id, turn_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let [(id, raw)] = rows.as_slice() else {
            return Err(Self::invalid(
                "runtime_execution_runs",
                rows.first().map_or(agent_id, |r| r.0.as_str()),
            ));
        };
        let mut run = decode("runtime_execution_runs", id, raw)?;
        if terminal(&run["status"]) {
            if run["status"] != status
                || run.get("error").unwrap_or(&Value::Null) != error.unwrap_or(&Value::Null)
            {
                return Err(Self::invalid("runtime_execution_runs", id));
            }
            return Ok(());
        }
        run["status"] = json!(status);
        run["finished"] = json!(now());
        run["result"] = json!("");
        run["resultItemId"] = Value::Null;
        run["error"] = error.cloned().unwrap_or(Value::Null);
        save_run(tx, &run)
    }
}
fn save_run(tx: &Transaction<'_>, run: &Value) -> StoreResult<()> {
    if terminal(&run["status"]) {
        tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.turnStatus',?1,'$.resultRunId',?2) WHERE run=?2",
            params![run["status"].as_str(),run["id"].as_str()])?;
    }
    tx.execute("INSERT INTO runtime_execution_runs(id,agent,account,epoch,thread,turn,created,record) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET thread=excluded.thread,turn=excluded.turn,record=excluded.record",
        params![run["id"].as_str(),run["agent"].as_str(),run["accountKey"].as_str(),run["epoch"].as_i64(),run["threadId"].as_str(),run["turnId"].as_str(),run["created"].as_f64(),run.to_string()])?;
    Ok(())
}
fn terminal(status: &Value) -> bool {
    matches!(
        status.as_str(),
        Some("completed" | "failed" | "interrupted" | "cancelled")
    )
}
fn invalid(id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: "runtime_execution_runs".into(),
        id: id.into(),
    }
}
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn decode(table: &str, id: &str, raw: &str) -> StoreResult<Value> {
    let invalid = || ExecutionHistoryRepository::invalid(table, id);
    let record: Value = serde_json::from_str(raw).map_err(|_| invalid())?;
    let valid = record.is_object()
        && record["id"] == id
        && record["epoch"].is_i64()
        && if table == "runtime_execution_runs" {
            ["agent", "accountKey", "status"]
                .iter()
                .all(|key| record[*key].is_string())
                && record["created"].as_f64().is_some_and(f64::is_finite)
                && [
                    "threadId",
                    "turnId",
                    "firstAttemptId",
                    "rootAttemptId",
                    "latestAttemptId",
                ]
                .iter()
                .all(|key| record[*key].is_null() || record[*key].is_string())
        } else {
            record["runId"].is_string()
                && record["events"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().all(Value::is_string))
                && record.get("claimResult").is_none_or(Value::is_object)
                && record.get("inputIdentity").is_none_or(Value::is_array)
        };
    if !valid {
        return Err(invalid());
    }
    Ok(record)
}
