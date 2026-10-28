//! Retain typed transport queues and move only completed replay bodies.
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    AccessOutboundState, FederationAckState, FederationDeliveryState, FederationMessageKind,
    MigrationStatus, OutboxKind, OutboxStatus, OwnerKind, RemoteOutcome, ServerDeliveryState,
    ServerFetchState, ServerOperationKind, enum_check,
};

use crate::multi_server_delivery::{
    AccessPending, HttpAccessReply, SavedRemoteReply, federation_envelope_signature,
};
use crate::outbox::OutboxRow;
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, ReceiptProjection, Result,
    SourceReceipt, admitted_owner_token, delete_source, import_outbox, import_result,
    read_source_batch, source_exists as exists, unclassifiable, validate_completed_discard,
};
use crate::request_results::ResultEnvelope;

pub(super) const ID: &str = "2026-10-28-receipts-remote";
pub(super) const SOURCES: &[&str] = &[
    "runtime_access_receipts",
    "runtime_access_outbound",
    "runtime_server_inbox",
    "runtime_server_outbox",
    "runtime_server_fetch",
    "runtime_federation_inbox",
    "runtime_federation_outbox",
];
pub(super) struct Family;
/// Protocol version in the supported pre-cutover envelope format. Immutable source contract.
const SOURCE_FEDERATION_PROTOCOL_VERSION: u64 = 1;
/// The sole authorized historical wire normalization; payload/root/fingerprint stay exact.
const WIRE_OPERATION_REPORT: &str = "normalized-wire:server-outbox-operation";

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        ID
    }
    fn prepare(&self, tx: &Transaction<'_>, cutover_ms: i64) -> Result<()> {
        let mut report = MigrationCheckpoint::begin(tx, ID, WIRE_OPERATION_REPORT, cutover_ms)?;
        report.status = MigrationStatus::Complete;
        report.save(tx, ID, WIRE_OPERATION_REPORT, cutover_ms)?;
        for table in [
            "runtime_server_inbox",
            "runtime_server_outbox",
            "runtime_server_fetch",
        ] {
            if exists(tx, table)? && !column(tx, table, "admission")? {
                tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN admission TEXT"))?;
            }
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
        let projection = projection(source)?;
        let rows = read_source_batch(tx, source, checkpoint, &projection, "1")?;
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        for mut row in rows {
            let mut record: Value = row.json(source)?;
            if source == "runtime_server_outbox" && row.signature.is_empty() {
                let body = record["body"]
                    .as_str()
                    .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
                row.signature = crate::sha256_hex(body.as_bytes());
                tx.execute(
                    "UPDATE runtime_server_outbox SET fingerprint=?1 WHERE rowid=?2",
                    params![row.signature, row.rowid],
                )?;
            }
            match source {
                "runtime_access_receipts" => {
                    access_inbound(tx, &row, &record, checkpoint.cutover_ms)?
                }
                "runtime_access_outbound" => {
                    access_outbound(tx, &row, &mut record, checkpoint.cutover_ms)?
                }
                "runtime_server_inbox" | "runtime_server_outbox" => {
                    server(tx, source, &row, &record, checkpoint.cutover_ms)?
                }
                "runtime_server_fetch" => fetch(tx, &row, &record, checkpoint.cutover_ms)?,
                "runtime_federation_inbox" | "runtime_federation_outbox" => {
                    federation(tx, source, &row, &mut record, checkpoint.cutover_ms)?
                }
                _ => return Err(unclassifiable(source, &row.id, &row.signature)),
            }
            next.cursor = row.rowid;
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
        // Virtual guard columns enforce generated enum CHECKs without cloning
        // transport payloads. The bounded pass has classified every existing row.
        for table in ["runtime_server_inbox", "runtime_server_outbox"] {
            guard(
                tx,
                table,
                "receipt_state",
                "state",
                &enum_check::<ServerDeliveryState>("receipt_state"),
            )?;
        }
        guard(
            tx,
            "runtime_server_fetch",
            "receipt_state",
            "state",
            &enum_check::<ServerFetchState>("receipt_state"),
        )?;
        guard(
            tx,
            "runtime_server_outbox",
            "receipt_kind",
            "json_extract(body,'$.action')",
            &enum_check::<ServerOperationKind>("receipt_kind"),
        )?;
        guard(
            tx,
            "runtime_access_outbound",
            "receipt_state",
            "json_extract(record,'$.outcome')",
            &enum_check::<AccessOutboundState>("receipt_state"),
        )?;
        guard(
            tx,
            "runtime_federation_inbox",
            "receipt_ack",
            "json_extract(record,'$.ack')",
            &enum_check::<FederationAckState>("receipt_ack"),
        )?;
        guard(
            tx,
            "runtime_federation_inbox",
            "receipt_kind",
            "json_extract(record,'$.kind')",
            &enum_check::<FederationMessageKind>("receipt_kind"),
        )?;
        guard(
            tx,
            "runtime_federation_outbox",
            "receipt_state",
            "json_extract(record,'$.status')",
            &enum_check::<FederationDeliveryState>("receipt_state"),
        )?;
        guard(
            tx,
            "runtime_federation_outbox",
            "receipt_kind",
            "json_extract(record,'$.kind')",
            &enum_check::<FederationMessageKind>("receipt_kind"),
        )?;
        Ok(())
    }
}

fn column(tx: &Transaction<'_>, table: &str, name: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo(?1) WHERE name=?2)",
        params![table, name],
        |r| r.get(0),
    )?)
}
fn guard(
    tx: &Transaction<'_>,
    table: &str,
    name: &str,
    expression: &str,
    check: &str,
) -> Result<()> {
    if exists(tx, table)? && !column(tx, table, name)? {
        let nullable = if table == "runtime_federation_inbox" && name == "receipt_kind" {
            ""
        } else {
            "NOT NULL"
        };
        tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} TEXT GENERATED ALWAYS AS ({expression}) VIRTUAL {nullable} {check}"))?;
    }
    Ok(())
}

fn projection(source: &str) -> Result<ReceiptProjection<'static>> {
    Ok(match source {
        "runtime_access_receipts" => ReceiptProjection {
            id: "json_array('access',json_array(source.client),source.request_id,json_array())",
            signature: "source.hash",
            result: "json_object('client',source.client,'request_id',source.request_id,'session',source.session,'status',source.status,'record',source.record)",
        },
        "runtime_access_outbound" => ReceiptProjection {
            id: "json_array('access-outbound',json_array(source.server),source.request_id,json_array())",
            signature: "source.hash",
            result: "source.record",
        },
        "runtime_server_inbox" => ReceiptProjection {
            id: "source.id",
            signature: "source.signature",
            result: "json_object('state',source.state,'result',source.result,'principal',source.principal,'actor',source.actor)",
        },
        "runtime_server_outbox" => ReceiptProjection {
            id: "source.id",
            signature: "coalesce(source.fingerprint,'')",
            result: "json_object('state',source.state,'result',source.result,'body',source.body,'server',source.server)",
        },
        "runtime_server_fetch" => ReceiptProjection {
            id: "source.id",
            signature: "source.signature",
            result: "json_object('state',source.state,'result',source.result,'server',source.server)",
        },
        "runtime_federation_inbox" => ReceiptProjection {
            id: "json_array('federation-inbox',json_array(source.peer_id),source.message_id,json_array())",
            signature: "CASE WHEN json_valid(source.record) THEN coalesce(json_extract(source.record,'$.hash'),'') ELSE '' END",
            result: "source.record",
        },
        "runtime_federation_outbox" => ReceiptProjection {
            id: "json_array('federation-outbox',json_array(source.peer_id),source.id,json_array())",
            signature: "CASE WHEN json_valid(source.record) THEN coalesce(json_extract(source.record,'$.hash'),'') ELSE '' END",
            result: "source.record",
        },
        _ => {
            return Err(ReceiptMigrationError::Incomplete {
                migration: ID.into(),
                source_table: source.into(),
            });
        }
    })
}

fn decode<T: serde::de::DeserializeOwned>(
    source: &str,
    row: &SourceReceipt,
    value: Value,
) -> Result<T> {
    serde_json::from_value(value).map_err(|_| unclassifiable(source, &row.id, &row.signature))
}
fn save(
    tx: &Transaction<'_>,
    source: &str,
    row: &SourceReceipt,
    reply: Value,
    actor: Option<String>,
    cutover: i64,
) -> Result<()> {
    if !reply.is_object() {
        return Err(unclassifiable(source, &row.id, &row.signature));
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
        cutover,
    )
}

fn deletion(
    tx: &Transaction<'_>,
    source: &str,
    family: &str,
    row: &SourceReceipt,
    root: &str,
    cutover: i64,
) -> Result<()> {
    match validate_completed_discard(source, root, cutover) {
        Ok(()) => {}
        Err(ReceiptMigrationError::FutureDatedCompletedRequest {
            issued_ms,
            cutover_ms,
            ..
        }) => {
            return Err(ReceiptMigrationError::FutureDatedCompletedRequest {
                table: source.into(),
                key: row.id.clone(),
                issued_ms,
                cutover_ms,
            });
        }
        Err(error) => return Err(error),
    }
    let mut count =
        MigrationCheckpoint::begin(tx, ID, &format!("deleted-completed-c:{family}"), cutover)?;
    count.cursor += 1;
    count.status = MigrationStatus::Complete;
    count.save(tx, ID, &format!("deleted-completed-c:{family}"), cutover)?;
    Ok(())
}

fn access_inbound(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    wrapper: &Value,
    cutover: i64,
) -> Result<()> {
    let source = "runtime_access_receipts";
    let invalid = || unclassifiable(source, &row.id, &row.signature);
    let record: Value = serde_json::from_str(wrapper["record"].as_str().ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    if !record.is_object() {
        return Err(invalid());
    }
    match wrapper["status"].as_str() {
        Some("complete") => {
            let _: HttpAccessReply = decode(source, row, record.clone())?;
            if !record["status"]
                .as_u64()
                .is_some_and(|n| (100..=599).contains(&n))
                || !record["headers"].is_array()
                || !record["body"].is_string()
                || !record["completed"].is_number()
            {
                return Err(invalid());
            }
            if data_encoding::BASE64
                .decode(record["body"].as_str().ok_or_else(invalid)?.as_bytes())
                .is_err()
            {
                return Err(invalid());
            }
            save(tx, source, row, record, None, cutover)?;
        }
        Some("tombstone") => deletion(
            tx,
            source,
            "access-inbound",
            row,
            wrapper["request_id"].as_str().ok_or_else(invalid)?,
            cutover,
        )?,
        Some("pending" | "unknown" | "running") => {
            let client = wrapper["client"].as_str().ok_or_else(invalid)?;
            let request = wrapper["request_id"].as_str().ok_or_else(invalid)?;
            let session = wrapper["session"].as_str().ok_or_else(invalid)?;
            let key = json!(["access-pending", [client], request, []]).to_string();
            import_outbox(
                tx,
                &OutboxRow {
                    id: key.clone(),
                    kind: OutboxKind::RemoteDelivery,
                    owner_kind: OwnerKind::Request,
                    owner_id: key,
                    request_id: Some(request.into()),
                    signature: row.signature.clone(),
                    status: OutboxStatus::Unknown,
                    account: None,
                    stream_id: None,
                    generation: None,
                    ordinal: None,
                    next_at: None,
                    payload: AccessPending {
                        client: client.into(),
                        request_id: request.into(),
                        session: session.into(),
                        record: serde_json::value::RawValue::from_string(
                            wrapper["record"].as_str().ok_or_else(invalid)?.to_owned(),
                        )
                        .map_err(|_| invalid())?,
                        admission: admitted_owner_token(&row.id, &row.signature, cutover),
                    },
                    created: cutover,
                },
            )?;
        }
        _ => return Err(invalid()),
    }
    delete_source(tx, source, row.rowid)
}

fn access_outbound(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    record: &mut Value,
    cutover: i64,
) -> Result<()> {
    let source = "runtime_access_outbound";
    let state: AccessOutboundState = decode(source, row, record["outcome"].clone())?;
    match state {
        AccessOutboundState::Unknown => {
            if !record["created"].is_number() {
                return Err(unclassifiable(source, &row.id, &row.signature));
            }
            record["admission"] = json!(admitted_owner_token(&row.id, &row.signature, cutover));
            tx.execute(
                "UPDATE runtime_access_outbound SET record=?1 WHERE rowid=?2",
                params![record.to_string(), row.rowid],
            )?;
        }
        AccessOutboundState::Complete => {
            if !record["value"].is_object() {
                return Err(unclassifiable(source, &row.id, &row.signature));
            }
            save(
                tx,
                source,
                row,
                json!({"outcome":state,"value":record.get("value")}),
                None,
                cutover,
            )?;
            delete_source(tx, source, row.rowid)?;
        }
        AccessOutboundState::Redacted => {
            let key: Value = serde_json::from_str(&row.id)
                .map_err(|_| unclassifiable(source, &row.id, &row.signature))?;
            let root = key[2]
                .as_str()
                .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
            deletion(tx, source, "access-outbound", row, root, cutover)?;
            delete_source(tx, source, row.rowid)?;
        }
    }
    Ok(())
}

fn server(
    tx: &Transaction<'_>,
    source: &str,
    row: &SourceReceipt,
    wrapper: &Value,
    cutover: i64,
) -> Result<()> {
    let state: ServerDeliveryState = decode(source, row, wrapper["state"].clone())?;
    let mut destination = row.clone();
    let mut wire_root = row.id.clone();
    if source == "runtime_server_outbox" {
        let invalid = || unclassifiable(source, &row.id, &row.signature);
        let body: Value = wrapper["body"]
            .as_str()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .ok_or_else(invalid)?;
        let peer = wrapper["server"].as_str().ok_or_else(invalid)?;
        let root = body["requestId"].as_str().ok_or_else(invalid)?;
        wire_root = root.to_owned();
        let action = body["action"].as_str().ok_or_else(invalid)?;
        let _: ServerOperationKind = decode(source, row, body["action"].clone())?;
        let mut operation = vec![action];
        if let Some(parts) = body.get("operation") {
            for part in parts.as_array().ok_or_else(invalid)? {
                operation.push(part.as_str().ok_or_else(invalid)?);
            }
        }
        destination.id = crate::multi_server_delivery::remote_operation_key(
            "server-outbox",
            &[peer],
            root,
            &operation,
        );
    }
    if state == ServerDeliveryState::Complete {
        let response: Value = wrapper["result"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
        let outcome: RemoteOutcome = decode(source, row, response["outcome"].clone())?;
        if outcome == RemoteOutcome::Unknown {
            return Err(unclassifiable(source, &row.id, &row.signature));
        }
        if response["requestId"] != wire_root {
            return Err(unclassifiable(source, &row.id, &row.signature));
        }
        let actor = wrapper["actor"].as_str().map(str::to_owned).or_else(|| {
            wrapper["body"]
                .as_str()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|body| body["actor"].as_str().map(str::to_owned))
        });
        let reply = json!({"principal":wrapper.get("principal"),"server":wrapper.get("server"),"actor":actor,"body":wrapper.get("body"),"response":response});
        let _: SavedRemoteReply = decode(source, row, reply.clone())?;
        save(tx, source, &destination, reply, actor, cutover)?;
        delete_source(tx, source, row.rowid)?;
    } else {
        if source == "runtime_server_outbox" {
            let mut body: Value = wrapper["body"]
                .as_str()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
            let object = body
                .as_object_mut()
                .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
            let normalize_operation = !object.contains_key("operation");
            object.entry("operation").or_insert(json!([]));
            if !body["operation"]
                .as_array()
                .is_some_and(|v| v.iter().all(Value::is_string))
            {
                return Err(unclassifiable(source, &row.id, &row.signature));
            }
            let _: ServerOperationKind = decode(source, row, body["action"].clone())?;
            if matches!(
                body["action"].as_str(),
                Some("add_location" | "remove_location")
            ) {
                let checked = &body["payload"]["_projectAdmission"];
                if checked["project"] != body["payload"]["project"]
                    || !checked["project"].is_string()
                    || !checked["seq"].is_i64()
                    || !checked["workspace"].is_string()
                    || !checked["actor"].is_null() && !checked["actor"].is_string()
                    || !checked["epoch"].is_null() && !checked["epoch"].is_i64()
                {
                    return Err(unclassifiable(source, &row.id, &row.signature));
                }
            }
            tx.execute(
                "UPDATE runtime_server_outbox SET body=?1 WHERE rowid=?2",
                params![body.to_string(), row.rowid],
            )?;
            if normalize_operation {
                let mut report =
                    MigrationCheckpoint::begin(tx, ID, WIRE_OPERATION_REPORT, cutover)?;
                report.cursor += 1;
                report.status = MigrationStatus::Complete;
                report.save(tx, ID, WIRE_OPERATION_REPORT, cutover)?;
            }
        }
        tx.execute(
            &format!("UPDATE {source} SET admission=?1 WHERE rowid=?2"),
            params![
                json!(admitted_owner_token(
                    &destination.id,
                    &row.signature,
                    cutover
                ))
                .to_string(),
                row.rowid
            ],
        )?;
    }
    Ok(())
}

fn fetch(tx: &Transaction<'_>, row: &SourceReceipt, wrapper: &Value, cutover: i64) -> Result<()> {
    let source = "runtime_server_fetch";
    let state: ServerFetchState = decode(source, row, wrapper["state"].clone())?;
    match state {
        ServerFetchState::Preparing | ServerFetchState::Fetching => {
            tx.execute(
                "UPDATE runtime_server_fetch SET admission=?1 WHERE rowid=?2",
                params![
                    json!(admitted_owner_token(&row.id, &row.signature, cutover)).to_string(),
                    row.rowid
                ],
            )?;
        }
        ServerFetchState::Complete => {
            let reply: Value = wrapper["result"]
                .as_str()
                .and_then(|s| serde_json::from_str(s).ok())
                .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
            if reply["requestId"] != row.id
                || reply["server"] != wrapper["server"]
                || !reply["destination"].is_string()
                || !reply["bytes"].is_u64()
            {
                return Err(unclassifiable(source, &row.id, &row.signature));
            }
            save(tx, source, row, reply, None, cutover)?;
            delete_source(tx, source, row.rowid)?;
        }
    }
    Ok(())
}

fn federation(
    tx: &Transaction<'_>,
    source: &str,
    row: &SourceReceipt,
    record: &mut Value,
    cutover: i64,
) -> Result<()> {
    if source == "runtime_federation_inbox" {
        let _: FederationAckState = decode(source, row, record["ack"].clone())?;
        if !record["created"].is_number() || !record["hash"].is_string() {
            return Err(unclassifiable(source, &row.id, &row.signature));
        }
        if !record["kind"].is_null() {
            let _: FederationMessageKind = decode(source, row, record["kind"].clone())?;
        }
        let reply = record
            .as_object_mut()
            .and_then(|r| r.remove("receipt"))
            .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
        save(tx, source, row, reply, None, cutover)?;
        let key: Value = serde_json::from_str(&row.id)
            .map_err(|_| unclassifiable(source, &row.id, &row.signature))?;
        record["requestId"] = key[2].clone();
        record["admission"] = json!(admitted_owner_token(&row.id, &row.signature, cutover));
        tx.execute(
            "UPDATE runtime_federation_inbox SET record=?1 WHERE rowid=?2",
            params![record.to_string(), row.rowid],
        )?;
    } else {
        let _: FederationMessageKind = decode(source, row, record["kind"].clone())?;
        let state: FederationDeliveryState = decode(source, row, record["status"].clone())?;
        let mut identity = row.clone();
        if state == FederationDeliveryState::Queued
            || record.get("receipt").is_some_and(Value::is_object)
        {
            if !exists(tx, "runtime_federation_identity")? {
                return Err(unclassifiable(source, &row.id, &row.signature));
            }
            let sender: Option<String> = tx.query_row(
                "SELECT CASE WHEN json_valid(record) THEN json_extract(record,'$.stateId') ELSE NULL END FROM runtime_federation_identity WHERE id='local'",
                [],
                |r| r.get(0),
            ).optional()?.flatten();
            let sender = sender.ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
            let envelope = json!({"protocol":SOURCE_FEDERATION_PROTOCOL_VERSION,"senderServer":sender,"id":record["id"],"kind":record["kind"],"room":record["room"],"payload":record["payload"],"created":record["created"]});
            let hash = federation_envelope_signature(&envelope)
                .map_err(|_| unclassifiable(source, &row.id, &row.signature))?;
            if record.get("hash").is_some_and(|saved| saved != &hash) {
                return Err(unclassifiable(source, &row.id, &row.signature));
            }
            identity.signature = hash;
        }
        match state {
            FederationDeliveryState::Queued => {
                record["requestId"] = record["id"].clone();
                record["admission"] = json!(admitted_owner_token(
                    &identity.id,
                    &identity.signature,
                    cutover
                ));
                tx.execute(
                    "UPDATE runtime_federation_outbox SET record=?1 WHERE rowid=?2",
                    params![record.to_string(), row.rowid],
                )?;
            }
            FederationDeliveryState::Delivered => {
                if let Some(reply) = record.get("receipt").filter(|v| v.is_object()).cloned() {
                    let reply = json!({"ack":{"id":record["id"],"hash":identity.signature,"requestId":record["id"]},"receipt":reply});
                    save(tx, source, &identity, reply, None, cutover)?;
                } else {
                    let root = record["id"]
                        .as_str()
                        .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
                    deletion(tx, source, "federation-outbox", row, root, cutover)?;
                }
                delete_source(tx, source, row.rowid)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt_migration_contract::{MIGRATION_BATCH_ROWS, prepare_schema};
    use rusqlite::Connection;
    const CUTOVER: i64 = 1_000_000;
    fn fixture(db: &mut Connection) {
        db.execute_batch("CREATE TABLE runtime_access_receipts(client TEXT,request_id TEXT,hash TEXT,session TEXT,status TEXT,record TEXT,PRIMARY KEY(client,request_id));CREATE TABLE runtime_access_outbound(server TEXT,request_id TEXT,hash TEXT,record TEXT,PRIMARY KEY(server,request_id));CREATE TABLE runtime_server_inbox(id TEXT PRIMARY KEY,signature TEXT,state TEXT,result TEXT,principal TEXT,actor TEXT);CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,fingerprint TEXT,state TEXT,result TEXT,body TEXT,server TEXT DEFAULT 'server');CREATE TABLE runtime_server_fetch(id TEXT PRIMARY KEY,signature TEXT,state TEXT,result TEXT,server TEXT);CREATE TABLE runtime_federation_inbox(peer_id TEXT,message_id TEXT,record TEXT,PRIMARY KEY(peer_id,message_id));CREATE TABLE runtime_federation_outbox(id TEXT PRIMARY KEY,peer_id TEXT,record TEXT);").expect("old format");
        let tx = db.transaction().expect("transaction");
        prepare_schema(&tx).expect("schema");
        Family.prepare(&tx, CUTOVER).expect("prepare");
        tx.commit().expect("schema commit");
    }
    fn insert(db: &Connection, client: &str, id: &str, status: &str) {
        db.execute("INSERT INTO runtime_access_receipts VALUES(?1,?2,'signature','session',?3,?4)",params![client,id,status,json!({"status":200,"headers":[["content-type","application/json"]],"body":"e30=","completed":1.0}).to_string()]).expect("source");
    }
    fn batch(db: &mut Connection, source: &str, commit: bool) -> MigrationCheckpoint {
        let tx = db.transaction().expect("transaction");
        let checkpoint = MigrationCheckpoint::begin(&tx, ID, source, CUTOVER).expect("checkpoint");
        let next = Family.move_batch(&tx, source, &checkpoint).expect("move");
        next.save(&tx, ID, source, CUTOVER).expect("save");
        if commit {
            tx.commit().expect("commit");
        } else {
            tx.rollback().expect("crash");
        }
        next
    }
    fn migrate(db: &mut Connection) {
        for source in SOURCES {
            while batch(db, source, true).status != MigrationStatus::Complete {}
        }
        let tx = db.transaction().expect("transaction");
        Family.finish(&tx).expect("finish");
        tx.commit().expect("finish commit");
    }

    #[test]
    fn receipts_remote_old_rows_second_open_preserves_scope_and_unknown() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(&db, "client-a", "same", "complete");
        insert(&db, "client-b", "same", "complete");
        insert(&db, "client-a", "unknown", "pending");
        db.execute("INSERT INTO runtime_access_outbound VALUES('server','unknown','hash','{\"outcome\":\"unknown\",\"created\":1.0}')",[]).expect("outbound");
        migrate(&mut db);
        let key = json!(["access", ["client-a"], "same", []]).to_string();
        let saved: (String, i64) = db
            .query_row(
                "SELECT result,created FROM request_results WHERE id=?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("new reply");
        assert_eq!(saved.1, CUTOVER);
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("two scopes"),
            2
        );
        let (status, next): (OutboxStatus, Option<i64>) = db
            .query_row("SELECT status,next_at FROM runtime_outbox", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("pending inbound retained");
        assert_eq!(status, OutboxStatus::Unknown);
        assert_eq!(next, None);
        let raw: String = db
            .query_row("SELECT record FROM runtime_access_outbound", [], |r| {
                r.get(0)
            })
            .expect("outbound owner");
        let value: Value = serde_json::from_str(&raw).expect("record");
        assert_eq!(value["outcome"], "unknown");
        assert_eq!(value["admission"]["created"], CUTOVER);
        migrate(&mut db);
        assert_eq!(
            db.query_row(
                "SELECT result,created FROM request_results WHERE id=?1",
                [&key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            )
            .expect("same reply"),
            saved
        );
        assert!(
            db.execute(
                "UPDATE runtime_access_outbound SET record='{\"outcome\":\"bogus\"}'",
                []
            )
            .is_err()
        );
    }

    #[test]
    fn receipts_remote_crash_between_batches_resumes() {
        let root = tempfile::tempdir().expect("directory");
        let path = root.path().join("canvas.sqlite3");
        {
            let mut db = Connection::open(&path).expect("database");
            fixture(&mut db);
            for n in 0..=MIGRATION_BATCH_ROWS {
                insert(&db, "client", &format!("request-{n}"), "complete");
            }
            assert_eq!(
                batch(&mut db, SOURCES[0], true).cursor,
                MIGRATION_BATCH_ROWS as i64
            );
            batch(&mut db, SOURCES[0], false);
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
    fn receipts_remote_unclassifiable_aborts_without_deletion() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(&db, "client", "good", "complete");
        insert(&db, "client", "bad", "bogus");
        let tx = db.transaction().expect("transaction");
        let checkpoint =
            MigrationCheckpoint::begin(&tx, ID, SOURCES[0], CUTOVER).expect("checkpoint");
        let error = Family
            .move_batch(&tx, SOURCES[0], &checkpoint)
            .expect_err("unclassifiable");
        let key = json!(["access", ["client"], "bad", []]).to_string();
        assert!(
            matches!(error,ReceiptMigrationError::Unclassifiable{table,key:actual,signature} if table==SOURCES[0] && actual==key && signature=="signature")
        );
        tx.rollback().expect("rollback");
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("no partial import"),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_access_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source kept"),
            2
        );
    }

    #[test]
    fn receipts_remote_transport_namespace_cannot_collide_with_public_location_reply() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let public = json!({"requestId":"shared","outcome":"applied","projectId":"project","value":{"serverId":"peer","path":"/original"}});
        let body=json!({"requestId":"shared","action":"add_location","operation":["location"],"payload":{"project":"project","request":{"action":"add_location","project":"project","server":"peer","path":"/original"}}}).to_string();
        let response = json!({"requestId":"shared","outcome":"applied","value":{"path":"/original"},"error":null});
        let signature = crate::sha256_hex(body.as_bytes());
        let tx = db.transaction().expect("transaction");
        import_result(
            &tx,
            "shared",
            "public signature",
            &ResultEnvelope {
                actor: None,
                reply: public.clone(),
                references: vec![],
            },
            CUTOVER,
        )
        .expect("already migrated public C");
        tx.commit().expect("commit");
        db.execute("INSERT INTO runtime_server_outbox(id,fingerprint,state,result,body,server) VALUES('shared',?1,'complete',?2,?3,'peer')",params![signature,response.to_string(),body]).expect("completed transport same raw PK");
        migrate(&mut db);
        let key = crate::multi_server_delivery::remote_operation_key(
            "server-outbox",
            &["peer"],
            "shared",
            &["add_location", "location"],
        );
        let public_saved = crate::request_results::RequestResults::replay(
            &db,
            "shared",
            "public signature",
            CUTOVER + 1,
        )
        .expect("public signature")
        .expect("public reply");
        assert_eq!(
            serde_json::from_str::<Value>(&public_saved.result).expect("envelope")["reply"],
            public
        );
        let saved =
            crate::request_results::RequestResults::replay(&db, &key, &signature, CUTOVER + 1)
                .expect("transport signature")
                .expect("transport reply");
        let saved: ResultEnvelope<SavedRemoteReply> =
            serde_json::from_str(&saved.result).expect("exact transport envelope");
        assert_eq!(saved.reply.body.as_deref(), Some(body.as_str()));
        assert_eq!(saved.reply.server.as_deref(), Some("peer"));
        assert_eq!(saved.reply.response.request_id, "shared");
        migrate(&mut db);
    }

    #[test]
    fn receipts_remote_completed_server_and_federation_results_are_not_sendable() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let response =
            json!({"requestId":"request","outcome":"applied","value":{"ok":true},"error":null});
        db.execute("INSERT INTO runtime_server_outbox(id,fingerprint,state,result,body) VALUES('request','signature','complete',?1,'{\"requestId\":\"request\",\"action\":\"event\",\"actor\":\"agent\"}')",[response.to_string()]).expect("completed server");
        let inbox = json!({"hash":"hash","ack":"pending","created":1.0,"kind":"chat-message","room":"room","receipt":{"protocol":1,"messageId":"message","accepted":true,"room":"room","sequence":1}});
        db.execute(
            "INSERT INTO runtime_federation_inbox VALUES('peer','message',?1)",
            [inbox.to_string()],
        )
        .expect("federation inbox");
        migrate(&mut db);
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_server_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("completed removed"),
            0
        );
        let raw: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [crate::multi_server_delivery::remote_operation_key(
                    "server-outbox",
                    &["server"],
                    "request",
                    &["event"],
                )],
                |r| r.get(0),
            )
            .expect("reply");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("envelope")["reply"]["response"],
            response
        );
        let raw: String = db
            .query_row("SELECT record FROM runtime_federation_inbox", [], |r| {
                r.get(0)
            })
            .expect("slim ACK owner");
        let owner: Value = serde_json::from_str(&raw).expect("owner");
        assert!(owner.get("receipt").is_none());
        assert_eq!(owner["ack"], "pending");
        assert_eq!(owner["hash"], "hash");
    }

    #[test]
    fn receipts_remote_pending_server_keys_and_original_fingerprint_are_preserved() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let body=json!({"requestId":"old-pending","action":"event","actor":"agent","payload":{"text":"original"}}).to_string();
        let expected = crate::sha256_hex(body.as_bytes());
        db.execute("INSERT INTO runtime_server_outbox(id,state,body,server) VALUES('old-pending','running',?1,'peer')",[&body]).expect("pending outgoing");
        db.execute("INSERT INTO runtime_server_inbox(id,signature,state,principal,actor) VALUES('old-incoming','incoming-signature','running','principal','agent')",[]).expect("pending incoming");
        migrate(&mut db);
        let (id, signature, raw, body): (String, String, String, String) = db
            .query_row(
                "SELECT id,fingerprint,admission,body FROM runtime_server_outbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("pending owner");
        assert_eq!(id, "old-pending");
        assert_eq!(signature, expected);
        let token: crate::request_results::AdmittedRequest =
            serde_json::from_str(&raw).expect("admitted");
        assert_eq!(
            token.id(),
            crate::multi_server_delivery::remote_operation_key(
                "server-outbox",
                &["peer"],
                "old-pending",
                &["event"]
            )
        );
        assert_eq!(token.signature(), signature);
        let body: Value = serde_json::from_str(&body).expect("body");
        assert_eq!(body["operation"], json!([]));
        assert_eq!(body["payload"]["text"], "original");
        assert_eq!(
            db.query_row("SELECT id FROM runtime_server_inbox", [], |r| r
                .get::<_, String>(0))
                .expect("incoming"),
            "old-incoming"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("no final result"),
            0
        );
        let tx = db.transaction().expect("transaction");
        assert_eq!(
            MigrationCheckpoint::load(&tx, ID, "normalized-wire:server-outbox-operation")
                .expect("report")
                .expect("normalization count")
                .cursor,
            1
        );
        tx.rollback().expect("read only");
        migrate(&mut db);
        let tx = db.transaction().expect("transaction");
        assert_eq!(
            MigrationCheckpoint::load(&tx, ID, "normalized-wire:server-outbox-operation")
                .expect("report")
                .expect("stable count")
                .cursor,
            1
        );
        tx.rollback().expect("read only");
    }

    #[test]
    fn receipts_remote_completed_without_reply_counts_each_family_once() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(&db, "client", "expired", "tombstone");
        db.execute("INSERT INTO runtime_access_outbound VALUES('server','redacted','hash','{\"outcome\":\"redacted\",\"completed\":1.0}')",[]).expect("redacted reply");
        let delivered = json!({"id":"2f4bf964-ef79-5f3e-936c-f839d664e5f2","kind":"chat-message","status":"delivered"});
        db.execute(
            "INSERT INTO runtime_federation_outbox VALUES(?1,'peer',?2)",
            params![delivered["id"].as_str(), delivered.to_string()],
        )
        .expect("delivered");
        migrate(&mut db);
        migrate(&mut db);
        let tx = db.transaction().expect("transaction");
        for family in ["access-inbound", "access-outbound", "federation-outbox"] {
            assert_eq!(
                MigrationCheckpoint::load(&tx, ID, &format!("deleted-completed-c:{family}"))
                    .expect("report")
                    .expect("family count")
                    .cursor,
                1
            );
        }
        assert_eq!(
            tx.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("no synthetic marker replay"),
            0
        );
    }

    #[test]
    fn receipts_remote_pending_location_without_checked_admission_is_not_authorized() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        let body =
            json!({"requestId":"location","action":"add_location","payload":{"project":"project"}})
                .to_string();
        db.execute(
            "INSERT INTO runtime_server_outbox(id,state,body) VALUES('location','running',?1)",
            [body],
        )
        .expect("old pending");
        let tx = db.transaction().expect("transaction");
        let checkpoint = MigrationCheckpoint::begin(&tx, ID, "runtime_server_outbox", CUTOVER)
            .expect("checkpoint");
        assert!(
            matches!(Family.move_batch(&tx,"runtime_server_outbox",&checkpoint),Err(ReceiptMigrationError::Unclassifiable{table,key,..}) if table=="runtime_server_outbox" && key=="location")
        );
        tx.rollback().expect("preserve uncertain pending source");
    }

    #[test]
    fn receipts_remote_queued_federation_uses_original_content_signature_and_domain_id() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE runtime_federation_identity(id TEXT PRIMARY KEY,record TEXT);INSERT INTO runtime_federation_identity VALUES('local','{\"stateId\":\"sender\"}');").expect("public federation identity");
        let id = "2f4bf964-ef79-5f3e-936c-f839d664e5f2";
        let old = json!({"id":id,"kind":"chat-message","room":"room","payload":{"text":"original"},"created":1.0,"status":"queued","attempts":0,"nextAt":0.0,"error":null});
        db.execute(
            "INSERT INTO runtime_federation_outbox VALUES(?1,'peer',?2)",
            params![id, old.to_string()],
        )
        .expect("queued source");
        migrate(&mut db);
        let raw: String = db
            .query_row("SELECT record FROM runtime_federation_outbox", [], |r| {
                r.get(0)
            })
            .expect("typed queued owner");
        let owner: Value = serde_json::from_str(&raw).expect("record");
        let token: crate::request_results::AdmittedRequest =
            serde_json::from_value(owner["admission"].clone()).expect("token");
        let expected=federation_envelope_signature(&json!({"protocol":SOURCE_FEDERATION_PROTOCOL_VERSION,"senderServer":"sender","id":id,"kind":"chat-message","room":"room","payload":{"text":"original"},"created":1.0})).expect("shared content signature");
        assert_eq!(token.signature(), expected);
        assert_eq!(owner["requestId"], id);
        assert_eq!(owner["id"], id);
        assert_eq!(owner["status"], "queued");
        assert!(
            studio_runtime_schema::validate("PersistedFederationOutbox", &owner).expect("schema")
        );
        migrate(&mut db);
    }
}
