//! Move account state and remaining journal obligations in bounded source order.
use crate::receipt_migration_contract::load_operation_receipt as load;
use crate::receipt_migration_contract::*;
use crate::{
    outbox::{Outbox, OutboxRow, Publication, PublicationClass},
    request_results::ResultEnvelope,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    HoldStatus, MigrationStatus, OutboxKind, OutboxStatus, OwnerKind,
};
const TABLE: &str = "runtime_operation_receipts";
/// Frozen source-era hold signature, read only by this dated importer.
const SOURCE_INVALID_AGENT_HOLD_SIGNATURE: &str = "invalid-agent-hold-v1";
fn fail(row: &SourceReceipt) -> ReceiptMigrationError {
    unclassifiable(TABLE, &row.id, &row.signature)
}
fn ensure_account(tx: &Transaction<'_>, account: &str, generation: i64) -> Result<()> {
    tx.execute("INSERT INTO runtime_account_state(account,next_generation,journal_ack,recovery_required,streams) VALUES(?1,?2,0,0,'null') ON CONFLICT(account) DO UPDATE SET next_generation=max(next_generation,excluded.next_generation)",params![account,generation])?;
    Ok(())
}
fn scope(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    stream: &str,
) -> Result<(String, Option<String>)> {
    if let Some((account, agent)) = stream.rsplit_once(":vm:") {
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [agent],
                |r| r.get(0),
            )
            .optional()?;
        let raw = raw.ok_or_else(|| fail(row))?;
        let record: Value = serde_json::from_str(&raw).map_err(|_| fail(row))?;
        if record["executionMode"] != "vm"
            || record
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != account
        {
            return Err(fail(row));
        }
        Ok((account.into(), Some(agent.into())))
    } else {
        Ok((stream.into(), None))
    }
}
fn phase_key<'a>(row: &'a SourceReceipt, prefix: &str, phase: &str) -> Result<&'a str> {
    row.id
        .strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(phase))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail(row))
}

fn state(tx: &Transaction<'_>, row: &SourceReceipt) -> Result<()> {
    if row.id.starts_with("account-worker:cursor:") {
        if row.signature != "ack" {
            return Err(fail(row));
        }
        let key = phase_key(row, "account-worker:cursor:", ":ack")?;
        let (stream, generation) = key.rsplit_once(':').ok_or_else(|| fail(row))?;
        let generation = generation
            .parse::<i64>()
            .ok()
            .filter(|v| *v >= 0)
            .ok_or_else(|| fail(row))?;
        let ack = row.json::<i64>(TABLE)?;
        if ack < 0 {
            return Err(fail(row));
        }
        let (account, agent) = scope(tx, row, stream)?;
        if let Some(agent) = agent {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.providerStream',json_object('generation',?2,'ack',?3,'connection',NULL,'streams',json('null'))) WHERE id=?1 AND coalesce(json_extract(record,'$.providerStream.generation'),-1)<?2",params![agent,generation,ack])?;
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.providerStream.ack',max(coalesce(json_extract(record,'$.providerStream.ack'),0),?3)) WHERE id=?1 AND json_extract(record,'$.providerStream.generation')=?2",params![agent,generation,ack])?;
        } else {
            ensure_account(tx, &account, generation)?;
            tx.execute("UPDATE runtime_account_state SET connection_generation=?2,journal_ack=?3 WHERE account=?1 AND coalesce(connection_generation,-1)<?2",params![account,generation,ack])?;
            tx.execute("UPDATE runtime_account_state SET journal_ack=max(journal_ack,?3) WHERE account=?1 AND connection_generation=?2",params![account,generation,ack])?;
        }
        return Ok(());
    }
    if row.id.starts_with("account-worker:direct-generation:") {
        if row.signature != "direct-provider-generation" {
            return Err(fail(row));
        }
        let account = phase_key(row, "account-worker:direct-generation:", ":next")?;
        let generation = row.json::<i64>(TABLE)?;
        if generation < 0 {
            return Err(fail(row));
        }
        return ensure_account(tx, account, generation);
    }
    if let Some(account) = row.id.strip_prefix("account-limits-observation:") {
        let value: Value = row.json(TABLE)?;
        if row.signature != "limits-observation"
            || !value.is_object()
            || value["cursor"].as_u64().is_none()
            || !(value["scope"].is_null() || value["scope"].is_object())
        {
            return Err(fail(row));
        }
        ensure_account(tx, account, 0)?;
        tx.execute(
            "UPDATE runtime_account_state SET limits_observation=?2 WHERE account=?1",
            params![account, row.result],
        )?;
        return Ok(());
    }
    if let Some(account) = row.id.strip_prefix("liveness:session-start-failure:") {
        let value: Value = row.json(TABLE)?;
        if row.signature != "liveness"
            || value["reason"].as_str().is_none()
            || value["at"].as_f64().is_none()
        {
            return Err(fail(row));
        }
        ensure_account(tx, account, 0)?;
        tx.execute(
            "UPDATE runtime_account_state SET start_failure=?2 WHERE account=?1",
            params![account, row.result],
        )?;
        return Ok(());
    }
    if row.id.starts_with("account-worker:recovery:") {
        if row.signature != "required" {
            return Err(fail(row));
        }
        let key = phase_key(row, "account-worker:recovery:", ":required")?;
        let (account, generation) = key.rsplit_once(':').ok_or_else(|| fail(row))?;
        let generation = generation
            .parse::<i64>()
            .ok()
            .filter(|v| *v >= 0)
            .ok_or_else(|| fail(row))?;
        let value: Value = row.json(TABLE)?;
        if value["handle"].as_str().is_none() || value["phase"].as_str().is_none() {
            return Err(fail(row));
        }
        ensure_account(tx, account, generation)?;
        tx.execute(
            "UPDATE runtime_account_state SET recovery_required=1 WHERE account=?1",
            [account],
        )?;
        return Ok(());
    }
    if let Some(agent) = row.id.strip_prefix("account-worker:invalid-agent-hold:") {
        let value: Value = row.json(TABLE)?;
        if row.signature != SOURCE_INVALID_AGENT_HOLD_SIGNATURE
            || value["held"] != true
            || value["diagnostic"].as_str().is_none()
        {
            return Err(fail(row));
        }
        if tx.execute(
            "UPDATE runtime_agents SET hold_status=?2,hold_incident=?3 WHERE id=?1",
            params![agent, HoldStatus::Invalid, value["diagnostic"].as_str()],
        )? != 1
        {
            return Err(fail(row));
        }
        return Ok(());
    }
    Err(fail(row))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    cursor: u64,
    digest: String,
    ready: bool,
    committed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notification: Option<Value>,
}
fn journal(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    if row.signature != "settled" {
        return Err(fail(row));
    }
    let key = phase_key(row, "account-worker:journal:", ":settled")?;
    let (key, cursor) = key.rsplit_once(':').ok_or_else(|| fail(row))?;
    let (stream, generation) = key.rsplit_once(':').ok_or_else(|| fail(row))?;
    let generation = generation
        .parse::<i64>()
        .ok()
        .filter(|v| *v >= 0)
        .ok_or_else(|| fail(row))?;
    let cursor = cursor
        .parse::<u64>()
        .ok()
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or_else(|| fail(row))?;
    let mut value: Journal = row.json(TABLE)?;
    if value.cursor != cursor {
        return Err(fail(row));
    }
    let (account, agent) = scope(tx, row, stream)?;
    if agent.is_none() {
        ensure_account(tx, &account, generation)?;
    }
    let ack = load(
        tx,
        &format!("account-worker:cursor:{stream}:{generation}:ack"),
    )?
    .map(|r| r.json::<u64>(TABLE))
    .transpose()?
    .unwrap_or(0);
    // Legacy notification identity was derived from this exact journal tuple.
    let signature=crate::sha256_hex(json!({"providerHandle":format!("account:{stream}"),"generation":generation,"cursor":cursor}).to_string().as_bytes());
    let notification = load(tx, &format!("provider-notification:{signature}"))?;
    if let Some(notification) = &notification {
        let outcome: Value = notification.json(TABLE)?;
        validate_outcome(notification, &outcome)?;
        value.notification = Some(json!({"signature":notification.signature,"outcome":outcome}));
    }
    if cursor > ack {
        let id = format!("journal:{stream}:{generation}:{cursor:020}");
        import_outbox(
            tx,
            &OutboxRow {
                id: id.clone(),
                kind: OutboxKind::JournalEntry,
                owner_kind: if agent.is_some() {
                    OwnerKind::Agent
                } else {
                    OwnerKind::Account
                },
                owner_id: agent.unwrap_or(account.clone()),
                request_id: None,
                signature: crate::sha256_hex(id.as_bytes()),
                status: if value.committed {
                    OutboxStatus::Committed
                } else {
                    OutboxStatus::Pending
                },
                account: Some(account),
                stream_id: Some(stream.into()),
                generation: Some(generation),
                ordinal: Some(cursor as i64),
                next_at: if value.committed { None } else { Some(0) },
                payload: value,
                created: cutover,
            },
        )?;
    }
    if let Some(notification) = notification {
        delete_source(tx, TABLE, notification.rowid)?;
    }
    Ok(())
}
fn validate_outcome(row: &SourceReceipt, value: &Value) -> Result<()> {
    match value["status"].as_str() {
        Some("written") => Ok(()),
        Some("ignored") if value["reason"].as_str().is_some() => Ok(()),
        Some("unhandled")
            if value["reason"].as_str().is_some() && value["kind"].as_str().is_some() =>
        {
            Ok(())
        }
        Some("requiresservice") if value["service"].is_string() => Ok(()),
        _ => Err(fail(row)),
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum EffectPayload {
    Followup(Value),
    Publication(Publication),
}
fn effects(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let (id, account, kind, signature, ordinal, payload) =
        if row.id.starts_with("account-worker:effect:") {
            let value: Value = row.json(TABLE)?;
            let key = phase_key(row, "account-worker:effect:", ":pending")?;
            let (account, hash) = key.rsplit_once(':').ok_or_else(|| fail(row))?;
            if row.signature != "pending" || hash.len() != 64 {
                return Err(fail(row));
            }
            let (payload, order) = if value.get("effect").is_some() {
                (
                    value["effect"].clone(),
                    value["order"].as_i64().ok_or_else(|| fail(row))?,
                )
            } else {
                (value, row.rowid)
            };
            if !payload.is_object() {
                return Err(fail(row));
            }
            (
                format!("effect:{key}"),
                account.to_owned(),
                OutboxKind::FollowupEffect,
                hash.to_owned(),
                order,
                EffectPayload::Followup(payload),
            )
        } else {
            let key = phase_key(row, "account-worker:notification-publication:", ":pending")?;
            let (key, index) = key.rsplit_once(':').ok_or_else(|| fail(row))?;
            let (account, sequence) = key.rsplit_once(':').ok_or_else(|| fail(row))?;
            let sequence = sequence
                .parse::<i64>()
                .ok()
                .filter(|v| *v >= 0)
                .ok_or_else(|| fail(row))?;
            let index = index.parse::<u64>().map_err(|_| fail(row))?;
            let mut value: Publication = row.json(TABLE)?;
            let class = match row.signature.as_str() {
                "pending" => PublicationClass::Pending,
                "token-rate" => PublicationClass::TokenRate,
                "invalidation" => PublicationClass::Invalidation,
                _ => return Err(fail(row)),
            };
            if value.account != account
                || i64::try_from(value.sequence).ok() != Some(sequence)
                || u64::try_from(value.index).ok() != Some(index)
            {
                return Err(fail(row));
            }
            let id = format!("notification-publication:{account}:{sequence:020}:{index:020}");
            value.class = class;
            (
                id.clone(),
                account.to_owned(),
                OutboxKind::Publication,
                crate::sha256_hex(id.as_bytes()),
                sequence,
                EffectPayload::Publication(value),
            )
        };
    ensure_account(tx, &account, 0)?;
    import_outbox(
        tx,
        &OutboxRow {
            id,
            kind,
            owner_kind: OwnerKind::Account,
            owner_id: account.clone(),
            request_id: None,
            signature,
            status: OutboxStatus::Pending,
            account: Some(account),
            stream_id: None,
            generation: None,
            ordinal: Some(ordinal),
            next_at: Some(0),
            payload,
            created: cutover,
        },
    )
}

fn calls(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let (base, phase) = row.id.rsplit_once(':').ok_or_else(|| fail(row))?;
    if !matches!(phase, "submitted" | "written" | "resolved" | "restarted")
        || row.signature != phase
    {
        return Err(fail(row));
    }
    let key = base
        .strip_prefix("account-worker:")
        .ok_or_else(|| fail(row))?;
    let submitted = load(tx, &format!("{base}:submitted"))?;
    let Some(submitted) = submitted else {
        if let Some(saved) = tx
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [format!("{base}:resolved")],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            let saved: Value = serde_json::from_str(&saved).map_err(|_| fail(row))?;
            let source: Value = row.json(TABLE)?;
            let valid = saved["reply"] == json!({"abandoned":"continued-by-message"})
                && match phase {
                    "resolved" => source == saved["reply"],
                    "restarted" => source == true,
                    "written" => source["generation"].as_u64().is_some(),
                    _ => false,
                };
            return if valid { Ok(()) } else { Err(fail(row)) };
        }
        let target = Outbox::get::<Value>(tx, key)
            .map_err(|_| fail(row))?
            .ok_or_else(|| fail(row))?;
        let value: Value = row.json(TABLE)?;
        let valid = match phase {
            "written" => target.payload["payload"]["written"] == value,
            "restarted" => value == true && target.payload["payload"]["restarted"] == true,
            _ => false,
        };
        return if valid { Ok(()) } else { Err(fail(row)) };
    };
    if submitted.signature != "submitted" {
        return Err(fail(row));
    }
    let value: Value = submitted.json(TABLE)?;
    let context = &value["context"];
    let request = &value["request"];
    let agent = context["intent"]["agent_id"]
        .as_str()
        .ok_or_else(|| fail(row))?;
    let account = context["intent"]["account_key"]
        .as_str()
        .ok_or_else(|| fail(row))?;
    let generation = request["generation"]
        .as_i64()
        .filter(|v| *v >= 0)
        .ok_or_else(|| fail(row))?;
    if context["operation"] != key
        || request["operation"] != key
        || request["method"] != context["method"]
        || context["intent"]["epoch"].as_i64().is_none()
    {
        return Err(fail(row));
    }
    let record: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent],
            |r| r.get(0),
        )
        .optional()?;
    let record = record
        .map(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| fail(row)))
        .transpose()?;
    let vm = record
        .as_ref()
        .is_some_and(|record| record["executionMode"] == "vm");
    if !vm {
        ensure_account(tx, account, generation)?;
    }
    let written = load(tx, &format!("{base}:written"))?
        .map(|r| r.json::<Value>(TABLE))
        .transpose()?;
    let restarted = load(tx, &format!("{base}:restarted"))?
        .map(|r| r.json::<bool>(TABLE))
        .transpose()?
        .unwrap_or(false);
    if let Some(resolved) = load(tx, &format!("{base}:resolved"))? {
        let proof: Value = resolved.json(TABLE)?;
        if resolved.signature != "resolved"
            || proof != json!({"abandoned":"continued-by-message"})
            || !restarted
            || !matches!(
                context["method"].as_str(),
                Some("thread/start" | "thread/resume")
            )
        {
            return Err(fail(row));
        }
        // Explicit abandonment closes this transport identity, never claiming NotSent.
        import_result(
            tx,
            &resolved.id,
            &resolved.signature,
            &ResultEnvelope {
                actor: Some(agent.into()),
                reply: proof,
                references: vec![],
            },
            cutover,
        )?;
        return Ok(());
    }
    let stream = if vm {
        format!("{account}:vm:{agent}")
    } else {
        account.into()
    };
    import_outbox(
        tx,
        &OutboxRow {
            id: key.into(),
            kind: OutboxKind::ProviderCall,
            owner_kind: OwnerKind::Agent,
            owner_id: agent.into(),
            request_id: None,
            signature: crate::sha256_hex(value.to_string().as_bytes()),
            status: if restarted || written.is_none() {
                OutboxStatus::Unknown
            } else {
                OutboxStatus::Submitted
            },
            account: Some(account.into()),
            stream_id: Some(stream),
            generation: Some(generation),
            ordinal: None,
            next_at: None,
            payload: json!({"family":"account","payload":{"context":context,"request":request,"written":written,"restarted":restarted}}),
            created: cutover,
        },
    )
}

#[derive(Serialize, Deserialize)]
struct StreamKey {
    #[serde(default)]
    agent_id: String,
    account: String,
    generation: u64,
    thread: String,
    turn: Option<String>,
    item: String,
    method: String,
}
fn key_lane(tx: &Transaction<'_>, row: &SourceReceipt, key: &mut Value) -> Result<Option<String>> {
    let mut typed: StreamKey = serde_json::from_value(key.clone()).map_err(|_| fail(row))?;
    let mut statement=tx.prepare("SELECT id,record FROM runtime_agents WHERE json_extract(record,'$.threadId')=?1 AND coalesce(json_extract(record,'$.accountKey'),'default')=?2")?;
    let candidates = statement
        .query_map(params![typed.thread, typed.account], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let candidates = candidates
        .into_iter()
        .filter(|(id, _)| typed.agent_id.is_empty() || *id == typed.agent_id)
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return Err(fail(row));
    }
    let (agent, raw) = candidates.into_iter().next().ok_or_else(|| fail(row))?;
    let record: Value = serde_json::from_str(&raw).map_err(|_| fail(row))?;
    typed.agent_id = agent.clone();
    *key = serde_json::to_value(&typed).map_err(|_| fail(row))?;
    Ok((record["executionMode"] == "vm").then_some(agent))
}
fn blank_streams() -> Value {
    json!({"entries":[],"closed":[],"analytics":[],"analytics_skipped":[],"post_commit":[],"publication_sequence":0,"analytics_overflow":0})
}
fn streams(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let (base, phase) = row.id.rsplit_once(':').ok_or_else(|| fail(row))?;
    let account = base
        .strip_prefix("account-worker:streams:")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail(row))?;
    if !matches!(phase, "metadata" | "pending") {
        return Err(fail(row));
    }
    let metadata = load(tx, &format!("{base}:metadata"))?;
    let mut lanes = std::collections::BTreeMap::<Option<String>, Value>::new();
    if let Some(metadata) = &metadata {
        let source: Value = metadata.json(TABLE)?;
        let mut host = blank_streams();
        for field in ["post_commit", "publication_sequence", "analytics_overflow"] {
            if let Some(value) = source.get(field) {
                host[field] = value.clone();
            }
        }
        lanes.insert(None, host);
        for field in ["entries", "closed", "analytics", "analytics_skipped"] {
            let values = source
                .get(field)
                .map(|v| v.as_array().ok_or_else(|| fail(row)))
                .transpose()?;
            for mut value in values.cloned().unwrap_or_default() {
                let key = if matches!(field, "entries" | "analytics") {
                    &mut value["key"]
                } else {
                    &mut value
                };
                if key["account"] != account {
                    return Err(fail(row));
                }
                let lane = key_lane(tx, row, key)?;
                let target = lanes.entry(lane).or_insert_with(blank_streams);
                target[field]
                    .as_array_mut()
                    .ok_or_else(|| fail(row))?
                    .push(value);
            }
        }
    } else if phase != "pending" {
        return Err(fail(row));
    }
    let pending = load(tx, &format!("{base}:pending"))?
        .map(|r| r.json::<Vec<Value>>(TABLE))
        .transpose()?
        .unwrap_or_default();
    let mut pending_lanes = std::collections::BTreeMap::<Option<String>, Vec<Value>>::new();
    for mut value in pending {
        let lane = key_lane(tx, row, &mut value["key"])?;
        if let Some(metadata) = lanes.get(&lane)
            && !metadata["entries"]
                .as_array()
                .ok_or_else(|| fail(row))?
                .iter()
                .any(|entry| entry["key"] == value["key"])
        {
            return Err(fail(row));
        }
        pending_lanes.entry(lane).or_default().push(value);
    }
    if let Some(metadata) = metadata {
        ensure_account(tx, account, 0)?;
        for (lane, value) in lanes {
            let wrapper = json!({"revision":metadata.signature,"metadata":value}).to_string();
            if let Some(agent) = lane {
                tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.providerStream.streams',json(?2)) WHERE id=?1",params![agent,wrapper])?;
            } else {
                tx.execute(
                    "UPDATE runtime_account_state SET streams=?2 WHERE account=?1",
                    params![account, wrapper],
                )?;
            }
        }
        for (lane, payload) in pending_lanes {
            let stream = lane.as_ref().map_or_else(
                || account.to_owned(),
                |agent| format!("{account}:vm:{agent}"),
            );
            let id = format!("streams:{stream}");
            import_outbox(
                tx,
                &OutboxRow {
                    id: id.clone(),
                    kind: OutboxKind::StreamFlush,
                    owner_kind: if lane.is_some() {
                        OwnerKind::Agent
                    } else {
                        OwnerKind::Account
                    },
                    owner_id: lane.unwrap_or_else(|| account.into()),
                    request_id: None,
                    signature: crate::sha256_hex(id.as_bytes()),
                    status: OutboxStatus::Pending,
                    account: Some(account.into()),
                    stream_id: Some(stream),
                    generation: None,
                    ordinal: None,
                    next_at: Some(0),
                    payload,
                    created: cutover,
                },
            )?;
        }
    } else {
        for (lane, payload) in pending_lanes {
            let stream = lane.as_ref().map_or_else(
                || account.to_owned(),
                |agent| format!("{account}:vm:{agent}"),
            );
            let target = Outbox::get::<Vec<Value>>(tx, &format!("streams:{stream}"))
                .map_err(|_| fail(row))?
                .ok_or_else(|| fail(row))?;
            if target.payload != payload {
                return Err(fail(row));
            }
        }
    }
    Ok(())
}

fn stream_base(tx: &Transaction<'_>, row: &SourceReceipt) -> Result<()> {
    if row.signature != "stream-base" {
        return Err(fail(row));
    }
    let source_hash = row
        .id
        .strip_prefix("account-worker:stream-base:")
        .ok_or_else(|| fail(row))?;
    let text: String = row.json(TABLE)?;
    let text_hash = crate::sha256_hex(text.as_bytes());
    // The metadata copy is already durable. Resolve one exact base per bounded
    // source row, rather than fetching an unbounded metadata dependency group.
    let mut statement =tx.prepare("SELECT account,streams,NULL FROM runtime_account_state WHERE streams!='null' UNION ALL SELECT id,json_extract(record,'$.providerStream.streams'),id FROM runtime_agents WHERE json_type(record,'$.providerStream.streams')='object'")?;
    let mut rows = statement.query([])?;
    let mut destination = None;
    while let Some(owner) = rows.next()? {
        let account: String = owner.get(0)?;
        let raw: String = owner.get(1)?;
        let agent: Option<String> = owner.get(2)?;
        let mut streams: Value = serde_json::from_str(&raw).map_err(|_| fail(row))?;
        let entries = streams["metadata"]["entries"]
            .as_array_mut()
            .ok_or_else(|| fail(row))?;
        for entry in entries {
            let mut key: StreamKey =
                serde_json::from_value(entry["key"].clone()).map_err(|_| fail(row))?;
            let hash = crate::sha256_hex(
                serde_json::to_string(&key)
                    .map_err(|_| fail(row))?
                    .as_bytes(),
            );
            // These are the three source serializers actually used by the old
            // base writer: actor present, actor defaulted, and pre-actor keys.
            key.agent_id.clear();
            let empty_hash = crate::sha256_hex(
                serde_json::to_string(&key)
                    .map_err(|_| fail(row))?
                    .as_bytes(),
            );
            #[derive(Serialize)]
            struct BeforeActor<'a> {
                account: &'a str,
                generation: u64,
                thread: &'a str,
                turn: &'a Option<String>,
                item: &'a str,
                method: &'a str,
            }
            let original = BeforeActor {
                account: &key.account,
                generation: key.generation,
                thread: &key.thread,
                turn: &key.turn,
                item: &key.item,
                method: &key.method,
            };
            let original_hash = crate::sha256_hex(
                serde_json::to_string(&original)
                    .map_err(|_| fail(row))?
                    .as_bytes(),
            );
            if [hash, empty_hash, original_hash]
                .iter()
                .any(|hash| hash == source_hash)
            {
                if destination.is_some() || entry["base"] != text_hash {
                    return Err(fail(row));
                }
                entry["base"] = json!(text);
                destination = Some((account.clone(), agent.clone(), streams.to_string()));
                break;
            }
        }
    }
    drop(rows);
    drop(statement);
    let (account, agent, streams) = destination.ok_or_else(|| fail(row))?;
    if let Some(agent) = agent {
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.providerStream.streams',json(?2)) WHERE id=?1",params![agent,streams])?;
    } else {
        tx.execute(
            "UPDATE runtime_account_state SET streams=?2 WHERE account=?1",
            params![account, streams],
        )?;
    }
    Ok(())
}

fn submission_input(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let record: Value = row.json("runtime_items")?;
    let agent = record["agent"]
        .as_str()
        .or_else(|| row.signature.strip_prefix("agent:"))
        .ok_or_else(|| fail(row))?;
    let inputs = record["inputs"].as_array().ok_or_else(|| fail(row))?;
    let first = inputs
        .first()
        .and_then(|v| v["id"].as_str())
        .ok_or_else(|| fail(row))?;
    let receipt_id = format!(
        "provider-submission:{}",
        crate::sha256_hex(format!("{agent}:{first}").as_bytes())
    );
    let Some(receipt) = load(tx, &receipt_id)? else {
        return Ok(());
    };
    let outcome: Value = receipt.json(TABLE)?;
    validate_outcome(&receipt, &outcome)?;
    let admission = admitted_owner_token(&receipt.id, &receipt.signature, cutover);
    let submission = json!({"attemptId":null,"firstEventId":first,"admission":admission,"publicationOutcome":outcome});
    for input in inputs {
        let id = input["id"].as_str().ok_or_else(|| fail(&receipt))?;
        tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES(?1,json_object('providerSubmission',json(?2))) ON CONFLICT(id) DO UPDATE SET record=json_set(record,'$.providerSubmission',json(?2))",params![id,submission.to_string()])?;
    }
    import_result(
        tx,
        &receipt.id,
        &receipt.signature,
        &ResultEnvelope {
            actor: Some(agent.into()),
            reply: outcome,
            references: vec![row.id.clone()],
        },
        cutover,
    )?;
    delete_source(tx, TABLE, receipt.rowid)
}
pub(super) struct Family;
pub(super) const SOURCES: &[&str] = &[
    "journal",
    "state",
    "calls",
    "effects",
    "streams",
    "stream-pending",
    "stream-bases",
    "submission-inputs",
    "unmatched",
];
impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-account-worker"
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
        let predicate = match source {
            "journal" => "source.id GLOB 'account-worker:journal:*'",
            "state" => {
                "source.id GLOB 'account-worker:cursor:*' OR source.id GLOB 'account-worker:direct-generation:*' OR source.id GLOB 'account-worker:recovery:*' OR source.id GLOB 'account-limits-observation:*' OR source.id GLOB 'liveness:session-start-failure:*' OR source.id GLOB 'account-worker:invalid-agent-hold:*'"
            }
            "calls" => "source.id GLOB 'account-worker:account:*'",
            "effects" => {
                "source.id GLOB 'account-worker:effect:*' OR source.id GLOB 'account-worker:notification-publication:*'"
            }
            "streams" => "source.id GLOB 'account-worker:streams:*:metadata'",
            "stream-pending" => "source.id GLOB 'account-worker:streams:*:pending'",
            "stream-bases" => "source.id GLOB 'account-worker:stream-base:*'",
            "submission-inputs" => {
                "json_extract(source.record,'$.role')='user' AND json_type(source.record,'$.inputs')='array' AND json_array_length(source.record,'$.inputs')>0"
            }
            "unmatched" => {
                "source.id GLOB 'provider-notification:*' OR source.id GLOB 'provider-submission:*' OR source.id GLOB 'account-worker:stream-base:*'"
            }
            _ => {
                return Err(ReceiptMigrationError::Incomplete {
                    migration: self.id().into(),
                    source_table: source.into(),
                });
            }
        };
        let rows = if source == "submission-inputs" {
            if !tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='runtime_items')",[],|r|r.get::<_,bool>(0))? {
                vec![]
            } else {
                read_source_batch(
                    tx,
                    "runtime_items",
                    checkpoint,
                    &ReceiptProjection {
                        id: "source.id",
                        signature: "'agent:' || source.agent",
                        result: "source.record",
                    },
                    predicate,
                )?
            }
        } else {
            read_receipt_batch(tx, TABLE, checkpoint, predicate)?
        };
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        let mut budget = BatchBudget::default();
        for row in rows {
            let mut related = Vec::new();
            match source {
                "journal" => {
                    let key = phase_key(&row, "account-worker:journal:", ":settled")?;
                    let (key, cursor) = key.rsplit_once(':').ok_or_else(|| fail(&row))?;
                    let (stream, generation) = key.rsplit_once(':').ok_or_else(|| fail(&row))?;
                    let generation = generation.parse::<u64>().map_err(|_| fail(&row))?;
                    let cursor = cursor.parse::<u64>().map_err(|_| fail(&row))?;
                    let hash=crate::sha256_hex(json!({"providerHandle":format!("account:{stream}"),"generation":generation,"cursor":cursor}).to_string().as_bytes());
                    related.push(format!("provider-notification:{hash}"));
                }
                "calls" => {
                    let base = row
                        .id
                        .rsplit_once(':')
                        .map(|(base, _)| base)
                        .ok_or_else(|| fail(&row))?;
                    for phase in ["submitted", "written", "resolved", "restarted"] {
                        related.push(format!("{base}:{phase}"));
                    }
                }
                "streams" | "stream-pending" => {
                    let base = row
                        .id
                        .rsplit_once(':')
                        .map(|(base, _)| base)
                        .ok_or_else(|| fail(&row))?;
                    related.push(format!("{base}:metadata"));
                    related.push(format!("{base}:pending"));
                }
                _ => {}
            }
            let mut count = 1;
            let mut bytes = row.id.len() + row.signature.len() + row.result.len();
            for id in related {
                if id != row.id {
                    let size:Option<i64>=tx.query_row("SELECT length(CAST(id AS BLOB))+length(CAST(signature AS BLOB))+length(CAST(result AS BLOB)) FROM runtime_operation_receipts WHERE id=?1",[id],|r|r.get(0)).optional()?;
                    if let Some(size) = size {
                        count += 1;
                        bytes += size as usize;
                    }
                }
            }
            if budget.rows + count > MIGRATION_BATCH_ROWS || !budget.include(bytes) {
                break;
            }
            for _ in 1..count {
                if !budget.include(0) {
                    return Err(fail(&row));
                }
            }
            match source {
                "journal" => journal(tx, &row, checkpoint.cutover_ms)?,
                "state" => state(tx, &row)?,
                "calls" => calls(tx, &row, checkpoint.cutover_ms)?,
                "effects" => effects(tx, &row, checkpoint.cutover_ms)?,
                "streams" | "stream-pending" => streams(tx, &row, checkpoint.cutover_ms)?,
                "stream-bases" => stream_base(tx, &row)?,
                "submission-inputs" => submission_input(tx, &row, checkpoint.cutover_ms)?,
                _ => return Err(fail(&row)),
            }
            if source != "submission-inputs" {
                delete_source(tx, TABLE, row.rowid)?;
            }
            next.cursor = row.rowid;
        }
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        for source in SOURCES {
            if MigrationCheckpoint::load(tx, self.id(), source)?
                .is_none_or(|cp| cp.status != MigrationStatus::Complete)
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
    use serde_json::json;
    fn database() -> rusqlite::Connection {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("transaction");
        prepare_schema(&tx).expect("schema");
        tx.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL,hold_status TEXT,hold_incident TEXT)").expect("source");
        tx.commit().expect("commit");
        db
    }
    fn move_source(tx: &Transaction<'_>, source: &str) {
        let mut cp = MigrationCheckpoint::begin(tx, Family.id(), source, 1000).expect("checkpoint");
        while cp.status != MigrationStatus::Complete {
            cp = Family.move_batch(tx, source, &cp).expect("move");
            cp.save(tx, Family.id(), source, 1000).expect("save");
        }
    }
    #[test]
    fn receipt_account_worker_journal_gaps_and_notification_proof_survive() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:cursor:default:7:ack','ack','1')",[]).expect("ack");
        for cursor in [1, 3] {
            let settlement =
                json!({"cursor":cursor,"digest":"digest","committed":true,"ready":true});
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'settled',?2)",
                params![
                    format!("account-worker:journal:default:7:{cursor}:settled"),
                    settlement.to_string()
                ],
            )
            .expect("journal");
            let identity =
                json!({"providerHandle":"account:default","generation":7,"cursor":cursor});
            let id = format!(
                "provider-notification:{}",
                crate::sha256_hex(identity.to_string().as_bytes())
            );
            tx.execute("INSERT INTO runtime_operation_receipts VALUES(?1,'native-input-signature','{\"status\":\"written\"}')",[id]).expect("notification");
        }
        move_source(&tx, "journal");
        move_source(&tx, "state");
        assert!(
            Outbox::get::<Value>(&tx, "journal:default:7:00000000000000000001")
                .expect("covered")
                .is_none()
        );
        let row = Outbox::get::<Value>(&tx, "journal:default:7:00000000000000000003")
            .expect("gap")
            .expect("kept");
        assert_eq!(row.status, OutboxStatus::Committed);
        assert_eq!(
            row.payload["notification"]["signature"],
            "native-input-signature"
        );
        assert_eq!(
            tx.query_row(
                "SELECT journal_ack FROM runtime_account_state WHERE account='default'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("ack"),
            1
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("consumed"),
            0
        );
        move_source(&tx, "journal");
        assert_eq!(
            Outbox::get::<Value>(&tx, &row.id)
                .expect("same gap")
                .expect("kept"),
            row
        );
    }
    #[test]
    fn receipt_account_worker_rejects_invalid_publication_header() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let row = SourceReceipt {
            rowid: 1,
            id: "account-worker:notification-publication:default:5:2:pending".into(),
            signature: "pending".into(),
            result: r#"{"account":"default","sequence":5,"index":2,"stateInvalidation":"bad"}"#
                .into(),
        };
        assert!(matches!(
            effects(&tx, &row, 1000),
            Err(ReceiptMigrationError::Unclassifiable { .. })
        ));
    }
    #[test]
    fn receipt_account_worker_mixed_old_key_formats_keep_numeric_journal_and_publication_order() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        for key in ["2".to_owned(), format!("{:020}", 2), format!("{:020}", 10)] {
            let cursor = key.parse::<u64>().expect("cursor");
            let payload = json!({"cursor":cursor,"digest":format!("digest-{cursor}"),"committed":true,"ready":true});
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'settled',?2)",
                params![
                    format!("account-worker:journal:default:7:{key}:settled"),
                    payload.to_string()
                ],
            )
            .expect("journal source");
        }
        for (key, index) in [("5:10".to_owned(), 10), (format!("{:020}:{:020}", 5, 2), 2)] {
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'pending',?2)",
                params![
                    format!("account-worker:notification-publication:default:{key}:pending"),
                    json!({"account":"default","sequence":5,"index":index,"publicationOverflow":1})
                        .to_string()
                ],
            )
            .expect("publication source");
        }
        move_source(&tx, "journal");
        move_source(&tx, "effects");
        let journal = Outbox::for_stream::<Value>(&tx, "default", 7, 10).expect("journal");
        assert_eq!(journal.len(), 2);
        assert_eq!(
            journal
                .iter()
                .map(|row| row.ordinal.expect("cursor"))
                .collect::<Vec<_>>(),
            vec![2, 10]
        );
        assert_eq!(journal[0].id, "journal:default:7:00000000000000000002");
        let publications = Outbox::for_account::<Value>(
            &tx,
            "default",
            OutboxKind::Publication,
            OutboxStatus::Pending,
            10,
        )
        .expect("publications");
        assert_eq!(
            publications
                .iter()
                .map(|row| row.payload["index"].as_u64().expect("index"))
                .collect::<Vec<_>>(),
            vec![2, 10]
        );
        assert_eq!(
            publications[0].id,
            "notification-publication:default:00000000000000000005:00000000000000000002"
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source consumed"),
            0
        );
    }
    #[test]
    fn receipt_account_worker_submitted_only_stays_unknown() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('agent','{\"id\":\"agent\",\"epoch\":3}')",[]).expect("agent");
        let key = "account:default:operation";
        let value = json!({"context":{"intent":{"agent_id":"agent","account_key":"default","epoch":3},"operation":key,"method":"thread/start"},"request":{"id":key,"generation":7,"operation":key,"method":"thread/start"}});
        tx.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
            params![format!("account-worker:{key}:submitted"), value.to_string()],
        )
        .expect("submission");
        move_source(&tx, "calls");
        let row = Outbox::get::<Value>(&tx, key)
            .expect("lookup")
            .expect("held");
        assert_eq!(row.status, OutboxStatus::Unknown);
        assert_eq!(row.next_at, None);
        move_source(&tx, "calls");
        assert_eq!(
            Outbox::get::<Value>(&tx, key)
                .expect("lookup")
                .expect("held"),
            row
        );
        let plan:String=tx.query_row("EXPLAIN QUERY PLAN SELECT id FROM runtime_outbox WHERE owner_kind='agent' AND owner_id='agent' AND kind='provider_call' ORDER BY id",[],|r|r.get(3)).expect("plan");
        assert!(plan.contains("runtime_outbox_owner"));
    }
    #[test]
    fn receipt_account_worker_host_and_vm_streams_keep_exact_base_and_pending() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let mut entries = Vec::new();
        let mut pending = Vec::new();
        for (agent, thread, mode, text) in [
            ("host", "h", "host", "host base"),
            ("vm", "v", "vm", "vm base"),
        ] {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",params![agent,json!({"id":agent,"epoch":3,"accountKey":"default","threadId":thread,"executionMode":mode}).to_string()]).expect("agent");
            let key = StreamKey {
                agent_id: String::new(),
                account: "default".into(),
                generation: 7,
                thread: thread.into(),
                turn: Some("turn".into()),
                item: "item".into(),
                method: "item/agentMessage/delta".into(),
            };
            let id = format!(
                "account-worker:stream-base:{}",
                crate::sha256_hex(serde_json::to_string(&key).expect("key").as_bytes())
            );
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'stream-base',?2)",
                params![id, json!(text).to_string()],
            )
            .expect("base");
            entries.push(json!({"key":key,"flushed":true,"base":crate::sha256_hex(text.as_bytes()),"pending":"","count":0,"characters":0,"params":null}));
            pending.push(json!({"key":key,"pending":" tail","count":1,"params":{"delta":" tail"}}));
        }
        let metadata = json!({"entries":entries,"closed":[],"analytics":[],"analytics_skipped":[],"post_commit":[],"publication_sequence":5,"analytics_overflow":2});
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:streams:default:metadata','revision',?1)",[metadata.to_string()]).expect("metadata");
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:streams:default:pending','pending',?1)",[json!(pending).to_string()]).expect("pending");
        for source in ["streams", "stream-pending", "stream-bases"] {
            move_source(&tx, source);
        }
        let host: String = tx
            .query_row(
                "SELECT streams FROM runtime_account_state WHERE account='default'",
                [],
                |r| r.get(0),
            )
            .expect("host");
        let host: Value = serde_json::from_str(&host).expect("host metadata");
        assert_eq!(host["metadata"]["entries"][0]["key"]["agent_id"], "host");
        assert_eq!(host["metadata"]["entries"][0]["base"], "host base");
        assert_eq!(
            host["metadata"]["entries"]
                .as_array()
                .expect("entries")
                .len(),
            1
        );
        assert_eq!(host["metadata"]["publication_sequence"], 5);
        let vm: String = tx
            .query_row("SELECT record FROM runtime_agents WHERE id='vm'", [], |r| {
                r.get(0)
            })
            .expect("vm");
        let vm: Value = serde_json::from_str(&vm).expect("vm record");
        assert_eq!(
            vm["providerStream"]["streams"]["metadata"]["entries"][0]["base"],
            "vm base"
        );
        assert_eq!(
            vm["providerStream"]["streams"]["metadata"]["publication_sequence"],
            0
        );
        for (scope, owner) in [
            ("default", OwnerKind::Account),
            ("default:vm:vm", OwnerKind::Agent),
        ] {
            let row = Outbox::get::<Value>(&tx, &format!("streams:{scope}"))
                .expect("flush")
                .expect("owed");
            assert_eq!(row.owner_kind, owner);
            assert_eq!(row.payload[0]["pending"], " tail");
            assert_eq!(row.signature, crate::sha256_hex(row.id.as_bytes()));
        }
    }
    #[test]
    fn receipt_account_worker_copies_ack_and_generation_once() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("transaction");
        prepare_schema(&tx).expect("schema");
        tx.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL); CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL,hold_status TEXT,hold_incident TEXT)").expect("source");
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:cursor:default:7:ack','ack','22')",[]).expect("cursor");
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('account-worker:direct-generation:default:next','direct-provider-generation','9')",[]).expect("counter");
        let cp = MigrationCheckpoint::begin(&tx, Family.id(), "state", 1000).expect("checkpoint");
        Family.move_batch(&tx, "state", &cp).expect("import");
        let state:(i64,i64,i64)=tx.query_row("SELECT next_generation,connection_generation,journal_ack FROM runtime_account_state WHERE account='default'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).expect("state");
        assert_eq!(state, (9, 7, 22));
        tx.execute("UPDATE runtime_account_state SET next_generation=10", [])
            .expect("allocate");
        Family.move_batch(&tx, "state", &cp).expect("reopen");
        assert_eq!(
            tx.query_row(
                "SELECT next_generation FROM runtime_account_state",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("counter"),
            10
        );
        assert_eq!(json!(state), json!([9, 7, 22]));
    }
    #[test]
    fn receipt_account_worker_state_batch_crash_keeps_counter_checkpoint_atomic() {
        let mut db = database();
        {
            let tx = db.transaction().expect("transaction");
            for index in 0..=MIGRATION_BATCH_ROWS {
                tx.execute("INSERT INTO runtime_operation_receipts VALUES(?1,'direct-provider-generation','7')",[format!("account-worker:direct-generation:account-{index}:next")]).expect("counter");
            }
            tx.commit().expect("fixtures");
        }
        {
            let tx = db.transaction().expect("transaction");
            let cp = MigrationCheckpoint::begin(&tx, Family.id(), "state", 1000).expect("begin");
            let next = Family.move_batch(&tx, "state", &cp).expect("first");
            next.save(&tx, Family.id(), "state", 1000).expect("save");
            tx.commit().expect("commit");
        }
        {
            let tx = db.transaction().expect("transaction");
            let cp = MigrationCheckpoint::load(&tx, Family.id(), "state")
                .expect("load")
                .expect("checkpoint");
            let next = Family.move_batch(&tx, "state", &cp).expect("second");
            next.save(&tx, Family.id(), "state", 1000).expect("save");
            tx.rollback().expect("crash");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("remaining"),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_account_state", [], |r| r
                .get::<_, i64>(0))
                .expect("accounts"),
            MIGRATION_BATCH_ROWS as i64
        );
        let tx = db.transaction().expect("transaction");
        move_source(&tx, "state");
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_account_state", [], |r| r
                .get::<_, i64>(0))
                .expect("accounts"),
            (MIGRATION_BATCH_ROWS + 1) as i64
        );
    }
}
