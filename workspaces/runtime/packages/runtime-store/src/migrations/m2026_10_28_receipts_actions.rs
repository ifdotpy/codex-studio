//! Classified action obligations keep their original request identities and exact replies.
use crate::outbox::OutboxRow;
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, ReceiptProjection, Result,
    SourceReceipt, admitted_owner_token, delete_source, import_outbox, import_result,
    read_receipt_batch, read_source_batch, unclassifiable,
};
use crate::request_results::{AdmittedRequest, ResultEnvelope};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    MigrationStatus, MonitorInputStatus, NativeActionAdmissionStatus, OutboxKind, OutboxStatus,
    OwnerKind, RequestIdentity, SteerStage, TerminalInputDelivery,
};

pub(super) struct ActionsMigration;
pub(super) const SOURCES: &[&str] = &[
    "runtime_operation_receipts",
    "runtime_native_action_receipts",
    "user_terminal_receipts",
    "runtime_agents",
    "runtime_safety_retries",
    "runtime_limit_reset_attempts",
    "runtime_process_actions",
    "runtime_monitors",
];
/// Source-era default for records predating the transport sequence. Unit: attempts.
const MISSING_RESET_TRANSPORT_SEQUENCE: u64 = 0;
const ACTIONS: &str = "source.id GLOB 'native-command:*' OR source.id GLOB 'monitor-http-input:*' OR source.id GLOB 'claude-steer:*' OR source.id GLOB 'manual-management:*' OR source.id GLOB 'park-event:*'";

#[derive(Serialize)]
struct NativeActionIntent<'a> {
    idle: Option<IdleActionProvenance>,
    admitted: AdmittedRequest,
    receipt: &'a Value,
    outcome: &'a Value,
}
#[derive(Serialize)]
struct InputIntent<'a> {
    admitted: AdmittedRequest,
    receipt: &'a Value,
}
#[derive(Serialize)]
struct CommandCancelIntent<'a> {
    admitted: AdmittedRequest,
    request: &'a Value,
}
#[derive(Serialize)]
struct TerminalActionReply<'a> {
    response: &'a str,
    failure: Option<&'a str>,
}
#[derive(Serialize)]
struct TerminalActionIntent<'a> {
    admitted: AdmittedRequest,
    reply: TerminalActionReply<'a>,
}

fn obligation<P: Serialize>(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    cutover: i64,
    effect: (Option<&str>, OutboxKind, (OwnerKind, &str)),
    status: OutboxStatus,
    payload: P,
    provenance: (Option<String>, Option<i64>),
) -> Result<()> {
    let (namespace, kind, owner) = effect;
    let (account, generation) = provenance;
    import_outbox(
        tx,
        &OutboxRow {
            id: namespace.map_or_else(
                || row.id.clone(),
                |namespace| json!([namespace, row.id]).to_string(),
            ),
            kind,
            owner_kind: owner.0,
            owner_id: owner.1.into(),
            request_id: Some(row.id.clone()),
            signature: row.signature.clone(),
            status,
            account,
            stream_id: None,
            generation,
            ordinal: None,
            next_at: if status == OutboxStatus::Pending {
                Some(cutover)
            } else {
                None
            },
            payload,
            created: cutover,
        },
    )
}
fn string<'a>(value: &'a Value, key: &str, table: &str, row: &SourceReceipt) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| unclassifiable(table, &row.id, &row.signature))
}
#[derive(Serialize)]
struct IdleActionProvenance {
    episode_id: String,
    before: Value,
}
fn idle_provenance(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    agent: &str,
) -> Result<Option<IdleActionProvenance>> {
    if !row.id.starts_with("idle-compaction:") {
        return Ok(None);
    }
    let invalid = || unclassifiable(SOURCES[1], &row.id, &row.signature);
    let source: Option<(String, String)> = tx
        .query_row(
            "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
            [&row.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let before = if let Some((signature, raw)) = source {
        if signature != "idle-compaction" {
            return Err(invalid());
        }
        let value: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        value["before"].clone()
    } else {
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [agent],
                |r| r.get(0),
            )
            .optional()?;
        let value: Value =
            serde_json::from_str(&raw.ok_or_else(invalid)?).map_err(|_| invalid())?;
        if value["idleCompactionEpisode"]["id"] != row.id {
            return Err(invalid());
        }
        value["idleCompactionEpisode"]["before"].clone()
    };
    if !before.is_object() {
        return Err(invalid());
    }
    let _: studio_runtime_schema::persistence::ExecutionNodeStatus =
        serde_json::from_value(before["status"].clone()).map_err(|_| invalid())?;
    Ok(Some(IdleActionProvenance {
        episode_id: row.id.clone(),
        before: json!({"status":before["status"],"startAttempt":before["startAttempt"],"turnEpoch":before["turnEpoch"]}),
    }))
}

fn native(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let table = SOURCES[1];
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    let parts: Vec<String> = row.json(table)?;
    if parts.len() != 2 {
        return Err(invalid());
    }
    let saved = json!({"receipt":serde_json::from_str::<Value>(&parts[0]).map_err(|_|invalid())?,"outcome":serde_json::from_str::<Value>(&parts[1]).map_err(|_|invalid())?});
    let receipt = &saved["receipt"];
    let outcome = &saved["outcome"];
    let agent = string(receipt, "agentId", table, row)?;
    let account = string(receipt, "accountKey", table, row)?;
    let _: NativeActionAdmissionStatus =
        serde_json::from_value(receipt["status"].clone()).map_err(|_| invalid())?;
    if receipt["requestId"] != row.id
        || !matches!(receipt["action"].as_str(), Some("compact" | "review"))
        || !receipt["epoch"].as_i64().is_some_and(|v| v >= 0)
        || !receipt["acceptedAt"].as_f64().is_some_and(f64::is_finite)
        || receipt["threadId"].as_str().is_none_or(str::is_empty)
        || receipt["attemptId"].as_str().is_none_or(str::is_empty)
        || (!outcome["error"].is_null() && !outcome["error"].is_string())
    {
        return Err(invalid());
    }
    match outcome["status"].as_str() {
        Some("pending" | "unknown") => obligation(
            tx,
            row,
            cutover,
            (
                Some("native-action"),
                OutboxKind::NativeAction,
                (OwnerKind::Agent, agent),
            ),
            if outcome["status"] == "pending" {
                OutboxStatus::Pending
            } else {
                OutboxStatus::Unknown
            },
            NativeActionIntent {
                idle: idle_provenance(tx, row, agent)?,
                admitted: admitted_owner_token(&row.id, &row.signature, cutover),
                receipt,
                outcome,
            },
            (Some(account.into()), None),
        ),
        Some("failed" | "acknowledged") => {
            let mut reply = json!({"receipt":receipt,"outcome":outcome,"replayed":false});
            if outcome["status"] == "acknowledged"
                && outcome.get("result").is_some_and(|v| !v.is_null())
            {
                reply["result"] = outcome["result"].clone();
            }
            import_result(
                tx,
                &row.id,
                &row.signature,
                &ResultEnvelope {
                    actor: Some(agent.into()),
                    reply,
                    references: vec![agent.into()],
                },
                cutover,
            )
        }
        _ => Err(invalid()),
    }
}
fn terminal(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let table = SOURCES[2];
    let value: Value = row.json(table)?;
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    let terminal = value["id"].as_str();
    let uncertain = if let Some(delivery) = value.get("delivery") {
        match serde_json::from_value::<TerminalInputDelivery>(delivery.clone())
            .map_err(|_| invalid())?
        {
            TerminalInputDelivery::Uncertain => true,
            TerminalInputDelivery::Sent => false,
        }
    } else if terminal.is_some()
        && matches!(
            value["status"].as_str(),
            Some("running" | "exited" | "closed")
        )
    {
        true
    } else {
        return Err(invalid());
    };
    let reply = TerminalActionReply {
        response: &row.result,
        failure: None,
    };
    if uncertain {
        obligation(
            tx,
            row,
            cutover,
            (
                Some("terminal-action"),
                OutboxKind::TerminalAction,
                terminal.map_or((OwnerKind::Request, row.id.as_str()), |id| {
                    (OwnerKind::Terminal, id)
                }),
            ),
            OutboxStatus::Unknown,
            TerminalActionIntent {
                admitted: admitted_owner_token(&row.id, &row.signature, cutover),
                reply,
            },
            (None, None),
        )
    } else {
        import_result(
            tx,
            &row.id,
            &row.signature,
            &ResultEnvelope {
                actor: None,
                reply,
                references: terminal.into_iter().map(str::to_owned).collect(),
            },
            cutover,
        )
    }
}
fn command(tx: &Transaction<'_>, row: &SourceReceipt, value: &Value, cutover: i64) -> Result<()> {
    let table = SOURCES[0];
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    let signature: Value = serde_json::from_str(&row.signature).map_err(|_| invalid())?;
    let task = string(&signature, "task", table, row)?;
    match signature["action"].as_str() {
        Some("input") => import_result(
            tx,
            &row.id,
            &row.signature,
            &ResultEnvelope {
                actor: None,
                reply: value,
                references: vec![],
            },
            cutover,
        ),
        Some("cancel") => {
            let mut request = value["request"].clone();
            let ticket = request["ticket"].clone();
            let operation = string(&ticket, "request", table, row)?.to_owned();
            let call = native_call(&request, &operation, table, row)?;
            let (token, root) = captured_token(tx, table, row, &call, cutover)?;
            request["root_request_id"] = json!(root);
            request["admitted_request"] = json!(token);
            if ticket["operation"] != task || !request["method"].is_string() {
                return Err(invalid());
            }
            match value["status"].as_str() {
                Some("unknown") => obligation(
                    tx,
                    row,
                    cutover,
                    (
                        Some("native-command"),
                        OutboxKind::NativeAction,
                        (OwnerKind::Task, task),
                    ),
                    OutboxStatus::Unknown,
                    CommandCancelIntent {
                        admitted: admitted_owner_token(&row.id, &row.signature, cutover),
                        request: &request,
                    },
                    (
                        Some(string(&ticket, "account", table, row)?.into()),
                        ticket["generation"].as_i64(),
                    ),
                ),
                Some("acknowledged") if value.get("result").is_some() => import_result(
                    tx,
                    &row.id,
                    &row.signature,
                    &ResultEnvelope {
                        actor: Some(string(&ticket, "agent", table, row)?.into()),
                        reply: json!({"request":request,"result":value["result"]}),
                        references: vec![task.into()],
                    },
                    cutover,
                ),
                _ => Err(invalid()),
            }
        }
        _ => Err(invalid()),
    }
}
fn monitor(tx: &Transaction<'_>, row: &SourceReceipt, value: &Value, cutover: i64) -> Result<()> {
    let table = SOURCES[0];
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    if !row.signature.starts_with("http-input-v1:") {
        return Err(invalid());
    }
    let monitor = string(value, "monitor", table, row)?;
    let mut value = value.clone();
    value["operation"] = monitor_operation(tx, row, monitor, value["operation"].clone(), cutover)?;
    let actor = string(&value["operation"], "agent", table, row)?;
    if !value["operation"]["epoch"].is_i64() || !value["operation"]["connectionId"].is_string() {
        return Err(invalid());
    }
    let status: MonitorInputStatus =
        serde_json::from_value(value["status"].clone()).map_err(|_| invalid())?;
    match status {
        MonitorInputStatus::Expired => Err(invalid()),
        MonitorInputStatus::Pending | MonitorInputStatus::Unknown => obligation(
            tx,
            row,
            cutover,
            (
                Some("monitor-http-input"),
                OutboxKind::MonitorInput,
                (OwnerKind::Monitor, monitor),
            ),
            OutboxStatus::Unknown,
            InputIntent {
                admitted: admitted_owner_token(&row.id, &row.signature, cutover),
                receipt: &value,
            },
            (
                Some(string(&value["operation"], "accountKey", table, row)?.into()),
                None,
            ),
        ),
        MonitorInputStatus::Acknowledged | MonitorInputStatus::Rejected => import_result(
            tx,
            &row.id,
            &row.signature,
            &ResultEnvelope {
                actor: Some(actor.into()),
                reply: value,
                references: vec![monitor.into()],
            },
            cutover,
        ),
    }
}
fn steer(tx: &Transaction<'_>, row: &SourceReceipt, mut value: Value, cutover: i64) -> Result<()> {
    let table = SOURCES[0];
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    if row.signature != "claude-steer" {
        return Err(invalid());
    }
    let stage: SteerStage =
        serde_json::from_value(value["stage"].clone()).map_err(|_| invalid())?;
    let agent = string(&value, "agent", table, row)?.to_owned();
    let account = string(&value, "accountKey", table, row)?.to_owned();
    let stream = string(&value, "threadId", table, row)?.to_owned();
    let created = value["created"]
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(invalid)?;
    value["admission"] =
        serde_json::to_value(admitted_owner_token(&row.id, &row.signature, cutover))
            .map_err(|_| invalid())?;
    value["rootRequestId"] = json!(RequestIdentity::issue_root());
    let status = match stage {
        SteerStage::Reserved => OutboxStatus::Pending,
        SteerStage::Submitted => OutboxStatus::Submitted,
        SteerStage::Unconfirmed => OutboxStatus::Unknown,
    };
    import_outbox(
        tx,
        &OutboxRow {
            id: row.id.clone(),
            kind: OutboxKind::Steer,
            owner_kind: OwnerKind::Agent,
            owner_id: agent,
            request_id: Some(row.id.clone()),
            signature: row.signature.clone(),
            status,
            account: Some(account),
            stream_id: Some(stream),
            generation: None,
            ordinal: None,
            next_at: None,
            payload: value,
            created: (created * 1000.0) as i64,
        },
    )
}
fn fired(tx: &Transaction<'_>, row: &SourceReceipt, value: &Value) -> Result<()> {
    let table = SOURCES[0];
    let name = string(value, "event", table, row)?;
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    let root = row
        .id
        .strip_prefix("park-event:")
        .and_then(|key| key.strip_suffix(&format!(":{name}")))
        .filter(|id| !id.is_empty())
        .ok_or_else(invalid)?;
    if row.signature
        != crate::sha256_hex(
            crate::python_json(&json!({"action":"emit_event","event":name})).as_bytes(),
        )
    {
        return Err(invalid());
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [root],
            |r| r.get(0),
        )
        .optional()?;
    let mut agent: Value =
        serde_json::from_str(&raw.ok_or_else(invalid)?).map_err(|_| invalid())?;
    if !agent.is_object() {
        return Err(invalid());
    }
    let mut names: std::collections::BTreeSet<String> = match agent.get("firedEvents") {
        Some(v) => serde_json::from_value(v.clone()).map_err(|_| invalid())?,
        None => Default::default(),
    };
    names.insert(name.into());
    agent["firedEvents"] = json!(names);
    tx.execute(
        "UPDATE runtime_agents SET record=?1 WHERE id=?2",
        params![agent.to_string(), root],
    )?;
    Ok(())
}
#[derive(Serialize)]
struct ManualReply<'a> {
    outcome: studio_runtime_schema::persistence::ToolRequestOutcome,
    value: &'a Value,
}
#[derive(Serialize)]
struct ManualManagement<'a> {
    authorization: Value,
    reply: ManualReply<'a>,
}
fn manual(tx: &Transaction<'_>, row: &SourceReceipt, value: &Value, cutover: i64) -> Result<()> {
    use studio_runtime_schema::persistence::{RecoveryReceiptStatus, ToolRequestOutcome};
    let table = SOURCES[0];
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    let mut identity = row
        .id
        .strip_prefix("manual-management:")
        .ok_or_else(invalid)?
        .splitn(3, ':');
    let lead = identity
        .next()
        .filter(|v| !v.is_empty())
        .ok_or_else(invalid)?;
    let epoch = identity
        .next()
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(invalid)?;
    if identity.next().is_none_or(str::is_empty) {
        return Err(invalid());
    }
    let status: RecoveryReceiptStatus =
        serde_json::from_value(value["status"].clone()).map_err(|_| invalid())?;
    let outcome = match status {
        RecoveryReceiptStatus::Unknown | RecoveryReceiptStatus::Unconfirmed => {
            ToolRequestOutcome::Unknown
        }
        RecoveryReceiptStatus::Blocked
        | RecoveryReceiptStatus::Skipped
        | RecoveryReceiptStatus::Superseded
        | RecoveryReceiptStatus::HistoryMissing => ToolRequestOutcome::NotApplied,
        RecoveryReceiptStatus::InputRestored
        | RecoveryReceiptStatus::Held
        | RecoveryReceiptStatus::Reconciled
        | RecoveryReceiptStatus::Adopted
        | RecoveryReceiptStatus::Accepted
        | RecoveryReceiptStatus::Checked
        | RecoveryReceiptStatus::Released => ToolRequestOutcome::Applied,
    };
    let reply = ManualReply { outcome, value };
    if outcome != ToolRequestOutcome::Unknown {
        return import_result(
            tx,
            &row.id,
            &row.signature,
            &ResultEnvelope {
                actor: Some(lead.into()),
                reply,
                references: vec![],
            },
            cutover,
        );
    }
    let mut query=tx.prepare("SELECT id,json_extract(record,'$.nativeRelease.authorization') FROM runtime_agents WHERE json_extract(record,'$.nativeRelease.authorization.request_id')=?1 LIMIT 2")?;
    let sources = query
        .query_map([&row.id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if sources.len() != 1 {
        return Err(invalid());
    }
    let (target, raw) = &sources[0];
    let mut authorization: Value = serde_json::from_str(raw).map_err(|_| invalid())?;
    if authorization["request_id"] != row.id
        || authorization["signature"] != row.signature
        || authorization["lead"]["id"] != lead
        || authorization["lead"]["epoch"] != epoch
        || authorization["target"]["id"] != *target
    {
        return Err(invalid());
    }
    let account = string(&authorization["target"], "accountKey", table, row)?.to_owned();
    authorization["root_request_id"] = json!(RequestIdentity::issue_root());
    authorization["admission"] =
        serde_json::to_value(admitted_owner_token(&row.id, &row.signature, cutover))
            .map_err(|_| invalid())?;
    tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.nativeRelease.authorization',json(?1)) WHERE id=?2",params![authorization.to_string(),target])?;
    obligation(
        tx,
        row,
        cutover,
        (None, OutboxKind::ManagedTool, (OwnerKind::Agent, target)),
        OutboxStatus::Unknown,
        ManualManagement {
            authorization,
            reply,
        },
        (Some(account), None),
    )
}

type Call = studio_runtime_schema::provider_call_contract::ProviderCallIntent<
    studio_runtime_providers::Method,
    AdmittedRequest,
>;
fn retained(
    tx: &Transaction<'_>,
    table: &str,
    row: &SourceReceipt,
    account: &str,
    operation: &str,
    cutover: i64,
) -> Result<Option<(Call, AdmittedRequest)>> {
    let key = format!(
        "account-worker:{}",
        studio_runtime_schema::provider_call_contract::transport_key(account, operation)
    );
    let raw: Option<(String, String)> = tx
        .query_row(
            "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
            [format!("{key}:submitted")],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((source_signature, raw)) = raw else {
        return Ok(None);
    };
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    let mut saved: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
    if !saved["call"].is_object() {
        return Err(invalid());
    }
    saved["call"]["root_request_id"] = json!("");
    saved["call"]["admitted_request"] = Value::Null;
    let mut call: Call = serde_json::from_value(saved["call"].clone()).map_err(|_| invalid())?;
    if call.scope.account != account || call.operation != operation {
        return Err(invalid());
    }
    let token = admitted_owner_token(
        &format!("{key}:result"),
        &call.fingerprint(account, call.method.as_str()),
        cutover,
    );
    call.root_request_id = crate::receipt_migration_contract::hydrate_source_root(
        tx,
        "runtime_operation_receipts",
        &format!("{key}:submitted"),
        &source_signature,
        "result",
        "$.call.root_request_id",
    )?;
    Ok(Some((call, token)))
}
fn captured_token(
    tx: &Transaction<'_>,
    table: &str,
    row: &SourceReceipt,
    call: &Call,
    cutover: i64,
) -> Result<(AdmittedRequest, String)> {
    let signature = call.fingerprint(&call.scope.account, call.method.as_str());
    if let Some((saved, token)) = retained(
        tx,
        table,
        row,
        &call.scope.account,
        &call.operation,
        cutover,
    )? {
        if saved.fingerprint(&saved.scope.account, saved.method.as_str()) != signature
            || saved.scope != call.scope
        {
            return Err(unclassifiable(table, &row.id, &row.signature));
        }
        return Ok((token, saved.root_request_id));
    }
    Err(unclassifiable(table, &row.id, &row.signature))
}
fn native_call(request: &Value, operation: &str, table: &str, row: &SourceReceipt) -> Result<Call> {
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    let ticket = &request["ticket"];
    let method =
        studio_runtime_providers::Method::from_wire(string(request, "method", table, row)?)
            .ok_or_else(invalid)?;
    Ok(Call {
        root_request_id: String::new(),
        admitted_request: None,
        scope: studio_runtime_schema::provider_call_contract::ConnectionScope {
            account: string(ticket, "account", table, row)?.into(),
            connection: string(ticket, "connection", table, row)?.into(),
            generation: ticket["generation"].as_u64().ok_or_else(invalid)?,
        },
        operation: operation.into(),
        actor: Some(
            studio_runtime_schema::provider_call_contract::RequestActor {
                agent: string(ticket, "agent", table, row)?.into(),
                epoch: ticket["epoch"].as_i64().ok_or_else(invalid)?,
            },
        ),
        transfer: None,
        deleted_actor_session: None,
        method,
        params: request.get("params").cloned().ok_or_else(invalid)?,
    })
}
fn action_known(tx: &Transaction<'_>, id: &str) -> Result<bool> {
    if crate::receipt_migration_contract::source_exists(tx, SOURCES[1])?
        && tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_native_action_receipts WHERE id=?1)",
            [id],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Ok(true);
    }
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM request_results WHERE id=?1 UNION ALL SELECT 1 FROM runtime_outbox WHERE request_id=?1 AND kind='native_action')",[id],|r|r.get(0))?)
}
fn agent_owners(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    mut value: Value,
    cutover: i64,
) -> Result<Value> {
    use studio_runtime_schema::persistence::{NativeReleasePhase, NativeSettingsStatus};
    let table = "runtime_agents";
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    if !value.is_object() {
        return Err(invalid());
    }
    if value["startAttempt"]["actionRequestId"].is_string()
        && value["startAttempt"]["rootRequestId"].is_null()
    {
        value["startAttempt"]["rootRequestId"] = json!(RequestIdentity::issue_root());
    }
    if value["startAttempt"]["nativeRequest"].is_object() {
        let mut request = value["startAttempt"]["nativeRequest"].clone();
        let operation = string(&request["ticket"], "operation", table, row)?.to_owned();
        let call = native_call(&request, &operation, table, row)?;
        let (token, root) = captured_token(tx, table, row, &call, cutover)?;
        request["root_request_id"] = json!(root);
        value["startAttempt"]["rootRequestId"] = json!(root);
        request["admitted_request"] = json!(token);
        value["startAttempt"]["nativeRequest"] = request;
        value["startAttempt"]["providerRequest"] = json!(token);
    }
    if value["startAttempt"]["modelSettings"].is_object() {
        let mut settings = value["startAttempt"]["modelSettings"].clone();
        let status: NativeSettingsStatus =
            serde_json::from_value(settings["status"].clone()).map_err(|_| invalid())?;
        let call:Call=serde_json::from_value(json!({"root_request_id":"","admitted_request":null,"scope":settings["scope"],"operation":settings["operation"],"actor":{"agent":row.id,"epoch":value["epoch"]},"transfer":null,"deleted_actor_session":null,"method":settings["method"],"params":settings["params"]})).map_err(|_|invalid())?;
        let (token, root) = if retained(
            tx,
            table,
            row,
            &call.scope.account,
            &call.operation,
            cutover,
        )?
        .is_some()
        {
            let (token, root) = captured_token(tx, table, row, &call, cutover)?;
            (Some(token), root)
        } else if matches!(
            status,
            NativeSettingsStatus::Acknowledged
                | NativeSettingsStatus::Rejected
                | NativeSettingsStatus::NotSent
        ) {
            (
                None,
                crate::receipt_migration_contract::hydrate_source_root(
                    tx,
                    table,
                    &row.id,
                    &row.signature,
                    "record",
                    "$.startAttempt.modelSettings.rootRequestId",
                )?,
            )
        } else {
            return Err(invalid());
        };
        if let Some(token) = token {
            settings["providerRequest"] = json!(token);
        } else if let Some(settings) = settings.as_object_mut() {
            settings.remove("providerRequest");
        }
        settings["rootRequestId"] = json!(root);
        value["startAttempt"]["modelSettings"] = settings;
    }
    if value["nativeRelease"].is_object() {
        let mut release = value["nativeRelease"].clone();
        let _: NativeReleasePhase =
            serde_json::from_value(release["phase"].clone()).map_err(|_| invalid())?;
        let operation = format!("{}:unsubscribe", string(&release, "id", table, row)?);
        let account = release["accountKey"].as_str().unwrap_or("default");
        release["admittedRequest"] = match retained(tx, table, row, account, &operation, cutover)? {
            Some((_, token)) => json!(token),
            None => Value::Null,
        };
        if release["authorization"].is_object() {
            let mut auth = release["authorization"].clone();
            let request = string(&auth, "request_id", table, row)?.to_owned();
            let signature = string(&auth, "signature", table, row)?.to_owned();
            if !auth["lead"].is_object() || !auth["target"].is_object() {
                return Err(invalid());
            }
            auth["root_request_id"] =
                json!(crate::receipt_migration_contract::hydrate_source_root(
                    tx,
                    table,
                    &row.id,
                    &row.signature,
                    "record",
                    "$.nativeRelease.authorization.root_request_id"
                )?);
            if auth["admission"].is_null() {
                auth["admission"] = json!(admitted_owner_token(&request, &signature, cutover));
            }
            let token: AdmittedRequest =
                serde_json::from_value(auth["admission"].clone()).map_err(|_| invalid())?;
            if token.id() != request || token.signature() != signature {
                return Err(invalid());
            }
            release["authorization"] = auth;
        }
        value["nativeRelease"] = release;
    }
    if value["nativeReview"].is_object() && value["nativeReview"]["actionRequestId"].is_null() {
        let epoch = value["epoch"].as_i64().ok_or_else(invalid)?;
        let old = format!("native-review:{}:{epoch}", row.id);
        value["nativeReview"]["actionRequestId"] = json!(if action_known(tx, &old)? {
            old
        } else {
            RequestIdentity::issue_root()
        });
    }
    if value["nativeReviewThread"].is_object() {
        let mut thread = value["nativeReviewThread"].clone();
        let _: studio_runtime_schema::persistence::NativeReviewThreadStatus =
            serde_json::from_value(thread["status"].clone()).map_err(|_| invalid())?;
        let operation = string(&thread, "operation", table, row)?;
        let account = value["accountKey"].as_str().unwrap_or("default");
        let authority = retained(tx, table, row, account, operation, cutover)?;
        let (token, root) = authority.map_or_else(
            || (None, RequestIdentity::issue_root()),
            |(call, token)| (Some(token), call.root_request_id),
        );
        thread["rootRequestId"] = json!(root);
        thread["admittedRequest"] = json!(token);
        value["nativeReviewThread"] = thread;
    }
    Ok(value)
}
fn monitor_operation(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    id: &str,
    mut operation: Value,
    cutover: i64,
) -> Result<Value> {
    let table = "runtime_monitors";
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    let account = string(&operation, "accountKey", table, row)?;
    let authority = retained(tx, table, row, account, &format!("monitor:{id}"), cutover)?;
    if let Some((call, token)) = authority {
        if call
            .actor
            .as_ref()
            .is_none_or(|a| operation["agent"] != a.agent || operation["epoch"] != a.epoch)
            || operation["connectionId"] != call.scope.connection
        {
            return Err(invalid());
        }
        operation["rootRequestId"] = json!(call.root_request_id);
        operation["providerRequest"] = json!(token);
        operation["providerGeneration"] = json!(call.scope.generation);
    } else if operation["rootRequestId"].is_null() {
        let current: Option<String> =
            if crate::receipt_migration_contract::source_exists(tx, table)? {
                tx.query_row(
                    "SELECT record FROM runtime_monitors WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
            } else {
                None
            };
        let same = current
            .as_ref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .is_some_and(|value| {
                value["operation"]["agent"] == operation["agent"]
                    && value["operation"]["epoch"] == operation["epoch"]
                    && value["operation"]["connectionId"] == operation["connectionId"]
            });
        operation["rootRequestId"] = json!(if same {
            crate::receipt_migration_contract::hydrate_source_root(
                tx,
                table,
                id,
                &row.signature,
                "record",
                "$.operation.rootRequestId",
            )?
        } else {
            RequestIdentity::issue_root()
        });
        operation["providerRequest"] = Value::Null;
    }
    Ok(operation)
}
fn monitor_owner(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    mut value: Value,
    cutover: i64,
) -> Result<Value> {
    let invalid = || unclassifiable("runtime_monitors", &row.id, &row.signature);
    if !value.is_object() {
        return Err(invalid());
    }
    if value["operation"].is_object() {
        let operation = monitor_operation(tx, row, &row.id, value["operation"].clone(), cutover)?;
        value["operation"] = operation;
        if let Some(queue) = value["streamQueue"].as_array_mut() {
            for entry in queue {
                if entry["operation"].is_object() {
                    entry["operation"] =
                        monitor_operation(tx, row, &row.id, entry["operation"].clone(), cutover)?;
                }
            }
        }
    } else if value["rootRequestId"].is_null() {
        value["rootRequestId"] = json!(RequestIdentity::issue_root());
    }
    let account = value["operation"]["accountKey"].as_str().map(str::to_owned);
    if let Some(account) = account {
        if value["inputAttempt"].is_object() {
            let mut input = value["inputAttempt"].clone();
            let id = string(&input, "id", "runtime_monitors", row)?;
            let authority = retained(
                tx,
                "runtime_monitors",
                row,
                &account,
                &format!("monitor-input:{}:{id}", row.id),
                cutover,
            )?;
            let (root, token) = authority.map_or_else(
                || (RequestIdentity::issue_root(), None),
                |(call, token)| (call.root_request_id, Some(token)),
            );
            input["rootRequestId"] = json!(root);
            input["providerRequest"] = json!(token);
            value["inputAttempt"] = input;
        }
        if value["cancelRequested"] == true {
            let authority = retained(
                tx,
                "runtime_monitors",
                row,
                &account,
                &format!("monitor-cancel:{}", row.id),
                cutover,
            )?;
            let (root, token) = authority.map_or_else(
                || (RequestIdentity::issue_root(), None),
                |(call, token)| (call.root_request_id, Some(token)),
            );
            value["cancelRootRequestId"] = json!(root);
            value["cancelProviderRequest"] = json!(token);
        }
        if let Some(generation) = value["stallProbeGeneration"].as_i64() {
            let authority = retained(
                tx,
                "runtime_monitors",
                row,
                &account,
                &format!("liveness:{}:{generation}", row.id),
                cutover,
            )?;
            let (root, token) = authority.map_or_else(
                || (RequestIdentity::issue_root(), None),
                |(call, token)| (call.root_request_id, Some(token)),
            );
            value["livenessProbe"] =
                json!({"generation":generation,"rootRequestId":root,"providerRequest":token});
        }
    }
    Ok(value)
}

fn operation_owner(
    tx: &Transaction<'_>,
    table: &str,
    row: &SourceReceipt,
    mut value: Value,
    cutover: i64,
) -> Result<Value> {
    use studio_runtime_schema::persistence::{
        ResetCreditCertainty, ResetCreditOutcome, ResetCreditStatus, SafetyRetryDispatch,
        SafetyRetryStage,
    };
    let invalid = || unclassifiable(table, &row.id, &row.signature);
    if !value.is_object() {
        return Err(invalid());
    }
    if table == "runtime_safety_retries" {
        let _: SafetyRetryStage =
            serde_json::from_value(value["stage"].clone()).map_err(|_| invalid())?;
        if !value["dispatch"].is_null() {
            let _: SafetyRetryDispatch =
                serde_json::from_value(value["dispatch"].clone()).map_err(|_| invalid())?;
        }
        value["rootRequestId"] = json!(RequestIdentity::issue_root());
        value["admission"] = json!(admitted_owner_token(&row.id, "safety-retry", cutover));
        if let Some(method) = value["rpcMethod"]
            .as_str()
            .and_then(studio_runtime_providers::Method::from_wire)
            && matches!(
                method,
                studio_runtime_providers::Method::TurnInterrupt
                    | studio_runtime_providers::Method::ThreadFork
                    | studio_runtime_providers::Method::TurnStart
            )
        {
            let call:Call=serde_json::from_value(json!({"root_request_id":"","admitted_request":null,"scope":value["scope"],"operation":value["operationId"],"actor":{"agent":value["agent"],"epoch":value["epoch"]},"transfer":null,"deleted_actor_session":null,"method":method,"params":value["requestParams"]})).map_err(|_|invalid())?;
            let (token, root) = captured_token(tx, table, row, &call, cutover)?;
            value["admittedRequest"] = json!(token);
            value["rootRequestId"] = json!(root);
        }
    } else if table == "runtime_limit_reset_attempts" {
        let status: ResetCreditStatus =
            serde_json::from_value(value["status"].clone()).map_err(|_| invalid())?;
        if !value["certainty"].is_null() {
            let _: ResetCreditCertainty =
                serde_json::from_value(value["certainty"].clone()).map_err(|_| invalid())?;
        }
        if !value["outcome"].is_null() {
            let _: ResetCreditOutcome =
                serde_json::from_value(value["outcome"].clone()).map_err(|_| invalid())?;
        }
        value["root_request_id"] = json!(RequestIdentity::issue_root());
        value["admission"] = json!(admitted_owner_token(&row.id, "limit-reset", cutover));
        let operation = format!(
            "limit-reset:{}:{}",
            string(&value, "idempotency_key", table, row)?,
            match value.get("transport_attempts") {
                None => MISSING_RESET_TRANSPORT_SEQUENCE,
                Some(value) => value.as_u64().ok_or_else(invalid)?,
            }
        );
        let account = string(&value, "account_key", table, row)?;
        let authority = retained(tx, table, row, account, &operation, cutover)?;
        let token = if let Some((call, token)) = authority {
            if call.method != studio_runtime_providers::Method::AccountRateLimitResetCreditConsume
                || call.actor.is_some()
                || call.params["creditId"] != value["credit_id"]
                || call.params["idempotencyKey"] != value["idempotency_key"]
            {
                return Err(invalid());
            }
            value["root_request_id"] = json!(call.root_request_id);
            Some(token)
        } else {
            None
        };
        if status == ResetCreditStatus::Uncertain && token.is_none() {
            return Err(invalid());
        }
        value["provider_request"] = json!(token);
    } else {
        if !matches!(
            value["phase"].as_str(),
            Some("dispatching" | "uncertain" | "completed")
        ) {
            return Err(invalid());
        }
        let admitted: AdmittedRequest = match value
            .get("admittedRequest")
            .filter(|value| !value.is_null())
        {
            Some(value) => serde_json::from_value(value.clone()).map_err(|_| invalid())?,
            None => admitted_owner_token(&row.id, &row.signature, cutover),
        };
        if admitted.id().is_empty() || admitted.signature() != row.signature {
            return Err(invalid());
        }
        value["admittedRequest"] = json!(admitted);
        value["rootRequestId"] = json!(crate::receipt_migration_contract::hydrate_source_root(
            tx,
            table,
            &row.id,
            &row.signature,
            "record",
            "$.rootRequestId"
        )?);
    }
    Ok(value)
}

impl ReceiptFamilyMigration for ActionsMigration {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-actions"
    }
    fn prepare(&self, _tx: &Transaction<'_>, _cutover_ms: i64) -> Result<()> {
        Ok(())
    }
    fn move_batch(
        &self,
        tx: &Transaction<'_>,
        source: &str,
        checkpoint: &MigrationCheckpoint,
    ) -> Result<MigrationCheckpoint> {
        let mut next = checkpoint.clone();
        if checkpoint.status == MigrationStatus::Complete {
            return Ok(next);
        }
        if !SOURCES.contains(&source) {
            return Err(ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: source.into(),
            });
        }
        if !crate::receipt_migration_contract::source_exists(tx, source)? {
            next.status = MigrationStatus::Complete;
            return Ok(next);
        }
        let domain = matches!(
            source,
            "runtime_agents"
                | "runtime_safety_retries"
                | "runtime_limit_reset_attempts"
                | "runtime_process_actions"
                | "runtime_monitors"
        );
        let rows = if domain {
            read_source_batch(
                tx,
                source,
                checkpoint,
                &ReceiptProjection {
                    id: "source.id",
                    signature: if source == "runtime_process_actions" {
                        "source.signature"
                    } else {
                        "'action-owner'"
                    },
                    result: "source.record",
                },
                "1",
            )?
        } else if source == SOURCES[1] {
            read_source_batch(
                tx,
                source,
                checkpoint,
                &ReceiptProjection {
                    id: "source.id",
                    signature: "source.signature",
                    result: "json_array(source.receipt,source.outcome)",
                },
                "1",
            )?
        } else {
            read_receipt_batch(
                tx,
                source,
                checkpoint,
                if source == SOURCES[0] { ACTIONS } else { "1" },
            )?
        };
        for row in rows {
            if domain {
                let value: Value = row.json(source)?;
                let updated = if source == "runtime_agents" {
                    agent_owners(tx, &row, value, checkpoint.cutover_ms)?
                } else if source == "runtime_monitors" {
                    monitor_owner(tx, &row, value, checkpoint.cutover_ms)?
                } else {
                    operation_owner(tx, source, &row, value, checkpoint.cutover_ms)?
                };
                tx.execute(
                    &format!("UPDATE {source} SET record=?1 WHERE rowid=?2"),
                    params![updated.to_string(), row.rowid],
                )?;
                next.cursor = row.rowid;
                continue;
            }
            match source {
                "runtime_native_action_receipts" => native(tx, &row, checkpoint.cutover_ms)?,
                "user_terminal_receipts" => terminal(tx, &row, checkpoint.cutover_ms)?,
                "runtime_operation_receipts" => {
                    let value: Value = row.json(source)?;
                    if row.id.starts_with("native-command:") {
                        command(tx, &row, &value, checkpoint.cutover_ms)?;
                    } else if row.id.starts_with("monitor-http-input:") {
                        monitor(tx, &row, &value, checkpoint.cutover_ms)?;
                    } else if row.id.starts_with("claude-steer:") {
                        steer(tx, &row, value, checkpoint.cutover_ms)?;
                    } else if row.id.starts_with("park-event:") {
                        fired(tx, &row, &value)?;
                    } else if row.id.starts_with("manual-management:") {
                        manual(tx, &row, &value, checkpoint.cutover_ms)?;
                    } else {
                        return Err(unclassifiable(source, &row.id, &row.signature));
                    }
                }
                _ => return Err(unclassifiable(source, &row.id, &row.signature)),
            }
            delete_source(tx, source, row.rowid)?;
            next.cursor = row.rowid;
        }
        next.status = if tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {source} source WHERE rowid>?1 AND ({}))",
                if source == SOURCES[0] { ACTIONS } else { "1" }
            ),
            [next.cursor],
            |r| r.get::<_, bool>(0),
        )? {
            MigrationStatus::Running
        } else {
            MigrationStatus::Complete
        };
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        for source in &SOURCES[..3] {
            if crate::receipt_migration_contract::source_exists(tx, source)?
                && tx.query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM {source} source WHERE {})",
                        if *source == SOURCES[0] { ACTIONS } else { "1" }
                    ),
                    [],
                    |r| r.get::<_, bool>(0),
                )?
            {
                return Err(ReceiptMigrationError::Incomplete {
                    migration: self.id().into(),
                    source_table: (*source).into(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::Outbox;
    use crate::receipt_migration_contract::{
        MIGRATION_BATCH_BYTES, MIGRATION_BATCH_ROWS, prepare_schema,
    };
    use rusqlite::Connection;
    fn fixture(db: &Connection) {
        db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT); CREATE TABLE runtime_native_action_receipts(id TEXT PRIMARY KEY,signature TEXT,receipt TEXT,outcome TEXT); CREATE TABLE user_terminal_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT); CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);").expect("source");
    }
    fn insert_native(db: &Connection, n: usize, status: &str) {
        let id = format!("native-{n}");
        let receipt = json!({"requestId":id,"agentId":"agent","action":"compact","accountKey":"account","threadId":"thread","epoch":1,"attemptId":"attempt","acceptedAt":1.0,"status":"accepted"});
        db.execute(
            "INSERT INTO runtime_native_action_receipts VALUES(?1,'signature',?2,?3)",
            params![
                id,
                receipt.to_string(),
                json!({"status":status,"result":{"ok":true}}).to_string()
            ],
        )
        .expect("native receipt");
    }
    fn batch(db: &mut Connection, source: &str) -> Result<MigrationCheckpoint> {
        let tx = db.transaction()?;
        prepare_schema(&tx)?;
        let cp = MigrationCheckpoint::begin(&tx, ActionsMigration.id(), source, 1000)?;
        let next = ActionsMigration.move_batch(&tx, source, &cp)?;
        next.save(&tx, ActionsMigration.id(), source, 1000)?;
        tx.commit()?;
        Ok(next)
    }
    #[test]
    fn receipts_actions_fixtures_exact_replies_and_unknown_survive_reopen() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        insert_native(&db, 0, "acknowledged");
        insert_native(&db, 1, "unknown");
        insert_native(&db, 2, "pending");
        let monitor = json!({"monitor":"monitor","operation":{"agent":"agent","accountKey":"account","epoch":1,"connectionId":"1"},"status":"Unknown"});
        db.execute("INSERT INTO runtime_operation_receipts VALUES('monitor-http-input:root','http-input-v1:hash',?1)",[monitor.to_string()]).expect("monitor");
        db.execute(
            "INSERT INTO user_terminal_receipts VALUES('terminal-input','signature',?1)",
            [json!({"ok":false,"delivery":"uncertain"}).to_string()],
        )
        .expect("terminal");
        for source in SOURCES {
            batch(&mut db, source).expect("migrate");
        }
        let native: OutboxRow<Value> =
            Outbox::get(&db, &json!(["native-action", "native-1"]).to_string())
                .expect("read")
                .expect("unknown");
        assert_eq!(native.status, OutboxStatus::Unknown);
        assert_eq!(native.next_at, None);
        assert_eq!(native.payload["admitted"]["id"], "native-1");
        let input: OutboxRow<Value> = Outbox::get(
            &db,
            &json!(["terminal-action", "terminal-input"]).to_string(),
        )
        .expect("read")
        .expect("input");
        assert_eq!(input.owner_kind, OwnerKind::Request);
        let saved: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id='native-0'",
                [],
                |r| r.get(0),
            )
            .expect("saved reply");
        assert_eq!(
            serde_json::from_str::<Value>(&saved).expect("JSON")["reply"]["result"],
            json!({"ok":true})
        );
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        for source in SOURCES {
            batch(&mut db, source).expect("second open");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("outbox"),
            4
        );
        assert_eq!(
            Outbox::pending::<Value>(&db, OutboxKind::NativeAction, 2000, 500)
                .expect("send selector")
                .len(),
            1
        );
    }
    #[test]
    fn receipts_actions_crash_batch_resumes_and_bad_status_aborts_atomically() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        {
            let setup = db.transaction().expect("fixture transaction");
            for n in 0..=MIGRATION_BATCH_ROWS {
                insert_native(&setup, n, "unknown");
            }
            setup.commit().expect("fixture commit");
        }
        assert_eq!(batch(&mut db, SOURCES[1]).expect("first batch").cursor, 500);
        {
            let tx = db.transaction().expect("tx");
            let cp = MigrationCheckpoint::load(&tx, ActionsMigration.id(), SOURCES[1])
                .expect("load")
                .expect("checkpoint");
            ActionsMigration
                .move_batch(&tx, SOURCES[1], &cp)
                .expect("uncommitted move");
        }
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        assert_eq!(batch(&mut db, SOURCES[1]).expect("resume").cursor, 501);
        insert_native(&db, 999, "future-outcome");
        db.execute(
            "DELETE FROM runtime_payload_migrations WHERE name!='receipts:cutover'",
            [],
        )
        .expect("new migration test");
        assert!(
            matches!(batch(&mut db,SOURCES[1]),Err(ReceiptMigrationError::Unclassifiable{table,key,signature:_}) if table==SOURCES[1]&&key=="native-999")
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM runtime_native_action_receipts",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("preserved"),
            1
        );
    }
    #[test]
    fn receipts_actions_byte_limit_oversized_row_alone_and_named_events_keep_all_names() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        let large =
            json!({"id":"terminal","status":"running","text":"x".repeat(MIGRATION_BATCH_BYTES+1)})
                .to_string();
        for (id, result) in [
            ("small", json!({"delivery":"sent","ok":true}).to_string()),
            ("large", large),
            ("tail", json!({"delivery":"sent","ok":true}).to_string()),
        ] {
            db.execute(
                "INSERT INTO user_terminal_receipts VALUES(?1,'sig',?2)",
                params![id, result],
            )
            .expect("receipt");
        }
        assert_eq!(batch(&mut db, SOURCES[2]).expect("small batch").cursor, 1);
        assert_eq!(batch(&mut db, SOURCES[2]).expect("large alone").cursor, 2);
        assert_eq!(batch(&mut db, SOURCES[2]).expect("tail").cursor, 3);
        db.execute(
            "INSERT INTO runtime_agents VALUES('root:with:colons','{}')",
            [],
        )
        .expect("root");
        for name in ["build:done", "review:done"] {
            let sig = crate::sha256_hex(
                crate::python_json(&json!({"action":"emit_event","event":name})).as_bytes(),
            );
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                params![
                    format!("park-event:root:with:colons:{name}"),
                    sig,
                    json!({"event":name,"count":0,"woken":[]}).to_string()
                ],
            )
            .expect("fired fact");
        }
        batch(&mut db, SOURCES[0]).expect("facts migrated");
        let raw: String = db
            .query_row("SELECT record FROM runtime_agents", [], |r| r.get(0))
            .expect("owner");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("JSON")["firedEvents"],
            json!(["build:done", "review:done"])
        );
    }
    #[test]
    fn receipts_actions_manual_known_reply_and_unknown_authorization_are_preserved() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        let id = "manual-management:lead:1:request";
        let auth = json!({"lead":{"id":"lead","epoch":1},"target":{"id":"worker","epoch":2,"accountKey":"account"},"request_id":id,"signature":"sig"});
        db.execute(
            "INSERT INTO runtime_agents VALUES('worker',?1)",
            [json!({"nativeRelease":{"authorization":auth}}).to_string()],
        )
        .expect("captured authorization");
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'sig',?2)",
            params![
                id,
                json!({"status":"unknown","manualRecoveryComplete":true}).to_string()
            ],
        )
        .expect("unknown");
        db.execute("INSERT INTO runtime_operation_receipts VALUES('manual-management:lead:1:final','sig',?1)",[json!({"status":"released"}).to_string()]).expect("final reply");
        batch(&mut db, SOURCES[0]).expect("migrate");
        let row: OutboxRow<Value> = Outbox::get(&db, id)
            .expect("read")
            .expect("unknown retained");
        assert_eq!(row.status, OutboxStatus::Unknown);
        assert_eq!(row.payload["authorization"]["admission"]["id"], id);
        assert_eq!(row.payload["reply"]["outcome"], "unknown");
        let reply: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id='manual-management:lead:1:final'",
                [],
                |r| r.get(0),
            )
            .expect("final");
        assert_eq!(
            serde_json::from_str::<Value>(&reply).expect("JSON")["reply"]["value"],
            json!({"status":"released"})
        );
    }
    fn call_fixture(db: &Connection, operation: &str, method: &str) -> (Call, String) {
        let call:Call=serde_json::from_value(json!({"root_request_id":"","admitted_request":null,"scope":{"account":"account","connection":"1","generation":1},"operation":operation,"actor":{"agent":"agent","epoch":1},"transfer":null,"deleted_actor_session":null,"method":method,"params":{"threadId":"thread"}})).expect("call");
        let key =
            studio_runtime_schema::provider_call_contract::transport_key("account", operation);
        let id = format!("account-worker:{key}:submitted");
        db.execute("INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",params![id,json!({"call":call,"request":{"id":key,"generation":1,"method":method,"operation":key,"thread":"thread","lightweight":true,"deadline":60000,"remoteId":null,"unknown":false}}).to_string()]).expect("retained source");
        (call, id)
    }
    #[test]
    fn receipts_actions_source_and_owner_roots_commit_together_and_do_not_refresh() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        let (call, source) = call_fixture(&db, "attempt", "thread/compact/start");
        let request = json!({"ticket":{"operation":"attempt","agent":"agent","epoch":1,"account":"account","thread":"thread","connection":"1","generation":1,"request":"native-0"},"method":"thread/compact/start","params":{"threadId":"thread"}});
        db.execute("INSERT INTO runtime_agents VALUES('agent',?1)",[json!({"id":"agent","epoch":1,"startAttempt":{"id":"attempt","actionRequestId":"native-0","nativeRequest":request}}).to_string()]).expect("captured owner");
        {
            let tx = db.transaction().expect("tx");
            prepare_schema(&tx).expect("schema");
            let cp = MigrationCheckpoint::begin(&tx, ActionsMigration.id(), "runtime_agents", 1000)
                .expect("begin");
            ActionsMigration
                .move_batch(&tx, "runtime_agents", &cp)
                .expect("classify");
        }
        let raw: String = db
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                [&source],
                |r| r.get(0),
            )
            .expect("rolled back source");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("JSON")["call"]["root_request_id"],
            ""
        );
        batch(&mut db, "runtime_agents").expect("commit both");
        let raw: String = db
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                [&source],
                |r| r.get(0),
            )
            .expect("source");
        let saved: Value = serde_json::from_str(&raw).expect("JSON");
        let root = saved["call"]["root_request_id"]
            .as_str()
            .expect("root")
            .to_owned();
        assert_eq!(
            uuid::Uuid::parse_str(&root).expect("v7").get_version_num(),
            7
        );
        let raw: String = db
            .query_row("SELECT record FROM runtime_agents", [], |r| r.get(0))
            .expect("owner");
        let owner: Value = serde_json::from_str(&raw).expect("JSON");
        assert_eq!(owner["startAttempt"]["rootRequestId"], root);
        assert_eq!(
            owner["startAttempt"]["nativeRequest"]["root_request_id"],
            root
        );
        assert_eq!(
            owner["startAttempt"]["providerRequest"]["signature"],
            call.fingerprint("account", call.method.as_str())
        );
        batch(&mut db, "runtime_agents").expect("no refresh");
        let after: String = db
            .query_row("SELECT record FROM runtime_agents", [], |r| r.get(0))
            .expect("unchanged");
        assert_eq!(raw, after);
    }
    #[test]
    fn receipts_actions_same_external_root_keeps_both_families_and_idle_authority() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        insert_native(&db, 0, "unknown");
        db.execute(
            "INSERT INTO user_terminal_receipts VALUES('native-0','terminal-sig',?1)",
            [json!({"delivery":"uncertain","ok":false}).to_string()],
        )
        .expect("same root");
        batch(&mut db, SOURCES[1]).expect("native");
        batch(&mut db, SOURCES[2]).expect("terminal");
        for namespace in ["native-action", "terminal-action"] {
            assert!(
                Outbox::get::<Value>(&db, &json!([namespace, "native-0"]).to_string())
                    .expect("lookup")
                    .is_some()
            );
        }
        let episode = "idle-compaction:episode";
        let receipt = json!({"requestId":episode,"agentId":"agent","action":"compact","accountKey":"account","threadId":"thread","epoch":1,"attemptId":"attempt","acceptedAt":1.0,"status":"accepted"});
        db.execute("INSERT INTO runtime_native_action_receipts VALUES(?1,'sig',?2,'{\"status\":\"pending\"}')",params![episode,receipt.to_string()]).expect("idle action");
        db.execute(
            "DELETE FROM runtime_payload_migrations WHERE name!='receipts:cutover'",
            [],
        )
        .expect("new test migration");
        assert!(matches!(
            batch(&mut db, SOURCES[1]),
            Err(ReceiptMigrationError::Unclassifiable { .. })
        ));
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'idle-compaction',?2)",
            params![
                episode,
                json!({"before":{"status":"idle","startAttempt":{"id":"before"},"turnEpoch":7}})
                    .to_string()
            ],
        )
        .expect("original provenance");
        batch(&mut db, SOURCES[1]).expect("exact provenance");
        let row: OutboxRow<Value> =
            Outbox::get(&db, &json!(["native-action", episode]).to_string())
                .expect("read")
                .expect("idle");
        assert_eq!(row.payload["idle"]["episode_id"], episode);
        assert_eq!(row.payload["idle"]["before"]["turnEpoch"], 7);
    }
    #[test]
    fn receipts_actions_monitor_current_input_and_queue_keep_matching_provider_identity() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        db.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT)")
            .expect("monitors");
        call_fixture(&db, "monitor:monitor", "command/exec");
        let operation =
            json!({"agent":"agent","epoch":1,"accountKey":"account","connectionId":"1"});
        db.execute(
            "INSERT INTO runtime_monitors VALUES('monitor',?1)",
            [json!({"operation":operation,"streamQueue":[{"operation":operation}]}).to_string()],
        )
        .expect("monitor owner");
        db.execute("INSERT INTO runtime_operation_receipts VALUES('monitor-http-input:root','http-input-v1:hash',?1)",[json!({"monitor":"monitor","operation":operation,"status":"Unknown"}).to_string()]).expect("pending input");
        batch(&mut db, SOURCES[0]).expect("input mapped");
        batch(&mut db, "runtime_monitors").expect("owner mapped");
        let raw: String = db
            .query_row("SELECT record FROM runtime_monitors", [], |r| r.get(0))
            .expect("current");
        let owner: Value = serde_json::from_str(&raw).expect("JSON");
        let input: OutboxRow<Value> = Outbox::get(
            &db,
            &json!(["monitor-http-input", "monitor-http-input:root"]).to_string(),
        )
        .expect("read")
        .expect("input");
        assert_eq!(input.payload["receipt"]["operation"], owner["operation"]);
        assert_eq!(owner["streamQueue"][0]["operation"], owner["operation"]);
        assert_eq!(owner["operation"]["providerGeneration"], 1);
    }
    #[test]
    fn receipts_actions_completed_current_settings_do_not_require_a_retired_transport() {
        for status in ["acknowledged", "rejected", "not_sent"] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&db);
            db.execute("INSERT INTO runtime_agents VALUES('agent',?1)",[json!({"id":"agent","epoch":1,"startAttempt":{"actionRequestId":"native-0","modelSettings":{"operation":"attempt:settings","scope":{"account":"account","connection":"1","generation":1},"method":"thread/settings/update","params":{"threadId":"thread"},"status":status}}}).to_string()]).expect("positive settled settings");
            batch(&mut db, "runtime_agents")
                .expect("completed owner needs no transport obligation");
            let raw: String = db
                .query_row("SELECT record FROM runtime_agents", [], |r| r.get(0))
                .expect("owner");
            let owner: Value = serde_json::from_str(&raw).expect("JSON");
            assert_eq!(owner["startAttempt"]["modelSettings"]["status"], status);
            assert!(
                owner["startAttempt"]["modelSettings"]
                    .get("providerRequest")
                    .is_none()
            );
            assert_eq!(
                uuid::Uuid::parse_str(
                    owner["startAttempt"]["modelSettings"]["rootRequestId"]
                        .as_str()
                        .expect("root")
                )
                .expect("UUID")
                .get_version_num(),
                7
            );
        }
    }
    #[test]
    fn receipts_actions_completed_current_release_keeps_typed_authorization() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        let id = "manual-management:lead:1:request";
        db.execute("INSERT INTO runtime_agents VALUES('worker',?1)",[json!({"id":"worker","nativeRelease":{"id":"manual-release:digest","phase":"released","accountKey":"account","authorization":{"lead":{"id":"lead","epoch":1},"target":{"id":"worker","epoch":1},"request_id":id,"signature":"signature"}}}).to_string()]).expect("closed release");
        batch(&mut db, "runtime_agents").expect("migrate authorization");
        let raw: String = db
            .query_row("SELECT record FROM runtime_agents", [], |r| r.get(0))
            .expect("owner");
        let owner: Value = serde_json::from_str(&raw).expect("JSON");
        let auth = &owner["nativeRelease"]["authorization"];
        assert_eq!(auth["admission"]["id"], id);
        assert_eq!(
            uuid::Uuid::parse_str(auth["root_request_id"].as_str().expect("root"))
                .expect("UUID")
                .get_version_num(),
            7
        );
    }
    #[test]
    fn receipts_actions_safety_and_credit_preserve_existing_effect_and_credit_fence() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        db.execute_batch("CREATE TABLE runtime_safety_retries(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_limit_reset_attempts(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_process_actions(id TEXT PRIMARY KEY,signature TEXT,record TEXT)").expect("owners");
        call_fixture(&db, "retry:interrupt", "turn/interrupt");
        db.execute("INSERT INTO runtime_safety_retries VALUES('retry',?1)",[json!({"id":"retry","stage":"interrupt","dispatch":"unknown","agent":"agent","epoch":1,"scope":{"account":"account","connection":"1","generation":1},"rpcMethod":"turn/interrupt","operationId":"retry:interrupt","requestParams":{"threadId":"thread"}}).to_string()]).expect("safety");
        let call:Call=serde_json::from_value(json!({"root_request_id":"","admitted_request":null,"scope":{"account":"account","connection":"1","generation":1},"operation":"limit-reset:charge-key:1","actor":null,"transfer":null,"deleted_actor_session":null,"method":studio_runtime_providers::Method::AccountRateLimitResetCreditConsume,"params":{"creditId":"credit","idempotencyKey":"charge-key"}})).expect("call");
        let key = studio_runtime_schema::provider_call_contract::transport_key(
            "account",
            &call.operation,
        );
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
            params![
                format!("account-worker:{key}:submitted"),
                json!({"call":call}).to_string()
            ],
        )
        .expect("admitted reset");
        db.execute("INSERT INTO runtime_limit_reset_attempts VALUES('credit-attempt',?1)",[json!({"id":"credit-attempt","account_key":"account","credit_id":"credit","idempotency_key":"charge-key","transport_attempts":1,"status":"uncertain","certainty":"unknown","outcome":null}).to_string()]).expect("credit owner");
        db.execute(
            "INSERT INTO runtime_process_actions VALUES('opaque-action','signature',?1)",
            [json!({"requestId":"opaque-action","phase":"uncertain"}).to_string()],
        )
        .expect("process action");
        for source in [
            "runtime_safety_retries",
            "runtime_limit_reset_attempts",
            "runtime_process_actions",
        ] {
            batch(&mut db, source).expect("hydrate");
        }
        let raw: String = db
            .query_row("SELECT record FROM runtime_limit_reset_attempts", [], |r| {
                r.get(0)
            })
            .expect("owner");
        let credit: Value = serde_json::from_str(&raw).expect("JSON");
        assert_eq!(credit["idempotency_key"], "charge-key");
        assert_eq!(credit["transport_attempts"], 1);
        assert_eq!(
            credit["provider_request"]["id"],
            format!("account-worker:{key}:result")
        );
        assert_eq!(credit["admission"]["id"], "credit-attempt");
        let raw: String = db
            .query_row("SELECT record FROM runtime_safety_retries", [], |r| {
                r.get(0)
            })
            .expect("retry");
        let retry: Value = serde_json::from_str(&raw).expect("JSON");
        assert_eq!(retry["dispatch"], "unknown");
        assert!(retry["admittedRequest"].is_object());
    }
    #[test]
    fn receipts_actions_process_owner_admission_preserves_opaque_identity_root_and_phase() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("process.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        db.execute_batch("CREATE TABLE runtime_process_actions(id TEXT PRIMARY KEY,signature TEXT NOT NULL,record TEXT NOT NULL)").expect("process owner");
        let root = RequestIdentity::issue_root();
        for phase in ["dispatching", "uncertain", "completed"] {
            let id = format!("opaque-action:{phase}");
            let record = json!({"requestId":id,"rootRequestId":root,"signature":"original-signature","phase":phase,"scopeAgent":"owner","scopeAccountKey":"account","scopeEpoch":3,"action":"stop_owned","responseTarget":null,"processes":[{"key":"native-process-key","generation":"generation-7"}],"created":1.0,"result":{"requestId":id,"status":"uncertain","processes":[]}});
            db.execute("INSERT INTO runtime_process_actions(id,signature,record) VALUES(?1,'original-signature',?2)",params![id,record.to_string()]).expect("old process owner");
        }
        batch(&mut db, "runtime_process_actions").expect("hydrate parent capabilities");
        let snapshot = |db: &Connection| -> Vec<(String, String, String)> {
            db.prepare("SELECT id,signature,record FROM runtime_process_actions ORDER BY id")
                .expect("query")
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .expect("rows")
                .collect::<rusqlite::Result<_>>()
                .expect("records")
        };
        let before = snapshot(&db);
        for (id, signature, raw) in &before {
            let record: Value = serde_json::from_str(raw).expect("owner");
            let admitted: AdmittedRequest =
                serde_json::from_value(record["admittedRequest"].clone())
                    .expect("strict admitted owner token");
            assert_eq!(admitted.id(), id);
            assert_eq!(admitted.signature(), signature);
            assert_eq!(admitted.created(), 1000);
            assert_eq!(record["requestId"], *id);
            assert_eq!(record["signature"], *signature);
            assert_eq!(record["rootRequestId"], root);
            assert_eq!(
                record["phase"],
                id.strip_prefix("opaque-action:").expect("phase")
            );
            assert_eq!(
                record["processes"],
                json!([{"key":"native-process-key","generation":"generation-7"}])
            );
            let after_ttl = 1000 + studio_runtime_schema::persistence::REQUEST_RESULT_TTL_MS + 1;
            let final_token = admitted.derive_result(
                "provider-call",
                &[
                    "account",
                    "owner",
                    "3",
                    &format!("process-action:{id}:stop"),
                ],
                "provider-fingerprint",
            );
            assert_eq!(final_token.created(), admitted.created());
            assert!(
                final_token.created()
                    < after_ttl - studio_runtime_schema::persistence::REQUEST_RESULT_TTL_MS
            );
        }
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        batch(&mut db, "runtime_process_actions").expect("no token or root refresh");
        assert_eq!(snapshot(&db), before);
    }
    #[test]
    fn receipts_actions_existing_process_admission_is_strict_and_never_refreshed() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        db.execute_batch("CREATE TABLE runtime_process_actions(id TEXT PRIMARY KEY,signature TEXT NOT NULL,record TEXT NOT NULL)").expect("owner");
        let token = admitted_owner_token("original-parent", "original-signature", 17)
            .derive_result("process-action", &["opaque-action"], "original-signature");
        let root = RequestIdentity::issue_root();
        let original = json!({"requestId":"opaque-action","signature":"original-signature","phase":"uncertain","rootRequestId":root,"admittedRequest":token});
        db.execute("INSERT INTO runtime_process_actions(id,signature,record) VALUES('opaque-action','original-signature',?1)",[original.to_string()]).expect("already admitted");
        batch(&mut db, "runtime_process_actions").expect("preserve capability");
        let raw: String = db
            .query_row("SELECT record FROM runtime_process_actions", [], |r| {
                r.get(0)
            })
            .expect("owner");
        assert_eq!(serde_json::from_str::<Value>(&raw).expect("JSON"), original);
        let mut malformed = original.clone();
        malformed["admittedRequest"]["unexpected"] = json!(true);
        db.execute(
            "UPDATE runtime_process_actions SET record=?1",
            [malformed.to_string()],
        )
        .expect("malformed imported token");
        db.execute(
            "DELETE FROM runtime_payload_migrations WHERE name!='receipts:cutover'",
            [],
        )
        .expect("new import fixture");
        assert!(
            matches!(batch(&mut db,"runtime_process_actions"),Err(ReceiptMigrationError::Unclassifiable{table,key,signature}) if table=="runtime_process_actions"&&key=="opaque-action"&&signature=="original-signature")
        );
        let after: String = db
            .query_row("SELECT record FROM runtime_process_actions", [], |r| {
                r.get(0)
            })
            .expect("unchanged source");
        assert_eq!(
            serde_json::from_str::<Value>(&after).expect("JSON"),
            malformed
        );
    }
}
