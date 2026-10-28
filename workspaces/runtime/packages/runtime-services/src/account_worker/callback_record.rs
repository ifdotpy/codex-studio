//! Accepted callbacks own handoff authority; response bytes have one outbox owner.
use super::{JournalEntry, JournalPayload, WorkerError, WriteOutcome, store::invalid};
use crate::{
    provider_callbacks as callbacks,
    provider_session_task::SessionStatus,
    tool_requests::{self, CallbackTask},
};
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use studio_runtime_process::ProviderHandle;
use studio_runtime_providers::{
    DecodedJournalPayload, Frame, ResponseBody, decode_journal_entry, encode_line,
};
use studio_runtime_schema::persistence::{RequestIdentity, ToolRequestKind, ToolRequestStatus};
use studio_runtime_store::RuntimeStore;

/// Maximum wait for callback admission or final write evidence. Unit: seconds.
pub(super) const WRITE_RECEIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedReply {
    handle: String,
    generation: u64,
    pid: u32,
    response_key: String,
    sha256: String,
    authority: Option<super::callback::ManagedReplyAuthority>,
    /// Hydrated from the canonical response only while processing this task.
    #[serde(skip)]
    frame: String,
}
impl SavedReply {
    pub(super) fn new(frame: &Frame, status: &SessionStatus) -> Result<Self, WorkerError> {
        let info = status
            .attachment
            .info
            .as_ref()
            .ok_or(WorkerError::CallbackUnknown)?;
        let bytes = encode_line(frame).map_err(|_| WorkerError::CallbackUnknown)?;
        let saved = Self {
            handle: info.handle.as_str().into(),
            generation: info.generation.0,
            pid: info.pid,
            response_key: String::new(),
            sha256: studio_runtime_store::sha256_hex(&bytes),
            authority: None,
            frame: String::from_utf8(bytes).map_err(|_| WorkerError::CallbackUnknown)?,
        };
        if !saved.same_connection(status) {
            return Err(WorkerError::CallbackUnknown);
        }
        Ok(saved)
    }
    pub(super) fn same_connection(&self, status: &SessionStatus) -> bool {
        status
            .ready_connection(
                &status.attachment.handle,
                studio_runtime_process::Generation(self.generation),
            )
            .is_some_and(|info| info.handle.as_str() == self.handle && info.pid == self.pid)
    }
    fn response(&self, accepted: &Accepted) -> Result<Frame, WorkerError> {
        if self.generation != accepted.generation
            || self.handle != ProviderHandle::for_account(&accepted.account).as_str()
            || self.sha256 != studio_runtime_store::sha256_hex(self.frame.as_bytes())
        {
            return Err(WorkerError::CallbackUnknown);
        }
        let DecodedJournalPayload::Frame(frame @ Frame::Response { .. }) = decode_journal_entry(
            accepted.cursor,
            accepted.generation,
            self.frame.clone().into_bytes(),
        )
        .payload
        else {
            return Err(WorkerError::CallbackUnknown);
        };
        let Frame::Response { id, .. } = &frame else {
            return Err(WorkerError::CallbackUnknown);
        };
        if serde_json::to_value(id).map_err(|_| WorkerError::CallbackUnknown)?
            != accepted.request_id
        {
            return Err(WorkerError::CallbackUnknown);
        }
        Ok(frame)
    }
    fn hydrate(&mut self, db: &Connection) -> Result<(), WorkerError> {
        let frame = callbacks::response_frame(db, &self.response_key)
            .map_err(callback_error)?
            .ok_or(WorkerError::CallbackUnknown)?;
        self.frame =
            String::from_utf8(encode_line(&frame).map_err(|_| WorkerError::CallbackUnknown)?)
                .map_err(|_| WorkerError::CallbackUnknown)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Accepted {
    pub account: String,
    pub journal_scope: String,
    pub generation: u64,
    pub cursor: u64,
    pub request_id: Value,
    pub method: String,
    pub frame: String,
    pub actor: Option<String>,
    pub epoch: Option<i64>,
    #[serde(skip)]
    pub submitted: Option<SavedReply>,
    #[serde(skip)]
    pub root_request_id: String,
    #[serde(skip)]
    pub source_admission: Option<studio_runtime_store::request_results::AdmittedRequest>,
}
impl Accepted {
    pub(super) fn saved_response(
        &self,
        status: &SessionStatus,
    ) -> Result<Option<Frame>, WorkerError> {
        let Some(saved) = &self.submitted else {
            return Ok(None);
        };
        if self.managed_frame()?.is_some() && saved.authority.is_none() {
            return Err(WorkerError::CallbackUnknown);
        }
        if self.actor.is_some()
            || !status.supervisor_write_deduplication
            || !saved.same_connection(status)
        {
            return Err(WorkerError::CallbackUnknown);
        }
        saved.response(self).map(Some)
    }
    pub(super) fn operation(&self) -> String {
        operation(&self.journal_scope, self.generation, self.cursor)
    }
    fn managed_frame(&self) -> Result<Option<Frame>, WorkerError> {
        let frame = self.frame()?;
        Ok(
            matches!(&frame,Frame::Request(request) if request.method.as_str()=="item/tool/call")
                .then_some(frame),
        )
    }
    pub(super) fn frame(&self) -> Result<Frame, WorkerError> {
        match decode_journal_entry(
            self.cursor,
            self.generation,
            self.frame.clone().into_bytes(),
        )
        .payload
        {
            DecodedJournalPayload::Frame(frame) => Ok(frame),
            DecodedJournalPayload::RawStdout { .. } => Err(invalid(&self.operation())),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CallbackObligation {
    pub accepted: Accepted,
    pub submitted: Option<SavedReply>,
    pub recovery_authorized: bool,
    pub recovery_attempted: bool,
    pub request_fingerprint: String,
    pub root_request_id: String,
}
type Task = CallbackTask<CallbackObligation>;
fn task(db: &Connection, operation: &str) -> Result<Option<Task>, WorkerError> {
    tool_requests::get_callback(db, operation).map_err(tool_error)
}
fn put(tx: &Transaction<'_>, task: &Task) -> Result<(), WorkerError> {
    tool_requests::put_callback(tx, task).map_err(tool_error)
}
fn tool_error(error: tool_requests::ToolRequestError) -> WorkerError {
    match error {
        tool_requests::ToolRequestError::Store(error) => WorkerError::Store(error),
        _ => WorkerError::CallbackUnknown,
    }
}
fn callback_error(error: callbacks::CallbackError) -> WorkerError {
    match error {
        callbacks::CallbackError::Store(error) => WorkerError::Store(error),
        _ => WorkerError::CallbackUnknown,
    }
}
pub(super) fn operation(scope: &str, generation: u64, cursor: u64) -> String {
    format!("callback:{scope}:{generation}:{cursor}")
}

pub(super) fn accept(
    tx: &Transaction<'_>,
    account: &str,
    scope: &str,
    actor: Option<(&str, i64)>,
    entry: &JournalEntry,
) -> Result<(), WorkerError> {
    let JournalPayload::Frame(frame) = &entry.payload else {
        return Ok(());
    };
    let Some(Frame::Response { id, .. }) = super::callback::response(frame) else {
        return Ok(());
    };
    let method = match frame {
        Frame::Request(request) => request.method.as_str().to_owned(),
        Frame::UnknownMethod(request) => request.method.clone(),
        Frame::Notification(_) | Frame::Response { .. } => return Ok(()),
    };
    let operation = operation(scope, entry.generation.0, entry.cursor.0);
    let bytes = encode_line(frame).map_err(|_| invalid(&operation))?;
    let fingerprint = studio_runtime_store::sha256_hex(&bytes);
    if let Some(task) = task(tx, &operation)? {
        if task.payload.request_fingerprint != fingerprint {
            return Err(invalid(&operation));
        }
        return Ok(());
    }
    let accepted = Accepted {
        account: account.into(),
        journal_scope: scope.into(),
        generation: entry.generation.0,
        cursor: entry.cursor.0,
        request_id: serde_json::to_value(id).map_err(|_| invalid(&operation))?,
        method,
        frame: String::from_utf8(bytes).map_err(|_| invalid(&operation))?,
        actor: actor.map(|(id, _)| id.into()),
        epoch: actor.map(|(_, epoch)| epoch),
        submitted: None,
        root_request_id: String::new(),
        source_admission: None,
    };
    let context = accepted_context(tx, &accepted, "")?;
    let root_request_id =
        callbacks::admit_callback_request(tx, &context, frame).map_err(callback_error)?;
    put(
        tx,
        &CallbackTask {
            id: RequestIdentity::issue_root(),
            origin_key: operation,
            status: ToolRequestStatus::Queued,
            updated: chrono::Utc::now().timestamp_millis(),
            payload: CallbackObligation {
                accepted,
                submitted: None,
                recovery_authorized: false,
                recovery_attempted: false,
                request_fingerprint: fingerprint,
                root_request_id,
            },
        },
    )
}

fn retire(tx: &Transaction<'_>, operation: &str) -> Result<(), WorkerError> {
    let Some(mut task) = task(tx, operation)? else {
        return Ok(());
    };
    task.status = ToolRequestStatus::Completed;
    task.payload.accepted.frame.clear();
    task.payload.submitted = None;
    put(tx, &task)
}
pub(super) async fn retire_async(store: &RuntimeStore, operation: &str) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| retire(tx, &operation))
        .await
}
fn accepted_context(
    tx: &Transaction<'_>,
    accepted: &Accepted,
    connection: &str,
) -> Result<callbacks::CallbackContext, WorkerError> {
    let frame = accepted.frame()?;
    let params = match &frame {
        Frame::Request(request) => request.params.as_ref(),
        Frame::UnknownMethod(request) => request.fields.get("params"),
        _ => None,
    };
    let thread = params
        .and_then(|params| params["threadId"].as_str())
        .unwrap_or_default();
    let turn = params.and_then(|params| params["turnId"].as_str());
    Ok(super::callback::callback_context_in(
        tx,
        &accepted.account,
        accepted.generation,
        accepted.actor.as_deref(),
        thread,
        turn,
    )?
    .unwrap_or_else(|| callbacks::CallbackContext {
        agent: accepted.actor.clone().unwrap_or_default(),
        account: accepted.account.clone(),
        stream_id: accepted.journal_scope.clone(),
        generation: accepted.generation,
        connection: connection.into(),
        epoch: accepted
            .epoch
            .and_then(|epoch| u64::try_from(epoch).ok())
            .unwrap_or(0),
        thread: thread.into(),
        turn: turn.unwrap_or_default().into(),
    }))
}

/// First admission commits exact response provenance. Only the existing
/// supervisor proof permits one retry of the very same operation and bytes.
pub(super) async fn admit_reply(
    store: &RuntimeStore,
    operation: &str,
    reply: Option<(SavedReply, bool)>,
) -> Result<bool, WorkerError> {
    let operation = operation.to_owned();
    store.transaction_with(move|tx| {
        let Some(mut task)=task(tx,&operation)? else { return Ok(true); };
        if task.status==ToolRequestStatus::Completed { return Ok(true); }
        if task.status==ToolRequestStatus::Unknown { return Err(WorkerError::CallbackUnknown); }
        let Some((mut saved,deduplicated))=reply else {
            if task.status!=ToolRequestStatus::Queued { return Err(WorkerError::CallbackUnknown); }
            task.status=ToolRequestStatus::Running; put(tx,&task)?; return Ok(false);
        };
        let accepted=&task.payload.accepted;
        if accepted.operation()!=operation || accepted.actor.is_some() { return Err(WorkerError::CallbackUnknown); }
        let frame=saved.response(accepted)?;
        let refusal=matches!(&frame,Frame::Response{body:ResponseBody::Error(_),..}) || matches!(&frame,Frame::Response{body:ResponseBody::Result(value),..} if value.get("success")==Some(&Value::Bool(false)));
        let managed=accepted.managed_frame()?;
        let authority=managed.as_ref().map(|frame|super::callback::managed_reply_authority_in(tx,&accepted.account,accepted.generation,frame)).transpose()?.flatten();
        if let Some(original)=&task.payload.submitted {
            if managed.is_some() && !original.authority.as_ref().is_some_and(|old|authority.as_ref().is_some_and(|current|old.same_actor(current))) {
                task.status=ToolRequestStatus::Unknown; put(tx,&task)?; return Ok(true);
            }
            saved.authority=original.authority.clone();
            saved.response_key=original.response_key.clone();
            // Compare persisted provenance, including bytes' hash, never a freshly built body.
            if !deduplicated || task.payload.recovery_attempted || serde_json::to_value(&saved).map_err(|_|invalid(&operation))?!=serde_json::to_value(original).map_err(|_|invalid(&operation))? {
                task.status=ToolRequestStatus::Unknown; put(tx,&task)?; return Ok(true);
            }
            let mut original=original.clone(); original.hydrate(tx)?;
            if original.response(accepted)?!=frame { return Err(WorkerError::CallbackUnknown); }
            task.payload.recovery_attempted=true;
            put(tx,&task)?; return Ok(false);
        }
        saved.authority=authority;
        let callback_context=accepted_context(tx,accepted,&saved.handle)?;
        let identity=callbacks::save_frame_response(tx,&callback_context,&accepted.frame()?,&frame).map_err(callback_error)?;
        saved.response_key=callbacks::callback_origin_key(&callback_context,&identity.rpc_id);
        let admission=callbacks::begin_frame(tx,&callback_context.stream_id,accepted.generation,&identity.operation_id(),&frame).map_err(callback_error)?;
        if admission!=callbacks::FrameAdmission::Send { return Err(WorkerError::CallbackUnknown); }
        let missing=managed.is_some() && saved.authority.is_none();
        task.payload.submitted=Some(saved);
        task.status=if missing && !refusal { ToolRequestStatus::Unknown } else { ToolRequestStatus::Running };
        put(tx,&task)?;
        Ok(missing && !refusal)
    }).await
}

pub(super) async fn expired(
    store: &RuntimeStore,
    operation: &str,
    status: Option<SessionStatus>,
) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| {
            let Some(mut task) = task(tx, &operation)? else {
                return Ok(());
            };
            if task.status == ToolRequestStatus::Completed {
                return Ok(());
            }
            let mut accepted = task.payload.accepted.clone();
            if let Some(mut saved) = task.payload.submitted.clone() {
                if saved.hydrate(tx).is_ok() {
                    accepted.submitted = Some(saved);
                }
            }
            let recoverable = task.status != ToolRequestStatus::Unknown
                && !task.payload.recovery_attempted
                && status.as_ref().is_some_and(|status| {
                    accepted
                        .saved_response(status)
                        .is_ok_and(|frame| frame.is_some())
                });
            task.payload.recovery_authorized = recoverable;
            if !recoverable {
                task.status = ToolRequestStatus::Unknown;
            }
            put(tx, &task)
        })
        .await
}
pub(super) async fn refuse_recovery(
    store: &RuntimeStore,
    operation: &str,
) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| {
            if let Some(mut task) = task(tx, &operation)? {
                task.status = ToolRequestStatus::Unknown;
                task.payload.recovery_authorized = false;
                put(tx, &task)?;
            }
            Ok(())
        })
        .await
}

pub(super) async fn written(
    store: &RuntimeStore,
    operation: &str,
    outcome: WriteOutcome,
) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| {
            let Some(mut task) = task(tx, &operation)? else {
                return Ok(());
            };
            if task.status == ToolRequestStatus::Completed {
                return Ok(());
            }
            if matches!(outcome, WriteOutcome::NotSent | WriteOutcome::Unknown)
                && task.payload.recovery_authorized
                && !task.payload.recovery_attempted
            {
                return Ok(());
            }
            if let Some(saved) = &task.payload.submitted {
                callbacks::settle_response(
                    tx,
                    &saved.response_key,
                    match outcome {
                        WriteOutcome::Written => callbacks::DeliveryOutcome::Written,
                        WriteOutcome::NotSent => callbacks::DeliveryOutcome::NotSent,
                        WriteOutcome::Unknown => callbacks::DeliveryOutcome::Unknown,
                    },
                )
                .map_err(callback_error)?;
            }
            match outcome {
                WriteOutcome::Written => retire(tx, &operation),
                WriteOutcome::NotSent
                    if !task.payload.recovery_attempted
                        && task.status != ToolRequestStatus::Unknown =>
                {
                    task.payload.submitted = None;
                    task.status = ToolRequestStatus::Queued;
                    put(tx, &task)
                }
                WriteOutcome::NotSent | WriteOutcome::Unknown => {
                    task.status = ToolRequestStatus::Unknown;
                    put(tx, &task)
                }
            }
        })
        .await
}

// These selectors visit current callback tasks, never completed reply history.
const TASKS: &str = "FROM runtime_tool_requests WHERE kind=?1 AND status IN (?2,?3,?4) AND json_extract(record,'$.payload.accepted.account')=?5";
const UNSENT: &str = " AND (json_extract(record,'$.payload.accepted.generation')<?6 OR (status!=?4 AND (json_type(record,'$.payload.submitted')='null' OR json_extract(record,'$.payload.recovery_attempted')=0)))";
pub(super) fn any_unsent(
    db: &Connection,
    account: &str,
    generation: u64,
) -> Result<bool, WorkerError> {
    Ok(db.query_row(
        &format!("SELECT EXISTS(SELECT 1 {TASKS}{UNSENT})"),
        params![
            ToolRequestKind::ProviderCallback,
            ToolRequestStatus::Queued,
            ToolRequestStatus::Running,
            ToolRequestStatus::Unknown,
            account,
            i64::try_from(generation).map_err(|_| invalid(account))?
        ],
        |row| row.get(0),
    )?)
}
pub(super) async fn unsent(
    store: &RuntimeStore,
    account: &str,
    generation: u64,
    limit: usize,
) -> Result<Vec<Accepted>, WorkerError> {
    let account = account.to_owned();
    store.read_async("runtime-services/callback_tasks",move|db|{
        let mut query=db.prepare_cached(&format!("SELECT record {TASKS}{UNSENT} ORDER BY json_extract(record,'$.payload.accepted.generation'),json_extract(record,'$.payload.accepted.cursor') LIMIT ?7"))?;
        let rows=query.query_map(params![ToolRequestKind::ProviderCallback,ToolRequestStatus::Queued,ToolRequestStatus::Running,ToolRequestStatus::Unknown,account,i64::try_from(generation).map_err(|_|invalid(&account))?,i64::try_from(limit).map_err(|_|invalid(&account))?],|row|row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
        rows.into_iter().map(|raw|{
            let task:Task=serde_json::from_str(&raw).map_err(|_|invalid(&account))?;
            let mut accepted=task.payload.accepted;
            accepted.root_request_id=task.payload.root_request_id;
            accepted.source_admission=callbacks::callback_admission(db,&accepted.root_request_id).map_err(callback_error)?;
            if let Some(mut submitted)=task.payload.submitted {
                // An expired/missing canonical body is a hold, never another dispatch.
                submitted.hydrate(db)?;accepted.submitted=Some(submitted);
            }
            Ok(accepted)
        }).collect()
    }).await
}
const DEMAND: &str = "FROM runtime_tool_requests WHERE kind=?1 AND status=?2 AND json_type(record,'$.payload.submitted')='object' AND json_extract(record,'$.payload.recovery_attempted')=0 AND json_extract(record,'$.payload.accepted.actor') IS NULL";
pub(super) fn reply_accounts(
    db: &Connection,
    after: &str,
    limit: usize,
) -> Result<Vec<String>, WorkerError> {
    let mut query=db.prepare_cached(&format!("SELECT DISTINCT json_extract(record,'$.payload.accepted.account') {DEMAND} AND json_extract(record,'$.payload.accepted.account')>?3 ORDER BY 1 LIMIT ?4"))?;
    Ok(query
        .query_map(
            params![
                ToolRequestKind::ProviderCallback,
                ToolRequestStatus::Running,
                after,
                i64::try_from(limit).map_err(|_| invalid(after))?
            ],
            |row| row.get(0),
        )?
        .collect::<Result<Vec<_>, _>>()?)
}
pub(super) fn has_reply_demand(db: &Connection, account: &str) -> Result<bool, WorkerError> {
    Ok(db.query_row(&format!("SELECT EXISTS(SELECT 1 {DEMAND} AND json_extract(record,'$.payload.accepted.account')=?3)"),params![ToolRequestKind::ProviderCallback,ToolRequestStatus::Running,account],|row|row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use studio_runtime_process::{EventCursor, Generation};
    use studio_runtime_providers::{Method, RequestFrame, RequestId};

    fn request(cursor: u64, params: Value) -> JournalEntry {
        JournalEntry {
            received_at_ms: 1,
            cursor: EventCursor(cursor),
            generation: Generation(7),
            payload: JournalPayload::Frame(Frame::Request(RequestFrame {
                id: RequestId::String("opaque-rpc".into()),
                method: Method::CurrentTimeRead,
                params: Some(params),
                fields: Default::default(),
            })),
        }
    }
    #[tokio::test]
    async fn callback_task_reuses_logical_root_at_later_cursor_and_keeps_retired_fence() {
        let directory = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        let roots = store
            .transaction_with(|tx| {
                let first = request(1, json!({}));
                let second = request(2, json!({}));
                accept(tx, "account", "account", None, &first)?;
                accept(tx, "account", "account", None, &second)?;
                let first = task(tx, &operation("account", 7, 1))?.expect("first obligation");
                let second = task(tx, &operation("account", 7, 2))?.expect("second obligation");
                assert_ne!(first.id, second.id);
                assert_eq!(
                    first.payload.root_request_id,
                    second.payload.root_request_id
                );
                assert!(
                    accept(
                        tx,
                        "account",
                        "account",
                        None,
                        &request(1, json!({"changed":true}))
                    )
                    .is_err()
                );
                retire(tx, &first.origin_key)?;
                accept(tx, "account", "account", None, &request(1, json!({})))?;
                let retired = task(tx, &first.origin_key)?.expect("retained operation fence");
                assert_eq!(retired.status, ToolRequestStatus::Completed);
                assert!(retired.payload.accepted.frame.is_empty());
                Ok::<_, WorkerError>(second.payload.root_request_id)
            })
            .await
            .expect("transaction");
        let pending = unsent(&store, "account", 7, 10)
            .await
            .expect("pending tasks");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].root_request_id, roots);
        assert!(pending[0].source_admission.is_some());
    }
}
