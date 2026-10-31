//! Durable HTTP input identity. No native write is permitted without its receipt.

use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, MonitorsRepository, StoreError};

use crate::event_write::settle_request;
use crate::monitor_policy::{self, Effect, Input, InputOutcome, MonitorError, Operation};
use crate::monitor_runtime;
use studio_runtime_schema::persistence::{OutboxKind, OutboxStatus, OwnerKind, RequestIdentity};
use studio_runtime_schema::process_controls::{
    MonitorInputKind, MonitorInputResponse, MonitorInputStatus, ProcessActionResult,
    ProcessActionStatus, ProcessReceipt, ProcessResponse,
};
use studio_runtime_store::outbox::{Outbox, OutboxRow};
use studio_runtime_store::request_results::{Admission, AdmittedRequest, RequestResults};

const PREFIX: &str = "monitor-http-input:";
const TICKET_PREFIX: &str = "http:";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    operation: Operation,
    monitor: String,
    status: Status,
}

pub(crate) use studio_runtime_schema::persistence::MonitorInputStatus as Status;

pub(crate) fn replay_status(status: Status) -> Result<(), MonitorError> {
    match status {
        Status::Acknowledged => Ok(()),
        Status::Expired => Err(MonitorError::Expired),
        Status::Rejected => Err(MonitorError::Refused("Monitor input rejected")),
        Status::Pending | Status::Unknown => Err(MonitorError::InputOutcomeUnknown),
    }
}

#[derive(Deserialize, Serialize)]
struct InputIntent {
    admitted: AdmittedRequest,
    receipt: Receipt,
}

impl From<studio_runtime_store::outbox::OutboxError> for MonitorError {
    fn from(error: studio_runtime_store::outbox::OutboxError) -> Self {
        use studio_runtime_store::outbox::OutboxError;
        match error {
            OutboxError::Conflict(_) => Self::Refused("Monitor input request content changed"),
            error @ (OutboxError::Sql(_) | OutboxError::Json(_)) => Self::Store(error.into()),
        }
    }
}
impl From<studio_runtime_store::request_results::RequestResultError> for MonitorError {
    fn from(error: studio_runtime_store::request_results::RequestResultError) -> Self {
        use studio_runtime_store::request_results::RequestResultError;
        match error {
            RequestResultError::Conflict { .. } => {
                Self::Refused("Monitor input request content changed")
            }
            error @ (RequestResultError::Expired
            | RequestResultError::Sql(_)
            | RequestResultError::Json(_)
            | RequestResultError::Identity(_)) => Self::Store(error.into()),
        }
    }
}

fn outbox_id(root: &str) -> String {
    json!(["monitor-http-input", format!("{PREFIX}{root}")]).to_string()
}

fn identity(root: &str) -> Result<RequestIdentity, MonitorError> {
    RequestIdentity::new("monitor-http-input", &[], root, &[])
        .map_err(|error| MonitorError::Store(StoreError::RequestResult(error.into())))
}

fn saved(connection: &Connection, root: &str) -> Result<Option<(String, Receipt)>, MonitorError> {
    let key = format!("{PREFIX}{root}");
    if let Some(saved) = Outbox::get::<InputIntent>(connection, &outbox_id(root))? {
        if saved.kind != OutboxKind::MonitorInput
            || saved.owner_kind != OwnerKind::Monitor
            || saved.owner_id != saved.payload.receipt.monitor
        {
            return Err(MonitorError::Refused("Monitor input request owner changed"));
        }
        return Ok(Some((saved.signature, saved.payload.receipt)));
    }
    Ok(RequestResults::read(
        connection,
        &key,
        "monitor-http-input",
        &[],
        root,
        &[],
        chrono::Utc::now().timestamp_millis(),
    )?
    .map(|saved| (saved.signature, saved.reply)))
}

struct RequestContext {
    snapshot: monitor_policy::MonitorSnapshot,
    actor: AgentRecord,
    operation: Operation,
    signature: String,
    existing: Option<Receipt>,
}

pub(crate) struct Reservation {
    pub operation: Operation,
    pub ticket: String,
    pub effect: Effect,
}

pub(crate) fn acknowledged(request_id: &str) -> Value {
    json!({"requestId": request_id, "status": "acknowledged"})
}

/// Use the existing process receipt read without the original stdin content.
pub(crate) fn read(connection: &Connection, query: &Value) -> Result<ProcessReceipt, MonitorError> {
    let missing = ProcessReceipt::not_found;
    let request_id = query["requestId"].as_str().unwrap_or_default();
    let Some((_, receipt)) = saved(connection, request_id)? else {
        return Ok(missing());
    };
    if query["agent"] != receipt.operation.agent.as_str()
        || query["epoch"] != receipt.operation.epoch.get()
    {
        return Ok(missing());
    }
    let Some(actor) =
        AgentRepository::get(connection, &AgentId::from(receipt.operation.agent.as_str()))?
    else {
        return Ok(missing());
    };
    if actor.value()["accountKey"] != query["accountKey"]
        || actor.value()["epoch"] != query["epoch"]
    {
        return Ok(missing());
    }
    let Some(monitor) = MonitorsRepository::get(connection, &receipt.monitor)? else {
        return Ok(missing());
    };
    let snapshot = monitor_policy::MonitorSnapshot::checked_stored(&monitor, actor.value())?;
    if !snapshot.active_owner()
        || snapshot
            .operation()
            .is_none_or(|current| !current.same_identity(&receipt.operation))
    {
        return Ok(missing());
    }
    let (status, terminal) = match receipt.status {
        Status::Acknowledged => (MonitorInputStatus::Acknowledged, true),
        Status::Rejected => (MonitorInputStatus::Rejected, true),
        Status::Expired => {
            return Err(
                StoreError::Json(<serde_json::Error as serde::de::Error>::custom(
                    "invalid monitor input status: expired",
                ))
                .into(),
            );
        }
        Status::Pending => (MonitorInputStatus::Pending, false),
        Status::Unknown => (MonitorInputStatus::Unknown, false),
    };
    Ok(ProcessReceipt::found(ProcessActionResult {
        request_id: request_id.into(),
        status: if terminal {
            ProcessActionStatus::Completed
        } else {
            ProcessActionStatus::Uncertain
        },
        processes: vec![],
        response: Some(ProcessResponse::MonitorInput(MonitorInputResponse {
            kind: MonitorInputKind::MonitorInput,
            monitor_id: receipt.monitor,
            status,
        })),
        extra: Default::default(),
    }))
}

fn request_context(
    tx: &Transaction<'_>,
    id: &str,
    request_id: &str,
    input: &Input,
) -> Result<RequestContext, MonitorError> {
    if request_id.is_empty() {
        return Err(MonitorError::Refused(
            "Monitor input request identity is required",
        ));
    }
    let (snapshot, actor) = monitor_runtime::snapshot(tx, id)?;
    if !snapshot.active_owner() {
        return Err(MonitorError::Refused(
            "Monitor input owner is no longer active",
        ));
    }
    let operation = snapshot.operation().cloned().ok_or(MonitorError::Refused(
        "This interactive monitor is not active",
    ))?;
    let signature = format!(
        "http-input-v1:{}",
        studio_runtime_store::sha256_hex(
            &serde_json::to_vec(&(id, &operation, input)).map_err(StoreError::from)?
        )
    );
    let existing = if let Some((saved_signature, receipt)) = saved(tx, request_id)? {
        if saved_signature != signature {
            return Err(MonitorError::Refused(
                "Monitor input request content changed",
            ));
        }
        if receipt.monitor != id || !receipt.operation.same_identity(&operation) {
            return Err(MonitorError::Refused("Monitor input request owner changed"));
        }
        Some(receipt)
    } else {
        None
    };
    Ok(RequestContext {
        snapshot,
        actor,
        operation,
        signature,
        existing,
    })
}

fn put_receipt(
    tx: &Transaction<'_>,
    request_id: &str,
    context: &RequestContext,
    status: Status,
) -> Result<(), MonitorError> {
    let receipt = Receipt {
        operation: context.operation.clone(),
        monitor: context.snapshot.id().into(),
        status,
    };
    let admitted = match RequestResults::admit(
        tx,
        &identity(request_id)?,
        &context.signature,
        chrono::Utc::now().timestamp_millis(),
    )? {
        Admission::Fresh(admitted) => admitted,
        Admission::Replay(_) => {
            return Err(MonitorError::Refused(
                "Monitor input request already completed",
            ));
        }
    };
    if matches!(
        status,
        Status::Acknowledged | Status::Rejected | Status::Expired
    ) {
        settle_request(
            tx,
            &admitted,
            Some(receipt.operation.agent.as_str()),
            &receipt,
        )?;
    } else {
        Outbox::put(
            tx,
            &OutboxRow {
                id: outbox_id(request_id),
                kind: OutboxKind::MonitorInput,
                owner_kind: OwnerKind::Monitor,
                owner_id: receipt.monitor.clone(),
                request_id: Some(admitted.id().into()),
                signature: context.signature.clone(),
                status: OutboxStatus::Unknown,
                account: Some(receipt.operation.account_key.as_str().to_owned()),
                stream_id: None,
                generation: None,
                ordinal: None,
                next_at: None,
                created: admitted.created(),
                payload: InputIntent { admitted, receipt },
            },
        )?;
    }
    Ok(())
}

/// A busy sequence proves this new request never reached native admission.
/// Return a value so the caller commits this refusal before returning an error.
pub(crate) fn refuse_busy(
    tx: &Transaction<'_>,
    id: &str,
    request_id: &str,
    input: &Input,
) -> Result<Status, MonitorError> {
    let context = request_context(tx, id, request_id, input)?;
    if let Some(receipt) = &context.existing {
        return Ok(receipt.status);
    }
    put_receipt(tx, request_id, &context, Status::Rejected)?;
    Ok(Status::Rejected)
}

pub(crate) fn reserve(
    tx: &Transaction<'_>,
    id: &str,
    request_id: &str,
    input: &Input,
    now: f64,
) -> Result<Option<Reservation>, MonitorError> {
    let context = request_context(tx, id, request_id, input)?;
    if let Some(receipt) = &context.existing {
        replay_status(receipt.status)?;
        return Ok(None);
    }
    let ticket = format!("{TICKET_PREFIX}{request_id}");
    let mut decision =
        monitor_policy::begin_input(&context.snapshot, &context.operation, &ticket, input, now)?;
    monitor_runtime::initialize_input_identity(&mut decision, request_id);
    let effect = decision
        .effects
        .first()
        .cloned()
        .ok_or(MonitorError::Refused("Missing monitor input"))?;
    monitor_runtime::apply(tx, &context.snapshot, &context.actor, &decision, now)?;
    put_receipt(tx, request_id, &context, Status::Pending)?;
    Ok(Some(Reservation {
        operation: context.operation,
        ticket,
        effect,
    }))
}

/// This write shares the transaction which settles the native input attempt.
/// A late exact callback can resolve Unknown, but cannot replace a terminal receipt.
pub(crate) fn settle(
    tx: &Transaction<'_>,
    id: &str,
    operation: &Operation,
    ticket: &str,
    outcome: &InputOutcome,
) -> Result<(), MonitorError> {
    let Some(request_id) = ticket.strip_prefix(TICKET_PREFIX) else {
        return Ok(());
    };
    let key = outbox_id(request_id);
    let Some(mut saved) = Outbox::get::<InputIntent>(tx, &key)? else {
        return Ok(());
    };
    let receipt = &mut saved.payload.receipt;
    if receipt.monitor != id || !receipt.operation.same_identity(operation) {
        return Err(MonitorError::Refused(
            "Monitor input receipt identity changed",
        ));
    }
    if matches!(
        receipt.status,
        Status::Acknowledged | Status::Rejected | Status::Expired
    ) {
        return Ok(());
    }
    receipt.status = match outcome {
        InputOutcome::Acknowledged => Status::Acknowledged,
        InputOutcome::Expired => Status::Expired,
        InputOutcome::Rejected(_) => Status::Rejected,
        InputOutcome::Unknown(_) => Status::Unknown,
    };
    if matches!(
        receipt.status,
        Status::Acknowledged | Status::Rejected | Status::Expired
    ) {
        settle_request(
            tx,
            &saved.payload.admitted,
            Some(receipt.operation.agent.as_str()),
            &receipt,
        )?;
        Outbox::delete_settled(tx, &key, &saved.signature)?;
    } else {
        saved.status = OutboxStatus::Unknown;
        saved.next_at = None;
        Outbox::put(tx, &saved)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_pivot_monitor_expired_input_replays_permanent_result() {
        let mut db = Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("schema");
        db.execute_batch(
            "CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .expect("policy");
        let root = RequestIdentity::issue_root();
        let identity = identity(&root).expect("identity");
        let operation: Operation = Operation {
            agent: "agent".to_owned().try_into().unwrap(),
            epoch: 1_i64.try_into().unwrap(),
            account_key: "account".to_owned().try_into().unwrap(),
            connection_id: "connection".to_owned().try_into().unwrap(),
            evidence: Default::default(),
        };
        let tx = db.transaction().expect("transaction");
        let admitted =
            match RequestResults::admit(&tx, &identity, "signature", identity.issued_ms())
                .expect("admit")
            {
                Admission::Fresh(admitted) => admitted,
                Admission::Replay(_) => panic!("fresh"),
            };
        let key = outbox_id(&root);
        Outbox::put(
            &tx,
            &OutboxRow {
                id: key.clone(),
                kind: OutboxKind::MonitorInput,
                owner_kind: OwnerKind::Monitor,
                owner_id: "monitor".into(),
                request_id: Some(admitted.id().into()),
                signature: "signature".into(),
                status: OutboxStatus::Unknown,
                account: Some("account".into()),
                stream_id: None,
                generation: None,
                ordinal: None,
                next_at: None,
                created: admitted.created(),
                payload: InputIntent {
                    admitted,
                    receipt: Receipt {
                        operation: operation.clone(),
                        monitor: "monitor".into(),
                        status: Status::Unknown,
                    },
                },
            },
        )
        .expect("input owner");
        let ticket = format!("{TICKET_PREFIX}{root}");
        settle(&tx, "monitor", &operation, &ticket, &InputOutcome::Expired)
            .expect("permanent refusal");
        settle(
            &tx,
            "monitor",
            &operation,
            &ticket,
            &InputOutcome::Unknown("lost response".into()),
        )
        .expect("no resurrection");
        let (_, receipt) = saved(&tx, &root)
            .expect("read")
            .expect("exact final result");
        assert!(matches!(
            replay_status(receipt.status),
            Err(MonitorError::Expired)
        ));
        assert!(
            Outbox::get::<InputIntent>(&tx, &key)
                .expect("outbox")
                .is_none()
        );
    }

    #[test]
    fn receipt_pivot_monitor_unknown_survives_time_and_late_exact_ack_settles() {
        let mut db = Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("target schema");
        db.execute_batch(
            "CREATE TABLE runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .expect("policy");
        let root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let identity = studio_runtime_schema::persistence::RequestIdentity::new(
            "monitor-http-input",
            &[],
            &root,
            &[],
        )
        .expect("identity");
        let operation: Operation = Operation {
            agent: "agent".to_owned().try_into().unwrap(),
            epoch: 1_i64.try_into().unwrap(),
            account_key: "account".to_owned().try_into().unwrap(),
            connection_id: "connection".to_owned().try_into().unwrap(),
            evidence: Default::default(),
        };
        let payload = json!({"admitted":{"id":identity.id(),"signature":"signature","created":identity.issued_ms()},"receipt":{"operation":operation,"monitor":"monitor","status":"Unknown"}});
        let id = outbox_id(&root);
        db.execute("INSERT INTO runtime_outbox(id,kind,owner_kind,owner_id,request_id,signature,status,next_at,payload,created) VALUES(?1,'monitor_input','monitor','monitor',?2,'signature','unknown',NULL,?3,?4)",rusqlite::params![id,identity.id(),payload.to_string(),identity.issued_ms()]).expect("uncertain admission");
        let tx = db.transaction().expect("transaction");
        settle(
            &tx,
            "monitor",
            &operation,
            &format!("{TICKET_PREFIX}{root}"),
            &InputOutcome::Unknown("lost reply".into()),
        )
        .expect("keep unknown");
        assert!(
            studio_runtime_store::outbox::Outbox::pending::<Value>(
                &tx,
                studio_runtime_schema::persistence::OutboxKind::MonitorInput,
                i64::MAX,
                10
            )
            .expect("pending selector")
            .is_empty()
        );
        settle(
            &tx,
            "monitor",
            &operation,
            &format!("{TICKET_PREFIX}{root}"),
            &InputOutcome::Acknowledged,
        )
        .expect("late exact acknowledgement");
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("outbox"),
            0
        );
        assert_eq!(
            tx.query_row(
                "SELECT json_extract(result,'$.reply.status') FROM request_results WHERE id=?1",
                [identity.id()],
                |r| r.get::<_, String>(0)
            )
            .expect("final response"),
            "Acknowledged"
        );
    }
}
