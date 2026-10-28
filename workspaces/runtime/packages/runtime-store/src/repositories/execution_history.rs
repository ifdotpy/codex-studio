//! Execution evidence shares the lifecycle transaction; no commits or provider retries here.
use crate::request_results::{
    Admission, AdmittedRequest, RequestResultError, RequestResults, ResultEnvelope,
};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{AgentRecord, StoreError, StoreResult};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    AttemptSubmission, ExecutionReceiptKind, ExecutionStatus, RequestIdentity,
};

pub struct AttemptReceipt {
    pub input: Value,
    pub result: Value,
}

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
#[derive(Clone, Copy)]
enum VmAdmission {
    AuthBootstrap,
    CredentialRefresh,
}
impl VmAdmission {
    fn path(self) -> &'static str {
        match self {
            Self::AuthBootstrap => "$.vm_auth_bootstrap_admission",
            Self::CredentialRefresh => "$.vm_credential_refresh_admission",
        }
    }
}
impl ExecutionHistoryRepository {
    pub fn vm_auth_bootstrap_admission(
        db: &rusqlite::Connection,
        id: &str,
    ) -> StoreResult<Option<AdmittedRequest>> {
        Self::admission_at(db, id, VmAdmission::AuthBootstrap.path())
    }
    pub fn vm_credential_refresh_admission(
        db: &rusqlite::Connection,
        id: &str,
    ) -> StoreResult<Option<AdmittedRequest>> {
        Self::admission_at(db, id, VmAdmission::CredentialRefresh.path())
    }
    pub fn set_vm_auth_bootstrap_admission(
        tx: &Transaction<'_>,
        id: &str,
        source: &AdmittedRequest,
        admitted: &AdmittedRequest,
    ) -> StoreResult<()> {
        Self::set_vm_admission(tx, id, VmAdmission::AuthBootstrap, source, admitted)
    }
    pub fn set_vm_credential_refresh_admission(
        tx: &Transaction<'_>,
        id: &str,
        source: &AdmittedRequest,
        admitted: &AdmittedRequest,
    ) -> StoreResult<()> {
        Self::set_vm_admission(tx, id, VmAdmission::CredentialRefresh, source, admitted)
    }
    fn admission_at(
        db: &rusqlite::Connection,
        id: &str,
        path: &str,
    ) -> StoreResult<Option<AdmittedRequest>> {
        db.query_row(
            "SELECT json_extract(record,?1) FROM runtime_execution_attempts WHERE id=?2",
            params![path, id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .map(|raw| serde_json::from_str(&raw).map_err(StoreError::from))
        .transpose()
    }
    fn set_vm_admission(
        tx: &Transaction<'_>,
        id: &str,
        kind: VmAdmission,
        source: &AdmittedRequest,
        admitted: &AdmittedRequest,
    ) -> StoreResult<()> {
        if Self::admission(tx, id)?.as_ref() != Some(source) {
            return Err(Self::invalid("runtime_execution_attempts", id));
        }
        if let Some(saved) = Self::admission_at(tx, id, kind.path())? {
            return if saved == *admitted {
                Ok(())
            } else {
                Err(Self::invalid("runtime_execution_attempts", id))
            };
        }
        let changed = tx.execute(
            "UPDATE runtime_execution_attempts SET record=json_set(record,?1,json(?2)) WHERE id=?3",
            params![kind.path(), serde_json::to_string(admitted)?, id],
        )?;
        if changed != 1 {
            return Err(Self::invalid("runtime_execution_attempts", id));
        }
        Ok(())
    }
    /// Domain settlement survives response-cache expiry and never grants a fresh dispatch.
    pub fn start_settled(db: &rusqlite::Connection, id: &str) -> StoreResult<bool> {
        let row=db.query_row("SELECT json_extract(record,'$.submission'),json_extract(record,'$.executionOutcome'),json_extract(record,'$.turnStatus') FROM runtime_execution_attempts WHERE id=?1",[id],|row|Ok((row.get::<_,AttemptSubmission>(0)?,row.get::<_,Option<ExecutionStatus>>(1)?,row.get::<_,Option<ExecutionStatus>>(2)?))).optional()?;
        let Some((submission, outcome, turn)) = row else {
            return Ok(false);
        };
        let terminal = |status| {
            matches!(
                status,
                ExecutionStatus::Completed
                    | ExecutionStatus::Failed
                    | ExecutionStatus::Interrupted
                    | ExecutionStatus::Cancelled
            )
        };
        Ok(matches!(
            submission,
            AttemptSubmission::Accepted | AttemptSubmission::Observed | AttemptSubmission::Rejected
        ) || matches!(
            outcome,
            Some(
                ExecutionStatus::Unsent
                    | ExecutionStatus::Accepted
                    | ExecutionStatus::Observed
                    | ExecutionStatus::Rejected
            )
        ) || outcome.is_some_and(terminal)
            || turn.is_some_and(terminal))
    }
    pub fn record_current(tx: &Transaction<'_>, agent: &mut AgentRecord) -> StoreResult<()> {
        Self::record(tx, agent)?;
        if let Some(pointer) = Self::current_pointer(tx, agent)? {
            agent.extra.insert("activeExecution".into(), pointer);
        } else {
            agent.extra.remove("activeExecution");
        }
        Ok(())
    }
    /// Immutable lifecycle results live with the attempt whose identity they settle.
    pub fn replay(
        tx: &Transaction<'_>,
        id: &str,
        kind: ExecutionReceiptKind,
        input: &Value,
    ) -> StoreResult<Option<Value>> {
        match Self::saved_result(tx, id, kind, input)? {
            Some((true, result)) => Ok(Some(result)),
            Some((false, _)) => Err(Self::invalid("runtime_execution_attempts", id)),
            None => Ok(None),
        }
    }
    /// [`Self::replay`] for a start error settlement, whose first outcome wins.
    /// A saved receipt of `kind` with another input means a different outcome
    /// already settled this attempt (for example a thread reply deadline that
    /// held it). The later settlement is stale: it returns `false` and changes
    /// nothing, as Python ignores a result for a finished preparation.
    pub fn replay_settled(
        tx: &Transaction<'_>,
        id: &str,
        kind: ExecutionReceiptKind,
        input: &Value,
    ) -> StoreResult<Option<Value>> {
        Ok(Self::saved_result(tx, id, kind, input)?
            .map(|(same, result)| if same { result } else { Value::Bool(false) }))
    }
    /// The saved result of `kind`, and whether it was saved for `input`.
    fn saved_result(
        tx: &Transaction<'_>,
        id: &str,
        kind: ExecutionReceiptKind,
        input: &Value,
    ) -> StoreResult<Option<(bool, Value)>> {
        let Some(receipt) = Self::receipt(tx, id, kind)? else {
            return Ok(None);
        };
        Ok(Some((receipt.input == *input, receipt.result)))
    }
    pub fn admission(db: &rusqlite::Connection, id: &str) -> StoreResult<Option<AdmittedRequest>> {
        Self::admission_at(db, id, "$.admission")
    }
    /// Read exact cached suboperation evidence; the execution record carries no response bodies.
    pub fn receipt(
        db: &rusqlite::Connection,
        id: &str,
        kind: ExecutionReceiptKind,
    ) -> StoreResult<Option<AttemptReceipt>> {
        let Some(admission) = Self::admission(db, id)? else {
            return Ok(None);
        };
        let key = admission.derive_result("execution-attempt-result", &[kind.as_str()], "");
        let signature: Option<String> = db
            .query_row(
                "SELECT signature FROM request_results WHERE id=?1",
                [key.id()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(signature) = signature else {
            return Ok(None);
        };
        let token =
            admission.derive_result("execution-attempt-result", &[kind.as_str()], &signature);
        let saved = RequestResults::admitted_result(db, &token).map_err(StoreError::from)?;
        saved
            .map(|saved| {
                let envelope: ResultEnvelope<Value> = serde_json::from_str(&saved.result)?;
                Ok(AttemptReceipt {
                    input: serde_json::from_str(&signature)?,
                    result: envelope.reply,
                })
            })
            .transpose()
    }
    pub fn save_receipt(
        tx: &Transaction<'_>,
        id: &str,
        kind: ExecutionReceiptKind,
        input: Value,
        result: Value,
    ) -> StoreResult<()> {
        let admission = Self::admission(tx, id)?
            .ok_or_else(|| Self::invalid("runtime_execution_attempts", id))?;
        let admitted = admission.derive_result(
            "execution-attempt-result",
            &[kind.as_str()],
            &input.to_string(),
        );
        let envelope = ResultEnvelope {
            actor: None,
            reply: result,
            references: vec![id.into()],
        };
        let saved = RequestResults::settle(tx, &admitted, &envelope).map_err(|error| match error {
            RequestResultError::Conflict { .. } => Self::invalid("runtime_execution_attempts", id),
            other @ (RequestResultError::Expired
            | RequestResultError::Sql(_)
            | RequestResultError::Json(_)
            | RequestResultError::Identity(_)) => StoreError::from(other),
        })?;
        if serde_json::from_str::<ResultEnvelope<Value>>(&saved.result)? != envelope {
            return Err(Self::invalid("runtime_execution_attempts", id));
        }
        Ok(())
    }
    pub fn attempt(tx: &rusqlite::Connection, id: &str) -> StoreResult<Option<Value>> {
        tx.query_row(
            "SELECT record FROM runtime_execution_attempts WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|s| decode("runtime_execution_attempts", id, &s))
        .transpose()
    }
    /// Read the current execution by its owner pointer. History order never grants authority.
    pub fn current_run(tx: &Transaction<'_>, agent: &AgentRecord) -> StoreResult<Option<Value>> {
        let Some(id) = agent
            .extra
            .get("activeExecution")
            .and_then(|pointer| pointer["runId"].as_str())
        else {
            return Ok(None);
        };
        tx.query_row(
            "SELECT record FROM runtime_execution_runs WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|raw| {
            let run = decode("runtime_execution_runs", id, &raw)?;
            if run["agent"] != agent.id.0 {
                return Err(Self::invalid("runtime_execution_runs", id));
            }
            Ok(run)
        })
        .transpose()
    }

    pub(crate) fn current_pointer(
        tx: &Transaction<'_>,
        agent: &AgentRecord,
    ) -> StoreResult<Option<Value>> {
        let pointer = if let Some(id) = agent
            .extra
            .get("startAttempt")
            .and_then(|attempt| attempt["id"].as_str())
        {
            Self::attempt(tx, id)?.map(|attempt| json!({"runId":attempt["runId"],"attemptId":id}))
        } else {
            agent
                .extra
                .get("activeExecution")
                .filter(|value| !value.is_null())
                .cloned()
        };
        let Some(pointer) = pointer else {
            return Ok(None);
        };
        let run = pointer["runId"]
            .as_str()
            .ok_or_else(|| Self::invalid("runtime_agents", &agent.id.0))?;
        let scope = tx
            .query_row(
                "SELECT agent,account,epoch FROM runtime_execution_runs WHERE id=?1",
                [run],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Self::invalid("runtime_execution_runs", run))?;
        let account = agent
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default");
        Ok(
            (scope.0 == agent.id.0 && scope.1 == account && scope.2 == agent.epoch)
                .then_some(pointer),
        )
    }
    pub(crate) fn invalid(table: &str, id: &str) -> StoreError {
        StoreError::InvalidSyncObject {
            scope: table.into(),
            id: id.into(),
        }
    }
    pub fn reject_attempt(tx: &Transaction<'_>, id: &str, error: &str) -> StoreResult<()> {
        if let Some(mut saved) = Self::attempt(tx, id)?
            && !matches!(
                serde_json::from_value::<AttemptSubmission>(saved["submission"].clone()),
                Ok(AttemptSubmission::Accepted | AttemptSubmission::Observed)
            )
        {
            saved["submission"] = json!(AttemptSubmission::Rejected);
            saved["error"] = json!(error);
            tx.execute(
                "UPDATE runtime_execution_attempts SET record=?1 WHERE id=?2",
                params![saved.to_string(), id],
            )?;
        }
        Ok(())
    }
    /// Mark an attempt whose input batch returned to the queue before the
    /// provider received it (see `SchedulerRepository::return_unstarted_input`).
    pub fn mark_start_return(tx: &Transaction<'_>, id: &str) -> StoreResult<()> {
        let changed = tx.execute(
            "UPDATE runtime_execution_attempts SET record=json_set(record,'$.startReturned',json('true')) WHERE id=?1",
            [id],
        )?;
        if changed != 1 {
            return Err(Self::invalid("runtime_execution_attempts", id));
        }
        Ok(())
    }
    /// Attempts of the run of attempt `id` whose input batch already returned
    /// to the queue before the provider received it. Unit: attempts.
    pub fn start_returns(tx: &Transaction<'_>, id: &str) -> StoreResult<usize> {
        let count = tx.query_row(
            "SELECT COUNT(*) FROM runtime_execution_attempts WHERE run=(SELECT run FROM runtime_execution_attempts WHERE id=?1) AND json_extract(record,'$.startReturned')=1",
            [id],
            |row| row.get::<_, i64>(0),
        )?;
        usize::try_from(count).map_err(|_| Self::invalid("runtime_execution_attempts", id))
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
    /// Each accepted attempt keeps its own run, so several runs can share one
    /// native turn; [`Self::terminal`] owns that contract.
    pub fn record(tx: &Transaction<'_>, agent: &AgentRecord) -> StoreResult<()> {
        let Some(attempt) = agent
            .extra
            .get("startAttempt")
            .filter(|a| a["id"].is_string())
        else {
            return Self::record_recovery(tx, agent);
        };
        let id = attempt["id"].as_str().ok_or_else(|| invalid(&agent.id.0))?;
        let submitted = super::scheduler::ValidatedAttempt::submission(agent)?;
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
            "status":ExecutionStatus::Pending,"firstAttemptId":null,"rootAttemptId":null,"latestAttemptId":null}));
        if run["agent"] != agent.id.0 || run["accountKey"] != account || !run["id"].is_string() {
            return Err(invalid(run["id"].as_str().unwrap_or(id)));
        }
        let turn = attempt
            .get("turnId")
            .filter(|v| v.is_string())
            .or_else(|| attempt.get("observedTurnId").filter(|v| v.is_string()));
        let evidence = if attempt["turnId"].is_string() {
            AttemptSubmission::Accepted
        } else if turn.is_some() {
            AttemptSubmission::Observed
        } else {
            attempt
                .get("executionOutcome")
                .map(|value| serde_json::from_value::<AttemptSubmission>(value.clone()))
                .transpose()?
                .unwrap_or(
                    if submitted == super::scheduler::StartSubmission::Submitted {
                        AttemptSubmission::Unknown
                    } else {
                        AttemptSubmission::Unsent
                    },
                )
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
            run["status"] = json!(if turn.is_some() {
                ExecutionStatus::Running
            } else {
                match evidence {
                    AttemptSubmission::Accepted => ExecutionStatus::Accepted,
                    AttemptSubmission::Observed => ExecutionStatus::Observed,
                    AttemptSubmission::Pending => ExecutionStatus::Pending,
                    AttemptSubmission::Rejected => ExecutionStatus::Rejected,
                    AttemptSubmission::Unknown => ExecutionStatus::Unknown,
                    AttemptSubmission::Unsent => ExecutionStatus::Unsent,
                }
            });
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
            for key in [
                "inputIdentity",
                "claimResult",
                "terminalRepair",
                "admission",
                "rootRequestId",
                "providerSubmission",
                "firstEventId",
                "vm_auth_bootstrap_admission",
                "vm_credential_refresh_admission",
            ] {
                if let Some(value) = previous.get(key) {
                    saved[key] = value.clone();
                }
            }
        }
        if saved.get("admission").is_none() {
            let root = RequestIdentity::issue_root();
            let identity = RequestIdentity::new("execution-attempt", &[&agent.id.0], &root, &[id])
                .map_err(crate::request_results::RequestResultError::from)
                .map_err(StoreError::from)?;
            let signature =
                json!({"agent":agent.id.0,"epoch":agent.epoch,"attempt":id}).to_string();
            let admission =
                match RequestResults::admit(tx, &identity, &signature, (now() * 1000.0) as i64)
                    .map_err(StoreError::from)?
                {
                    Admission::Fresh(admitted) => admitted,
                    Admission::Replay(_) => {
                        return Err(Self::invalid("runtime_execution_attempts", id));
                    }
                };
            saved["admission"] = serde_json::to_value(admission)?;
            saved["rootRequestId"] = json!(root);
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
    fn record_recovery(tx: &Transaction<'_>, agent: &AgentRecord) -> StoreResult<()> {
        let previous: Option<String> = tx
            .query_row(
                "SELECT json_extract(record,'$.startAttempt.id') FROM runtime_agents WHERE id=?1",
                [&agent.id.0],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let run_id = agent
            .extra
            .get("activeExecution")
            .and_then(|pointer| pointer["runId"].as_str());
        let row:Option<(String,String)> = tx.query_row("SELECT id,record FROM runtime_execution_runs WHERE id=coalesce(?1,(SELECT run FROM runtime_execution_attempts WHERE id=?2))",params![run_id,previous],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((id, raw)) = row else {
            return Ok(());
        };
        let mut run = decode("runtime_execution_runs", &id, &raw)?;
        let before = run.clone();
        for field in [
            "disconnectRecovery",
            "restartRecovery",
            "connectionRecovery",
        ] {
            if let Some(marker) = agent.extra.get(field)
                && marker
                    .get("turnId")
                    .is_none_or(|v| v.is_null() || *v == run["turnId"])
                && marker
                    .get("accountKey")
                    .is_none_or(|v| *v == run["accountKey"])
                && marker.get("threadId").is_none_or(|v| *v == run["threadId"])
            {
                run[field] = marker.clone();
            }
        }
        if run != before {
            save_run(tx, &run)?;
        }
        Ok(())
    }
    /// Native outcome is retained even when missing transcript support makes the agent fail.
    ///
    /// One native turn can own several runs. A `turn/start` that reaches a
    /// provider while that thread's turn is still active is accepted with the
    /// active turn's id (Codex merges the input into the running turn), and
    /// [`Self::record`] keeps one run per accepted attempt. This happens after
    /// a worker restart: the restart marks the agent interrupted, its queued
    /// input starts again, and the provider turn never stopped (agents
    /// d0c9c794 and 50f9a796, 21 October 2026). The turn's outcome settles
    /// every one of its runs. A run that already carries a different outcome
    /// is a real conflict and refuses the whole settlement.
    pub fn terminal(
        tx: &Transaction<'_>,
        agent_id: &str,
        turn_id: &str,
        status: ExecutionStatus,
        error: Option<&Value>,
    ) -> StoreResult<()> {
        let mut stmt = tx.prepare_cached("SELECT r.id,r.record FROM runtime_execution_runs r JOIN runtime_agents a ON a.id=r.agent WHERE r.agent=?1 AND r.turn=?2 AND r.account=a.account_key AND r.thread IS a.thread_id ORDER BY r.created,r.id")?;
        let rows = stmt
            .query_map(params![agent_id, turn_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            return Err(Self::invalid("runtime_execution_runs", agent_id));
        }
        let mut open = Vec::with_capacity(rows.len());
        for (id, raw) in &rows {
            let run = decode("runtime_execution_runs", id, raw)?;
            if !terminal(&run["status"]) {
                open.push(run);
            } else if run["status"] != json!(status)
                || run.get("error").unwrap_or(&Value::Null) != error.unwrap_or(&Value::Null)
            {
                return Err(Self::invalid("runtime_execution_runs", id));
            }
        }
        let finished = now();
        for mut run in open {
            run["status"] = json!(status);
            run["finished"] = json!(finished);
            run["result"] = json!("");
            run["resultItemId"] = Value::Null;
            run["error"] = error.cloned().unwrap_or(Value::Null);
            save_run(tx, &run)?;
        }
        Ok(())
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
        serde_json::from_value::<ExecutionStatus>(status.clone()),
        Ok(ExecutionStatus::Completed
            | ExecutionStatus::Failed
            | ExecutionStatus::Interrupted
            | ExecutionStatus::Cancelled)
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
                && serde_json::from_value::<ExecutionStatus>(record["status"].clone()).is_ok()
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
                && record.get("submission").is_none_or(|value| {
                    serde_json::from_value::<AttemptSubmission>(value.clone()).is_ok()
                })
                && record.get("turnStatus").is_none_or(|value| {
                    value.is_null()
                        || serde_json::from_value::<ExecutionStatus>(value.clone()).is_ok()
                })
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

inventory::submit! { crate::repositories::SchemaRegistration { repository: "execution_history", requirements: &REQUIREMENTS } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_execution_pointer_follows_account_and_epoch_but_preserves_source_thread() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
            CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,record TEXT);
            CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);").expect("owners");
        let run = json!({"id":"run","agent":"agent","accountKey":"default","epoch":1,"status":"completed","created":1.0,"threadId":"source","turnId":"turn","firstAttemptId":null,"rootAttemptId":null,"latestAttemptId":null});
        db.execute(
            "INSERT INTO runtime_execution_runs VALUES('run','agent','default',1,?1)",
            [run.to_string()],
        )
        .expect("history");
        let tx = db.transaction().expect("transaction");
        let pointer = json!({"runId":"run","attemptId":null});
        let mut agent=AgentRecord::from_value(json!({"id":"agent","rootId":"agent","provider":"codex","epoch":1,"status":"idle","threadId":"destination","activeExecution":pointer})).expect("current owner");
        ExecutionHistoryRepository::record_current(&tx, &mut agent).expect("same account/epoch");
        assert_eq!(
            agent.extra["activeExecution"], pointer,
            "a repaired thread retains its source execution identity"
        );
        agent.epoch = 2;
        ExecutionHistoryRepository::record_current(&tx, &mut agent).expect("epoch advances");
        assert!(!agent.extra.contains_key("activeExecution"));
        agent.epoch = 1;
        agent.extra.insert("activeExecution".into(), pointer);
        agent
            .extra
            .insert("accountKey".into(), json!("another-account"));
        ExecutionHistoryRepository::record_current(&tx, &mut agent).expect("account advances");
        assert!(!agent.extra.contains_key("activeExecution"));
        assert_eq!(
            tx.query_row("SELECT COUNT(*) FROM runtime_execution_runs", [], |row| row
                .get::<_, i64>(0))
                .expect("history retained"),
            1
        );
    }

    #[test]
    fn attempt_replay_lives_in_request_results_and_keeps_exact_outcome() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        crate::receipt_schema::create(&db).expect("receipt schema");
        db.execute_batch("CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT);
            CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);
            INSERT INTO runtime_execution_attempts VALUES('attempt','run','{\"id\":\"attempt\",\"runId\":\"run\",\"epoch\":1,\"events\":[]}');")
            .expect("execution owner");
        let tx = db.transaction().expect("transaction");
        let root = RequestIdentity::issue_root();
        let identity = RequestIdentity::new("execution-attempt", &["agent"], &root, &["attempt"])
            .expect("identity");
        let Admission::Fresh(admission) =
            RequestResults::admit(&tx, &identity, "attempt", (now() * 1000.0) as i64)
                .expect("admission")
        else {
            panic!("fresh")
        };
        tx.execute(
            "UPDATE runtime_execution_attempts SET record=json_set(record,'$.admission',json(?1))",
            [serde_json::to_value(&admission).expect("token").to_string()],
        )
        .expect("persist admission");
        let final_effect = admission.derive_result("provider-call", &["bootstrap"], "fingerprint");
        ExecutionHistoryRepository::set_vm_auth_bootstrap_admission(
            &tx,
            "attempt",
            &admission,
            &final_effect,
        )
        .expect("reserve exact source");
        assert_eq!(
            ExecutionHistoryRepository::vm_auth_bootstrap_admission(&tx, "attempt")
                .expect("final pointer"),
            Some(final_effect.clone())
        );
        ExecutionHistoryRepository::set_vm_auth_bootstrap_admission(
            &tx,
            "attempt",
            &admission,
            &final_effect,
        )
        .expect("same final pointer");
        let wrong_source = admission.derive_result("wrong-source", &[], "different");
        assert!(
            ExecutionHistoryRepository::set_vm_auth_bootstrap_admission(
                &tx,
                "attempt",
                &wrong_source,
                &final_effect
            )
            .is_err()
        );
        assert!(
            ExecutionHistoryRepository::set_vm_auth_bootstrap_admission(
                &tx,
                "attempt",
                &admission,
                &wrong_source
            )
            .is_err()
        );
        tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.submission','unknown') WHERE id='attempt'",[]).expect("unknown start");
        assert!(
            !ExecutionHistoryRepository::start_settled(&tx, "attempt")
                .expect("unknown stays unresolved")
        );
        for status in ["accepted", "observed", "rejected"] {
            tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.submission',?1) WHERE id='attempt'",[status]).expect("settled owner");
            assert!(
                ExecutionHistoryRepository::start_settled(&tx, "attempt")
                    .expect("settlement is domain proof")
            );
        }
        let input = json!({"agent":"agent","epoch":1,"input":{"reason":"held"}});
        let reply = json!({"bound":true,"interrupt":false});
        ExecutionHistoryRepository::save_receipt(
            &tx,
            "attempt",
            ExecutionReceiptKind::StartResult,
            input.clone(),
            reply.clone(),
        )
        .expect("settle");
        assert_eq!(
            ExecutionHistoryRepository::replay(
                &tx,
                "attempt",
                ExecutionReceiptKind::StartResult,
                &input
            )
            .expect("replay"),
            Some(reply)
        );
        assert_eq!(
            ExecutionHistoryRepository::replay_settled(
                &tx,
                "attempt",
                ExecutionReceiptKind::StartResult,
                &json!({"changed":true})
            )
            .expect("stale settlement"),
            Some(json!(false))
        );
        assert!(
            ExecutionHistoryRepository::replay(
                &tx,
                "attempt",
                ExecutionReceiptKind::StartResult,
                &json!({"changed":true})
            )
            .is_err()
        );
        assert!(matches!(
            ExecutionHistoryRepository::save_receipt(
                &tx,
                "attempt",
                ExecutionReceiptKind::StartResult,
                json!({"changed":true}),
                json!(false),
            ),
            Err(StoreError::InvalidSyncObject { scope, id })
                if scope == "runtime_execution_attempts" && id == "attempt"
        ));
        assert!(
            ExecutionHistoryRepository::attempt(&tx, "attempt")
                .expect("owner")
                .expect("attempt")
                .get("receipts")
                .is_none()
        );
        let count: i64 = tx
            .query_row("SELECT COUNT(*) FROM request_results", [], |row| row.get(0))
            .expect("results");
        assert_eq!(count, 1);
    }

    #[test]
    fn execution_decode_rejects_an_unknown_status() {
        let run = json!({"id":"run","agent":"agent","accountKey":"default","epoch":1,"status":"invented","created":1.0});
        assert!(decode("runtime_execution_runs", "run", &run.to_string()).is_err());
    }
}
