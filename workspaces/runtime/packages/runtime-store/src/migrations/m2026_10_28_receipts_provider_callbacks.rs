//! Move callback identity fences and canonical response obligations together.
use crate::receipt_migration_contract::*;
use crate::{
    outbox::{Outbox, OutboxRow},
    request_results::ResultEnvelope,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    MigrationStatus, OutboxKind, OutboxStatus, OwnerKind, RequestIdentity, ToolRequestKind,
    ToolRequestStatus,
};
const TABLE: &str = "runtime_operation_receipts";
fn fail(row: &SourceReceipt) -> ReceiptMigrationError {
    unclassifiable(TABLE, &row.id, &row.signature)
}
fn load(tx: &Transaction<'_>, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| {
        serde_json::from_str(&raw).map_err(|_| unclassifiable(TABLE, id, "provider-callback"))
    })
    .transpose()
}
#[derive(Deserialize)]
struct LegacyContext {
    account: String,
    agent: String,
    generation: i64,
    epoch: u64,
    thread: String,
    turn: String,
    #[serde(rename = "connection")]
    _connection: String,
}
#[derive(Deserialize)]
struct LegacyIdentity {
    context: LegacyContext,
    rpc_id: LegacyRpc,
}
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum LegacyRpc {
    String(String),
    Number(serde_json::Number),
}
fn identity(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    mut identity: Value,
) -> Result<(String, Value)> {
    let LegacyIdentity { context: c, rpc_id } =
        LegacyIdentity::deserialize(&identity).map_err(|_| fail(row))?;
    if c.account.is_empty() || c.agent.is_empty() {
        return Err(fail(row));
    }
    let vm = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [&c.agent],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| fail(row)))
        .transpose()?
        .is_some_and(|r| r["executionMode"] == "vm");
    let stream = if vm {
        format!("{}:vm:{}", c.account, c.agent)
    } else {
        c.account.clone()
    };
    let bytes = serde_json::to_vec(&(
        &c.account,
        c.generation,
        &stream,
        &c.agent,
        c.epoch,
        &c.thread,
        &c.turn,
        &rpc_id,
    ))
    .map_err(|_| fail(row))?;
    let key = format!("provider-callback:{}", crate::sha256_hex(&bytes));
    identity["context"]["stream_id"] = json!(stream);
    if !vm {
        tx.execute("INSERT INTO runtime_account_state(account,next_generation,journal_ack,recovery_required,streams) VALUES(?1,?2,0,0,'null') ON CONFLICT(account) DO UPDATE SET next_generation=max(next_generation,excluded.next_generation)",params![c.account,c.generation])?;
    }
    Ok((key, identity))
}
fn old_key(identity: &Value) -> String {
    let c = &identity["context"];
    format!(
        "provider-callback:{}",
        crate::sha256_hex(
            json!([
                c["account"],
                c["generation"],
                c["connection"],
                identity["rpc_id"]
            ])
            .to_string()
            .as_bytes()
        )
    )
}
fn task(tx: &Transaction<'_>, key: &str) -> Result<Option<Value>> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_tool_requests WHERE origin_key=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| {
        serde_json::from_str(&raw)
            .map_err(|_| unclassifiable("runtime_tool_requests", key, "provider_callback"))
    })
    .transpose()
}
fn put_task(tx: &Transaction<'_>, value: &Value) -> Result<()> {
    tx.execute("INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![value["id"].as_str(),value.to_string()])?;
    Ok(())
}

fn settle_written(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    owner: &mut Value,
    request_id: &str,
    signature: &str,
    mut payload: Value,
    cutover: i64,
) -> Result<()> {
    let key = owner["originKey"]
        .as_str()
        .ok_or_else(|| fail(row))?
        .to_owned();
    if let Some(prior) = Outbox::get::<Value>(tx, &key).map_err(|_| fail(row))?
        && (prior.signature != signature
            || prior.payload["identity"] != payload["identity"]
            || prior.payload["body"] != payload["body"])
    {
        return Err(fail(row));
    }
    if payload["frame_fingerprint"].is_null() {
        let body = if let Some(value) = payload["body"].get("result") {
            studio_runtime_providers::ResponseBody::Result(value.clone())
        } else if let Some(value) = payload["body"].get("error") {
            studio_runtime_providers::ResponseBody::Error(value.clone())
        } else {
            return Err(fail(row));
        };
        let frame = studio_runtime_providers::Frame::Response {
            id: serde_json::from_value(payload["identity"]["rpc_id"].clone())
                .map_err(|_| fail(row))?,
            body,
            fields: Default::default(),
        };
        payload["frame_fingerprint"] = json!(crate::sha256_hex(
            &studio_runtime_providers::encode_line(&frame).map_err(|_| fail(row))?
        ));
    }
    import_result(
        tx,
        request_id,
        signature,
        &ResultEnvelope {
            actor: payload["identity"]["context"]["agent"]
                .as_str()
                .map(str::to_owned),
            reply: payload.clone(),
            references: vec![],
        },
        cutover,
    )?;
    owner["status"] = json!(ToolRequestStatus::Completed);
    owner["payload"]["reply_identity"] = payload["identity"].clone();
    owner["payload"]["frame_fingerprint"] = payload["frame_fingerprint"].clone();
    put_task(tx, owner)?;
    Outbox::delete_settled(tx, &key, signature).map_err(|_| fail(row))?;
    Ok(())
}

fn canonical(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    if row.signature != "provider-callback" {
        return Err(fail(row));
    }
    let source: Value = row.json(TABLE)?;
    let base = row
        .id
        .strip_suffix(":card")
        .or_else(|| row.id.strip_suffix(":resolved"))
        .unwrap_or(&row.id);
    if old_key(&source["identity"]) != base {
        return Err(fail(row));
    }
    let reply = load(tx, base)?;
    let card = load(tx, &format!("{base}:card"))?;
    let resolved = load(tx, &format!("{base}:resolved"))?;
    let saved = reply
        .as_ref()
        .or(card.as_ref())
        .or(resolved.as_ref())
        .ok_or_else(|| fail(row))?;
    for sibling in [reply.as_ref(), card.as_ref(), resolved.as_ref()]
        .into_iter()
        .flatten()
    {
        if sibling["identity"]["context"] != source["identity"]["context"]
            || sibling["identity"]["rpc_id"] != source["identity"]["rpc_id"]
            || sibling["signature"] != saved["signature"]
        {
            return Err(fail(row));
        }
    }
    let signature = saved["signature"].as_str().ok_or_else(|| fail(row))?;
    let (key, mut response_identity) = identity(tx, row, saved["identity"].clone())?;
    let admission = admitted_owner_token(base, signature, cutover);
    let previous = task(tx, &key)?;
    if reply.is_none()
        && let Some(previous) = &previous
    {
        let mut origin = response_identity.clone();
        origin["attempt"] = json!("");
        if previous["payload"]["identity"] != origin
            || previous["payload"]["signature"] != signature
        {
            return Err(fail(row));
        }
        if row.id.ends_with(":card") && previous["payload"]["card"] != source["card"] {
            return Err(fail(row));
        }
        if row.id.ends_with(":resolved")
            && previous["status"] != json!(ToolRequestStatus::Completed)
        {
            return Err(fail(row));
        }
        return Ok(());
    }
    let id = previous
        .as_ref()
        .and_then(|v| v["id"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(RequestIdentity::issue_root);
    let delivery = reply.as_ref().and_then(|v| v["delivery"].as_str());
    let (status, delivery_status) = match delivery {
        Some("prepared") => (ToolRequestStatus::Running, Some(OutboxStatus::Pending)),
        Some("submitted") => (ToolRequestStatus::Running, Some(OutboxStatus::Submitted)),
        Some("unknown") => (ToolRequestStatus::Unknown, Some(OutboxStatus::Unknown)),
        Some("written") => (ToolRequestStatus::Completed, Some(OutboxStatus::Committed)),
        Some("retired" | "resolved") => (ToolRequestStatus::Completed, None),
        None if resolved.is_some() => (ToolRequestStatus::Completed, None),
        None if card.is_some() => (ToolRequestStatus::Queued, None),
        _ => return Err(fail(row)),
    };
    if resolved.is_some()
        && matches!(
            delivery_status,
            Some(OutboxStatus::Submitted | OutboxStatus::Unknown)
        )
    {
        return Err(fail(row));
    }
    let card_id = card
        .as_ref()
        .and_then(|v| v.get("card"))
        .cloned()
        .or_else(|| saved.get("card").cloned())
        .unwrap_or(Value::Null);
    let tool = saved
        .get("tool_request")
        .cloned()
        .or_else(|| saved.get("toolRequest").cloned())
        .unwrap_or(Value::Null);
    let mut origin = response_identity.clone();
    origin["attempt"] = json!("");
    let mut owner = json!({"id":id,"kind":ToolRequestKind::ProviderCallback,"originKey":key,"status":status,"updated":cutover,"payload":{"identity":origin,"signature":signature,"card":card_id,"tool_request":tool,"credential_refresh_admission":null,"admission":admission,"reply_identity":if reply.is_some(){response_identity.clone()}else{Value::Null},"frame_fingerprint":null}});
    if let Some(previous) = &previous
        && (previous["payload"]["signature"] != signature
            || previous["payload"]["identity"] != owner["payload"]["identity"]
            || previous["payload"]["admission"] != owner["payload"]["admission"])
    {
        return Err(fail(row));
    }
    put_task(tx, &owner)?;
    if let Some(status) = delivery_status {
        let saved = reply.as_ref().ok_or_else(|| fail(row))?;
        let body = &saved["body"];
        if !body.is_object()
            || body.as_object().is_none_or(|v| v.len() != 1)
            || !(body.get("result").is_some() || body.get("error").is_some())
        {
            return Err(fail(row));
        }
        if response_identity.get("attempt").is_none() {
            response_identity["attempt"] = json!("");
        }
        let payload = json!({"identity":response_identity,"body":body,"card":card_id,"tool_request":tool,"unanswered":saved["unanswered"],"delete_requested_at":saved["deleteRequestedAt"],"frame_fingerprint":null});
        if status == OutboxStatus::Committed {
            return settle_written(tx, row, &mut owner, base, signature, payload, cutover);
        }
        let context = &response_identity["context"];
        import_outbox(
            tx,
            &OutboxRow {
                id: key,
                kind: OutboxKind::ProviderCallback,
                owner_kind: OwnerKind::Task,
                owner_id: id,
                request_id: Some(base.into()),
                signature: signature.into(),
                status,
                account: context["account"].as_str().map(str::to_owned),
                stream_id: context["stream_id"].as_str().map(str::to_owned),
                generation: context["generation"].as_i64(),
                ordinal: None,
                next_at: if status == OutboxStatus::Pending {
                    Some(0)
                } else {
                    None
                },
                payload,
                created: cutover,
            },
        )?;
    }
    Ok(())
}

fn frame(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let (base, phase) = row.id.rsplit_once(':').ok_or_else(|| fail(row))?;
    if phase != "frame" || row.signature != phase {
        return Err(fail(row));
    }
    let saved = load(tx, &format!("{base}:frame"))?;
    let saved = saved.ok_or_else(|| fail(row))?;
    let operation = saved["operation"].as_str().ok_or_else(|| fail(row))?;
    let (old_origin, attempt) = operation.rsplit_once(':').ok_or_else(|| fail(row))?;
    let account = saved["scope"]["account"]
        .as_str()
        .ok_or_else(|| fail(row))?;
    if base
        != format!(
            "account-worker:retained-frame:{account}:{}",
            crate::sha256_hex(operation.as_bytes())
        )
    {
        return Err(fail(row));
    }
    let mut destinations = Outbox::for_request::<Value>(tx, old_origin).map_err(|_| fail(row))?;
    if destinations.len() > 1 {
        return Err(fail(row));
    }
    let mut response = if let Some(response) = destinations.pop() {
        response
    } else {
        // A Written canonical row has already retired. Its exact immutable
        // result and owner fence identify the remaining transport sibling.
        let (signature, raw, created): (String, String, i64) = tx
            .query_row(
                "SELECT signature,result,created FROM request_results WHERE id=?1",
                [old_origin],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| fail(row))?;
        let envelope: ResultEnvelope<Value> = serde_json::from_str(&raw).map_err(|_| fail(row))?;
        let (key, _identity) = identity(tx, row, envelope.reply["identity"].clone())?;
        let owner = task(tx, &key)?.ok_or_else(|| fail(row))?;
        if owner["status"] != json!(ToolRequestStatus::Completed) {
            return Err(fail(row));
        }
        let context = &envelope.reply["identity"]["context"];
        OutboxRow {
            id: key,
            kind: OutboxKind::ProviderCallback,
            owner_kind: OwnerKind::Task,
            owner_id: owner["id"].as_str().ok_or_else(|| fail(row))?.into(),
            request_id: Some(old_origin.into()),
            signature,
            status: OutboxStatus::Committed,
            account: context["account"].as_str().map(str::to_owned),
            stream_id: context["stream_id"].as_str().map(str::to_owned),
            generation: context["generation"].as_i64(),
            ordinal: None,
            next_at: None,
            payload: envelope.reply,
            created,
        }
    };
    if response.kind != OutboxKind::ProviderCallback
        || response.payload["identity"]["attempt"] != attempt
    {
        return Err(fail(row));
    }
    let bytes = saved["frame"].as_str().ok_or_else(|| fail(row))?;
    let encoded: Value = serde_json::from_str(bytes).map_err(|_| fail(row))?;
    if encoded["id"] != response.payload["identity"]["rpc_id"]
        || (encoded.get("result").is_some() == encoded.get("error").is_some())
    {
        return Err(fail(row));
    }
    let body = if let Some(result) = encoded.get("result") {
        json!({"result":result})
    } else {
        json!({"error":encoded["error"]})
    };
    if response.payload["body"] != body {
        return Err(fail(row));
    }
    let fingerprint = crate::sha256_hex(bytes.as_bytes());
    if response.payload["frame_fingerprint"]
        .as_str()
        .is_some_and(|old| old != fingerprint)
    {
        return Err(fail(row));
    }
    let written = load(tx, &format!("{base}:written"))?;
    response.status = match written.as_ref().and_then(Value::as_str) {
        Some("written") => OutboxStatus::Committed,
        Some("unknown") | None if response.status == OutboxStatus::Committed => {
            OutboxStatus::Committed
        }
        Some("not-sent") if response.status == OutboxStatus::Committed => return Err(fail(row)),
        Some("unknown") | None => OutboxStatus::Unknown,
        Some("not-sent") => OutboxStatus::Pending,
        _ => return Err(fail(row)),
    };
    response.next_at = if response.status == OutboxStatus::Pending {
        Some(0)
    } else {
        None
    };
    response.payload["frame_fingerprint"] = json!(fingerprint);
    let mut owner = task(tx, &response.id)?.ok_or_else(|| fail(row))?;
    owner["payload"]["frame_fingerprint"] = json!(fingerprint);
    owner["status"] = json!(match response.status {
        OutboxStatus::Committed => ToolRequestStatus::Completed,
        OutboxStatus::Pending => ToolRequestStatus::Running,
        OutboxStatus::Submitted | OutboxStatus::Unknown | OutboxStatus::Unrecognized =>
            ToolRequestStatus::Unknown,
    });
    if response.status == OutboxStatus::Committed {
        settle_written(
            tx,
            row,
            &mut owner,
            old_origin,
            &response.signature,
            response.payload.clone(),
            cutover,
        )?;
    }
    tx.execute(
        "DELETE FROM runtime_operation_receipts WHERE id=?1 AND signature='written'",
        [format!("{base}:written")],
    )?;
    put_task(tx, &owner)?;
    if response.status == OutboxStatus::Committed {
        Ok(())
    } else {
        import_outbox(tx, &response)
    }
}

fn unanswered(tx: &Transaction<'_>, row: &SourceReceipt, cutover: i64) -> Result<()> {
    let (base, phase) = row.id.rsplit_once(':').ok_or_else(|| fail(row))?;
    let operation = base
        .strip_prefix("account-worker:")
        .ok_or_else(|| fail(row))?;
    if !matches!(
        phase,
        "accepted" | "submitted" | "unknown" | "recoverable" | "replayed" | "written" | "reply"
    ) {
        return Err(fail(row));
    }
    if phase == "reply" {
        let key: String = row.json(TABLE)?;
        let proof:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_outbox WHERE request_id=?1 UNION ALL SELECT 1 FROM request_results WHERE id=?1)",[key],|r|r.get(0))?;
        return if proof { Ok(()) } else { Err(fail(row)) };
    }
    if row.signature != phase {
        return Err(fail(row));
    }
    let accepted = load(tx, &format!("{base}:accepted"))?;
    let Some(accepted) = accepted else {
        let previous = task(tx, operation)?.ok_or_else(|| fail(row))?;
        let value: Value = row.json(TABLE)?;
        let completed = previous["status"] == json!(ToolRequestStatus::Completed);
        let completed_fingerprint: Option<String> = if completed {
            tx.query_row("SELECT json_extract(record,'$.payload.frame_fingerprint') FROM runtime_tool_requests WHERE id=?1",[previous["payload"]["root_request_id"].as_str().ok_or_else(||fail(row))?],|r|r.get(0)).optional()?.flatten()
        } else {
            None
        };
        let valid = match phase {
            "submitted" => {
                value == true
                    && (previous["status"] == json!(ToolRequestStatus::Unknown) || completed)
                    || value["sha256"] == previous["payload"]["submitted"]["sha256"]
                    || completed_fingerprint
                        .as_deref()
                        .is_some_and(|fp| value["sha256"].as_str() == Some(fp))
            }
            "unknown" => {
                value == true
                    && (previous["status"] == json!(ToolRequestStatus::Unknown) || completed)
            }
            "recoverable" => value == true && previous["payload"]["recovery_authorized"] == true,
            "replayed" => value == true && previous["payload"]["recovery_attempted"] == true,
            "written" => value == true && previous["status"] == json!(ToolRequestStatus::Completed),
            _ => false,
        };
        return if valid { Ok(()) } else { Err(fail(row)) };
    };
    let account = accepted["account"].as_str().ok_or_else(|| fail(row))?;
    let stream = accepted["journalScope"].as_str().ok_or_else(|| fail(row))?;
    let generation = accepted["generation"]
        .as_i64()
        .filter(|v| *v >= 0)
        .ok_or_else(|| fail(row))?;
    let cursor = accepted["cursor"].as_u64().ok_or_else(|| fail(row))?;
    if operation != format!("callback:{stream}:{generation}:{cursor}") {
        return Err(fail(row));
    }
    let request_bytes = accepted["frame"].as_str().ok_or_else(|| fail(row))?;
    let request: Value = serde_json::from_str(request_bytes).map_err(|_| fail(row))?;
    if request["id"] != accepted["requestId"] || request["method"] != accepted["method"] {
        return Err(fail(row));
    }
    let submitted = load(tx, &format!("{base}:submitted"))?;
    let mut flags = Vec::new();
    for marker in ["unknown", "recoverable", "replayed", "written"] {
        let value = load(tx, &format!("{base}:{marker}"))?;
        if value.as_ref().is_some_and(|value| value != &json!(true)) {
            return Err(fail(row));
        }
        flags.push(value.is_some());
    }
    let [unknown, authorized, attempted, written] = flags.as_slice() else {
        return Err(fail(row));
    };
    let mut state = if *written {
        ToolRequestStatus::Completed
    } else if *unknown || submitted == Some(json!(true)) {
        ToolRequestStatus::Unknown
    } else if submitted.is_some() {
        ToolRequestStatus::Running
    } else {
        ToolRequestStatus::Queued
    };
    let mut context=submitted.as_ref().and_then(|v|v.pointer("/authority/context")).cloned().unwrap_or_else(||json!({"account":account,"agent":accepted["actor"].as_str().unwrap_or(""),"generation":generation,"epoch":accepted["epoch"].as_u64().unwrap_or(0),"thread":request["params"]["threadId"].as_str().unwrap_or(""),"turn":request["params"]["turnId"].as_str().unwrap_or(""),"connection":submitted.as_ref().and_then(|v|v["handle"].as_str()).unwrap_or("")}));
    context["stream_id"] = json!(stream);
    let c = &context;
    let key = format!(
        "provider-callback:{}",
        crate::sha256_hex(
            json!([
                account,
                generation,
                stream,
                c["agent"],
                c["epoch"],
                c["thread"],
                c["turn"],
                accepted["requestId"]
            ])
            .to_string()
            .as_bytes()
        )
    );
    let signature = crate::sha256_hex(
        json!([
            request["method"],
            request.get("params").cloned().unwrap_or(Value::Null)
        ])
        .to_string()
        .as_bytes(),
    );
    let mut owner=task(tx,&key)?.unwrap_or_else(|| {
        let id=RequestIdentity::issue_root();
        json!({"id":id,"kind":ToolRequestKind::ProviderCallback,"originKey":key,"status":state,"updated":cutover,"payload":{"identity":{"context":context,"rpc_id":accepted["requestId"],"attempt":""},"signature":signature,"card":null,"tool_request":null,"credential_refresh_admission":null,"admission":admitted_owner_token(&format!("{base}:accepted"),&signature,cutover),"reply_identity":null,"frame_fingerprint":null}})
    });
    let canonical_id = owner["id"].as_str().ok_or_else(|| fail(row))?.to_owned();
    if owner["status"] == json!(ToolRequestStatus::Completed) {
        state = ToolRequestStatus::Completed;
    }
    let saved = if let Some(saved) = submitted.as_ref().filter(|value| value.is_object()) {
        if saved["version"] != 1
            || saved["generation"].as_i64() != Some(generation)
            || saved["pid"].as_u64().is_none()
        {
            return Err(fail(row));
        }
        let bytes = saved["frame"].as_str().ok_or_else(|| fail(row))?;
        let fingerprint = crate::sha256_hex(bytes.as_bytes());
        if saved["sha256"] != fingerprint {
            return Err(fail(row));
        }
        let response: Value = serde_json::from_str(bytes).map_err(|_| fail(row))?;
        if response["id"] != accepted["requestId"]
            || (response.get("result").is_some() == response.get("error").is_some())
        {
            return Err(fail(row));
        }
        let old = Outbox::get::<Value>(tx, &key).map_err(|_| fail(row))?;
        let result_id = owner["payload"]["admission"]["id"]
            .as_str()
            .ok_or_else(|| fail(row))?
            .to_owned();
        let retained = if state == ToolRequestStatus::Completed {
            let raw: Option<String> = tx
                .query_row(
                    "SELECT result FROM request_results WHERE id=?1",
                    [&result_id],
                    |r| r.get(0),
                )
                .optional()?;
            raw.map(|raw| {
                serde_json::from_str::<ResultEnvelope<Value>>(&raw)
                    .map(|envelope| envelope.reply)
                    .map_err(|_| fail(row))
            })
            .transpose()?
        } else {
            None
        };
        let mut reply_identity = old
            .as_ref()
            .map(|v| v.payload["identity"].clone())
            .or_else(|| retained.as_ref().map(|v| v["identity"].clone()))
            .or_else(|| {
                owner["payload"]["reply_identity"]
                    .is_object()
                    .then(|| owner["payload"]["reply_identity"].clone())
            })
            .unwrap_or_else(
                || json!({"context":context,"rpc_id":accepted["requestId"],"attempt":fingerprint}),
            );
        reply_identity["context"]["stream_id"] = json!(stream);
        let body = if let Some(value) = response.get("result") {
            json!({"result":value})
        } else {
            json!({"error":response["error"]})
        };
        if old.as_ref().is_some_and(|old| old.payload["body"] != body)
            || retained.as_ref().is_some_and(|saved| saved["body"] != body)
        {
            return Err(fail(row));
        }
        let payload = if let Some(mut old) = old.as_ref().map(|r| r.payload.clone()).or(retained) {
            old["frame_fingerprint"] = json!(fingerprint);
            old
        } else {
            json!({"identity":reply_identity,"body":body,"card":null,"tool_request":null,"unanswered":null,"delete_requested_at":null,"frame_fingerprint":fingerprint})
        };
        let delivery = if state == ToolRequestStatus::Completed {
            OutboxStatus::Committed
        } else if *unknown {
            OutboxStatus::Unknown
        } else {
            OutboxStatus::Submitted
        };
        let request_id = owner["payload"]["admission"]["id"]
            .as_str()
            .ok_or_else(|| fail(row))?
            .to_owned();
        if delivery == OutboxStatus::Committed {
            settle_written(
                tx,
                row,
                &mut owner,
                &request_id,
                &signature,
                payload,
                cutover,
            )?;
        } else {
            import_outbox(
                tx,
                &OutboxRow {
                    id: key.clone(),
                    kind: OutboxKind::ProviderCallback,
                    owner_kind: OwnerKind::Task,
                    owner_id: canonical_id.clone(),
                    request_id: Some(request_id),
                    signature: signature.clone(),
                    status: delivery,
                    account: Some(account.into()),
                    stream_id: Some(stream.into()),
                    generation: Some(generation),
                    ordinal: None,
                    next_at: None,
                    payload,
                    created: cutover,
                },
            )?;
        }
        owner["payload"]["reply_identity"] = reply_identity;
        owner["payload"]["frame_fingerprint"] = json!(fingerprint);
        Some(
            json!({"handle":saved["handle"],"generation":generation,"pid":saved["pid"],"response_key":key,"sha256":fingerprint,"authority":saved["authority"]}),
        )
    } else {
        None
    };
    owner["status"] = json!(state);
    put_task(tx, &owner)?;
    let previous = task(tx, operation)?;
    let id = previous
        .as_ref()
        .and_then(|v| v["id"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(RequestIdentity::issue_root);
    let mut stored_accepted = accepted.clone();
    let saved = if state == ToolRequestStatus::Completed {
        stored_accepted["frame"] = json!("");
        None
    } else {
        saved
    };
    let task = json!({"id":id,"kind":ToolRequestKind::ProviderCallback,"originKey":operation,"status":state,"updated":cutover,"payload":{"accepted":stored_accepted,"submitted":saved,"recovery_authorized":authorized,"recovery_attempted":attempted,"request_fingerprint":crate::sha256_hex(request_bytes.as_bytes()),"root_request_id":canonical_id}});
    if !stream.contains(":vm:") {
        tx.execute("INSERT INTO runtime_account_state(account,next_generation,journal_ack,recovery_required,streams) VALUES(?1,?2,0,0,'null') ON CONFLICT(account) DO UPDATE SET next_generation=max(next_generation,excluded.next_generation)",params![account,generation])?;
    }
    put_task(tx, &task)
}
pub(super) struct Family;
pub(super) const SOURCES: &[&str] = &["callbacks", "frames", "frame-outcomes", "unanswered"];
impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-provider-callbacks"
    }
    fn prepare(&self, tx: &Transaction<'_>, _cutover_ms: i64) -> Result<()> {
        crate::schema::prepare_callback_origin(tx)?;
        Ok(())
    }
    fn move_batch(
        &self,
        tx: &Transaction<'_>,
        source: &str,
        checkpoint: &MigrationCheckpoint,
    ) -> Result<MigrationCheckpoint> {
        let predicate = match source {
            "callbacks" => "source.id GLOB 'provider-callback:*'",
            "frames" => "source.id GLOB 'account-worker:retained-frame:*:frame'",
            "frame-outcomes" => "source.id GLOB 'account-worker:retained-frame:*'",
            "unanswered" => "source.id GLOB 'account-worker:callback:*'",
            _ => {
                return Err(ReceiptMigrationError::Incomplete {
                    migration: self.id().into(),
                    source_table: source.into(),
                });
            }
        };
        let rows = read_receipt_batch(tx, TABLE, checkpoint, predicate)?;
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        let mut budget = BatchBudget::default();
        for row in rows {
            if source == "callbacks" || source == "unanswered" {
                let base = if source == "callbacks" {
                    row.id
                        .strip_suffix(":card")
                        .or_else(|| row.id.strip_suffix(":resolved"))
                        .unwrap_or(&row.id)
                } else {
                    row.id
                        .rsplit_once(':')
                        .map(|(base, _)| base)
                        .ok_or_else(|| fail(&row))?
                };
                let (count,bytes):(usize,usize)=tx.query_row("SELECT count(*),coalesce(sum(length(CAST(id AS BLOB))+length(CAST(signature AS BLOB))+length(CAST(result AS BLOB))),0) FROM runtime_operation_receipts WHERE id IN (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![base,format!("{base}:card"),format!("{base}:resolved"),format!("{base}:accepted"),format!("{base}:submitted"),format!("{base}:recoverable"),format!("{base}:replayed"),format!("{base}:unknown"),format!("{base}:written"),format!("{base}:reply")],|r|Ok((r.get::<_,i64>(0)? as usize,r.get::<_,i64>(1)? as usize)))?;
                let count = count.max(1);
                if budget.rows + count > MIGRATION_BATCH_ROWS
                    || !budget.include(bytes.max(row.result.len()))
                {
                    break;
                }
                for _ in 1..count {
                    if !budget.include(0) {
                        return Err(fail(&row));
                    }
                }
            }
            // A transport frame may consume its one small outcome sibling too.
            // Reserve both source rows, retaining the scanned frame cursor.
            if source == "frames" {
                let sibling_bytes:i64=tx.query_row("SELECT coalesce(sum(length(id)+length(signature)+length(result)),0) FROM runtime_operation_receipts WHERE id=?1",[row.id.strip_suffix(":frame").map(|base|format!("{base}:written")).ok_or_else(||fail(&row))?],|r|r.get(0))?;
                if budget.rows + 2 > MIGRATION_BATCH_ROWS
                    || !budget.include(
                        row.id.len()
                            + row.signature.len()
                            + row.result.len()
                            + sibling_bytes as usize,
                    )
                {
                    break;
                }
                if !budget.include(0) {
                    break;
                }
            }
            match source {
                "callbacks" => canonical(tx, &row, checkpoint.cutover_ms)?,
                "frames" => frame(tx, &row, checkpoint.cutover_ms)?,
                "unanswered" => unanswered(tx, &row, checkpoint.cutover_ms)?,
                _ => return Err(fail(&row)),
            }
            delete_source(tx, TABLE, row.rowid)?;
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
    #[test]
    fn callback_key_bytes_match_legacy_json_for_rpc_variants() {
        for rpc in [
            json!(17),
            json!(-9),
            json!(1.25),
            json!("escaped \"\\\n 東京"),
        ] {
            let value = json!({"context":{"account":"a\\\"","agent":"b\n","generation":7,"epoch":3,"thread":"t","turn":"u","connection":"c"},"rpc_id":rpc});
            let LegacyIdentity { context: c, rpc_id } =
                LegacyIdentity::deserialize(&value).unwrap();
            let bytes = serde_json::to_vec(&(
                &c.account,
                c.generation,
                "stream",
                &c.agent,
                c.epoch,
                &c.thread,
                &c.turn,
                &rpc_id,
            ))
            .unwrap();
            assert_eq!(
                bytes,
                json!(["a\\\"", 7, "stream", "b\n", 3, "t", "u", rpc])
                    .to_string()
                    .as_bytes()
            );
        }
    }
    fn database() -> rusqlite::Connection {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("transaction");
        prepare_schema(&tx).expect("schema");
        tx.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);CREATE TABLE runtime_tool_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL)").expect("source");
        Family.prepare(&tx, 1000).expect("prepare callbacks");
        assert!(tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name='runtime_tool_request_origin')", [], |row| row.get::<_,bool>(0)).expect("origin index"));
        tx.commit().expect("commit");
        db
    }
    fn fixture(tx: &Transaction<'_>, rpc: u64, delivery: &str) -> String {
        let identity = json!({"context":{"agent":"agent","account":"default","generation":7,"epoch":3,"thread":"thread","turn":"turn","connection":"original"},"rpc_id":rpc,"attempt":"attempt"});
        let key = old_key(&identity);
        let saved = json!({"identity":identity,"signature":"input-signature","body":{"result":{"approved":true}},"delivery":delivery,"journalCursor":9});
        tx.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'provider-callback',?2)",
            params![key, saved.to_string()],
        )
        .expect("callback");
        key
    }
    fn move_all(tx: &Transaction<'_>) {
        for source in SOURCES {
            let mut cp =
                MigrationCheckpoint::begin(tx, Family.id(), source, 1000).expect("checkpoint");
            while cp.status != MigrationStatus::Complete {
                cp = Family.move_batch(tx, source, &cp).expect("move");
                cp.save(tx, Family.id(), source, 1000).expect("save");
            }
        }
    }
    #[test]
    fn receipt_provider_callbacks_written_import_settles_raw_reply_and_owner_without_outbox() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let old = fixture(&tx, 17, "written");
        move_all(&tx);
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("no completed body"),
            0
        );
        let raw: String = tx
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [&old],
                |r| r.get(0),
            )
            .expect("original raw result");
        let envelope: Value = serde_json::from_str(&raw).expect("envelope");
        assert_eq!(
            envelope["reply"]["body"],
            json!({"result":{"approved":true}})
        );
        let owner = task(&tx, &new_key(&envelope["reply"]["identity"]))
            .expect("owner")
            .expect("fence");
        assert_eq!(owner["status"], json!(ToolRequestStatus::Completed));
        assert!(owner["payload"]["frame_fingerprint"].as_str().is_some());
        let before = owner;
        move_all(&tx);
        assert_eq!(
            task(&tx, before["originKey"].as_str().expect("key"))
                .expect("owner")
                .expect("fence"),
            before
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("no resurrection"),
            0
        );
    }
    fn new_key(identity: &Value) -> String {
        let c = &identity["context"];
        format!(
            "provider-callback:{}",
            crate::sha256_hex(
                json!([
                    c["account"],
                    c["generation"],
                    c["stream_id"],
                    c["agent"],
                    c["epoch"],
                    c["thread"],
                    c["turn"],
                    identity["rpc_id"]
                ])
                .to_string()
                .as_bytes()
            )
        )
    }
    #[test]
    fn receipt_provider_callbacks_late_written_frame_settles_unknown_and_removes_outbox() {
        for delivery in ["unknown", "written"] {
            let mut db = database();
            let tx = db.transaction().expect("transaction");
            let old = fixture(&tx, 17, delivery);
            let operation = format!("{old}:attempt");
            let base = format!(
                "account-worker:retained-frame:default:{}",
                crate::sha256_hex(operation.as_bytes())
            );
            let bytes = String::from_utf8(
                studio_runtime_providers::encode_line(&studio_runtime_providers::Frame::Response {
                    id: studio_runtime_providers::RequestId::Number(17),
                    body: studio_runtime_providers::ResponseBody::Result(json!({"approved":true})),
                    fields: Default::default(),
                })
                .expect("encode"),
            )
            .expect("UTF8");
            tx.execute("INSERT INTO runtime_operation_receipts VALUES(?1,'frame',?2)",params![format!("{base}:frame"),json!({"scope":{"account":"default","generation":7,"connection":"original"},"operation":operation,"frame":bytes}).to_string()]).expect("transport frame");
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'written','\"written\"')",
                [format!("{base}:written")],
            )
            .expect("late Written");
            move_all(&tx);
            assert_eq!(
                tx.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                    .get::<_, i64>(0))
                    .expect("no completed body"),
                0
            );
            let raw: String = tx
                .query_row(
                    "SELECT result FROM request_results WHERE id=?1",
                    [&old],
                    |r| r.get(0),
                )
                .expect("exact result");
            let envelope: Value = serde_json::from_str(&raw).expect("envelope");
            assert_eq!(
                envelope["reply"]["frame_fingerprint"],
                crate::sha256_hex(bytes.as_bytes())
            );
            let owner = task(&tx, &new_key(&envelope["reply"]["identity"]))
                .expect("owner")
                .expect("fence");
            assert_eq!(owner["status"], json!(ToolRequestStatus::Completed));
            move_all(&tx);
            assert_eq!(
                tx.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                    .get::<_, i64>(0))
                    .expect("no resurrection"),
                0
            );
        }
    }
    #[test]
    fn receipt_provider_callbacks_unknown_import_is_one_stable_fence_on_reopen() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let old = fixture(&tx, 17, "unknown");
        let mut cp =
            MigrationCheckpoint::begin(&tx, Family.id(), "callbacks", 1000).expect("begin");
        cp = Family.move_batch(&tx, "callbacks", &cp).expect("move");
        cp.save(&tx, Family.id(), "callbacks", 1000).expect("save");
        let rows = Outbox::for_request::<Value>(&tx, &old).expect("canonical response");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, OutboxStatus::Unknown);
        assert_eq!(rows[0].next_at, None);
        assert!(rows[0].payload.get("journal_cursor").is_none());
        let before: Value = task(&tx, &rows[0].id).expect("task").expect("owner");
        assert!(before["payload"].get("journal_cursor").is_none());
        let after = Family
            .move_batch(&tx, "callbacks", &cp)
            .expect("second open");
        assert_eq!(after.status, MigrationStatus::Complete);
        assert_eq!(
            task(&tx, &rows[0].id).expect("task").expect("owner"),
            before
        );
        let plan:String=tx.query_row("EXPLAIN QUERY PLAN SELECT id FROM runtime_outbox WHERE request_id='callback' ORDER BY id",[],|r|r.get(3)).expect("plan");
        assert!(plan.contains("runtime_outbox_request"));
    }
    #[test]
    fn receipt_provider_callbacks_crash_between_batches_rolls_back_owner_and_cursor() {
        let mut db = database();
        {
            let tx = db.transaction().expect("transaction");
            for rpc in 0..=MIGRATION_BATCH_ROWS {
                fixture(&tx, rpc as u64, "unknown");
            }
            tx.commit().expect("fixtures");
        }
        {
            let tx = db.transaction().expect("transaction");
            let cp =
                MigrationCheckpoint::begin(&tx, Family.id(), "callbacks", 1000).expect("begin");
            let next = Family
                .move_batch(&tx, "callbacks", &cp)
                .expect("first batch");
            next.save(&tx, Family.id(), "callbacks", 1000)
                .expect("checkpoint");
            tx.commit().expect("commit batch");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("remainder"),
            1
        );
        {
            let tx = db.transaction().expect("transaction");
            let cp = MigrationCheckpoint::load(&tx, Family.id(), "callbacks")
                .expect("load")
                .expect("checkpoint");
            let next = Family
                .move_batch(&tx, "callbacks", &cp)
                .expect("second batch");
            next.save(&tx, Family.id(), "callbacks", 1000)
                .expect("save");
            tx.rollback().expect("crash");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("rollback"),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_tool_requests", [], |r| r
                .get::<_, i64>(0))
                .expect("owners"),
            MIGRATION_BATCH_ROWS as i64
        );
        let tx = db.transaction().expect("transaction");
        let cp = MigrationCheckpoint::load(&tx, Family.id(), "callbacks")
            .expect("load")
            .expect("checkpoint");
        Family.move_batch(&tx, "callbacks", &cp).expect("resume");
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_tool_requests", [], |r| r
                .get::<_, i64>(0))
                .expect("owners"),
            (MIGRATION_BATCH_ROWS + 1) as i64
        );
    }
    #[test]
    fn receipt_provider_callbacks_saved_recovery_authorization_and_count_survive() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let request = "{\"id\":17,\"method\":\"native/unsupported\",\"params\":{}}\n";
        let response = "{\"id\":17,\"error\":{\"code\":-1}}\n";
        let accepted = json!({"account":"default","journalScope":"default","generation":7,"cursor":9,"requestId":17,"method":"native/unsupported","frame":request,"actor":null,"epoch":null});
        let saved = json!({"version":1,"handle":"account:default","generation":7,"pid":123,"frame":response,"sha256":crate::sha256_hex(response.as_bytes())});
        for (phase, value) in [
            ("accepted", accepted),
            ("submitted", saved),
            ("recoverable", json!(true)),
            ("replayed", json!(true)),
            ("unknown", json!(true)),
        ] {
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                params![
                    format!("account-worker:callback:default:7:9:{phase}"),
                    phase,
                    value.to_string()
                ],
            )
            .expect("phase");
        }
        let cp = MigrationCheckpoint::begin(&tx, Family.id(), "unanswered", 1000).expect("begin");
        Family.move_batch(&tx, "unanswered", &cp).expect("move");
        let task = task(&tx, "callback:default:7:9")
            .expect("task")
            .expect("obligation");
        assert_eq!(task["status"], json!(ToolRequestStatus::Unknown));
        assert_eq!(task["payload"]["recovery_authorized"], true);
        assert_eq!(task["payload"]["accepted"]["cursor"], 9);
        assert_eq!(task["payload"]["recovery_attempted"], true);
        assert_eq!(task["payload"]["submitted"]["frame"], Value::Null);
        assert_eq!(
            task["payload"]["request_fingerprint"],
            crate::sha256_hex(request.as_bytes())
        );
    }
    #[test]
    fn receipt_provider_callbacks_written_obligation_has_no_response_body_outbox() {
        let mut db = database();
        let tx = db.transaction().expect("transaction");
        let request = "{\"id\":17,\"method\":\"native/unsupported\",\"params\":{}}\n";
        let response = "{\"id\":17,\"error\":{\"code\":-1}}\n";
        let accepted = json!({"account":"default","journalScope":"default","generation":7,"cursor":9,"requestId":17,"method":"native/unsupported","frame":request,"actor":null,"epoch":null});
        let saved = json!({"version":1,"handle":"account:default","generation":7,"pid":123,"frame":response,"sha256":crate::sha256_hex(response.as_bytes())});
        for (phase, value) in [
            ("accepted", accepted),
            ("submitted", saved),
            ("written", json!(true)),
        ] {
            tx.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                params![
                    format!("account-worker:callback:default:7:9:{phase}"),
                    phase,
                    value.to_string()
                ],
            )
            .expect("phase");
        }
        move_all(&tx);
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("no completed body"),
            0
        );
        let obligation = task(&tx, "callback:default:7:9")
            .expect("obligation")
            .expect("fence");
        assert_eq!(obligation["status"], json!(ToolRequestStatus::Completed));
        assert_eq!(obligation["payload"]["accepted"]["frame"], "");
        assert_eq!(obligation["payload"]["submitted"], Value::Null);
        move_all(&tx);
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("no resurrection"),
            0
        );
    }
    #[test]
    fn receipt_provider_callbacks_rejects_unknown_phase_without_deleting_source() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("transaction");
        prepare_schema(&tx).expect("schema");
        tx.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);CREATE TABLE runtime_tool_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL)").expect("source");
        tx.execute("INSERT INTO runtime_operation_receipts VALUES('provider-callback:opaque:bogus','provider-callback','{}')",[]).expect("fixture");
        let cp =
            MigrationCheckpoint::begin(&tx, Family.id(), "callbacks", 1000).expect("checkpoint");
        assert!(matches!(
            Family.move_batch(&tx, "callbacks", &cp),
            Err(ReceiptMigrationError::Unclassifiable { .. })
        ));
        assert_eq!(
            tx.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source"),
            1
        );
    }
}
