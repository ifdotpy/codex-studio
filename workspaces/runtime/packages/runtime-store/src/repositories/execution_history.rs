//! Execution evidence shares the lifecycle transaction; no commits or provider retries here.
use crate::request_results::{
    Admission, AdmittedRequest, RequestResultError, RequestResults, ResultEnvelope,
};
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{AgentRecord, StoreError, StoreResult};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    AttemptSubmission, ExecutionReceiptKind, ExecutionStatus, RequestIdentity,
};
use studio_runtime_schema::{json_object::JsonObject, queue_command::Field};

pub struct AttemptReceipt<I> {
    pub input: I,
    pub result: Value,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRun<T = String, E = serde_json::Number> {
    pub id: String,
    pub agent: String,
    pub account_key: String,
    pub epoch: E,
    pub created: serde_json::Number,
    #[serde(deserialize_with = "string_status")]
    pub status: ExecutionStatus,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub thread_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub turn_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub first_attempt_id: Field<T>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub root_attempt_id: Field<T>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    pub latest_attempt_id: Field<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
fn string_status<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ExecutionStatus, D::Error> {
    let status = String::deserialize(deserializer)?;
    status
        .parse()
        .map_err(|_| serde::de::Error::custom("invalid execution status"))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionAttempt {
    #[serde(flatten)]
    pub start: crate::StartAttemptView,
    pub run_id: String,
    #[serde(default, deserialize_with = "submission")]
    pub submission: Option<AttemptSubmission>,
    pub turn_status: Option<ExecutionStatus>,
    pub input_identity: Option<Box<RawValue>>,
    pub claim_result: Option<Box<RawValue>>,
    pub provider_submission: Option<Box<RawValue>>,
    pub terminal_repair: Option<Box<RawValue>>,
    #[serde(skip)]
    pub snapshot: Option<Box<RawValue>>,
}

fn submission<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<AttemptSubmission>, D::Error> {
    AttemptSubmission::deserialize(deserializer).map(Some)
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
        ) || outcome.is_some_and(execution_terminal)
            || turn.is_some_and(execution_terminal))
    }
    pub fn record_current(tx: &Transaction<'_>, agent: &mut AgentRecord) -> StoreResult<()> {
        Self::record(tx, agent)?;
        if let Some((run, attempt)) = Self::current_pointer(tx, agent)? {
            agent.set_active_execution_run_id(&run)?;
            agent.set_active_execution_attempt_id(attempt.as_deref())?;
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
        let Some(receipt) = Self::receipt::<Value>(tx, id, kind)? else {
            return Ok(None);
        };
        Ok(Some((receipt.input == *input, receipt.result)))
    }
    pub fn admission(db: &rusqlite::Connection, id: &str) -> StoreResult<Option<AdmittedRequest>> {
        Self::admission_at(db, id, "$.admission")
    }
    /// Read exact cached suboperation evidence; the execution record carries no response bodies.
    pub fn receipt<I: serde::de::DeserializeOwned>(
        db: &rusqlite::Connection,
        id: &str,
        kind: ExecutionReceiptKind,
    ) -> StoreResult<Option<AttemptReceipt<I>>> {
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
        let saved =
            RequestResults::settle(tx, &admitted, &envelope).map_err(|error| match error {
                RequestResultError::Conflict { .. } => {
                    Self::invalid("runtime_execution_attempts", id)
                }
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
    pub fn attempt(tx: &rusqlite::Connection, id: &str) -> StoreResult<Option<ExecutionAttempt>> {
        tx.query_row(
            "SELECT record FROM runtime_execution_attempts WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|s| {
            let mut attempt: ExecutionAttempt = serde_json::from_str(&s)
                .map_err(|_| Self::invalid("runtime_execution_attempts", id))?;
            if attempt.start.id.as_deref() != Some(id)
                || attempt.start.epoch.is_none()
                || attempt.start.events.is_none()
            {
                return Err(Self::invalid("runtime_execution_attempts", id));
            }
            attempt.snapshot = Some(serde_json::from_str(&s)?);
            Ok(attempt)
        })
        .transpose()
    }
    /// Read the current execution by its owner pointer. History order never grants authority.
    pub fn current_run(
        tx: &Transaction<'_>,
        agent: &AgentRecord,
    ) -> StoreResult<Option<ExecutionRun>> {
        let Ok(Some(pointer)) = agent.active_execution() else {
            return Ok(None);
        };
        let Some(id) = pointer.run_id.as_deref() else {
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
            if run.agent != agent.id.0 {
                return Err(Self::invalid("runtime_execution_runs", id));
            }
            Ok(run)
        })
        .transpose()
    }

    pub(crate) fn current_pointer(
        tx: &Transaction<'_>,
        agent: &AgentRecord,
    ) -> StoreResult<Option<(String, Option<String>)>> {
        let start = agent
            .start_attempt()
            .map_err(|_| Self::invalid("runtime_agents", &agent.id.0))?;
        let pointer = if let Some(id) = start.as_ref().and_then(|attempt| attempt.id.as_deref()) {
            Self::attempt(tx, id)?.map(|attempt| (attempt.run_id, Some(id.to_owned())))
        } else {
            agent
                .active_execution()
                .map_err(|_| Self::invalid("runtime_agents", &agent.id.0))?
                .map(|pointer| (pointer.run_id, pointer.attempt_id))
                .map(|(run, attempt)| {
                    run.map(|run| (run, attempt))
                        .ok_or_else(|| Self::invalid("runtime_agents", &agent.id.0))
                })
                .transpose()?
        };
        let Some((run, attempt)) = pointer else {
            return Ok(None);
        };
        let scope = tx
            .query_row(
                "SELECT agent,account,epoch FROM runtime_execution_runs WHERE id=?1",
                [&run],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Self::invalid("runtime_execution_runs", &run))?;
        let account = agent.account_key.as_str();
        Ok(
            (scope.0 == agent.id.0 && scope.1 == account && scope.2 == agent.epoch)
                .then_some((run, attempt)),
        )
    }
    pub(crate) fn invalid(table: &str, id: &str) -> StoreError {
        StoreError::InvalidSyncObject {
            scope: table.into(),
            id: id.into(),
        }
    }
    pub fn reject_attempt(tx: &Transaction<'_>, id: &str, error: &str) -> StoreResult<()> {
        if let Some(saved) = Self::attempt(tx, id)?
            && !matches!(
                saved.submission,
                Some(AttemptSubmission::Accepted | AttemptSubmission::Observed)
            )
        {
            tx.execute(
                "UPDATE runtime_execution_attempts SET record=json_set(record,'$.submission',?1,'$.error',?2) WHERE id=?3",
                params![AttemptSubmission::Rejected, error, id],
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
    pub fn save_claim<T: serde::Serialize>(
        tx: &Transaction<'_>,
        id: &str,
        identity: &T,
        result: &Value,
    ) -> StoreResult<()> {
        tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.inputIdentity',json(?1),'$.claimResult',json(?2)) WHERE id=?3",params![serde_json::to_string(identity)?,result.to_string(),id])?;
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
        let account = match attempt.get("accountKey") {
            Some(value) => value.as_str().unwrap_or("default"),
            None => agent.account_key.as_str(),
        };
        let thread = attempt
            .get("threadId")
            .cloned()
            .or_else(|| agent.thread_id.clone().map(Value::String))
            .unwrap_or(Value::Null);
        let run: Option<ExecutionRun> = tx.query_row(
            "SELECT r.id,r.record FROM runtime_execution_runs r WHERE r.id=(SELECT run FROM runtime_execution_attempts WHERE id=?1) OR (r.id IN (SELECT run FROM runtime_execution_inputs WHERE event=?2) AND r.agent=?3 AND r.account=?4 AND r.epoch=?5 AND r.turn IS NULL) ORDER BY (r.id=(SELECT run FROM runtime_execution_attempts WHERE id=?1)) DESC, r.created DESC LIMIT 1",
            params![id,attempt["events"][0].as_str(),agent.id.0,account,attempt["epoch"].as_i64()],
            |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?.map(|(id,raw)|decode("runtime_execution_runs",&id,&raw)).transpose()?;
        let mut run = match run {
            Some(run) => run,
            None => ExecutionRun {
                id: format!("run:{id}"),
                agent: agent.id.0.clone(),
                account_key: account.to_owned(),
                epoch: attempt["epoch"]
                    .as_number()
                    .cloned()
                    .ok_or_else(|| invalid(id))?,
                created: match attempt.get("created") {
                    Some(value) => value.as_number().cloned().ok_or_else(|| invalid(id))?,
                    None => serde_json::Number::from_f64(now()).ok_or_else(|| invalid(id))?,
                },
                status: ExecutionStatus::Pending,
                thread_id: Field::Null,
                turn_id: Field::Null,
                first_attempt_id: Field::Null,
                root_attempt_id: Field::Null,
                latest_attempt_id: Field::Null,
                extra: serde_json::Map::new(),
            },
        };
        if run.agent != agent.id.0 || run.account_key != account {
            return Err(invalid(&run.id));
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
                .map(|value| {
                    let status = value.as_str().ok_or_else(|| {
                        <serde_json::Error as serde::de::Error>::custom(
                            "execution outcome must be a string",
                        )
                    })?;
                    AttemptSubmission::deserialize(serde::de::value::StrDeserializer::<
                        serde_json::Error,
                    >::new(status))
                })
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
            run.turn_id = Field::Value(turn.as_str().expect("selected string").to_owned());
            if run.root_attempt_id.value().is_none() {
                run.root_attempt_id = Field::Value(id.to_owned());
            }
        }
        run.thread_id = match thread {
            Value::String(value) => Field::Value(value),
            Value::Null => Field::Null,
            Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
                return Err(invalid(&run.id));
            }
        };
        if run.first_attempt_id.value().is_none() {
            run.first_attempt_id = Field::Value(id.to_owned());
        }
        run.latest_attempt_id = Field::Value(id.to_owned());
        if !execution_terminal(run.status) {
            run.status = if turn.is_some() {
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
            };
        }
        merge_recovery(&mut run, agent);
        let mut saved: std::collections::BTreeMap<String, Box<RawValue>> =
            serde_json::from_str(&attempt.to_string())?;
        if let Some(previous) = Self::attempt(tx, id)? {
            let previous: std::collections::BTreeMap<String, Box<RawValue>> = serde_json::from_str(
                previous
                    .snapshot
                    .as_ref()
                    .ok_or_else(|| Self::invalid("runtime_execution_attempts", id))?
                    .get(),
            )?;
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
                    saved.insert(key.to_owned(), value.clone());
                }
            }
        }
        if !saved.contains_key("admission") {
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
            saved.insert(
                "admission".into(),
                serde_json::value::to_raw_value(&admission)?,
            );
            saved.insert(
                "rootRequestId".into(),
                serde_json::value::to_raw_value(&root)?,
            );
        }
        let run_id = &run.id;
        let terminal = execution_terminal(run.status);
        saved.insert("runId".into(), serde_json::value::to_raw_value(&run_id)?);
        saved.insert(
            "submission".into(),
            serde_json::value::to_raw_value(&evidence)?,
        );
        saved.insert(
            "prepareAttemptId".into(),
            serde_json::value::to_raw_value(&agent.extra.get("prepareAttempt"))?,
        );
        saved.insert(
            "turnStatus".into(),
            serde_json::value::to_raw_value(&terminal.then_some(run.status))?,
        );
        saved.insert(
            "resultRunId".into(),
            serde_json::value::to_raw_value(&terminal.then_some(run_id))?,
        );
        save_run(tx, &run)?;
        tx.execute("INSERT INTO runtime_execution_attempts(id,run,record) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET run=excluded.run,record=excluded.record",
            params![id,run_id,serde_json::to_string(&saved)?])?;
        tx.execute("INSERT OR IGNORE INTO runtime_execution_inputs(run,event) SELECT ?1,value FROM json_each(?2)",
            params![run_id,attempt["events"].to_string()])?;
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
        merge_recovery(&mut run, agent);
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
            if !execution_terminal(run.status) {
                open.push(run);
            } else if run.status != status
                || run.extra.get("error").unwrap_or(&Value::Null) != error.unwrap_or(&Value::Null)
            {
                return Err(Self::invalid("runtime_execution_runs", id));
            }
        }
        let finished = now();
        for mut run in open {
            run.status = status;
            run.extra.insert("finished".into(), json!(finished));
            run.extra.insert("result".into(), json!(""));
            run.extra.insert("resultItemId".into(), Value::Null);
            run.extra
                .insert("error".into(), error.cloned().unwrap_or(Value::Null));
            save_run(tx, &run)?;
        }
        Ok(())
    }
}
fn save_run(tx: &Transaction<'_>, run: &ExecutionRun) -> StoreResult<()> {
    if execution_terminal(run.status) {
        tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.turnStatus',?1,'$.resultRunId',?2) WHERE run=?2", params![run.status.as_str(), run.id])?;
    }
    tx.execute("INSERT INTO runtime_execution_runs(id,agent,account,epoch,thread,turn,created,record) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET thread=excluded.thread,turn=excluded.turn,record=excluded.record",
        params![run.id,run.agent,run.account_key,run.epoch.as_i64(),run.thread_id.value(),run.turn_id.value(),run.created.as_f64(),serde_json::to_string(run)?])?;
    Ok(())
}
fn merge_recovery(run: &mut ExecutionRun, agent: &AgentRecord) {
    for key in [
        "disconnectRecovery",
        "restartRecovery",
        "connectionRecovery",
    ] {
        if let Some(marker) = agent.extra.get(key)
            && marker.get("turnId").is_none_or(|v| {
                v.is_null()
                    || v.as_str()
                        .is_some_and(|turn| Some(turn) == run.turn_id.value().map(String::as_str))
            })
            && marker
                .get("accountKey")
                .is_none_or(|v| v.as_str() == Some(run.account_key.as_str()))
            && marker
                .get("threadId")
                .is_none_or(|v| match run.thread_id.value() {
                    Some(thread) => v.as_str() == Some(thread),
                    None => v.is_null(),
                })
        {
            run.extra.insert(key.into(), marker.clone());
        }
    }
}
fn execution_terminal(status: ExecutionStatus) -> bool {
    matches!(
        status,
        ExecutionStatus::Completed
            | ExecutionStatus::Failed
            | ExecutionStatus::Interrupted
            | ExecutionStatus::Cancelled
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

fn decode(table: &str, id: &str, raw: &str) -> StoreResult<ExecutionRun> {
    let invalid = || ExecutionHistoryRepository::invalid(table, id);
    let JsonObject(run): JsonObject<ExecutionRun> =
        serde_json::from_str(raw).map_err(|_| invalid())?;
    if run.id != id
        || run.epoch.as_i64().is_none()
        || !run.created.as_f64().is_some_and(f64::is_finite)
    {
        return Err(invalid());
    }
    Ok(run)
}

inventory::submit! { crate::repositories::SchemaRegistration { repository: "execution_history", requirements: &REQUIREMENTS } }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_recovery_turn_does_not_match_missing_run_turn() {
        let raw = r#"{"id":"run","agent":"agent","accountKey":"default","epoch":1,"created":1,"status":"running","turnId":null}"#;
        let mut run = decode("runtime_execution_runs", "run", raw).unwrap();
        let agent = AgentRecord::from_value(json!({"accountKey":"default","id":"agent","rootId":"agent","provider":"codex","epoch":1,"status":"idle","disconnectRecovery":{"turnId":false}})).unwrap();
        merge_recovery(&mut run, &agent);
        assert!(!run.extra.contains_key("disconnectRecovery"));
        for (marker, accepted) in [
            (json!({}), true),
            (json!({"turnId":null}), true),
            (json!({"turnId":[]}), false),
            (json!({"turnId":"other"}), false),
        ] {
            run.extra.clear();
            let agent = AgentRecord::from_value(json!({"accountKey":"default","id":"agent","rootId":"agent","provider":"codex","epoch":1,"status":"idle","disconnectRecovery":marker})).unwrap();
            merge_recovery(&mut run, &agent);
            assert_eq!(run.extra.contains_key("disconnectRecovery"), accepted);
        }
    }

    #[test]
    fn execution_run_wire_tracks_known_schema_fields() {
        let schema: Value = serde_json::from_str(include_str!(
            "../../../runtime-schema/src/schemas/persisted-execution-run-record.schema.json"
        ))
        .unwrap();
        let properties = schema["properties"].as_object().unwrap();
        let names: Vec<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "accountKey",
                "agent",
                "created",
                "epoch",
                "id",
                "latestAttemptId",
                "status",
                "threadId",
                "turnId"
            ]
        );
        let base = json!({"id":"run","agent":"agent","accountKey":"default","epoch":1,"created":1,"status":"running"});
        for (field, value) in [
            ("id", json!(null)),
            ("agent", json!(false)),
            ("accountKey", json!([])),
            ("created", json!("1")),
            ("epoch", json!("1")),
            ("epoch", json!(-1)),
            ("epoch", json!(1.5)),
            ("epoch", json!(1.0)),
            (
                "epoch",
                json!(studio_runtime_schema::shared_values::MAX_SAFE_INTEGER),
            ),
            (
                "epoch",
                json!(studio_runtime_schema::shared_values::MAX_SAFE_INTEGER + 1),
            ),
            ("status", json!("future")),
            ("threadId", json!(3)),
            ("turnId", json!({})),
            ("latestAttemptId", json!([])),
            ("threadId", json!(null)),
            ("turnId", json!("turn")),
            ("latestAttemptId", json!(null)),
            ("firstAttemptId", json!({"opaque":true})),
        ] {
            let mut record = base.clone();
            record[field] = value;
            let schema_valid =
                studio_runtime_schema::validate("PersistedExecutionRunRecord", &record).unwrap();
            let typed = JsonObject::<
                ExecutionRun<Value, studio_runtime_schema::AgentCounter>,
            >::deserialize(&record)
            .is_ok();
            assert_eq!(typed, schema_valid, "{record}");
            assert_eq!(
                crate::ExecutionRunRecord::from_value(record.clone()).is_ok(),
                schema_valid,
                "stored owner: {record}"
            );
        }
    }

    #[test]
    fn persisted_run_preserves_nullable_fields_numbers_and_unconstrained_evidence() {
        let raw = r#"{"id":"run","agent":"agent","accountKey":"default","epoch":1,"created":1,"status":"running","threadId":null,"rootAttemptId":{"future":true},"extension":[null,4]}"#;
        let JsonObject(wire): JsonObject<ExecutionRun<Value>> = serde_json::from_str(raw).unwrap();
        let output = serde_json::to_string(&wire).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        let roundtrip: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(roundtrip, original);
        assert!(!output.contains("latestAttemptId"));
        for number in ["1.0", "9007199254740993"] {
            let variant = raw.replace("\"created\":1,", &format!("\"created\":{number},"));
            let JsonObject(run): JsonObject<ExecutionRun<Value>> =
                serde_json::from_str(&variant).unwrap();
            let written = serde_json::to_string(&run).unwrap();
            assert!(
                written.contains(&format!("\"created\":{number}")),
                "{written}"
            );
        }
        assert!(decode("runtime_execution_runs", "run", raw).is_err());
        let null_raw = raw.replace(r#"{"future":true}"#, "null");
        assert!(decode("runtime_execution_runs", "run", &null_raw).is_ok());
        let negative_epoch = null_raw.replace(r#""epoch":1"#, r#""epoch":-1"#);
        assert!(decode("runtime_execution_runs", "run", &negative_epoch).is_ok());
        let decimal_epoch = null_raw.replace(r#""epoch":1"#, r#""epoch":1.0"#);
        assert!(decode("runtime_execution_runs", "run", &decimal_epoch).is_err());
    }

    #[test]
    fn terminal_settlement_rejects_duplicate_execution_fields() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT,account_key TEXT,thread_id TEXT); INSERT INTO runtime_agents VALUES('agent','default','thread'); CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT); CREATE TABLE runtime_execution_attempts(id TEXT,run TEXT,record TEXT);").unwrap();
        let raw = r#"{"id":"old","id":"run","agent":"other","agent":"agent","accountKey":"default","epoch":2,"epoch":1,"status":"unknown","status":"running","threadId":"thread","turnId":"turn","created":1}"#;
        db.execute("INSERT INTO runtime_execution_runs VALUES('run','agent','default',1,'thread','turn',1,?1)", [raw]).unwrap();
        let tx = db.transaction().unwrap();
        assert!(
            ExecutionHistoryRepository::terminal(
                &tx,
                "agent",
                "turn",
                ExecutionStatus::Completed,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn submission_accepts_opaque_attempt_extensions_history_refuses() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,record TEXT);")
            .unwrap();
        let raw = r#"{"id":"run","agent":"agent","accountKey":"default","epoch":1,"created":1,"status":"running","firstAttemptId":false,"rootAttemptId":{"future":true}}"#;
        db.execute("INSERT INTO runtime_execution_runs VALUES('run',?1)", [raw])
            .unwrap();
        let agent = AgentRecord::from_value(json!({"accountKey":"default","id":"agent","rootId":"agent","provider":"codex","epoch":1,"status":"idle","activeExecution":{"runId":"run","attemptId":null}})).unwrap();
        assert_eq!(
            crate::repositories::work_items::WorkItemsRepository::submission_identity(&db, &agent)
                .unwrap(),
            Some(("run".into(), None))
        );
        let tx = db.transaction().unwrap();
        assert!(ExecutionHistoryRepository::current_run(&tx, &agent).is_err());
    }

    #[test]
    fn execution_views_preserve_stored_opaque_payloads() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch(
            "CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);
            CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,record TEXT);",
        )
        .expect("history schema");
        let attempt = r#"{"id":"attempt","runId":"run","epoch":1,"events":["event"],"inputIdentity":[ { "text": "hello" } ],"claimResult":{ "metadata": {"custom":true} },"providerSubmission":{"response": [1, 2]},"terminalRepair":{"note": "preserve"},"extension":{"future":true}}"#;
        let run = r#"{"id":"run","agent":"agent","accountKey":"default","epoch":1,"status":"completed","created":1.0,"threadId":"source","latestAttemptId":"attempt"}"#;
        db.execute(
            "INSERT INTO runtime_execution_attempts VALUES('attempt','run',?1)",
            [attempt],
        )
        .expect("stored attempt");
        db.execute("INSERT INTO runtime_execution_runs VALUES('run',?1)", [run])
            .expect("stored run");
        let saved = ExecutionHistoryRepository::attempt(&db, "attempt")
            .expect("read")
            .expect("attempt");
        assert_eq!(saved.snapshot.as_ref().unwrap().get(), attempt);
        assert_eq!(
            saved.input_identity.as_ref().unwrap().get(),
            r#"[ { "text": "hello" } ]"#
        );
        assert_eq!(
            saved.claim_result.as_ref().unwrap().get(),
            r#"{ "metadata": {"custom":true} }"#
        );
        assert_eq!(
            saved.provider_submission.as_ref().unwrap().get(),
            r#"{"response": [1, 2]}"#
        );
        assert_eq!(
            saved.terminal_repair.as_ref().unwrap().get(),
            r#"{"note": "preserve"}"#
        );
        let agent = AgentRecord::from_value(json!({"accountKey":"default","id":"agent","rootId":"agent","provider":"codex","epoch":1,"status":"idle","activeExecution":{"runId":"run","attemptId":"attempt"}})).expect("agent");
        let tx = db.transaction().expect("transaction");
        let saved_run = ExecutionHistoryRepository::current_run(&tx, &agent)
            .expect("read")
            .expect("run");
        assert_eq!(
            saved_run.latest_attempt_id.value().map(String::as_str),
            Some("attempt")
        );
        assert_eq!(
            saved_run.thread_id.value().map(String::as_str),
            Some("source")
        );
    }

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
        let mut agent=AgentRecord::from_value(json!({"accountKey":"default","id":"agent","rootId":"agent","provider":"codex","epoch":1,"status":"idle","threadId":"destination","activeExecution":pointer})).expect("current owner");
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
        agent.account_key = "another-account".to_string();
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
            [serde_json::to_string(&admission).expect("token")],
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
        assert!(tx.query_row("SELECT json_type(record,'$.receipts') IS NULL FROM runtime_execution_attempts WHERE id='attempt'", [], |row| row.get::<_, bool>(0)).expect("receipts removed"));
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
