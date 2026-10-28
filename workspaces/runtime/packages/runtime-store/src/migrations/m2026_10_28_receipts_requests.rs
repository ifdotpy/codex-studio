//! Classify public request replay independently of provider and domain migrations.
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    DeliveryStatus, MigrationStatus, OutboxKind, OutboxStatus, OwnerKind, RecipientDelivery,
    WorkEffectStatus,
};

use crate::outbox::{Outbox, OutboxRow};
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, ReceiptProjection, Result,
    SourceReceipt, admitted_owner_token, delete_source, import_result, read_receipt_batch,
    read_source_batch, unclassifiable, validate_completed_discard,
};
use crate::request_results::{AdmittedRequest, ResultEnvelope};

pub(super) const ID: &str = "2026-10-28-receipts-requests";
pub(super) const SOURCES: &[&str] = &[
    "runtime_operation_receipts",
    "messages",
    "runtime_chat_messages",
    "runtime_workspace_operations",
    "runtime_event_meta",
];
const SOURCE: &str = "runtime_operation_receipts";
/// Persistent count of classified completed settings markers without a replay body.
pub(super) const DELETED_SETTINGS_REPORT: &str = "deleted-completed-c:settings";
pub(super) const DELETED_USER_MESSAGE_REPORT: &str = "deleted-completed-c:user-message";
/// Closed W2e audit (89dabbd591): these synchronous owners saved only the reply,
/// never the exact original body. Their old spaced hashes cannot be normalized.
const CHANGED_SYNCHRONOUS_SIGNATURES: &[(&str, &str)] = &[
    ("peer-team:", "deleted-completed-c:peer-team"),
    ("sidebar-order:", "deleted-completed-c:sidebar-order"),
];
pub(super) struct Family;

#[derive(Deserialize, Serialize)]
struct WorkspaceKindView {
    kind: WorkspaceKind,
    #[serde(flatten)]
    fields: serde_json::Map<String, Value>,
}

#[derive(Deserialize, Serialize)]
#[cfg_attr(test, derive(strum::VariantNames))]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, strum(serialize_all = "snake_case"))]
enum WorkspaceKind {
    Capture,
    Checkpoint,
    Restore,
    Branch,
    Provision,
    ClaudeSettings,
    ClaudeRollback,
}

#[derive(Serialize)]
struct PendingRequest {
    request: AdmittedRequest,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RulesEffect {
    CancelMonitors { monitor_ids: Vec<String> },
}

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        ID
    }
    fn prepare(&self, tx: &Transaction<'_>, cutover_ms: i64) -> Result<()> {
        for (_, report) in CHANGED_SYNCHRONOUS_SIGNATURES {
            let mut count = MigrationCheckpoint::begin(tx, ID, report, cutover_ms)?;
            count.status = MigrationStatus::Complete;
            count.save(tx, ID, report, cutover_ms)?;
        }
        Ok(())
    }

    fn move_batch(
        &self,
        tx: &Transaction<'_>,
        source: &str,
        checkpoint: &MigrationCheckpoint,
    ) -> Result<MigrationCheckpoint> {
        if !SOURCES.contains(&source) {
            return Err(ReceiptMigrationError::Incomplete {
                migration: ID.into(),
                source_table: source.into(),
            });
        }
        if checkpoint.status == MigrationStatus::Complete {
            return Ok(checkpoint.clone());
        }
        if source == "runtime_workspace_operations" {
            return move_workspace_operations(tx, checkpoint);
        }
        if source == "runtime_event_meta" {
            return move_remote_event_identities(tx, checkpoint);
        }
        if source != SOURCE {
            return move_messages(tx, source, checkpoint);
        }
        let rows = read_receipt_batch(tx, source, checkpoint, "1")?;
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        for row in rows {
            next.cursor = row.rowid;
            // A classified phase can consume its sibling in this same batch.
            // The cursor still advances over the fetched page, including that sibling.
            let live: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts WHERE rowid=?1)",
                [row.rowid],
                |r| r.get(0),
            )?;
            if !live {
                continue;
            }
            if domain_owned(&row) {
                continue;
            }
            let known_prefix = known_request_prefix(&row.id);
            let hash_signature =
                row.signature.len() == 64 && row.signature.bytes().all(|b| b.is_ascii_hexdigit());
            let json_signature = row.signature.starts_with('{') || row.signature.starts_with('[');
            if !known_prefix && !hash_signature && !json_signature {
                continue;
            }
            let mut reply: Value = row.json(source)?;
            if !reply.is_object() {
                return Err(unclassifiable(source, &row.id, &row.signature));
            }
            if hash_signature && reply == json!({"applied":true}) {
                discard_completed_reply(tx, &row, DELETED_SETTINGS_REPORT, checkpoint.cutover_ms)?;
                continue;
            }
            let mut actor = row
                .id
                .strip_prefix("queue:")
                .and_then(|s| s.split_once(':'))
                .map(|(actor, _)| actor.to_owned());
            if row.id.starts_with("mcp-reconnect:") {
                migrate_mcp(tx, &row, &reply, checkpoint.cutover_ms)?;
                delete_source(tx, source, row.rowid)?;
                continue;
            }
            if row.id.starts_with("project-registration:") {
                migrate_registration(tx, &row, &reply, checkpoint.cutover_ms)?;
                delete_source(tx, source, row.rowid)?;
                continue;
            }
            if reply["requestId"] == row.id && reply["outcome"] == "unknown" {
                migrate_location(tx, &row, &reply, checkpoint.cutover_ms)?;
                delete_source(tx, source, row.rowid)?;
                continue;
            }
            if let Some((prefix, report)) = CHANGED_SYNCHRONOUS_SIGNATURES
                .iter()
                .find(|(prefix, _)| row.id.starts_with(prefix))
            {
                if !exact_reply(&row, &reply) {
                    return Err(unclassifiable(source, &row.id, &row.signature));
                }
                let root = row
                    .id
                    .strip_prefix(prefix)
                    .expect("matched exact family prefix");
                validate_completed_discard(source, root, checkpoint.cutover_ms).map_err(
                    |error| match error {
                        ReceiptMigrationError::FutureDatedCompletedRequest {
                            table,
                            issued_ms,
                            cutover_ms,
                            ..
                        } => ReceiptMigrationError::FutureDatedCompletedRequest {
                            table,
                            key: row.id.clone(),
                            issued_ms,
                            cutover_ms,
                        },
                        other @ ReceiptMigrationError::Sql(_)
                        | other @ ReceiptMigrationError::Unclassifiable { .. }
                        | other @ ReceiptMigrationError::Incomplete { .. } => other,
                    },
                )?;
                discard_completed_reply(tx, &row, report, checkpoint.cutover_ms)?;
                continue;
            }
            if row.id.starts_with("managed-rules:") {
                actor = Some(migrate_rules(tx, &row, &mut reply, checkpoint.cutover_ms)?);
            } else if !exact_reply(&row, &reply) {
                if known_prefix {
                    return Err(unclassifiable(source, &row.id, &row.signature));
                }
                // Another independently registered owner must classify this row.
                continue;
            }
            if direct_user_reply(&row, &reply) && !normalize_requested_delivery(tx, &row)? {
                discard_completed_reply(
                    tx,
                    &row,
                    DELETED_USER_MESSAGE_REPORT,
                    checkpoint.cutover_ms,
                )?;
                continue;
            }
            if let Some(deliveries) = reply.get_mut("deliveries") {
                *deliveries = delivery_map(source, &row, deliveries)?;
            }
            import_result(
                tx,
                &row.id,
                &row.signature,
                &ResultEnvelope {
                    actor,
                    reply,
                    references: Vec::new(),
                },
                checkpoint.cutover_ms,
            )?;
            delete_source(tx, source, row.rowid)?;
        }
        Ok(next)
    }

    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        for source in SOURCES {
            if !MigrationCheckpoint::load(tx, ID, source)?
                .is_some_and(|c| c.status == MigrationStatus::Complete)
            {
                return Err(ReceiptMigrationError::Incomplete {
                    migration: ID.into(),
                    source_table: (*source).into(),
                });
            }
        }
        Ok(())
    }
}

fn move_remote_event_identities(
    tx: &Transaction<'_>,
    checkpoint: &MigrationCheckpoint,
) -> Result<MigrationCheckpoint> {
    const TABLE: &str = "runtime_event_meta";
    let rows = read_source_batch(
        tx,
        TABLE,
        checkpoint,
        &ReceiptProjection {
            id: "source.id",
            signature: "''",
            result: "source.record",
        },
        "1",
    )?;
    let has_outbox = crate::receipt_migration_contract::source_exists(tx, "runtime_server_outbox")?;
    let mut next = checkpoint.clone();
    next.status = if rows.is_empty() {
        MigrationStatus::Complete
    } else {
        MigrationStatus::Running
    };
    for row in rows {
        let invalid = || unclassifiable(TABLE, &row.id, &row.signature);
        let mut metadata: Value = row.json(TABLE)?;
        if !metadata.is_object() {
            return Err(invalid());
        }
        if let Some(root) = metadata.get("remoteRequestId") {
            if !root.is_null()
                && root
                    .as_str()
                    .is_none_or(|root| uuid::Uuid::parse_str(root).is_err())
            {
                return Err(invalid());
            }
            next.cursor = row.rowid;
            continue;
        }
        let mut proven = Vec::new();
        for (operation, action) in [
            ("input", "input"),
            ("anchor-input", "input"),
            ("event", "event"),
        ] {
            let wire = uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                format!("studio-cross-server:{operation}:{}", row.id).as_bytes(),
            )
            .to_string();
            let source: Option<(Option<String>, Option<String>)> = if has_outbox {
                tx.query_row("SELECT CASE WHEN json_valid(body) THEN json_extract(body,'$.requestId') END,CASE WHEN json_valid(body) THEN json_extract(body,'$.action') END FROM runtime_server_outbox WHERE id=?1",[&wire],|r|Ok((r.get(0)?,r.get(1)?))).optional()?
            } else {
                None
            };
            let source_matches = source.as_ref().is_some_and(|(root, kind)| {
                root.as_deref() == Some(wire.as_str()) && kind.as_deref() == Some(action)
            });
            let cached:Option<(Option<String>,Option<String>)>=tx.query_row("SELECT CASE WHEN json_valid(json_extract(result,'$.reply.body')) THEN json_extract(json_extract(result,'$.reply.body'),'$.requestId') END,CASE WHEN json_valid(json_extract(result,'$.reply.body')) THEN json_extract(json_extract(result,'$.reply.body'),'$.action') END FROM request_results WHERE id=?1 AND json_type(result,'$.reply.server')='text'",[&wire],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let cached_matches = cached.as_ref().is_some_and(|(root, kind)| {
                root.as_deref() == Some(wire.as_str()) && kind.as_deref() == Some(action)
            });
            if source_matches || cached_matches {
                proven.push(wire);
            }
        }
        metadata["remoteRequestId"] = if proven.len() == 1 {
            json!(proven[0])
        } else {
            Value::Null
        };
        tx.execute(
            "UPDATE runtime_event_meta SET record=?1 WHERE rowid=?2",
            params![metadata.to_string(), row.rowid],
        )?;
        next.cursor = row.rowid;
    }
    Ok(next)
}

fn discard_completed_reply(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    report: &str,
    cutover: i64,
) -> Result<()> {
    validate_completed_discard(SOURCE, &row.id, cutover)?;
    let mut count = MigrationCheckpoint::begin(tx, ID, report, cutover)?;
    count.cursor += 1;
    count.status = MigrationStatus::Complete;
    count.save(tx, ID, report, cutover)?;
    delete_source(tx, SOURCE, row.rowid)
}

fn direct_user_reply(row: &SourceReceipt, reply: &Value) -> bool {
    let signature: Value = match serde_json::from_str(&row.signature) {
        Ok(signature) => signature,
        Err(_) => return false,
    };
    reply["id"] == row.id
        && reply.get("room").is_none()
        && signature
            .as_object()
            .is_some_and(|fields| fields.len() == 3)
        && signature["agent"].is_string()
        && signature["text"].is_string()
        && signature["assets"].is_array()
}

fn normalize_requested_delivery(tx: &Transaction<'_>, row: &SourceReceipt) -> Result<bool> {
    const META: &str = "runtime_event_meta";
    let invalid = || unclassifiable(META, &row.id, &row.signature);
    if !crate::receipt_migration_contract::source_exists(tx, META)? {
        return Ok(false);
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_event_meta WHERE id=?1",
            [&row.id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(false);
    };
    let mut meta: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
    if !meta.is_object()
        || meta
            .get("sendNow")
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
    {
        return Err(invalid());
    }
    let requested = meta
        .get("requestedDelivery")
        .filter(|value| !value.is_null())
        .or_else(|| {
            if meta["sendNow"] == true {
                None
            } else {
                meta.get("delivery").filter(|value| !value.is_null())
            }
        })
        .cloned();
    let Some(requested) = requested else {
        return Ok(false);
    };
    let moment = studio_runtime_schema::DeliveryMoment::parse_legacy(
        requested.as_str().ok_or_else(invalid)?,
    )
    .map_err(|_| invalid())?;
    meta["requestedDelivery"] = json!(moment);
    if let Some(delivery) = meta.get("delivery").filter(|value| !value.is_null()) {
        meta["delivery"] = json!(
            studio_runtime_schema::DeliveryMoment::parse_legacy(
                delivery.as_str().ok_or_else(invalid)?
            )
            .map_err(|_| invalid())?
        );
    }
    tx.execute(
        "UPDATE runtime_event_meta SET record=?1 WHERE id=?2",
        params![meta.to_string(), row.id],
    )?;
    Ok(true)
}

fn move_workspace_operations(
    tx: &Transaction<'_>,
    checkpoint: &MigrationCheckpoint,
) -> Result<MigrationCheckpoint> {
    use crate::receipt_migration_contract::BatchBudget;
    use studio_runtime_schema::provider_call_contract::{
        ProviderCallIntent, TransportRequest, transport_key,
    };
    const TABLE: &str = "runtime_workspace_operations";
    let rows = read_source_batch(
        tx,
        TABLE,
        checkpoint,
        &ReceiptProjection {
            id: "source.id",
            signature: "CASE WHEN json_valid(source.record) THEN coalesce(json_extract(source.record,'$.signature'),'') ELSE '' END",
            result: "source.record",
        },
        "1",
    )?;
    let mut next = checkpoint.clone();
    next.status = if rows.is_empty() {
        MigrationStatus::Complete
    } else {
        MigrationStatus::Running
    };
    let mut budget = BatchBudget::default();
    for row in rows {
        let invalid = || unclassifiable(TABLE, &row.id, &row.signature);
        let mut view: WorkspaceKindView =
            serde_json::from_str(&row.result).map_err(|_| invalid())?;
        let mut owner = serde_json::to_value(&view).map_err(|_| invalid())?;
        if !schema("PersistedWorkspaceOperationRecord", &owner) || owner["id"] != row.id {
            return Err(invalid());
        }
        if matches!(
            view.kind,
            WorkspaceKind::ClaudeSettings | WorkspaceKind::ClaudeRollback
        ) && view.fields.contains_key("controlAttempt")
        {
            if !budget.include(row.result.len() + row.id.len() + row.signature.len()) {
                break;
            }
            view.fields.remove("controlAttempt");
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?1 WHERE rowid=?2",
                params![
                    serde_json::to_string(&view).map_err(|_| invalid())?,
                    row.rowid
                ],
            )?;
        }
        if !matches!(view.kind, WorkspaceKind::Branch | WorkspaceKind::Restore)
            || owner["phase"] == "completed"
        {
            next.cursor = row.rowid;
            continue;
        }
        let previous_admission = owner
            .get("providerAdmission")
            .map(|admission| {
                serde_json::from_value::<AdmittedRequest>(admission.clone()).map_err(|_| invalid())
            })
            .transpose()?;
        let operation = format!(
            "{}:{}",
            row.id,
            if matches!(view.kind, WorkspaceKind::Branch) {
                "fork"
            } else {
                "provider"
            }
        );
        let account = owner
            .pointer("/source/accountKey")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let transport_id = transport_key(account, &operation);
        let submitted_id = format!("account-worker:{transport_id}:submitted");
        let bytes: Option<i64> = tx
            .query_row(
                "SELECT length(CAST(result AS BLOB)) FROM runtime_operation_receipts WHERE id=?1",
                [&submitted_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(bytes) = bytes else {
            if owner["rootRequestId"].as_str().is_some_and(|root| {
                studio_runtime_schema::persistence::RequestIdentity::new(
                    "workspace-provider",
                    &[],
                    root,
                    &[],
                )
                .is_ok()
            }) {
                next.cursor = row.rowid;
                continue;
            }
            return Err(invalid());
        };
        let size = row
            .result
            .len()
            .saturating_add(row.id.len())
            .saturating_add(row.signature.len())
            .saturating_add(usize::try_from(bytes).map_err(|_| invalid())?);
        if !budget.include(size) {
            break;
        }
        let raw: String = tx.query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [&submitted_id],
            |r| r.get(0),
        )?;
        let mut submitted: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        let mut call = submitted["call"].clone();
        let fields = call.as_object_mut().ok_or_else(invalid)?;
        fields.entry("root_request_id").or_insert(json!(""));
        fields.entry("admitted_request").or_insert(Value::Null);
        let call: ProviderCallIntent<String, AdmittedRequest> =
            serde_json::from_value(call).map_err(|_| invalid())?;
        let request: TransportRequest<String> =
            serde_json::from_value(submitted["request"].clone()).map_err(|_| invalid())?;
        if call.scope.account != account
            || call.operation != operation
            || call
                .actor
                .as_ref()
                .is_none_or(|actor| owner["agent"] != actor.agent)
            || !matches!(call.method.as_str(), "thread/start" | "thread/fork")
        {
            return Err(invalid());
        }
        if request.generation != call.scope.generation
            || request.method != call.method
            || request.operation != transport_id
        {
            return Err(invalid());
        }
        let signature = call.fingerprint(account, &call.method);
        let result_id = submitted_id
            .strip_suffix(":submitted")
            .map(|key| format!("{key}:result"))
            .ok_or_else(invalid)?;
        let admission = admitted_owner_token(&result_id, &signature, checkpoint.cutover_ms);
        if previous_admission.as_ref().is_some_and(|previous| {
            previous.id() != admission.id()
                || previous.signature() != admission.signature()
                || previous.created() != admission.created()
        }) {
            return Err(invalid());
        }
        let owner_root = owner
            .get("rootRequestId")
            .filter(|root| !root.is_null())
            .map(|root| root.as_str().ok_or_else(invalid))
            .transpose()?
            .filter(|root| !root.is_empty());
        let source_root =
            (!call.root_request_id.is_empty()).then_some(call.root_request_id.as_str());
        for root in [owner_root, source_root].into_iter().flatten() {
            if studio_runtime_schema::persistence::RequestIdentity::new(
                "provider-call",
                &[],
                root,
                &[],
            )
            .is_err()
            {
                return Err(invalid());
            }
        }
        if owner_root
            .zip(source_root)
            .is_some_and(|(owner, source)| owner != source)
        {
            return Err(invalid());
        }
        let root = match source_root.or(owner_root) {
            Some(root) => root.to_owned(),
            None => crate::receipt_migration_contract::hydrate_source_root(
                tx,
                SOURCE,
                &submitted_id,
                "submitted",
                "result",
                "$.call.root_request_id",
            )?,
        };
        submitted["call"]["root_request_id"] = json!(root);
        submitted["call"]["admitted_request"] = json!(admission);
        owner["providerAdmission"] = json!(admission);
        owner["rootRequestId"] = json!(root);
        tx.execute(
            "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
            params![submitted.to_string(), submitted_id],
        )?;
        tx.execute(
            "UPDATE runtime_workspace_operations SET record=?1 WHERE rowid=?2",
            params![owner.to_string(), row.rowid],
        )?;
        next.cursor = row.rowid;
    }
    Ok(next)
}

fn move_messages(
    tx: &Transaction<'_>,
    source: &str,
    checkpoint: &MigrationCheckpoint,
) -> Result<MigrationCheckpoint> {
    let rows = read_source_batch(
        tx,
        source,
        checkpoint,
        &ReceiptProjection {
            id: "source.id",
            signature: "''",
            result: "source.deliveries",
        },
        "1",
    )?;
    let mut next = checkpoint.clone();
    next.status = if rows.is_empty() {
        MigrationStatus::Complete
    } else {
        MigrationStatus::Running
    };
    for row in rows {
        let deliveries: Value = row.json(source)?;
        let deliveries = delivery_map(source, &row, &deliveries)?;
        tx.execute(
            &format!("UPDATE {source} SET deliveries=?1 WHERE rowid=?2"),
            params![deliveries.to_string(), row.rowid],
        )?;
        next.cursor = row.rowid;
    }
    Ok(next)
}

fn delivery_map(source: &str, row: &SourceReceipt, deliveries: &Value) -> Result<Value> {
    let invalid = || unclassifiable(source, &row.id, &row.signature);
    let old: std::collections::BTreeMap<String, String> =
        serde_json::from_value(deliveries.clone()).map_err(|_| invalid())?;
    let typed = old
        .into_iter()
        .map(|(recipient, value)| {
            let delivery = if let Some(error) = value.strip_prefix("failed:") {
                RecipientDelivery {
                    status: DeliveryStatus::Failed,
                    error: Some(error.into()),
                }
            } else if let Some(error) = value.strip_prefix("cancelled:") {
                RecipientDelivery {
                    status: DeliveryStatus::Cancelled,
                    error: Some(error.into()),
                }
            } else {
                RecipientDelivery {
                    status: serde_json::from_value(json!(value)).map_err(|_| invalid())?,
                    error: None,
                }
            };
            Ok((recipient, delivery))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    serde_json::to_value(typed).map_err(|_| invalid())
}

fn domain_owned(row: &SourceReceipt) -> bool {
    [
        "aw:",
        "account-worker:",
        "provider-callback:",
        "provider-notification:",
        "provider-submission:",
        "work-result-failures:",
        "work-archive:",
        "work-write-vm-intent:",
        "project-destructive-removal:",
        "idle-compaction:",
        "title-generation:",
        "liveness:",
        "native-command:",
        "monitor-http-input:",
        "manual-management:",
        "park-event:",
        "studio-errors:",
        "linux-vm-provider-start:",
        "linux-vm-credential-copy:",
        "voice-stop:",
        "voice-delete:",
        "context-terminal:",
    ]
    .iter()
    .any(|prefix| row.id.starts_with(prefix))
        || [
            "provider-callback",
            "accepted-archive-v1",
            "idle-compaction",
            "title-generation-effect-v1",
            "liveness",
            "work-result-failures",
        ]
        .contains(&row.signature.as_str())
}

fn known_request_prefix(id: &str) -> bool {
    [
        "plan-request:",
        "plan:",
        "queue:",
        "user:",
        "managed-rules:",
        "park-request:",
        "peer-team:",
        "sidebar-order:",
        "project-delete:",
        "project-registration:",
        "mcp-reconnect:",
    ]
    .iter()
    .any(|p| id.starts_with(p))
}

fn schema(name: &str, reply: &Value) -> bool {
    studio_runtime_schema::validate(name, reply).unwrap_or(false)
}

fn exact_reply(row: &SourceReceipt, reply: &Value) -> bool {
    if row.id.starts_with("plan-request:") || row.id.starts_with("plan:") {
        return schema("PersistedPlanRecord", reply);
    }
    if row.id.starts_with("queue:") {
        return reply["status"] == "updated"
            && reply["revision"].is_string()
            && reply["capabilities"].is_object();
    }
    if row.id.starts_with("user:") {
        return schema("PersistedComplaintRecord", reply);
    }
    if row.id.starts_with("park-request:") {
        return reply["event"].is_string() && reply["count"].is_u64() && reply["woken"].is_array();
    }
    if row.id.starts_with("peer-team:") {
        return reply["id"].is_string() && reply["name"].is_string();
    }
    if row.id.starts_with("sidebar-order:") {
        return reply["revision"].is_number() && reply["groups"].is_array();
    }
    if row.id.starts_with("project-delete:") {
        return reply["id"].is_string()
            && reply["outcome"] == "deleted"
            && reply["deletedAgentIds"].is_array()
            && reply["deletedRoomIds"].is_array();
    }
    // Unprefixed public IDs need an identifiable response contract, never a generic object fallback.
    (reply["id"].is_string() && reply["name"].is_string())
        || (reply["id"].is_string()
            && reply["concurrency"].is_number()
            && reply["agentMode"].is_string()
            && reply["agentModeRevision"].is_number()
            && reply["maxAgents"].is_number())
        || (reply["id"].is_string() && reply.get("defaultGate").is_some())
        || (reply["path"].is_string()
            && reply["line"].is_number()
            && reply["text"].is_string()
            && schema("PersistedAnnotationRecord", reply))
        || (reply["title"].is_string() && reply["rootId"].is_string() && schema("WorkItem", reply))
        || (reply["id"] == row.id
            && serde_json::from_value::<DeliveryStatus>(reply["status"].clone()).is_ok()
            && reply
                .get("error")
                .is_none_or(|e| e.is_null() || e.is_string()))
        || (reply["requestId"] == row.id
            && matches!(reply["outcome"].as_str(), Some("applied" | "not_applied")))
        || (reply["requestId"] == row.id
            && reply["server"].is_string()
            && reply["branch"].is_string()
            && reply["commit"].is_string()
            && reply["bytes"].is_u64()
            && reply["destination"].is_string())
        || (reply["id"].is_string() && reply["room"].is_string() && reply["deliveries"].is_object())
        || (reply["id"].is_string() && reply["room"].is_string() && reply["recipient"] == "user")
}

fn migrate_rules(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    reply: &mut Value,
    cutover: i64,
) -> Result<String> {
    let invalid = || unclassifiable(SOURCE, &row.id, &row.signature);
    let tail = row.id.strip_prefix("managed-rules:").ok_or_else(invalid)?;
    let (actor, _request) = tail.split_once(':').ok_or_else(invalid)?;
    let monitors: Vec<String> =
        serde_json::from_value(reply["cancelMonitorIds"].clone()).map_err(|_| invalid())?;
    if reply.get("value").is_none() || monitors.windows(2).any(|p| p[0] >= p[1]) {
        return Err(invalid());
    }
    if !monitors.is_empty() {
        let id = Outbox::effect_id(
            &row.id,
            OutboxKind::ManagedTool,
            OwnerKind::Agent,
            actor,
            &["rule-monitor-cancellation"],
        );
        crate::receipt_migration_contract::import_outbox(
            tx,
            &OutboxRow {
                id,
                kind: OutboxKind::ManagedTool,
                owner_kind: OwnerKind::Agent,
                owner_id: actor.into(),
                request_id: Some(row.id.clone()),
                signature: row.signature.clone(),
                status: OutboxStatus::Pending,
                account: None,
                stream_id: None,
                generation: None,
                ordinal: None,
                next_at: Some(cutover),
                payload: RulesEffect::CancelMonitors {
                    monitor_ids: monitors,
                },
                created: cutover,
            },
        )?;
    }
    *reply = json!({"value":reply["value"]});
    Ok(actor.into())
}

fn migrate_mcp(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    reply: &Value,
    cutover: i64,
) -> Result<()> {
    let invalid = || unclassifiable(SOURCE, &row.id, &row.signature);
    let signature: Value = serde_json::from_str(&row.signature).map_err(|_| invalid())?;
    let actor = signature["id"].as_str().ok_or_else(invalid)?;
    let key = row.id.strip_suffix(":result").unwrap_or(&row.id);
    let result_key = format!("{key}:result");
    let final_reply = if row.id.ends_with(":result") {
        Some(reply.clone())
    } else {
        if reply["status"] != "submitted" {
            return Err(invalid());
        }
        let saved: Option<(String, String)> = tx
            .query_row(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
                [&result_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        saved
            .map(|(signature, raw)| {
                if signature != row.signature {
                    return Err(invalid());
                }
                serde_json::from_str::<Value>(&raw)
                    .map_err(|_| unclassifiable(SOURCE, &result_key, &signature))
            })
            .transpose()?
    };
    if let Some(reply) = &final_reply {
        if !matches!(
            reply["status"].as_str(),
            Some("completed" | "failed" | "unknown")
        ) || !reply["message"].is_string()
        {
            return Err(invalid());
        }
        import_result(
            tx,
            key,
            &row.signature,
            &ResultEnvelope {
                actor: Some(actor.into()),
                reply,
                references: Vec::new(),
            },
            cutover,
        )?;
        let claim_signature: Option<String> = tx
            .query_row(
                "SELECT signature FROM runtime_operation_receipts WHERE id=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        if claim_signature
            .as_ref()
            .is_some_and(|s| s != &row.signature)
        {
            return Err(invalid());
        }
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id IN (?1,?2) AND signature=?3",
            params![key, result_key, row.signature],
        )?;
    }
    if final_reply
        .as_ref()
        .is_none_or(|reply| reply["status"] == "unknown")
    {
        crate::receipt_migration_contract::import_outbox(
            tx,
            &OutboxRow {
                id: key.into(),
                kind: OutboxKind::McpReconnect,
                owner_kind: OwnerKind::Agent,
                owner_id: actor.into(),
                request_id: Some(key.into()),
                signature: row.signature.clone(),
                status: OutboxStatus::Unknown,
                account: None,
                stream_id: None,
                generation: None,
                ordinal: None,
                next_at: None,
                payload: PendingRequest {
                    request: admitted_owner_token(key, &row.signature, cutover),
                },
                created: cutover,
            },
        )?;
    }
    Ok(())
}

fn migrate_registration(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    reply: &Value,
    cutover: i64,
) -> Result<()> {
    let invalid = || unclassifiable(SOURCE, &row.id, &row.signature);
    let project = row
        .id
        .strip_prefix("project-registration:")
        .ok_or_else(invalid)?;
    if reply["projectId"] != project || !reply["requestId"].is_string() {
        return Err(invalid());
    }
    let status = match reply["outcome"].as_str() {
        Some("applied") => WorkEffectStatus::Applied,
        Some("not_applied") => WorkEffectStatus::Failed,
        Some("unknown") => WorkEffectStatus::Unknown,
        _ => return Err(invalid()),
    };
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_projects WHERE id=?1",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    let mut owner: Value = raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .ok_or_else(invalid)?;
    if !owner["registration"].is_object() {
        return Err(invalid());
    }
    owner["registration"]["signature"] = json!(row.signature);
    owner["registration"]["requestId"] = reply["requestId"].clone();
    owner["registration"]["status"] = json!(status);
    tx.execute(
        "UPDATE runtime_projects SET record=?1 WHERE id=?2",
        params![owner.to_string(), project],
    )?;
    let _ = cutover;
    Ok(())
}

fn migrate_location(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    reply: &Value,
    cutover: i64,
) -> Result<()> {
    let invalid = || unclassifiable(SOURCE, &row.id, &row.signature);
    let project = if let Some(project) = reply["projectId"].as_str() {
        Some(project.to_owned())
    } else {
        tx.query_row(
            "SELECT json_extract(body,'$.payload.project') FROM runtime_server_outbox WHERE id=?1",
            [&row.id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
    }
    .ok_or_else(invalid)?;
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_projects WHERE id=?1",
            [&project],
            |r| r.get(0),
        )
        .optional()?;
    let mut owner: Value = raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .ok_or_else(invalid)?;
    if !owner.is_object() {
        return Err(invalid());
    }
    if owner.get("locationIntents").is_none() {
        owner["locationIntents"] = json!({});
    }
    let intents = owner["locationIntents"]
        .as_object_mut()
        .ok_or_else(invalid)?;
    let signature = location_signature(tx, row)?.unwrap_or_else(|| row.signature.clone());
    let token = if let Some(existing) = intents.get(&row.id) {
        let existing: AdmittedRequest =
            serde_json::from_value(existing["request"].clone()).map_err(|_| invalid())?;
        if existing.signature() != signature {
            return Err(unclassifiable(
                "runtime_projects",
                &project,
                existing.signature(),
            ));
        }
        admitted_owner_token(existing.id(), &signature, existing.created())
    } else {
        admitted_owner_token(&row.id, &signature, cutover)
    };
    intents.insert(row.id.clone(), json!({"request":token}));
    tx.execute(
        "UPDATE runtime_projects SET record=?1 WHERE id=?2",
        params![owner.to_string(), project],
    )?;
    Ok(())
}

/// Only immutable captured request bytes establish a new signature spelling.
/// The signed transport body and its fingerprint remain their original authority.
fn location_signature(tx: &Transaction<'_>, row: &SourceReceipt) -> Result<Option<String>> {
    if !crate::receipt_migration_contract::source_exists(tx, "runtime_server_outbox")? {
        return Ok(None);
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT body FROM runtime_server_outbox WHERE id=?1",
            [&row.id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let invalid = || unclassifiable(SOURCE, &row.id, &row.signature);
    let body: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
    let request = &body["payload"]["request"];
    if request.is_null() {
        return Ok(None);
    }
    if body["requestId"] != row.id
        || !matches!(
            body["action"].as_str(),
            Some("add_location" | "remove_location")
        )
        || request["action"] != body["action"]
        || request["project"] != body["payload"]["project"]
        || !request["project"].is_string()
        || !request["server"].is_string()
        || !request["path"].is_null() && !request["path"].is_string()
    {
        return Err(invalid());
    }
    let signature = crate::sha256_hex(
        studio_runtime_schema::json_compat::python_json_compact(request).as_bytes(),
    );
    if row.signature != signature {
        return Err(invalid());
    }
    Ok(Some(signature))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt_migration_contract::{MIGRATION_BATCH_ROWS, prepare_schema};
    use rusqlite::Connection;
    const CUTOVER: i64 = 1_000_000;
    const SIGNATURE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn receipts_requests_claude_control_attempt_is_removed_without_changing_identity() {
        use strum::VariantNames;
        let schema: Value = serde_json::from_str(include_str!(
            "../../../runtime-schema/src/schemas/persisted-workspace-operation-record.schema.json"
        ))
        .expect("workspace schema");
        let kinds: Vec<_> = schema["properties"]["kind"]["enum"]
            .as_array()
            .expect("workspace kinds")
            .iter()
            .map(|kind| kind.as_str().expect("kind"))
            .collect();
        assert_eq!(kinds, WorkspaceKind::VARIANTS);
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch(
            "CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY,record TEXT)",
        )
        .expect("source");
        for kind in ["claude_settings", "claude_rollback"] {
            let owner = json!({"id":kind,"kind":kind,"agent":"agent","phase":"recovery_required","requestId":"original","controlAttempt":2});
            db.execute(
                "INSERT INTO runtime_workspace_operations VALUES(?1,?2)",
                params![kind, owner.to_string()],
            )
            .expect("owner");
        }
        let tx = db.transaction().expect("transaction");
        let checkpoint =
            MigrationCheckpoint::begin(&tx, ID, "runtime_workspace_operations", CUTOVER)
                .expect("checkpoint");
        Family
            .move_batch(&tx, "runtime_workspace_operations", &checkpoint)
            .expect("migration");
        for kind in ["claude_settings", "claude_rollback"] {
            let raw: String = tx
                .query_row(
                    "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                    [kind],
                    |row| row.get(0),
                )
                .expect("owner");
            let owner: Value = serde_json::from_str(&raw).expect("JSON");
            assert!(owner.get("controlAttempt").is_none());
            assert_eq!(owner["requestId"], "original");
            assert_eq!(owner["phase"], "recovery_required");
        }
    }

    fn fixture(db: &mut Connection) {
        db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT); CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,body TEXT);").expect("old format");
        let tx = db.transaction().expect("transaction");
        prepare_schema(&tx).expect("schema");
        tx.commit().expect("schema commit");
    }
    fn insert(db: &Connection, id: &str, reply: Value) {
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
            params![id, SIGNATURE, reply.to_string()],
        )
        .expect("source");
    }
    fn plan() -> Value {
        json!({"id":"agent","rootId":"root","text":"original","version":1,"updated":1.0})
    }
    fn batch(db: &mut Connection, commit: bool) -> MigrationCheckpoint {
        batch_source(db, SOURCE, commit)
    }
    fn batch_source(db: &mut Connection, source: &str, commit: bool) -> MigrationCheckpoint {
        let tx = db.transaction().expect("transaction");
        let checkpoint = MigrationCheckpoint::begin(&tx, ID, source, CUTOVER).expect("checkpoint");
        let next = Family.move_batch(&tx, source, &checkpoint).expect("batch");
        next.save(&tx, ID, source, CUTOVER)
            .expect("checkpoint save");
        if commit {
            tx.commit().expect("commit");
        } else {
            tx.rollback().expect("crash");
        }
        next
    }
    fn migrate(db: &mut Connection) {
        let tx = db.transaction().expect("transaction");
        Family.prepare(&tx, CUTOVER).expect("prepare");
        tx.commit().expect("prepare commit");
        for source in SOURCES {
            while batch_source(db, source, true).status != MigrationStatus::Complete {}
        }
        let tx = db.transaction().expect("transaction");
        Family.finish(&tx).expect("finish");
        tx.commit().expect("finish commit");
    }

    #[test]
    fn receipts_requests_old_row_second_open_and_original_reply() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(&db, "plan-request:old", plan());
        migrate(&mut db);
        let saved: (String, String, i64) = db
            .query_row(
                "SELECT signature,result,created FROM request_results WHERE id='plan-request:old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("new result");
        assert_eq!(saved.0, SIGNATURE);
        assert_eq!(saved.2, CUTOVER);
        let envelope: Value = serde_json::from_str(&saved.1).expect("envelope");
        assert_eq!(envelope["reply"], plan());
        migrate(&mut db);
        assert_eq!(
            db.query_row(
                "SELECT signature,result,created FROM request_results WHERE id='plan-request:old'",
                [],
                |r| Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?
                ))
            )
            .expect("same result"),
            saved
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source gone"),
            0
        );
    }

    #[test]
    fn receipts_requests_crash_between_batches_resumes() {
        let root = tempfile::tempdir().expect("directory");
        let path = root.path().join("canvas.sqlite3");
        {
            let mut db = Connection::open(&path).expect("database");
            fixture(&mut db);
            for n in 0..=MIGRATION_BATCH_ROWS {
                insert(&db, &format!("plan-request:{n}"), plan());
            }
            assert_eq!(batch(&mut db, true).cursor, MIGRATION_BATCH_ROWS as i64);
            batch(&mut db, false);
        }
        let mut db = Connection::open(&path).expect("reopen");
        migrate(&mut db);
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("all results"),
            (MIGRATION_BATCH_ROWS + 1) as i64
        );
    }

    #[test]
    fn receipts_requests_unclassifiable_rolls_back_and_completed_lossy_marker_is_counted() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(&db, "old-settings", json!({"applied":true}));
        insert(&db, "plan-request:bad", json!({"unexpected":true}));
        let tx = db.transaction().expect("transaction");
        let checkpoint = MigrationCheckpoint::begin(&tx, ID, SOURCE, CUTOVER).expect("checkpoint");
        assert!(
            matches!(Family.move_batch(&tx,SOURCE,&checkpoint),Err(ReceiptMigrationError::Unclassifiable{table,key,signature}) if table==SOURCE && key=="plan-request:bad" && signature==SIGNATURE)
        );
        tx.rollback().expect("rollback");
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source retained"),
            2
        );
        db.execute(
            "DELETE FROM runtime_operation_receipts WHERE id='plan-request:bad'",
            [],
        )
        .expect("remove malformed fixture");
        migrate(&mut db);
        let tx = db.transaction().expect("transaction");
        let count = MigrationCheckpoint::load(&tx, ID, DELETED_SETTINGS_REPORT)
            .expect("report")
            .expect("count");
        assert_eq!(count.cursor, 1);
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM request_results WHERE id='old-settings'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("no marker reply"),
            0
        );
        assert!(
            studio_runtime_schema::persistence::RequestIdentity::new(
                "settings",
                &["actor"],
                "old-settings",
                &[]
            )
            .is_err()
        );
        tx.commit().expect("commit");
        migrate(&mut db);
        let tx = db.transaction().expect("transaction");
        assert_eq!(
            MigrationCheckpoint::load(&tx, ID, DELETED_SETTINGS_REPORT)
                .expect("report")
                .expect("count")
                .cursor,
            1
        );
    }

    #[test]
    fn receipts_requests_rules_reply_and_cancellation_obligation_share_commit() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(
            &db,
            "managed-rules:actor:request",
            json!({"value":{"status":"paused"},"cancelMonitorIds":["monitor"]}),
        );
        migrate(&mut db);
        let raw: String = db
            .query_row("SELECT result FROM request_results", [], |r| r.get(0))
            .expect("reply");
        let reply: Value = serde_json::from_str(&raw).expect("envelope");
        assert_eq!(reply["reply"], json!({"value":{"status":"paused"}}));
        let raw: String = db
            .query_row("SELECT payload FROM runtime_outbox", [], |r| r.get(0))
            .expect("obligation");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("payload"),
            json!({"kind":"cancel_monitors","monitor_ids":["monitor"]})
        );
    }

    #[test]
    fn receipts_requests_unknown_mcp_never_authorizes_resend() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let signature =
            json!({"id":"actor","session":{"account":"account","generation":1}}).to_string();
        db.execute("INSERT INTO runtime_operation_receipts VALUES('mcp-reconnect:old',?1,'{\"status\":\"submitted\"}')",[signature]).expect("claim");
        migrate(&mut db);
        let (status, next): (OutboxStatus, Option<i64>) = db
            .query_row("SELECT status,next_at FROM runtime_outbox", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("uncertain owner");
        assert_eq!(status, OutboxStatus::Unknown);
        assert_eq!(next, None);
    }

    #[test]
    fn receipts_requests_completed_lossy_uuid_is_expired_and_future_uuid_stops() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let root = "0000000f-4240-7000-8000-000000000000";
        insert(&db, root, json!({"applied":true}));
        migrate(&mut db);
        let identity = studio_runtime_schema::persistence::RequestIdentity::new(
            "settings",
            &["actor"],
            root,
            &[],
        )
        .expect("old UUIDv7");
        let tx = db.transaction().expect("transaction");
        assert!(matches!(
            crate::request_results::RequestResults::admit(&tx, &identity, SIGNATURE, CUTOVER + 1),
            Err(crate::request_results::RequestResultError::Identity(
                studio_runtime_schema::persistence::RequestIdentityError::Expired { .. }
            ))
        ));
        tx.rollback().expect("no new owner mutation");
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let future = "0000000f-4241-7000-8000-000000000000";
        insert(&db, future, json!({"applied":true}));
        let tx = db.transaction().expect("transaction");
        let checkpoint = MigrationCheckpoint::begin(&tx, ID, SOURCE, CUTOVER).expect("checkpoint");
        assert!(
            matches!(Family.move_batch(&tx,SOURCE,&checkpoint),Err(ReceiptMigrationError::FutureDatedCompletedRequest{table,key,issued_ms,cutover_ms}) if table==SOURCE && key==future && issued_ms==CUTOVER+1 && cutover_ms==CUTOVER)
        );
        tx.rollback().expect("preserve source");
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("future source retained"),
            1
        );
    }

    #[test]
    fn receipts_requests_message_delivery_maps_preserve_errors_and_source_fields() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE messages(id TEXT PRIMARY KEY,deliveries TEXT,text TEXT);CREATE TABLE runtime_chat_messages(seq INTEGER PRIMARY KEY,id TEXT UNIQUE,deliveries TEXT,text TEXT);").expect("message owners");
        let old = json!({"a":"pending","b":"failed:full: original error","c":"cancelled:reason","d":"sent"});
        db.execute(
            "INSERT INTO messages VALUES('message',?1,'original text')",
            [old.to_string()],
        )
        .expect("message");
        db.execute(
            "INSERT INTO runtime_chat_messages VALUES(42,'chat',?1,'chat text')",
            [old.to_string()],
        )
        .expect("chat message");
        insert(&db, "input", json!({"id":"input","status":"queued"}));
        migrate(&mut db);
        for source in &SOURCES[1..3] {
            let (raw, text): (String, String) = db
                .query_row(&format!("SELECT deliveries,text FROM {source}"), [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .expect("message");
            let map: std::collections::BTreeMap<String, RecipientDelivery> =
                serde_json::from_str(&raw).expect("typed delivery map");
            assert_eq!(map["a"].status, DeliveryStatus::Pending);
            assert_eq!(map["b"].error.as_deref(), Some("full: original error"));
            assert_eq!(map["c"].status, DeliveryStatus::Cancelled);
            assert_eq!(map["d"].status, DeliveryStatus::Sent);
            assert!(text.ends_with("text"));
        }
        assert_eq!(
            db.query_row("SELECT seq FROM runtime_chat_messages", [], |r| r
                .get::<_, i64>(0))
                .expect("original sequence"),
            42
        );
        assert!(
            db.query_row("SELECT 1 FROM request_results WHERE id='input'", [], |r| {
                r.get::<_, i64>(0)
            })
            .is_ok()
        );
        migrate(&mut db);
    }

    #[test]
    fn receipts_requests_unknown_message_delivery_aborts_its_batch() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE messages(id TEXT PRIMARY KEY,deliveries TEXT);INSERT INTO messages VALUES('good','{\"recipient\":\"pending\"}');INSERT INTO messages VALUES('bad','{\"recipient\":\"unexpected\"}');").expect("message fixtures");
        let tx = db.transaction().expect("transaction");
        let checkpoint =
            MigrationCheckpoint::begin(&tx, ID, "messages", CUTOVER).expect("checkpoint");
        assert!(
            matches!(Family.move_batch(&tx,"messages",&checkpoint),Err(ReceiptMigrationError::Unclassifiable{table,key,..}) if table=="messages" && key=="bad")
        );
        tx.rollback().expect("rollback");
        let old: String = db
            .query_row("SELECT deliveries FROM messages WHERE id='good'", [], |r| {
                r.get(0)
            })
            .expect("old map");
        assert_eq!(
            serde_json::from_str::<Value>(&old).expect("map"),
            json!({"recipient":"pending"})
        );
    }

    #[test]
    fn receipts_requests_mcp_siblings_keep_uncertainty_in_either_keyset_order() {
        for reverse in [false, true] {
            for status in ["completed", "unknown"] {
                let mut db = Connection::open_in_memory().expect("database");
                fixture(&mut db);
                let signature = json!({"id":"actor"}).to_string();
                let mut rows = vec![
                    ("mcp-reconnect:old", json!({"status":"submitted"})),
                    (
                        "mcp-reconnect:old:result",
                        json!({"status":status,"message":"exact outcome"}),
                    ),
                ];
                if reverse {
                    rows.reverse();
                }
                for (id, reply) in rows {
                    db.execute(
                        "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                        params![id, signature, reply.to_string()],
                    )
                    .expect("phase");
                }
                migrate(&mut db);
                assert_eq!(
                    db.query_row(
                        "SELECT count(*) FROM request_results WHERE id='mcp-reconnect:old'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .expect("one exact reply"),
                    1
                );
                assert_eq!(
                    db.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                        .get::<_, i64>(0))
                        .expect("pending obligation"),
                    i64::from(status == "unknown")
                );
            }
        }
    }

    #[test]
    fn receipts_requests_workspace_pending_hydrates_exact_provider_token_with_one_root() {
        for original_root in [
            None,
            Some(studio_runtime_schema::persistence::RequestIdentity::issue_root()),
        ] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            db.execute_batch(
                "CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY,record TEXT)",
            )
            .expect("workspace owners");
            let owner = json!({"id":"branch:old","kind":"branch","agent":"agent","phase":"provider_pending","signature":"original","source":{"accountKey":"default","threadId":"thread"}});
            db.execute(
                "INSERT INTO runtime_workspace_operations VALUES('branch:old',?1)",
                [owner.to_string()],
            )
            .expect("old owner");
            let operation = "branch:old:fork";
            let key =
                studio_runtime_schema::provider_call_contract::transport_key("default", operation);
            let mut call = json!({"scope":{"account":"default","connection":"connection","generation":7},"operation":operation,"actor":{"agent":"agent","epoch":3},"transfer":null,"deleted_actor_session":null,"method":"thread/fork","params":{"threadId":"thread"}});
            if let Some(root) = &original_root {
                call["root_request_id"] = json!(root);
            }
            let submitted = json!({"call":call,"request":{"id":1,"generation":7,"method":"thread/fork","operation":key,"thread":"thread","lightweight":true,"deadline":60000,"unknown":false,"remoteId":null}});
            let source_id = format!("account-worker:{key}:submitted");
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
                params![source_id, submitted.to_string()],
            )
            .expect("retained source");
            {
                let tx = db.transaction().expect("transaction");
                let checkpoint =
                    MigrationCheckpoint::begin(&tx, ID, "runtime_workspace_operations", CUTOVER)
                        .expect("checkpoint");
                let next = Family
                    .move_batch(&tx, "runtime_workspace_operations", &checkpoint)
                    .expect("hydrate owner/source");
                next.save(&tx, ID, "runtime_workspace_operations", CUTOVER)
                    .expect("checkpoint same transaction");
                tx.rollback().expect("crash before commit");
                let restored: String = db
                    .query_row(
                        "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                        [&source_id],
                        |r| r.get(0),
                    )
                    .expect("original source");
                assert_eq!(
                    serde_json::from_str::<Value>(&restored).expect("source JSON"),
                    submitted
                );
                let restored: String = db
                    .query_row("SELECT record FROM runtime_workspace_operations", [], |r| {
                        r.get(0)
                    })
                    .expect("original owner");
                assert_eq!(
                    serde_json::from_str::<Value>(&restored).expect("owner JSON"),
                    owner
                );
            }
            migrate(&mut db);
            let raw: String = db
                .query_row("SELECT record FROM runtime_workspace_operations", [], |r| {
                    r.get(0)
                })
                .expect("owner");
            let owner: Value = serde_json::from_str(&raw).expect("owner JSON");
            let token: AdmittedRequest =
                serde_json::from_value(owner["providerAdmission"].clone()).expect("trusted token");
            assert_eq!(token.id(), format!("account-worker:{key}:result"));
            assert_eq!(token.created(), CUTOVER);
            assert_ne!(token.signature(), "submitted");
            assert_ne!(token.signature(), "result");
            let root = owner["rootRequestId"].as_str().expect("durable owner root");
            if let Some(original) = &original_root {
                assert_eq!(root, original);
            }
            assert!(
                studio_runtime_schema::persistence::RequestIdentity::new(
                    "provider-call",
                    &[],
                    root,
                    &[]
                )
                .is_ok()
            );
            let raw: String = db
                .query_row(
                    "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                    [&source_id],
                    |r| r.get(0),
                )
                .expect("retained call");
            let source: Value = serde_json::from_str(&raw).expect("submitted JSON");
            assert_eq!(source["call"]["root_request_id"], root);
            assert!(
                db.query_row(
                    "SELECT 1 FROM runtime_operation_receipts WHERE id=?1",
                    [&source_id],
                    |r| r.get::<_, i64>(0)
                )
                .is_ok(),
                "provider migration still owns its source"
            );
            migrate(&mut db);
            let after: String = db
                .query_row("SELECT record FROM runtime_workspace_operations", [], |r| {
                    r.get(0)
                })
                .expect("same owner");
            assert_eq!(
                serde_json::from_str::<Value>(&after).expect("owner JSON"),
                owner
            );
        }
    }

    #[test]
    fn receipts_requests_workspace_pending_without_immutable_call_stops() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch(
            "CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY,record TEXT)",
        )
        .expect("workspace owners");
        let owner = json!({"id":"restore:old","kind":"restore","agent":"agent","phase":"recovery_required","signature":"original","source":{"accountKey":"default"}});
        db.execute(
            "INSERT INTO runtime_workspace_operations VALUES('restore:old',?1)",
            [owner.to_string()],
        )
        .expect("owner");
        let tx = db.transaction().expect("transaction");
        let checkpoint =
            MigrationCheckpoint::begin(&tx, ID, "runtime_workspace_operations", CUTOVER)
                .expect("checkpoint");
        assert!(
            matches!(Family.move_batch(&tx,"runtime_workspace_operations",&checkpoint),Err(ReceiptMigrationError::Unclassifiable{table,key,signature}) if table=="runtime_workspace_operations" && key=="restore:old" && signature=="original")
        );
        tx.rollback().expect("source preserved");
    }

    #[test]
    fn receipts_requests_oversized_row_runs_alone_before_next_keyset_page() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let mut oversized = plan();
        oversized["text"] =
            json!("x".repeat(crate::receipt_migration_contract::MIGRATION_BATCH_BYTES));
        insert(&db, "plan-request:oversized", oversized);
        insert(&db, "plan-request:next", plan());
        let first = batch(&mut db, true);
        assert_eq!(first.cursor, 1);
        assert_eq!(first.status, MigrationStatus::Running);
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("one oversized reply"),
            1
        );
        migrate(&mut db);
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("resumed next row"),
            2
        );
    }

    #[test]
    fn receipts_requests_every_completed_c_family_replays_original_raw_identity() {
        let work = json!({"id":"task","rootId":"root","title":"Task","status":"ready","created":1.0,"updated":2.0,"version":1});
        let mut cases = vec![
            ("plan-request:exact", plan()),
            ("plan:agent:1", plan()),
            ("work-create", work.clone()),
            ("work-mutate", work.clone()),
            ("work-claim", work.clone()),
            ("work-submit", work.clone()),
            ("work-accept", work),
            ("default-gate", json!({"id":"agent","defaultGate":null})),
            (
                "rename",
                json!({"id":"agent","name":"Original","request_id":"rename","status":"applied","_syncEntitiesAfter":42}),
            ),
            (
                "concurrency",
                json!({"id":"agent","concurrency":8,"agentMode":"multi","agentModeRevision":3,"maxAgents":12}),
            ),
            (
                "annotation",
                json!({"id":"annotation","agent":"agent","rootId":"root","path":"source.ts","line":3,"text":"Original","created":1.0}),
            ),
            (
                "user:complaint",
                json!({"id":"complaint","leadId":"root","status":"resolved","responses":[{"text":"Original"}]}),
            ),
            (
                "queue:agent:exact",
                json!({"status":"updated","revision":"original","capabilities":{"reorder":true,"receipts":true}}),
            ),
            (
                "park-request:root:exact",
                json!({"event":"ready","count":1,"woken":["agent"]}),
            ),
            (
                "project-delete:exact",
                json!({"id":"project","request_id":"exact","outcome":"deleted","deletedAgentIds":["agent"],"deletedRoomIds":["room"]}),
            ),
            (
                "input-message",
                json!({"id":"input-message","status":"queued","waitingFor":["workspace"]}),
            ),
            (
                "room-message",
                json!({"id":"room-message","room":"room","author":"user","text":"Original","at":1.0,"deliveries":{},"status":"delivered"}),
            ),
            (
                "location-applied",
                json!({"requestId":"location-applied","outcome":"applied","value":{"serverId":"server","path":"/project","gitOrigin":null}}),
            ),
            (
                "location-refused",
                json!({"requestId":"location-refused","outcome":"not_applied","error":"Original refusal"}),
            ),
            (
                "fetch",
                json!({"requestId":"fetch","server":"server","branch":"branch","commit":"commit","bytes":12,"destination":"/project"}),
            ),
        ];
        cases.push(("agent-room-message",json!({"id":"agent-room-message","room":"room","deliveries":{},"refused":{"blocked":"Original refusal"}})));
        cases.push((
            "lead-user-message",
            json!({"id":"complaint","room":"user:agent","recipient":"user"}),
        ));
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT);INSERT INTO runtime_event_meta VALUES('input-message','{\"requestedDelivery\":\"current_turn\",\"delivery\":\"current_turn\"}');").expect("original user timing");
        let user_signature =
            crate::python_json(&json!({"agent":"agent","text":"Original","assets":[]}));
        for (id, reply) in &cases {
            if *id == "input-message" {
                db.execute(
                    "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                    params![id, user_signature, reply.to_string()],
                )
                .expect("user message C");
            } else {
                insert(&db, id, reply.clone());
            }
        }
        migrate(&mut db);
        for (id, reply) in &cases {
            let (signature, raw): (String, String) = db
                .query_row(
                    "SELECT signature,result FROM request_results WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap_or_else(|error| panic!("missing family {id}: {error}"));
            assert_eq!(
                signature,
                if *id == "input-message" {
                    user_signature.clone()
                } else {
                    SIGNATURE.to_owned()
                }
            );
            assert_eq!(
                serde_json::from_str::<Value>(&raw).expect("envelope")["reply"],
                *reply,
                "family {id}"
            );
        }
        migrate(&mut db);
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("exact replies"),
            cases.len() as i64
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("all C sources moved"),
            0
        );
    }

    #[test]
    fn receipts_requests_missing_or_wrong_dictionary_shapes_are_never_imported() {
        for (id, reply) in [
            ("plan-request:bad", json!({})),
            (
                "queue:agent:bad",
                json!({"status":"updated","revision":"r","capabilities":[]}),
            ),
            ("user:bad", json!({"id":4})),
            (
                "park-request:bad",
                json!({"event":"ready","count":1,"woken":{}}),
            ),
            ("peer-team:bad", json!({"id":"project","name":{}})),
            ("sidebar-order:bad", json!({"revision":1,"groups":{}})),
            (
                "project-delete:bad",
                json!({"id":"project","outcome":"deleted","deletedAgentIds":{},"deletedRoomIds":[]}),
            ),
        ] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            insert(&db, id, reply);
            let tx = db.transaction().expect("transaction");
            let checkpoint =
                MigrationCheckpoint::begin(&tx, ID, SOURCE, CUTOVER).expect("checkpoint");
            assert!(
                matches!(Family.move_batch(&tx,SOURCE,&checkpoint),Err(ReceiptMigrationError::Unclassifiable{table,key,..}) if table==SOURCE && key==id)
            );
            tx.rollback().expect("source preserved");
            assert_eq!(
                db.query_row("SELECT count(*) FROM request_results", [], |r| r
                    .get::<_, i64>(0))
                    .expect("no invented reply"),
                0
            );
        }
    }

    #[test]
    fn receipts_requests_user_timing_is_proven_or_completed_reply_expires() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT);CREATE TABLE runtime_events(id TEXT PRIMARY KEY,status TEXT);").expect("event owners");
        let signature = crate::python_json(&json!({"agent":"agent","text":"Original","assets":[]}));
        let lost = "0000000f-4240-7000-8000-000000000000";
        for (id, meta) in [
            (
                "known",
                json!({"delivery":"current_turn","requestedDelivery":"next_turn","sendNow":true}),
            ),
            (
                "unpromoted",
                json!({"delivery":"after_turn","sendNow":false}),
            ),
            (lost, json!({"delivery":"current_turn","sendNow":true})),
        ] {
            let reply = json!({"id":id,"status":"queued"});
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                params![id, signature, reply.to_string()],
            )
            .expect("C reply");
            db.execute(
                "INSERT INTO runtime_event_meta VALUES(?1,?2)",
                params![id, meta.to_string()],
            )
            .expect("timing proof");
            db.execute("INSERT INTO runtime_events VALUES(?1,'pending')", [id])
                .expect("pending delivery");
        }
        migrate(&mut db);
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("reconstructable replies"),
            2
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM runtime_events WHERE status='pending'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("pending effects retained"),
            3
        );
        for id in ["known", "unpromoted"] {
            let raw: String = db
                .query_row(
                    "SELECT record FROM runtime_event_meta WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .expect("meta");
            let meta: Value = serde_json::from_str(&raw).expect("meta JSON");
            assert_eq!(meta["requestedDelivery"], "next_turn");
        }
        let tx = db.transaction().expect("transaction");
        assert_eq!(
            MigrationCheckpoint::load(&tx, ID, DELETED_USER_MESSAGE_REPORT)
                .expect("report")
                .expect("count")
                .cursor,
            1
        );
        let identity = studio_runtime_schema::persistence::RequestIdentity::new(
            "messages",
            &["agent"],
            lost,
            &[],
        )
        .expect("old root");
        assert!(matches!(
            crate::request_results::RequestResults::admit(&tx, &identity, &signature, CUTOVER + 1),
            Err(crate::request_results::RequestResultError::Identity(
                studio_runtime_schema::persistence::RequestIdentityError::Expired { .. }
            ))
        ));
        tx.commit().expect("read commit");
        migrate(&mut db);
        let tx = db.transaction().expect("transaction");
        assert_eq!(
            MigrationCheckpoint::load(&tx, ID, DELETED_USER_MESSAGE_REPORT)
                .expect("report")
                .expect("count")
                .cursor,
            1
        );
    }

    #[test]
    fn receipts_requests_invalid_original_timing_stops_with_metadata_identity() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT);INSERT INTO runtime_event_meta VALUES('message','{\"requestedDelivery\":{}}');").expect("invalid timing");
        let signature = crate::python_json(&json!({"agent":"agent","text":"Original","assets":[]}));
        db.execute("INSERT INTO runtime_operation_receipts VALUES('message',?1,'{\"id\":\"message\",\"status\":\"queued\"}')",[&signature]).expect("C reply");
        let tx = db.transaction().expect("transaction");
        let checkpoint = MigrationCheckpoint::begin(&tx, ID, SOURCE, CUTOVER).expect("checkpoint");
        assert!(
            matches!(Family.move_batch(&tx,SOURCE,&checkpoint),Err(ReceiptMigrationError::Unclassifiable{table,key,signature:stored}) if table=="runtime_event_meta" && key=="message" && stored==signature)
        );
        tx.rollback().expect("source preserved");
    }

    #[test]
    fn receipts_requests_location_signatures_use_captured_input_and_leave_wire_unchanged() {
        for _ in 0..1 {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            let input = json!({"action":"add_location","project":"project","server":"peer","path":"/original"});
            let signature = crate::sha256_hex(crate::python_json_compact(&input).as_bytes());
            let old_signature = signature.clone();
            db.execute("INSERT INTO runtime_projects VALUES('project','{}')", [])
                .expect("project");
            for (id, outcome) in [("complete", "applied"), ("pending", "unknown")] {
                let body = json!({"requestId":id,"action":"add_location","payload":{"project":"project","request":input,"signature":old_signature}}).to_string();
                db.execute(
                    "INSERT INTO runtime_server_outbox VALUES(?1,?2)",
                    params![id, body],
                )
                .expect("captured wire input");
                let reply = json!({"requestId":id,"projectId":"project","outcome":outcome,"value":{"serverId":"peer","path":"/original","gitOrigin":null}});
                db.execute(
                    "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                    params![id, old_signature, reply.to_string()],
                )
                .expect("location source");
            }
            let before: Vec<(String, String)> = db
                .prepare("SELECT id,body FROM runtime_server_outbox ORDER BY id")
                .expect("query")
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("rows")
                .collect::<rusqlite::Result<_>>()
                .expect("wire");
            migrate(&mut db);
            let tx = db.transaction().expect("transaction");
            let saved = crate::request_results::RequestResults::replay(
                &tx,
                "complete",
                &signature,
                CUTOVER + 1,
            )
            .expect("normalized exact replay")
            .expect("saved reply");
            assert_eq!(
                serde_json::from_str::<Value>(&saved.result).expect("envelope")["reply"]["value"]["gitOrigin"],
                Value::Null
            );
            let owner: String = tx
                .query_row(
                    "SELECT record FROM runtime_projects WHERE id='project'",
                    [],
                    |r| r.get(0),
                )
                .expect("owner");
            let owner: Value = serde_json::from_str(&owner).expect("owner JSON");
            let token: AdmittedRequest =
                serde_json::from_value(owner["locationIntents"]["pending"]["request"].clone())
                    .expect("pending token");
            assert_eq!(token.signature(), signature);
            assert_eq!(token.id(), "pending");
            tx.rollback().expect("read only");
            let after: Vec<(String, String)> = db
                .prepare("SELECT id,body FROM runtime_server_outbox ORDER BY id")
                .expect("query")
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("rows")
                .collect::<rusqlite::Result<_>>()
                .expect("wire");
            assert_eq!(after, before, "original signed body stays byte identical");
            migrate(&mut db);
        }
    }

    #[test]
    fn receipts_requests_pending_location_rejects_signature_and_token_mismatch() {
        for bad_source in [true, false] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            let input = json!({"action":"add_location","project":"project","server":"peer","path":"/original"});
            let signature = crate::sha256_hex(crate::python_json_compact(&input).as_bytes());
            let source_signature = if bad_source {
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            } else {
                &signature
            };
            let token = admitted_owner_token("location", "wrong owner signature", CUTOVER);
            db.execute(
                "INSERT INTO runtime_projects VALUES('project',?1)",
                [json!({"locationIntents":{"location":{"request":token}}}).to_string()],
            )
            .expect("owner");
            db.execute("INSERT INTO runtime_server_outbox VALUES('location',?1)",[json!({"requestId":"location","action":"add_location","payload":{"project":"project","request":input,"signature":signature}}).to_string()]).expect("captured request");
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES('location',?1,?2)",
                params![
                    source_signature,
                    json!({"requestId":"location","projectId":"project","outcome":"unknown"})
                        .to_string()
                ],
            )
            .expect("pending C marker");
            let tx = db.transaction().expect("transaction");
            let checkpoint =
                MigrationCheckpoint::begin(&tx, ID, SOURCE, CUTOVER).expect("checkpoint");
            let error = Family
                .move_batch(&tx, SOURCE, &checkpoint)
                .expect_err("mismatch cannot gain new authority");
            assert!(
                matches!(error,ReceiptMigrationError::Unclassifiable{table,key,..}if (bad_source&&table==SOURCE&&key=="location")||(!bad_source&&table=="runtime_projects"&&key=="project"))
            );
            tx.rollback().expect("source preserved");
            assert_eq!(
                db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                    .get::<_, i64>(0))
                    .expect("old marker"),
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM request_results", [], |r| r
                    .get::<_, i64>(0))
                    .expect("no reply authority"),
                0
            );
        }
    }

    #[test]
    fn receipts_requests_unchanged_ui_location_without_input_keeps_exact_signature() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(
            &db,
            "ui-old",
            json!({"requestId":"ui-old","outcome":"applied","value":{"serverId":"peer","path":"/original"}}),
        );
        migrate(&mut db);
        assert!(
            crate::request_results::RequestResults::replay(&db, "ui-old", SIGNATURE, CUTOVER + 1)
                .expect("unchanged UI signature")
                .is_some()
        );
    }

    #[test]
    fn receipts_requests_changed_synchronous_families_expire_and_fence_prefixed_future_roots() {
        for prefix in ["peer-team:", "sidebar-order:"] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            let root = "0000000f-4240-7000-8000-000000000000";
            let id = format!("{prefix}{root}");
            let reply = if prefix == "peer-team:" {
                json!({"id":"project","name":"Original"})
            } else {
                json!({"revision":1,"groups":[]})
            };
            insert(&db, &id, reply.clone());
            migrate(&mut db);
            assert!(
                crate::request_results::RequestResults::replay(
                    &db,
                    &id,
                    "canonical new hash",
                    CUTOVER + 1
                )
                .expect("no conflict")
                .is_none()
            );
            let tx = db.transaction().expect("transaction");
            let identity = studio_runtime_schema::persistence::RequestIdentity::new(
                prefix.trim_end_matches(':'),
                &[],
                root,
                &[],
            )
            .expect("old root");
            assert!(matches!(
                crate::request_results::RequestResults::admit(
                    &tx,
                    &identity,
                    "canonical new hash",
                    CUTOVER + 1
                ),
                Err(crate::request_results::RequestResultError::Identity(
                    studio_runtime_schema::persistence::RequestIdentityError::Expired { .. }
                ))
            ));
            assert_eq!(
                MigrationCheckpoint::load(
                    &tx,
                    ID,
                    &format!("deleted-completed-c:{}", prefix.trim_end_matches(':'))
                )
                .expect("report")
                .expect("family count")
                .cursor,
                1
            );
            tx.rollback().expect("no effects");
            migrate(&mut db);
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            let future = format!("{prefix}0000000f-4241-7000-8000-000000000000");
            insert(&db, &future, reply);
            let tx = db.transaction().expect("transaction");
            let checkpoint =
                MigrationCheckpoint::begin(&tx, ID, SOURCE, CUTOVER).expect("checkpoint");
            assert!(
                matches!(Family.move_batch(&tx,SOURCE,&checkpoint),Err(ReceiptMigrationError::FutureDatedCompletedRequest{table,key,issued_ms:1_000_001,..})if table==SOURCE&&key==future)
            );
            tx.rollback().expect("future source preserved");
        }
    }

    #[test]
    fn receipts_requests_project_registration_and_pending_location_share_owner_contract() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let project = json!({"id":"project","registration":{"server":"server","path":"/project","name":"Original"}});
        db.execute(
            "INSERT INTO runtime_projects VALUES('project',?1)",
            [project.to_string()],
        )
        .expect("project");
        insert(
            &db,
            "project-registration:project",
            json!({"projectId":"project","requestId":"location","outcome":"unknown"}),
        );
        insert(
            &db,
            "location",
            json!({"projectId":"project","requestId":"location","outcome":"unknown"}),
        );
        migrate(&mut db);
        let raw: String = db
            .query_row("SELECT record FROM runtime_projects", [], |r| r.get(0))
            .expect("owner");
        let owner: Value = serde_json::from_str(&raw).expect("owner JSON");
        assert_eq!(owner["registration"]["name"], "Original");
        assert_eq!(owner["registration"]["signature"], SIGNATURE);
        assert_eq!(owner["registration"]["requestId"], "location");
        assert_eq!(owner["registration"]["status"], "unknown");
        let token: AdmittedRequest =
            serde_json::from_value(owner["locationIntents"]["location"]["request"].clone())
                .expect("ProjectsRepository::location_request contract");
        assert_eq!(token.id(), "location");
        assert_eq!(token.signature(), SIGNATURE);
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("uncertainty stays on owner"),
            0
        );
        migrate(&mut db);
    }

    #[test]
    fn receipts_requests_remote_metadata_keeps_proven_wire_v5_after_rollback_and_reopen() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT)")
            .expect("metadata");
        let mut expected = Vec::new();
        for (operation, action) in [
            ("input", "input"),
            ("anchor-input", "input"),
            ("event", "event"),
        ] {
            let id = format!("old-{operation}");
            let wire = uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                format!("studio-cross-server:{operation}:{id}").as_bytes(),
            )
            .to_string();
            let body =
                json!({"requestId":wire,"action":action,"payload":{"text":"Original"}}).to_string();
            db.execute("INSERT INTO runtime_event_meta VALUES(?1,'{}')", [&id])
                .expect("old event");
            db.execute(
                "INSERT INTO runtime_server_outbox VALUES(?1,?2)",
                params![wire, body],
            )
            .expect("retained transport authority");
            expected.push((id, wire, body));
        }
        batch_source(&mut db, "runtime_event_meta", false);
        let raw: String = db
            .query_row("SELECT record FROM runtime_event_meta LIMIT 1", [], |r| {
                r.get(0)
            })
            .expect("rollback");
        assert_eq!(raw, "{}");
        migrate(&mut db);
        migrate(&mut db);
        for (id, wire, body) in expected {
            let raw: String = db
                .query_row(
                    "SELECT record FROM runtime_event_meta WHERE id=?1",
                    [&id],
                    |r| r.get(0),
                )
                .expect("metadata");
            let meta: Value = serde_json::from_str(&raw).expect("meta JSON");
            assert_eq!(meta["remoteRequestId"], wire);
            assert_eq!(
                uuid::Uuid::parse_str(meta["remoteRequestId"].as_str().expect("root"))
                    .expect("UUID")
                    .get_version_num(),
                5
            );
            assert_eq!(
                db.query_row(
                    "SELECT body FROM runtime_server_outbox WHERE id=?1",
                    [wire],
                    |r| r.get::<_, String>(0)
                )
                .expect("same request"),
                body
            );
        }
    }

    #[test]
    fn receipts_requests_remote_metadata_accepts_cached_proof_and_holds_unprovable_ids() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT);INSERT INTO runtime_event_meta VALUES('cached','{}');INSERT INTO runtime_event_meta VALUES('missing','{}');INSERT INTO runtime_event_meta VALUES('ambiguous','{}');").expect("metadata");
        let wire = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            b"studio-cross-server:event:cached",
        )
        .to_string();
        let body = json!({"requestId":wire,"action":"event","payload":{}}).to_string();
        let reply = crate::multi_server_delivery::SavedRemoteReply {
            principal: None,
            server: Some("peer".into()),
            actor: None,
            body: Some(body),
            response: crate::multi_server_delivery::RemoteReply {
                request_id: wire.clone(),
                outcome: studio_runtime_schema::persistence::RemoteOutcome::Applied,
                value: None,
                error: None,
                waiting_for: None,
            },
        };
        let tx = db.transaction().expect("transaction");
        import_result(
            &tx,
            &wire,
            SIGNATURE,
            &ResultEnvelope {
                actor: None,
                reply,
                references: Vec::new(),
            },
            CUTOVER,
        )
        .expect("completed outgoing proof");
        tx.commit().expect("proof commit");
        for operation in ["input", "anchor-input"] {
            let root = uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                format!("studio-cross-server:{operation}:ambiguous").as_bytes(),
            )
            .to_string();
            db.execute(
                "INSERT INTO runtime_server_outbox VALUES(?1,?2)",
                params![
                    root,
                    json!({"requestId":root,"action":"input","payload":{}}).to_string()
                ],
            )
            .expect("conflicting historical authorities");
        }
        migrate(&mut db);
        migrate(&mut db);
        for (id, expected) in [
            ("cached", Some(wire.as_str())),
            ("missing", None),
            ("ambiguous", None),
        ] {
            let raw: String = db
                .query_row(
                    "SELECT record FROM runtime_event_meta WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .expect("meta");
            let meta: Value = serde_json::from_str(&raw).expect("JSON");
            assert!(
                meta.get("remoteRequestId").is_some(),
                "old IDs never become fresh admission"
            );
            assert_eq!(meta["remoteRequestId"].as_str(), expected);
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("no new admission or effect"),
            1
        );
    }
}
