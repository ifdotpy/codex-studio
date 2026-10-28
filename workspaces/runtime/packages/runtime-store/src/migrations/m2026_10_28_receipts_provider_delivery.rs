//! Consolidate retained provider calls before journal cursor evidence is removed.
use crate::receipt_migration_contract::{
    BatchBudget, MIGRATION_BATCH_ROWS, MigrationCheckpoint, ReceiptFamilyMigration,
    ReceiptMigrationError, Result, SourceReceipt, admitted_owner_token, delete_source,
    import_result, read_receipt_batch, unclassifiable,
};
use crate::request_results::AdmittedRequest;
use crate::{
    outbox::{Outbox, OutboxRow},
    request_results::ResultEnvelope,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_providers::Method;
use studio_runtime_schema::persistence::{
    MigrationStatus, OutboxKind, OutboxStatus, OwnerKind, RequestIdentity,
};
use studio_runtime_schema::provider_call_contract::{
    CommittedAck, ProviderCallIntent, RequestResult as Outcome, RetainedCall,
    RetainedOutcome as Resolution, RetainedReply, TransportRequest, transport_key,
};

pub(super) struct Family;
pub(super) const SOURCES: &[&str] = &["runtime_operation_receipts"];
const SELECT: &str = "source.id GLOB 'account-worker:retained:*'";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submitted {
    call: Value,
    #[serde(default, deserialize_with = "present_request")]
    request: Option<Value>,
}
fn present_request<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

fn fail(row: &SourceReceipt) -> ReceiptMigrationError {
    unclassifiable(SOURCES[0], &row.id, &row.signature)
}
fn sibling(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    base: &str,
    phase: &str,
) -> Result<Option<(i64, Value)>> {
    let id = format!("{base}:{phase}");
    let saved: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT rowid,signature,result FROM runtime_operation_receipts WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    saved
        .map(|(rowid, signature, raw)| {
            if signature != phase {
                return Err(fail(row));
            }
            Ok((rowid, serde_json::from_str(&raw).map_err(|_| fail(row))?))
        })
        .transpose()
}

fn migrate(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let (base, phase) = row.id.rsplit_once(':').ok_or_else(|| fail(row))?;
    if !matches!(phase, "submitted" | "written" | "result") || row.signature != phase {
        return Err(fail(row));
    }
    let Some(submitted) = sibling(tx, row, base, "submitted")? else {
        // A preceding bounded batch already copied the whole operation. A sibling
        // can be removed only after checking its exact bytes against that proof.
        let key = base
            .strip_prefix("account-worker:")
            .ok_or_else(|| fail(row))?;
        let outbox = Outbox::get::<Value>(tx, key).map_err(|_| fail(row))?;
        let result: Option<String> = tx
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [format!("{base}:result")],
                |r| r.get(0),
            )
            .optional()?;
        let proof = if let Some(row) = outbox {
            row.payload["payload"].clone()
        } else if let Some(raw) = result {
            serde_json::from_str::<Value>(&raw).map_err(|_| fail(row))?["reply"].clone()
        } else {
            return Err(fail(row));
        };
        let value: Value = row.json(SOURCES[0])?;
        let valid = match phase {
            "written" => proof["written"] == true && proof["request"] == value,
            "result" => {
                (proof["outcome"]["result"] == value["result"]
                    && proof["outcome"]["cursor"] == value["cursor"]
                    && (value["committed_ack"].is_null()
                        || proof["outcome"]["committed_ack"] == value["committed_ack"]))
                    || (proof["result"] == value["result"]
                        && proof["ack"]["cursor"] == value["cursor"])
            }
            _ => false,
        };
        return if valid { Ok(()) } else { Err(fail(row)) };
    };
    let Submitted { mut call, request } =
        serde_json::from_value::<Submitted>(submitted.1).map_err(|_| fail(row))?;
    let account = call["scope"]["account"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail(row))?
        .to_owned();
    let generation = call["scope"]["generation"]
        .as_u64()
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or_else(|| fail(row))?;
    let operation = call["operation"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail(row))?
        .to_owned();
    let key = transport_key(&account, &operation);
    if base != format!("account-worker:{key}")
        || request.as_ref().is_some_and(|request| {
            request["generation"].as_u64() != Some(generation)
                || request["operation"] != key
                || request["id"] != key
                || request["method"] != call["method"]
                || request["lightweight"].as_bool().is_none()
                || request["deadline"].as_u64().is_none()
        })
        || !call["params"].is_object()
    {
        return Err(fail(row));
    }
    let actor = call["actor"]["agent"].as_str().map(str::to_owned);
    let epoch = call["actor"]["epoch"].as_i64();
    if actor.is_some() != epoch.is_some() {
        return Err(fail(row));
    }
    let source_root = call
        .get("root_request_id")
        .map(|value| value.as_str().map(str::to_owned).ok_or_else(|| fail(row)))
        .transpose()?;
    if let Some(root) = &source_root
        && RequestIdentity::new("provider-call", &[], root, &[]).is_err()
    {
        return Err(fail(row));
    }
    call["root_request_id"] = json!(source_root.as_deref().unwrap_or_default());
    call["admitted_request"] = Value::Null;
    let mut typed_call: ProviderCallIntent<Method, AdmittedRequest> =
        serde_json::from_value(call.clone()).map_err(|_| fail(row))?;
    let signature = typed_call.fingerprint(&account, typed_call.method.as_str());
    let result_id = format!("{base}:result");
    let admission = admitted_owner_token(&result_id, &signature, cutover);
    let previous = Outbox::get::<Value>(tx, &key).map_err(|_| fail(row))?;
    let root = if let Some(previous) = previous {
        if previous.signature != signature || previous.kind != OutboxKind::ProviderCall {
            return Err(fail(row));
        }
        previous.payload["payload"]["call"]["root_request_id"]
            .as_str()
            .ok_or_else(|| fail(row))?
            .to_owned()
    } else if let Some((saved_signature, raw)) = tx
        .query_row(
            "SELECT signature,result FROM request_results WHERE id=?1",
            [&result_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if saved_signature != signature {
            return Err(fail(row));
        }
        serde_json::from_str::<Value>(&raw).map_err(|_|fail(row))?["reply"]["call"]["root_request_id"].as_str().ok_or_else(||fail(row))?.to_owned()
    } else {
        source_root
            .clone()
            .unwrap_or_else(RequestIdentity::issue_root)
    };
    if source_root.as_ref().is_some_and(|source| source != &root) {
        return Err(fail(row));
    }
    if RequestIdentity::new("provider-call", &[], &root, &[]).is_err() {
        return Err(fail(row));
    }
    if let Some(monitor) = operation.strip_prefix("monitor:") {
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='runtime_monitors' AND type='table')",[],|r|r.get(0))?;
        if exists {
            tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.operation.providerRequest',json(?2),'$.operation.rootRequestId',?3) WHERE id=?1 AND json_type(record,'$.operation')='object'",params![monitor,serde_json::to_string(&admission).map_err(|_|fail(row))?,root])?;
        }
    }
    if let Some(transfer) = call["transfer"].as_object() {
        let id = transfer
            .get("transfer_id")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(row))?;
        let member = transfer
            .get("member")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(row))?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_account_transfers WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let mut value: Value =
            serde_json::from_str(&raw.ok_or_else(|| fail(row))?).map_err(|_| fail(row))?;
        let member = value["members"].get_mut(member).ok_or_else(|| fail(row))?;
        let authority = json!({"rootRequestId":root,"admittedRequest":admission});
        if member["providerIntent"]["operation"] == operation
            && member["providerIntent"]["method"] == call["method"]
            && member["providerIntent"]["params"] == call["params"]
        {
            member["providerAdmission"] = authority;
        } else if member["providerReadIntents"][&operation]["method"] == call["method"]
            && member["providerReadIntents"][&operation]["params"] == call["params"]
        {
            if member["providerReadAdmissions"].is_null() {
                member["providerReadAdmissions"] = json!({});
            }
            member["providerReadAdmissions"][&operation] = authority;
        } else {
            return Err(fail(row));
        }
        tx.execute(
            "UPDATE runtime_account_transfers SET record=?2 WHERE id=?1",
            params![id, value.to_string()],
        )?;
    }
    // The imported call carries its final token; no runtime path guesses an old key.
    call["admitted_request"] = serde_json::to_value(&admission).map_err(|_| fail(row))?;
    call["root_request_id"] = json!(root);
    typed_call.admitted_request = Some(admission.clone());
    typed_call.root_request_id = root;
    let written = sibling(tx, row, base, "written")?;
    let saved = sibling(tx, row, base, "result")?;
    let typed_transport: Option<TransportRequest<Method>> = match request {
        Some(request) => {
            let transport = written.as_ref().map_or(&request, |(_, value)| value);
            if transport["id"] != request["id"]
                || transport["operation"] != request["operation"]
                || transport["generation"] != request["generation"]
            {
                return Err(fail(row));
            }
            Some(serde_json::from_value(transport.clone()).map_err(|_| fail(row))?)
        }
        None if written.is_none() && saved.is_none() => None,
        None => return Err(fail(row)),
    };
    let mut outcome = saved
        .as_ref()
        .map(|(_, value)| {
            serde_json::from_value::<Resolution>(value.clone()).map_err(|_| fail(row))
        })
        .transpose()?;
    let vm = if let Some(agent) = &actor {
        let record: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [agent],
                |r| r.get(0),
            )
            .optional()?;
        record
            .map(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| fail(row)))
            .transpose()?
            .is_some_and(|record| record["executionMode"] == "vm")
    } else {
        false
    };
    if !vm {
        tx.execute("INSERT INTO runtime_account_state(account,next_generation,journal_ack,recovery_required,streams) VALUES(?1,?2,0,0,'null') ON CONFLICT(account) DO UPDATE SET next_generation=max(next_generation,excluded.next_generation)",params![account,generation as i64])?;
    }
    let stream = if vm {
        format!(
            "{account}:vm:{}",
            actor.as_deref().ok_or_else(|| fail(row))?
        )
    } else {
        account.clone()
    };
    let ack: Option<String> = tx
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [format!("account-worker:cursor:{stream}:{generation}:ack")],
            |r| r.get(0),
        )
        .optional()?;
    let ack = ack
        .map(|raw| serde_json::from_str::<u64>(&raw).map_err(|_| fail(row)))
        .transpose()?
        .unwrap_or(0);
    let committed = if let Some(outcome) = &outcome {
        let journal: Option<String> = tx
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1 OR id=?2",
                params![
                    format!(
                        "account-worker:journal:{stream}:{generation}:{:020}:settled",
                        outcome.cursor
                    ),
                    format!(
                        "account-worker:journal:{stream}:{generation}:{}:settled",
                        outcome.cursor
                    )
                ],
                |r| r.get(0),
            )
            .optional()?;
        let journal = journal
            .map(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| fail(row)))
            .transpose()?;
        match &outcome.committed_ack {
            Some(proof) => proof.matches(&stream, generation, outcome.cursor),
            None => {
                outcome.cursor <= ack
                    || journal.is_some_and(|value| {
                        value["committed"] == true
                            && value["cursor"].as_u64() == Some(outcome.cursor)
                    })
            }
        }
    } else {
        false
    };
    if committed && let Some(outcome) = outcome.as_mut() {
        outcome.committed_ack = Some(CommittedAck {
            stream_id: stream.clone(),
            generation,
            cursor: outcome.cursor,
        });
    }
    if committed
        && let Some(Resolution {
            result: result @ (Outcome::Reply(_) | Outcome::Rejected(_)),
            cursor,
            committed_ack: _,
        }) = &outcome
    {
        let reply = RetainedReply {
            call: typed_call,
            request: typed_transport.ok_or_else(|| fail(row))?,
            written: written.is_some(),
            result: result.clone(),
            ack: CommittedAck {
                stream_id: stream,
                generation,
                cursor: *cursor,
            },
        };
        import_result(
            tx,
            &result_id,
            &signature,
            &ResultEnvelope {
                actor: actor.clone(),
                reply,
                references: vec![],
            },
            cutover,
        )?;
        if call["method"] == "thread/start"
            && let Outcome::Reply(value) = result
            && let Some(thread) = value["thread"]["id"].as_str()
            && let Some(agent) = &actor
        {
            let raw: Option<String> = tx
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [agent],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(raw) = raw {
                let mut record: Value = serde_json::from_str(&raw).map_err(|_| fail(row))?;
                if record["threadProvenance"].is_null() {
                    record["threadProvenance"] = json!({});
                }
                let proof =
                    json!({"scope":call["scope"],"actor":call["actor"],"operation":operation});
                if record["threadProvenance"]
                    .get(thread)
                    .is_some_and(|old| old != &proof)
                {
                    return Err(fail(row));
                }
                record["threadProvenance"][thread] = proof;
                tx.execute(
                    "UPDATE runtime_agents SET record=?2 WHERE id=?1",
                    params![agent, record.to_string()],
                )?;
            }
        }
    } else {
        let status =
            if committed && matches!(outcome.as_ref().map(|v| &v.result), Some(Outcome::NotSent)) {
                OutboxStatus::Pending
            } else {
                OutboxStatus::Unknown
            };
        let retained = RetainedCall {
            call: typed_call,
            request: typed_transport,
            admitted: admission,
            written: written.is_some(),
            outcome,
        };
        let payload = json!({"family":"retained","payload":retained});
        Outbox::put(
            tx,
            &OutboxRow {
                id: key,
                kind: OutboxKind::ProviderCall,
                owner_kind: if actor.is_some() {
                    OwnerKind::Agent
                } else {
                    OwnerKind::Account
                },
                owner_id: actor.unwrap_or_else(|| account.clone()),
                request_id: Some(result_id),
                signature,
                status,
                account: Some(account),
                stream_id: Some(stream),
                generation: Some(generation as i64),
                ordinal: None,
                next_at: None,
                payload,
                created: cutover,
            },
        )
        .map_err(|_| fail(row))?;
    }
    Ok(())
}

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-provider-delivery"
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
        if source != SOURCES[0] {
            return Err(ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: source.into(),
            });
        }
        let rows = read_receipt_batch(tx, source, checkpoint, SELECT)?;
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        let mut budget = BatchBudget::default();
        for row in rows {
            let base = row
                .id
                .rsplit_once(':')
                .map(|(base, _)| base)
                .ok_or_else(|| fail(&row))?;
            let (count,bytes):(usize,usize)=tx.query_row("SELECT count(*),coalesce(sum(length(CAST(id AS BLOB))+length(CAST(signature AS BLOB))+length(CAST(result AS BLOB))),0) FROM runtime_operation_receipts WHERE id IN (?1,?2,?3)",params![format!("{base}:submitted"),format!("{base}:written"),format!("{base}:result")],|r|Ok((r.get::<_,i64>(0)? as usize,r.get::<_,i64>(1)? as usize)))?;
            if budget.rows + count > MIGRATION_BATCH_ROWS || !budget.include(bytes) {
                break;
            }
            for _ in 1..count {
                if !budget.include(0) {
                    return Err(fail(&row));
                }
            }
            migrate(tx, &row, checkpoint.cutover_ms)?;
            delete_source(tx, source, row.rowid)?;
            next.cursor = row.rowid;
        }
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        if !crate::receipt_migration_contract::source_exists(tx, SOURCES[0])? {
            return Ok(());
        }
        if tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts source WHERE {SELECT})"
            ),
            [],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: SOURCES[0].into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> rusqlite::Connection {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("transaction");
        crate::receipt_migration_contract::prepare_schema(&tx).expect("schema");
        tx.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL)").expect("sources");
        tx.execute(
            "INSERT INTO runtime_agents VALUES('agent',?1)",
            [json!({"id":"agent","epoch":3}).to_string()],
        )
        .expect("agent");
        tx.commit().expect("commit");
        db
    }
    fn fixture(tx: &Transaction<'_>, operation: &str, outcome: &Value) -> String {
        let key = format!(
            "retained:default:{}",
            crate::sha256_hex(operation.as_bytes())
        );
        let call = json!({"scope":{"account":"default","connection":"old","generation":7},"operation":operation,"actor":{"agent":"agent","epoch":3},"transfer":null,"deleted_actor_session":null,"method":"thread/start","params":{}});
        let request = json!({"id":key,"generation":7,"method":"thread/start","operation":key,"thread":null,"lightweight":false,"deadline":100,"unknown":true,"remoteId":null});
        for (phase, value) in [
            ("submitted", json!({"call":call,"request":request})),
            ("written", request),
            ("result", json!({"result":outcome,"cursor":12})),
        ] {
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                params![
                    format!("account-worker:{key}:{phase}"),
                    phase,
                    value.to_string()
                ],
            )
            .expect("phase");
        }
        key
    }
    #[test]
    fn receipt_provider_delivery_committed_proof_survives_cursor_deletion_and_reopen() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let key = fixture(
            &tx,
            "thread-create",
            &json!({"Reply":{"thread":{"id":"thread"}}}),
        );
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:cursor:default:7:ack','ack','12')",[]).expect("ack");
        let mut cp =
            MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("checkpoint");
        cp = Family.move_batch(&tx, SOURCES[0], &cp).expect("move");
        cp.save(&tx, Family.id(), SOURCES[0], 1000).expect("save");
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id='account-worker:cursor:default:7:ack'",
            [],
        )
        .expect("cursor removed after import");
        let result: String = tx
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [format!("account-worker:{key}:result")],
                |r| r.get(0),
            )
            .expect("result");
        let result: Value = serde_json::from_str(&result).expect("envelope");
        assert_eq!(result["reply"]["ack"]["cursor"], 12);
        assert_eq!(result["reply"]["call"]["scope"]["connection"], "old");
        let root = result["reply"]["call"]["root_request_id"]
            .as_str()
            .expect("root");
        assert_eq!(
            uuid::Uuid::parse_str(root).expect("UUID").get_version_num(),
            7
        );
        let owner: String = tx
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |r| r.get(0),
            )
            .expect("agent");
        assert_eq!(
            serde_json::from_str::<Value>(&owner).expect("record")["threadProvenance"]["thread"]["actor"]
                ["epoch"],
            3
        );
        assert_eq!(
            Family
                .move_batch(&tx, SOURCES[0], &cp)
                .expect("second open")
                .status,
            MigrationStatus::Complete
        );
        assert!(Outbox::get::<Value>(&tx, &key).expect("outbox").is_none());
    }
    #[test]
    fn receipt_provider_delivery_unknown_and_unacknowledged_not_sent_stay_unknown() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let a = fixture(&tx, "unknown", &json!("Unknown"));
        let b = fixture(&tx, "not-sent", &json!("NotSent"));
        let cp =
            MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("checkpoint");
        Family.move_batch(&tx, SOURCES[0], &cp).expect("move");
        for key in [a, b] {
            let row = Outbox::get::<Value>(&tx, &key)
                .expect("outbox")
                .expect("held");
            assert_eq!(row.status, OutboxStatus::Unknown);
            assert_eq!(row.next_at, None);
        }
        assert_eq!(
            tx.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("results"),
            0
        );
        let plan:String=tx.query_row("EXPLAIN QUERY PLAN SELECT id FROM runtime_outbox WHERE account='default' AND kind='provider_call' AND status='unknown' ORDER BY ordinal,id LIMIT 10",[],|r|r.get(3)).expect("plan");
        assert!(plan.contains("runtime_outbox_account"));
        assert!(!plan.contains("request_results"));
    }
    #[test]
    fn receipt_provider_delivery_unknown_keeps_exact_committed_ack_after_cursor_removal() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let key = fixture(&tx, "unknown-committed", &json!("Unknown"));
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:cursor:default:7:ack','ack','19')",[]).expect("ack");
        let cp =
            MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("checkpoint");
        Family.move_batch(&tx, SOURCES[0], &cp).expect("move");
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id='account-worker:cursor:default:7:ack'",
            [],
        )
        .expect("cursor removed");
        let row = Outbox::get::<Value>(&tx, &key)
            .expect("lookup")
            .expect("held");
        assert_eq!(row.status, OutboxStatus::Unknown);
        assert_eq!(row.next_at, None);
        assert_eq!(
            row.payload["payload"]["outcome"]["committed_ack"],
            json!({"stream_id":"default","generation":7,"cursor":12})
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("no fabricated result"),
            0
        );
    }
    #[test]
    fn receipt_provider_delivery_mismatched_ack_stays_unknown() {
        for proof in [
            json!({"stream_id":"other","generation":7,"cursor":12}),
            json!({"stream_id":"default","generation":8,"cursor":12}),
            json!({"stream_id":"default","generation":7,"cursor":11}),
        ] {
            let mut db = database();
            let tx = db.transaction().expect("transaction");
            let key = fixture(&tx, "mismatched-ack", &json!("NotSent"));
            tx.execute("UPDATE runtime_operation_receipts SET result=json_set(result,'$.committed_ack',json(?2)) WHERE id=?1", (format!("account-worker:{key}:result"), proof.to_string())).expect("mismatched proof");
            tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:cursor:default:7:ack','ack','19')", []).expect("later checkpoint");
            let cp =
                MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("checkpoint");
            Family
                .move_batch(&tx, SOURCES[0], &cp)
                .expect("preserve held result");
            let row = Outbox::get::<Value>(&tx, &key)
                .expect("lookup")
                .expect("held");
            assert_eq!(row.status, OutboxStatus::Unknown);
            assert_eq!(row.next_at, None);
            assert_eq!(row.payload["payload"]["outcome"]["cursor"], 12);
            assert_eq!(row.payload["payload"]["outcome"]["committed_ack"], proof);
        }
    }
    #[test]
    fn receipt_provider_delivery_not_sent_keeps_original_cursor_and_exact_proof() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let key = fixture(&tx, "not-sent-committed", &json!("NotSent"));
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:cursor:default:7:ack','ack','19')",[]).expect("ack");
        let cp =
            MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("checkpoint");
        Family.move_batch(&tx, SOURCES[0], &cp).expect("move");
        let row = Outbox::get::<Value>(&tx, &key)
            .expect("lookup")
            .expect("pending retry");
        assert_eq!(row.status, OutboxStatus::Pending);
        assert_eq!(row.next_at, None);
        assert_eq!(row.payload["payload"]["outcome"]["cursor"], 12);
        assert_eq!(
            row.payload["payload"]["outcome"]["committed_ack"],
            json!({"stream_id":"default","generation":7,"cursor":12})
        );
    }
    #[test]
    fn receipt_provider_delivery_migration_root_is_v7_and_stable_across_phase_resume() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        tx.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);INSERT INTO runtime_monitors VALUES('monitor','{\"operation\":{}}')").expect("monitor");
        let key = fixture(&tx, "monitor:monitor", &json!("Unknown"));
        let source = read_receipt_batch(
            &tx,
            SOURCES[0],
            &MigrationCheckpoint {
                cursor: 0,
                status: MigrationStatus::Running,
                cutover_ms: 1000,
            },
            SELECT,
        )
        .expect("source")
        .into_iter()
        .next()
        .expect("submitted");
        migrate(&tx, &source, 1000).expect("import one phase");
        delete_source(&tx, SOURCES[0], source.rowid).expect("consume submitted");
        let before = Outbox::get::<Value>(&tx, &key)
            .expect("lookup")
            .expect("call");
        let root = before.payload["payload"]["call"]["root_request_id"]
            .as_str()
            .expect("root");
        assert_eq!(
            uuid::Uuid::parse_str(root).expect("UUID").get_version_num(),
            7
        );
        let cp =
            MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("checkpoint");
        Family
            .move_batch(&tx, SOURCES[0], &cp)
            .expect("remaining phases");
        let after = Outbox::get::<Value>(&tx, &key)
            .expect("lookup")
            .expect("call");
        assert_eq!(after, before);
        let record: String = tx
            .query_row(
                "SELECT record FROM runtime_monitors WHERE id='monitor'",
                [],
                |r| r.get(0),
            )
            .expect("monitor");
        let record: Value = serde_json::from_str(&record).expect("record");
        assert_eq!(record["operation"]["rootRequestId"], root);
        assert_eq!(
            record["operation"]["providerRequest"],
            before.payload["payload"]["admitted"]
        );
    }
    #[test]
    fn receipt_provider_delivery_preserves_hydrated_source_root() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let key = fixture(&tx, "source-root", &json!("Unknown"));
        let root = RequestIdentity::issue_root();
        tx.execute("UPDATE runtime_operation_receipts SET result=json_set(result,'$.call.root_request_id',?1) WHERE signature='submitted'",[&root]).expect("owner hydration");
        let cp = MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("begin");
        Family.move_batch(&tx, SOURCES[0], &cp).expect("move");
        let call = Outbox::get::<Value>(&tx, &key)
            .expect("lookup")
            .expect("call");
        assert_eq!(call.payload["payload"]["call"]["root_request_id"], root);
    }
    #[test]
    fn receipt_provider_delivery_invalid_source_root_aborts_without_source_delete() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        fixture(&tx, "bad-root", &json!("Unknown"));
        tx.execute("UPDATE runtime_operation_receipts SET result=json_set(result,'$.call.root_request_id','invalid-root') WHERE signature='submitted'",[]).expect("invalid root");
        let cp = MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("begin");
        assert!(matches!(
            Family.move_batch(&tx, SOURCES[0], &cp),
            Err(ReceiptMigrationError::Unclassifiable { .. })
        ));
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source"),
            3
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("destination"),
            0
        );
    }
    #[test]
    fn receipt_provider_delivery_crash_between_bounded_phase_groups_is_atomic() {
        let mut db = database();
        {
            let tx = db.transaction().expect("transaction");
            for index in 0..(MIGRATION_BATCH_ROWS / 3 + 2) {
                fixture(&tx, &format!("operation-{index}"), &json!("Unknown"));
            }
            tx.commit().expect("fixtures");
        }
        let source_count = db
            .query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| {
                r.get::<_, i64>(0)
            })
            .expect("source count");
        {
            let tx = db.transaction().expect("transaction");
            let cp = MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("begin");
            let next = Family.move_batch(&tx, SOURCES[0], &cp).expect("batch");
            next.save(&tx, Family.id(), SOURCES[0], 1000)
                .expect("checkpoint");
            tx.commit().expect("commit");
        }
        let remaining = db
            .query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| {
                r.get::<_, i64>(0)
            })
            .expect("remaining");
        assert!(remaining > 0);
        assert!(source_count - remaining <= MIGRATION_BATCH_ROWS as i64);
        let held = db
            .query_row("SELECT count(*) FROM runtime_outbox", [], |r| {
                r.get::<_, i64>(0)
            })
            .expect("held");
        {
            let tx = db.transaction().expect("transaction");
            let cp = MigrationCheckpoint::load(&tx, Family.id(), SOURCES[0])
                .expect("load")
                .expect("checkpoint");
            let next = Family.move_batch(&tx, SOURCES[0], &cp).expect("second");
            next.save(&tx, Family.id(), SOURCES[0], 1000)
                .expect("checkpoint");
            tx.rollback().expect("crash");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("rollback"),
            remaining
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("rollback"),
            held
        );
        let tx = db.transaction().expect("transaction");
        let mut cp = MigrationCheckpoint::load(&tx, Family.id(), SOURCES[0])
            .expect("load")
            .expect("checkpoint");
        while cp.status != MigrationStatus::Complete {
            cp = Family.move_batch(&tx, SOURCES[0], &cp).expect("resume");
            cp.save(&tx, Family.id(), SOURCES[0], 1000)
                .expect("checkpoint");
        }
        Family.finish(&tx).expect("gate");
    }
}
