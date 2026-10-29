//! Move VM launch and authentication obligations without inferring a transport outcome.
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, Result, SourceReceipt,
    delete_source, import_result, read_receipt_batch, unclassifiable,
};
use crate::{
    outbox::{Outbox, OutboxRow},
    request_results::ResultEnvelope,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    MigrationStatus, OutboxKind, OutboxStatus, OwnerKind, WorkEffectStatus,
};

pub(super) struct Family;
pub(super) const SOURCES: &[&str] = &["runtime_operation_receipts"];
const SELECT: &str = "source.id GLOB 'vm-to-host:*' OR source.id GLOB 'linux-vm-provider-start:*' OR source.id GLOB 'linux-vm-credential-copy:*' OR source.id GLOB 'account-worker:vm-auth-bootstrap:*'";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LaunchIdentity {
    agent_id: String,
    epoch: i64,
    handle: String,
    request_id: String,
    launch_fingerprint: String,
}
#[derive(Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum OldLaunch {
    Reserved {
        identity: LaunchIdentity,
    },
    NotSent {
        identity: LaunchIdentity,
    },
    Started {
        identity: LaunchIdentity,
        info: Value,
    },
}

fn fail(row: &SourceReceipt) -> ReceiptMigrationError {
    unclassifiable(SOURCES[0], &row.id, &row.signature)
}

fn hold(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    request: &str,
    owner: (OwnerKind, &str),
    field: &str,
    payload: Value,
    cutover: i64,
) -> Result<()> {
    let (owner, owner_id) = owner;
    let status = if owner == OwnerKind::Vm {
        OutboxStatus::Submitted
    } else {
        OutboxStatus::Unknown
    };
    Outbox::put(
        tx,
        &OutboxRow {
            id: Outbox::effect_id(request, OutboxKind::ProviderCall, owner, owner_id, &[field]),
            kind: OutboxKind::ProviderCall,
            owner_kind: owner,
            owner_id: owner_id.into(),
            request_id: Some(request.into()),
            signature: row.signature.clone(),
            status,
            account: None,
            stream_id: None,
            generation: None,
            ordinal: None,
            next_at: None,
            payload,
            created: cutover,
        },
    )
    .map_err(|_| fail(row))
}

fn owner(
    tx: &Transaction<'_>,
    agent: &str,
    epoch: i64,
    field: &str,
    state: &Value,
) -> Result<bool> {
    // Update only the selected incarnation. Old uncertain incarnations stay held.
    Ok(tx.execute("UPDATE runtime_agents SET record=json_set(record,?3,json(?4)) WHERE id=?1 AND json_extract(record,'$.epoch')=?2 AND json_extract(record,'$.executionMode')='vm'",params![agent,epoch,format!("$.{field}"),state.to_string()])?!=0)
}

fn migrate(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    if let Some(root) = row.id.strip_prefix("vm-to-host:") {
        let reply: studio_runtime_schema::persistence::VmToHostReply = row.json(SOURCES[0])?;
        let expected = crate::sha256_hex(
            crate::python_json(&json!({"operation":"vm-to-host","id":reply.id})).as_bytes(),
        );
        if root.is_empty()
            || reply.id.is_empty()
            || reply.execution_mode != "native"
            || reply.workspace_mode != "shared"
            || row.signature != expected
        {
            return Err(fail(row));
        }
        return import_result(
            tx,
            &row.id,
            &row.signature,
            &ResultEnvelope {
                actor: Some("owner-api".into()),
                reply,
                references: Vec::new(),
            },
            cutover,
        );
    }
    if let Some(hash) = row.id.strip_prefix("linux-vm-credential-copy:") {
        if hash.len() != 48
            || row.signature.len() != 64
            || !row.signature.starts_with(hash)
            || !row.signature.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(fail(row));
        }
        return match row.result.as_str() {
            "complete" => import_result(
                tx,
                &row.id,
                &row.signature,
                &ResultEnvelope {
                    actor: None,
                    reply: "complete",
                    references: vec![],
                },
                cutover,
            ),
            "pending" => hold(
                tx,
                row,
                &row.id,
                (OwnerKind::Vm, &row.id),
                "credentialCopy",
                json!({"credentialCopy":{"requestId":row.id,"signature":row.signature,"status":WorkEffectStatus::Submitted}}),
                cutover,
            ),
            _ => Err(fail(row)),
        };
    }
    if row.id.starts_with("linux-vm-provider-start:") {
        let (identity, status, info) = match row.json::<OldLaunch>(SOURCES[0])? {
            OldLaunch::Reserved { identity } => (identity, WorkEffectStatus::Submitted, None),
            OldLaunch::NotSent { identity } => (identity, WorkEffectStatus::NotSent, None),
            OldLaunch::Started { identity, info } => {
                (identity, WorkEffectStatus::Complete, Some(info))
            }
        };
        if identity.agent_id.is_empty()
            || identity.epoch < 0
            || identity.request_id.is_empty()
            || identity.launch_fingerprint.len() != 64
            || row.id
                != format!(
                    "linux-vm-provider-start:{}:{}",
                    identity.agent_id, identity.epoch
                )
            || row.signature
                != format!(
                    "agent={};epoch={};launch={}",
                    identity.agent_id, identity.epoch, identity.launch_fingerprint
                )
        {
            return Err(fail(row));
        }
        let state =
            json!({"identity":identity,"signature":row.signature,"status":status,"info":info});
        if owner(tx, &identity.agent_id, identity.epoch, "vmLaunch", &state)? {
            return Ok(());
        }
        return if status == WorkEffectStatus::Complete {
            import_result(
                tx,
                &row.id,
                &row.signature,
                &ResultEnvelope {
                    actor: Some(identity.agent_id),
                    reply: state,
                    references: vec![],
                },
                cutover,
            )
        } else {
            hold(
                tx,
                row,
                &identity.request_id,
                (OwnerKind::Agent, &identity.agent_id),
                "vmLaunch",
                json!({"vmLaunch":state}),
                cutover,
            )
        };
    }
    let suffix = row
        .id
        .strip_prefix("account-worker:vm-auth-bootstrap:")
        .ok_or_else(|| fail(row))?;
    let (tuple, phase) = suffix.rsplit_once(':').ok_or_else(|| fail(row))?;
    if !matches!(phase, "submitted" | "complete") || row.signature != phase {
        return Err(fail(row));
    }
    let (tuple, hash) = tuple.rsplit_once(':').ok_or_else(|| fail(row))?;
    let (agent, epoch) = tuple.rsplit_once(':').ok_or_else(|| fail(row))?;
    let epoch = epoch
        .parse::<i64>()
        .ok()
        .filter(|v| *v >= 0)
        .ok_or_else(|| fail(row))?;
    let value: Value = row.json(SOURCES[0])?;
    let signature = value["signature"]
        .as_str()
        .filter(|s| s.len() == 64 && s.starts_with(hash) && hash.len() == 24)
        .ok_or_else(|| fail(row))?;
    let operation = row
        .id
        .strip_prefix("account-worker:")
        .and_then(|s| s.rsplit_once(':').map(|(id, _)| id))
        .ok_or_else(|| fail(row))?;
    let complete_id = format!("account-worker:{operation}:complete");
    let complete: Option<String> = tx
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [&complete_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(complete) = &complete
        && serde_json::from_str::<Value>(complete).map_err(|_| fail(row))? != value
    {
        return Err(fail(row));
    }
    let retained_complete: Option<String> = tx
        .query_row(
            "SELECT result FROM request_results WHERE id=?1 AND signature='complete'",
            [&complete_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(raw) = &retained_complete
        && serde_json::from_str::<Value>(raw).map_err(|_| fail(row))?["reply"] != value
    {
        return Err(fail(row));
    }
    let status = if phase == "complete" || complete.is_some() || retained_complete.is_some() {
        WorkEffectStatus::Complete
    } else {
        WorkEffectStatus::Submitted
    };
    let owner_record: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent],
            |r| r.get(0),
        )
        .optional()?;
    let owner_record = owner_record
        .map(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| fail(row)))
        .transpose()?;
    let current = owner_record
        .as_ref()
        .is_some_and(|record| record["epoch"].as_i64() == Some(epoch));
    let token =
        crate::receipt_migration_contract::admitted_owner_token(&complete_id, "complete", cutover);
    // Old hashes do not identify which callback/attempt admitted the external
    // effect. A selected incarnation may retry only with that exact source token.
    // Preserve the source and close startup if its authority cannot be proven.
    let proof = owner_record
        .as_ref()
        .and_then(|record| record.get("vmAuthBootstrap"))
        .filter(|value| {
            value["requestId"] == operation
                && value["fingerprint"] == signature
                && value["admittedRequest"]["id"] == complete_id
                && value["admittedRequest"]["signature"] == "complete"
                && value["rootRequestId"]
                    .as_str()
                    .is_some_and(|root| !root.is_empty())
        });
    if (current || status != WorkEffectStatus::Complete) && proof.is_none() {
        return Err(fail(row));
    }
    let state = json!({"agentId":agent,"epoch":epoch,"requestId":operation,"rootRequestId":proof.map(|p|p["rootRequestId"].clone()).unwrap_or(Value::Null),"fingerprint":signature,"state":status,"admittedRequest":token});
    if current {
        owner(tx, agent, epoch, "vmAuthBootstrap", &state)?;
    }
    if status == WorkEffectStatus::Complete {
        import_result(
            tx,
            &complete_id,
            "complete",
            &ResultEnvelope {
                actor: Some(agent.into()),
                reply: value,
                references: vec![],
            },
            cutover,
        )
    } else {
        hold(
            tx,
            row,
            &complete_id,
            (OwnerKind::Agent, agent),
            "vmAuthBootstrap",
            json!({"vmAuthBootstrap":state}),
            cutover,
        )
    }
}

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-vm-provider"
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
        for row in rows {
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
    use crate::outbox::{Outbox, OutboxRow};
    use serde_json::{Value, json};
    use studio_runtime_schema::persistence::OutboxStatus;
    fn database() -> rusqlite::Connection {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("transaction");
        crate::receipt_migration_contract::prepare_schema(&tx).expect("schema");
        tx.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL)").expect("sources");
        tx.commit().expect("commit");
        db
    }
    #[test]
    fn move_to_host_legacy_reply_is_imported_for_exact_replay() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let reply = json!({"id":"agent","executionMode":"native","workspaceMode":"shared","cwd":"/project","movedAgents":["agent"]});
        let signature = crate::sha256_hex(
            crate::python_json(&json!({"operation":"vm-to-host","id":"agent"})).as_bytes(),
        );
        tx.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
            params!["vm-to-host:legacy", signature, reply.to_string()],
        )
        .unwrap();
        let cp = MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).unwrap();
        Family.move_batch(&tx, SOURCES[0], &cp).unwrap();
        let saved = crate::request_results::RequestResults::replay(
            &tx,
            "vm-to-host:legacy",
            &signature,
            1001,
        )
        .unwrap()
        .unwrap();
        let envelope: ResultEnvelope<studio_runtime_schema::persistence::VmToHostReply> =
            serde_json::from_str(&saved.result).unwrap();
        assert_eq!(envelope.reply.id, "agent");
        assert_eq!(envelope.reply.moved_agents, ["agent"]);
        assert!(
            crate::request_results::RequestResults::replay(
                &tx,
                "vm-to-host:legacy",
                "different",
                1001
            )
            .is_err()
        );
    }
    #[test]
    fn receipt_vm_provider_obsolete_launch_unknown_never_becomes_not_sent() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        tx.execute("INSERT INTO runtime_agents VALUES('agent','{\"id\":\"agent\",\"epoch\":4,\"executionMode\":\"vm\"}')",[]).expect("agent");
        let hash = "a".repeat(64);
        let identity = json!({"agentId":"agent","epoch":3,"handle":"linux-vm:agent","requestId":"guest-request","launchFingerprint":hash});
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('linux-vm-provider-start:agent:3',?1,?2)",params![format!("agent=agent;epoch=3;launch={hash}"),json!({"phase":"reserved","identity":identity}).to_string()]).expect("launch");
        let cp = MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("begin");
        let next = Family.move_batch(&tx, SOURCES[0], &cp).expect("move");
        let id = Outbox::effect_id(
            "guest-request",
            OutboxKind::ProviderCall,
            OwnerKind::Agent,
            "agent",
            &["vmLaunch"],
        );
        let row = Outbox::get::<Value>(&tx, &id)
            .expect("lookup")
            .expect("held");
        assert_eq!(row.status, OutboxStatus::Unknown);
        assert_eq!(row.next_at, None);
        assert_eq!(
            row.payload["vmLaunch"]["status"],
            json!(WorkEffectStatus::Submitted)
        );
        assert_eq!(
            Family
                .move_batch(&tx, SOURCES[0], &next)
                .expect("second open")
                .status,
            MigrationStatus::Complete
        );
        let plan:String=tx.query_row("EXPLAIN QUERY PLAN SELECT id FROM runtime_outbox WHERE owner_kind='agent' AND owner_id='agent' AND kind='provider_call' ORDER BY id",[],|r|r.get(3)).expect("plan");
        assert!(plan.contains("runtime_outbox_owner"));
    }
    #[test]
    fn receipt_vm_provider_unlinked_bootstrap_closes_startup_and_preserves_source() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let hash = "a".repeat(64);
        let id = format!(
            "account-worker:vm-auth-bootstrap:agent:3:{}:submitted",
            &hash[..24]
        );
        tx.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
            params![id, json!({"signature":hash}).to_string()],
        )
        .expect("bootstrap");
        let cp = MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("begin");
        let error = Family
            .move_batch(&tx, SOURCES[0], &cp)
            .expect_err("missing exact caller authority");
        assert!(
            matches!(error,ReceiptMigrationError::Unclassifiable{table,key,signature} if table==SOURCES[0]&&key==id&&signature=="submitted")
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source retained"),
            1
        );
    }
    #[test]
    fn receipt_vm_provider_crash_between_credential_batches_is_atomic() {
        let mut db = database();
        {
            let tx = db.transaction().expect("transaction");
            for index in 0..=crate::receipt_migration_contract::MIGRATION_BATCH_ROWS {
                let signature = crate::sha256_hex(index.to_string().as_bytes());
                let id = format!("linux-vm-credential-copy:{}", &signature[..48]);
                tx.execute(
                    "INSERT INTO runtime_operation_receipts VALUES(?1,?2,'pending')",
                    params![id, signature],
                )
                .expect("copy");
            }
            tx.commit().expect("fixtures");
        }
        {
            let tx = db.transaction().expect("transaction");
            let cp = MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("begin");
            let next = Family.move_batch(&tx, SOURCES[0], &cp).expect("first");
            next.save(&tx, Family.id(), SOURCES[0], 1000)
                .expect("checkpoint");
            tx.commit().expect("commit");
        }
        {
            let tx = db.transaction().expect("transaction");
            let cp = MigrationCheckpoint::load(&tx, Family.id(), SOURCES[0])
                .expect("load")
                .expect("checkpoint");
            let next = Family.move_batch(&tx, SOURCES[0], &cp).expect("second");
            next.save(&tx, Family.id(), SOURCES[0], 1000).expect("save");
            tx.rollback().expect("crash");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("remainder"),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("held"),
            crate::receipt_migration_contract::MIGRATION_BATCH_ROWS as i64
        );
        let tx = db.transaction().expect("transaction");
        let cp = MigrationCheckpoint::load(&tx, Family.id(), SOURCES[0])
            .expect("load")
            .expect("checkpoint");
        Family.move_batch(&tx, SOURCES[0], &cp).expect("resume");
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source consumed"),
            0
        );
    }

    #[test]
    fn receipt_vm_provider_import_preserves_pending_and_exact_completed_reply() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("transaction");
        crate::receipt_migration_contract::prepare_schema(&tx).expect("schema");
        tx.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL); CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL)").expect("source");
        for (id, phase) in [("a", "pending"), ("b", "complete")] {
            let signature = id.repeat(64);
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                rusqlite::params![
                    format!("linux-vm-credential-copy:{}", id.repeat(48)),
                    signature,
                    phase
                ],
            )
            .expect("fixture");
        }
        Family.prepare(&tx, 1000).expect("prepare");
        let cp =
            MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("checkpoint");
        Family.move_batch(&tx, SOURCES[0], &cp).expect("import");
        let id = format!("linux-vm-credential-copy:{}", "a".repeat(48));
        let effect = Outbox::effect_id(
            &id,
            OutboxKind::ProviderCall,
            OwnerKind::Vm,
            &id,
            &["credentialCopy"],
        );
        let row: OutboxRow<Value> = Outbox::get(&tx, &effect)
            .expect("lookup")
            .expect("held copy");
        assert_eq!(row.status, OutboxStatus::Submitted);
        assert_eq!(row.next_at, None);
        assert_eq!(row.payload["credentialCopy"]["requestId"], id);
        let raw: String = tx
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [format!("linux-vm-credential-copy:{}", "b".repeat(48))],
                |r| r.get(0),
            )
            .expect("completed result");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("envelope")["reply"],
            json!("complete")
        );
    }
}
