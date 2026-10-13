//! Sparse retained local sign-in waits. No auth command or provider request is run here.
use crate::continuation::{
    Decision, POLL_SECONDS, Result, Snapshot, account, flag, now_checked, patch, present, text,
};
use serde_json::{Value, json};
const ERROR: &str = "Cannot read Claude Code sign-in status";
const SOURCE_FIELDS: [&str; 17] = [
    "id",
    "epoch",
    "accountKey",
    "threadId",
    "rootId",
    "parentId",
    "provider",
    "cwd",
    "branch",
    "role",
    "isLead",
    "imageWorkspace",
    "imageWorkspaceReady",
    "compactions",
    "claudeOptions",
    "pendingSettings",
    "pendingSettingsAccountKey",
];
const PIN_FIELDS: [&str; 10] = [
    "id",
    "provider",
    "home",
    "claudeOptions",
    "accountId",
    "_credentialIdentity",
    "deleted",
    "disconnected",
    "duplicateOf",
    "updatedAt",
];
fn projection(record: &Value, fields: &[&str]) -> Value {
    fields
        .iter()
        .map(|key| {
            (
                (*key).to_owned(),
                record.get(*key).cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}
pub fn account_pin(record: &Value) -> Value {
    projection(record, &PIN_FIELDS)
}
fn source(s: &Snapshot, settings: &Value) -> Value {
    let mut v = projection(&s.agent, &SOURCE_FIELDS);
    v["settings"] = settings.clone();
    v
}
fn digest(v: &Value) -> String {
    studio_runtime_store::sha256_hex(crate::event_write::python_json_compact(v).as_bytes())
}
fn inputs(s: &Snapshot) -> Option<Value> {
    let mut events: Vec<&Value> = s
        .events
        .iter()
        .filter(|e| {
            matches!(
                text(e, "status"),
                "pending" | "reserved" | "dispatching" | "uncertain"
            )
        })
        .collect();
    if events.len() > 64 {
        return None;
    }
    events.sort_by(|a, b| {
        a["created"]
            .as_f64()
            .partial_cmp(&b["created"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| text(a, "id").cmp(text(b, "id")))
    });
    let mut result = Vec::new();
    for e in events {
        if !matches!(text(e, "status"), "pending" | "reserved") || present(e, "turn_id") {
            return None;
        }
        let metadata = s
            .metadata
            .get(text(e, "id"))
            .and_then(Option::as_ref)
            .cloned()
            .unwrap_or(Value::Null);
        let mut assets = Vec::new();
        if let Some(ids) = metadata.get("assets").and_then(Value::as_array) {
            for id in ids {
                let id = id.as_str()?;
                let asset = s.assets.get(id)?;
                assets.push(json!([id, digest(asset)]));
            }
        }
        let immutable = projection(
            e,
            &["id", "agent", "epoch", "kind", "text", "created", "turn_id"],
        );
        result.push(json!({"id":e["id"],"digest":digest(&json!([immutable,metadata,assets]))}));
    }
    Some(json!(result))
}
fn eligible(s: &Snapshot, r: &Value, settings: &Value, closed: bool) -> bool {
    let a = &s.agent;
    let attempt = &a["startAttempt"];
    let blocked = present(a, "contextRepairWait")
        || matches!(
            text(&a["contextRepair"], "phase"),
            "preparing" | "submitted" | "unknown" | "ready"
        );
    let native_block = crate::continuation::native_thread_blocked(a);
    if closed
        || [
            "deletedAt",
            "agentArchive",
            "inFlight",
            "turnId",
            "nativeFailureHold",
            "activeTools",
            "workspaceOperation",
            "accountTransferId",
        ]
        .iter()
        .any(|k| present(a, k))
        || !flag(a, "autoWake")
        || !matches!(text(a, "status"), "waiting" | "failed")
        || blocked
        || native_block
        || source(s, settings) != r["source"]
        || attempt != &r["attempt"]
        || attempt.get("submitted") != Some(&Value::Bool(false))
        || ["activeAtReservation", "turnId", "observedTurnId"]
            .iter()
            .any(|k| present(attempt, k))
        || a.get("error") != r.get("error")
        || inputs(s).as_ref() != r.get("inputs")
        || !s.claims_valid(r)
    {
        return false;
    }
    if !attempt
        .get("events")
        .and_then(Value::as_array)
        .is_some_and(|ids| {
            ids.iter().all(|id| {
                s.events.iter().any(|e| {
                    e.get("id") == Some(id)
                        && text(e, "status") == "pending"
                        && !present(e, "turn_id")
                })
            })
        })
    {
        return false;
    }
    s.parents_running(true)
}
/// Capture before a local probe. The account layer injects before/after pins and timeout evidence.
#[derive(Clone)]
pub struct Capture {
    source: Value,
    attempt: Value,
    inputs: Value,
    claims: Vec<String>,
    pin: Value,
}
pub fn capture(s: &Snapshot, settings: &Value, pin: &Value) -> Option<Capture> {
    let attempt = s.agent.get("startAttempt")?;
    if text(&s.agent, "provider") != "claude"
        || attempt.get("submitted") != Some(&Value::Bool(false))
    {
        return None;
    }
    Some(Capture {
        source: source(s, settings),
        attempt: attempt.clone(),
        inputs: inputs(s)?,
        claims: s.claims.clone(),
        pin: account_pin(pin),
    })
}
pub fn record_timeout(
    s: &Snapshot,
    capture: &Capture,
    settings: &Value,
    current_pin: &Value,
    now: f64,
) -> Result<Decision> {
    now_checked(now)?;
    if !present(&capture.attempt, "events") || present(&capture.attempt, "action") {
        return Ok(Decision::default());
    }
    let mut expected = capture.attempt.clone();
    expected["executionOutcome"] = json!("unsent");
    if s.agent.get("startAttempt") != Some(&expected) || account_pin(current_pin) != capture.pin {
        return Ok(Decision::default());
    }
    let receipt = json!({"source":capture.source,"attempt":expected,"inputs":capture.inputs,"taskClaims":capture.claims,"accountPin":capture.pin,"id":format!("claude-auth-probe:{}",text(&capture.attempt,"id")),"agent":s.agent_id(),"error":ERROR,"accountKey":account(&s.agent),"status":"auth_probe_wait","cause":"claude_auth_probe","createdAt":now,"dueAt":now+POLL_SECONDS});
    if !eligible(s, &receipt, settings, false) {
        return Ok(Decision::default());
    }
    let mut agent = s.agent.clone();
    agent["status"] = json!("waiting");
    Ok(Decision {
        agent: Some(agent),
        record: Some(receipt),
        ..Decision::default()
    })
}
pub fn retained(s: &Snapshot, settings: &Value, pin: &Value) -> bool {
    let Some(r) = &s.record else {
        return false;
    };
    text(&s.agent, "provider") == "claude"
        && text(r, "status") == "auth_probe_wait"
        && text(r, "cause") == "claude_auth_probe"
        && text(r, "agent") == s.agent_id()
        && text(r, "error") == ERROR
        && r["createdAt"].as_f64().is_some_and(|created| {
            created > 0.0 && r["dueAt"].as_f64().is_some_and(|due| due >= created)
        })
        && text(r, "id") == format!("claude-auth-probe:{}", text(&s.agent["startAttempt"], "id"))
        && eligible(s, r, settings, false)
        && account_pin(pin) == r["accountPin"]
}
/// A ticket is bound to the exact rescheduled receipt and account pin, and cannot be constructed by callers.
#[derive(Clone)]
pub struct ProbeTicket {
    receipt: Value,
    pin: Value,
}
fn cancelled(r: &Value, reason: &str) -> Decision {
    let mut r = r.clone();
    patch(
        &mut r,
        json!({"status":"cancelled","dueAt":null,"reason":reason}),
    );
    Decision {
        record: Some(r),
        ..Decision::default()
    }
}
/// The scheduler limits concurrent profiles to two; this claim never probes while holding the store.
pub fn claim_probe(
    s: &Snapshot,
    settings: &Value,
    pin: &Value,
    now: f64,
) -> Result<(Decision, Option<ProbeTicket>)> {
    now_checked(now)?;
    let Some(r) = &s.record else {
        return Ok((Decision::default(), None));
    };
    if s.status() != Some(crate::continuation::WaitStatus::AuthProbeWait)
        || r["dueAt"].as_f64().is_none_or(|due| due > now)
    {
        return Ok((Decision::default(), None));
    }
    if !eligible(s, r, settings, false) {
        return Ok((cancelled(r, "The original input or chat changed"), None));
    }
    if account_pin(pin) != r["accountPin"] {
        return Ok((cancelled(r, "The selected account changed"), None));
    }
    let mut receipt = r.clone();
    receipt["dueAt"] = json!(now + POLL_SECONDS);
    let ticket = ProbeTicket {
        receipt: receipt.clone(),
        pin: account_pin(pin),
    };
    Ok((
        Decision {
            record: Some(receipt),
            ..Decision::default()
        },
        Some(ticket),
    ))
}
pub fn apply_probe(
    s: &Snapshot,
    ticket: &ProbeTicket,
    settings: &Value,
    pin: &Value,
    result: &Value,
    now: f64,
) -> Result<Decision> {
    now_checked(now)?;
    let Some(r) = &s.record else {
        return Ok(Decision::default());
    };
    if r != &ticket.receipt || s.status() != Some(crate::continuation::WaitStatus::AuthProbeWait) {
        return Ok(Decision::default());
    }
    if account_pin(pin) != ticket.pin || !eligible(s, r, settings, false) {
        return Ok(cancelled(r, "The original input or chat changed"));
    }
    if text(result, "status") != "ready"
        || ["accountId", "_credentialIdentity"].iter().any(|key| {
            text(&ticket.pin, key).is_empty() || result.get(*key) != ticket.pin.get(*key)
        })
    {
        return Ok(Decision {
            record: Some(r.clone()),
            ..Decision::default()
        });
    }
    let mut r = r.clone();
    patch(
        &mut r,
        json!({"status":"completed","dueAt":null,"proofAt":now}),
    );
    let mut a = s.agent.clone();
    patch(&mut a, json!({"status":"queued","error":null}));
    Ok(Decision {
        record: Some(r),
        agent: Some(a),
        ..Decision::default()
    })
}

pub const MAX_PROFILES: usize = 2;
const MAX_DUE_ROWS: usize = 16;
const MAX_PROFILE_ROWS: usize = 32;
/// One local account probe fans out only to exact receipts claimed for that account.
pub struct ProbeJob {
    pub account_key: String,
    pub account_pin: Value,
    pub tickets: Vec<(String, ProbeTicket)>,
}
pub struct ProbePlan {
    pub writes: Vec<(String, Decision)>,
    pub jobs: Vec<ProbeJob>,
}
/// Pure scheduler pass. The transaction adapter loads these rows afresh, commits all writes,
/// then starts jobs outside the transaction. `active_accounts` is owned by that adapter.
pub fn plan_probes(
    snapshots: &[Snapshot],
    active_accounts: &std::collections::BTreeSet<String>,
    settings: &std::collections::BTreeMap<String, Value>,
    pins: &std::collections::BTreeMap<String, Value>,
    now: f64,
) -> Result<ProbePlan> {
    now_checked(now)?;
    let mut due: Vec<&Snapshot> = snapshots
        .iter()
        .filter(|s| {
            s.record.as_ref().is_some_and(|r| {
                text(r, "status") == "auth_probe_wait"
                    && r["dueAt"].as_f64().is_some_and(|at| at <= now)
            })
        })
        .collect();
    due.sort_by(|a, b| {
        a.record
            .as_ref()
            .and_then(|r| r["dueAt"].as_f64())
            .partial_cmp(&b.record.as_ref().and_then(|r| r["dueAt"].as_f64()))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut active = active_accounts.clone();
    let mut plan = ProbePlan {
        writes: Vec::new(),
        jobs: Vec::new(),
    };
    for s in due.into_iter().take(MAX_DUE_ROWS) {
        let key = account(&s.agent);
        if active.contains(key) || active.len() >= MAX_PROFILES {
            continue;
        }
        let Some(pin) = pins.get(key) else {
            continue;
        };
        let Some(selected_settings) = settings.get(s.agent_id()) else {
            continue;
        };
        let (decision, ticket) = claim_probe(s, selected_settings, pin, now)?;
        if ticket.is_none() {
            if decision.record.is_some() {
                plan.writes.push((s.agent_id().into(), decision));
            }
            continue;
        }
        let mut tickets = Vec::new();
        for candidate in snapshots
            .iter()
            .filter(|candidate| {
                account(&candidate.agent) == key
                    && candidate
                        .record
                        .as_ref()
                        .is_some_and(|r| text(r, "status") == "auth_probe_wait")
            })
            .take(MAX_PROFILE_ROWS)
        {
            // Python reschedules every wait for a claimed profile. Each receipt is checked
            // again when the shared observation returns, including waits not yet due.
            if let Some(r) = &candidate.record {
                let mut receipt = r.clone();
                receipt["dueAt"] = json!(now + POLL_SECONDS);
                plan.writes.push((
                    candidate.agent_id().into(),
                    Decision {
                        record: Some(receipt.clone()),
                        ..Decision::default()
                    },
                ));
                tickets.push((
                    candidate.agent_id().into(),
                    ProbeTicket {
                        receipt,
                        pin: account_pin(pin),
                    },
                ));
            }
        }
        active.insert(key.into());
        plan.jobs.push(ProbeJob {
            account_key: key.into(),
            account_pin: account_pin(pin),
            tickets,
        });
    }
    Ok(plan)
}
