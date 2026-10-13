//! Pure account-transfer decisions. Stored JSON enters only through `Snapshot::read`.
//!
//! Submission is durable before an effect is dispatched. Unknown outcomes never
//! authorize another send; receipts retain their exact transport identity.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use thiserror::Error;

pub use studio_runtime_store::MAX_TRANSFER_RECORD_BYTES as MAX_RECORD_BYTES;
pub const EXECUTOR_REQUIRED: &str = "account-transfer/provider-history-executor";
pub const SETTINGS_REQUIRED: &str = "account-transfer/destination-settings";
/// Lead + 2048 workers + one queued team record.
pub const MAX_TRANSFER_AGENT_RECORDS: usize = 2050;

#[derive(Debug, Error)]
pub enum TransferError {
    #[error("invalid stored {object} at {pointer}")]
    Stored {
        object: &'static str,
        pointer: String,
    },
    #[error("{0}")]
    Refused(&'static str),
    #[error("Not implemented: requires {0}")]
    Requires(&'static str),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Waiting,
    Lazy,
    Reading,
    Interrupting,
    Submitted,
    LazySubmitted,
    Ready,
    Blocked,
    Unknown,
    Completed,
    Left,
}
impl Phase {
    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Left)
    }
    fn moving(self) -> bool {
        matches!(
            self,
            Self::Reading | Self::Interrupting | Self::Submitted | Self::LazySubmitted
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Cancel,
    Retry,
    FinishHistory,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Restart,
    Action(Action),
}

/// A transport ticket identifies one exact mutation, including a captured epoch.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    pub operation: String,
    pub agent: String,
    pub epoch: i64,
    pub account: String,
    pub connection: String,
    pub request: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransferIdentity {
    id: String,
    lead_id: String,
    target_account_key: String,
    status: TransferStatus,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum TransferStatus {
    Pending,
    Completed,
    Cancelled,
}

/// Validated snapshot with original member insertion order retained for the UI.
pub struct Snapshot {
    identity: TransferIdentity,
    operation: Value,
    order: Vec<String>,
    agents: BTreeMap<String, Value>,
}
#[derive(Clone)]
pub struct Followup {
    pub agent: String,
    pub id: String,
    pub text: &'static str,
}
pub struct Decision {
    pub operation: Value,
    pub agents: BTreeMap<String, Value>,
    pub followups: Vec<Followup>,
    pub changed: bool,
    order: Vec<String>,
}

fn invalid(object: &'static str, pointer: impl Into<String>) -> TransferError {
    TransferError::Stored {
        object,
        pointer: pointer.into(),
    }
}
fn validate(schema: &str, object: &'static str, value: &Value) -> Result<(), TransferError> {
    if let Some((pointer, _)) =
        studio_runtime_schema::first_error(schema, value).map_err(|_| invalid(object, "/"))?
    {
        return Err(invalid(object, pointer));
    }
    Ok(())
}
pub(crate) fn read_record(
    raw: &str,
    schema: &str,
    object: &'static str,
) -> Result<Value, TransferError> {
    if raw.len() > MAX_RECORD_BYTES {
        return Err(invalid(object, "/resourceLimit"));
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| invalid(object, "/"))?;
    validate(schema, object, &value)?;
    Ok(value)
}

impl Snapshot {
    pub fn read(
        raw: &str,
        agents: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, TransferError> {
        let operation = read_record(raw, "PersistedAccountTransferRecord", "account transfer")?;
        let order: MemberOrder =
            serde_json::from_str(raw).map_err(|_| invalid("account transfer", "/members"))?;
        let mut records = BTreeMap::new();
        let mut bytes = raw.len();
        for (id, raw) in agents {
            bytes = bytes.saturating_add(raw.len());
            if records.len() >= MAX_TRANSFER_AGENT_RECORDS || bytes > 64 * 1024 * 1024 {
                return Err(invalid("transfer snapshot", "/resourceLimit"));
            }
            // `runtime_agents` uses the table schema. Keep the open record as JSON;
            // the reducer reads only the fields it needs.
            let value = read_record(&raw, "PersistedAgentRecord", "transfer agent")?;
            if value["id"].as_str() != Some(id.as_str()) || records.insert(id, value).is_some() {
                return Err(invalid("transfer agent", "/id"));
            }
        }
        for id in order
            .members
            .iter()
            .map(String::as_str)
            .chain(operation["leadId"].as_str())
        {
            if !records.contains_key(id) {
                return Err(invalid("transfer agent", "/id"));
            }
        }
        let identity = serde_json::from_value(operation.clone())
            .map_err(|_| invalid("account transfer", "/"))?;
        Ok(Self {
            identity,
            operation,
            order: order.members,
            agents: records,
        })
    }
    pub fn public_view(&self) -> Result<Value, TransferError> {
        public_view(&self.operation, &self.order)
    }
    pub fn lead_id(&self) -> &str {
        &self.identity.lead_id
    }
    pub fn operation(&self) -> &Value {
        &self.operation
    }
    fn phase(&self, id: &str) -> Result<Phase, TransferError> {
        serde_json::from_value(self.operation["members"][id]["phase"].clone())
            .map_err(|_| invalid("account transfer", "/members/phase"))
    }
    fn pending(&self) -> bool {
        self.identity.status == TransferStatus::Pending
    }
    fn decision(
        &self,
        operation: Value,
        agents: BTreeMap<String, Value>,
        followups: Vec<Followup>,
    ) -> Decision {
        let changed = operation != self.operation || agents != self.agents || !followups.is_empty();
        Decision {
            operation,
            agents,
            followups,
            changed,
            order: self.order.clone(),
        }
    }
    /// A returned operation is bound to its original request content, even after completion.
    pub fn replay(
        &self,
        request: &str,
        lead: &str,
        target: &str,
        scope: &str,
    ) -> Result<bool, TransferError> {
        let receipt = self.operation.get("requests").and_then(|r| r.get(request));
        let candidate =
            receipt.or_else(|| (self.identity.id == request).then_some(&self.operation));
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        if candidate["leadId"] != lead
            || candidate["targetAccountKey"] != target
            || candidate
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("team")
                != scope
        {
            return Err(TransferError::Refused(
                "This transfer id has different content",
            ));
        }
        Ok(true)
    }
    pub fn decide(&self, command: Command) -> Result<Decision, TransferError> {
        let mut op = self.operation.clone();
        let mut agents = self.agents.clone();
        let mut followups = Vec::new();
        if !self.pending() {
            return Ok(self.decision(op, agents, followups));
        }
        match command {
            Command::Restart => {
                for id in &self.order {
                    let member = &mut op["members"][id];
                    match self.phase(id)? {
                        Phase::Reading => {
                            member["phase"] = json!(if flag(member, "lazy") {
                                "lazy"
                            } else {
                                "waiting"
                            })
                        }
                        Phase::Submitted | Phase::LazySubmitted => {
                            member["phase"] = json!("unknown");
                            member["error"] = json!(
                                "The server restarted before the transfer receipt arrived. No request was repeated."
                            );
                        }
                        Phase::Interrupting => {
                            member["phase"] = json!("blocked");
                            member["interruptOutcome"] = json!("unknown");
                            member["error"] = json!(
                                "The server restarted before the turn interrupt receipt arrived. No request was repeated."
                            );
                        }
                        _ => {}
                    }
                }
            }
            Command::Action(Action::FinishHistory) => op["finishHistory"] = json!(true),
            Command::Action(Action::Retry) => {
                if self
                    .order
                    .iter()
                    .any(|id| flag(&op["members"][id], "archiveInvalidated"))
                {
                    return Err(TransferError::Refused(
                        "The source changed after history export. Cancel this transfer and start a new one.",
                    ));
                }
                for id in &self.order {
                    if self.phase(id)? != Phase::Blocked {
                        continue;
                    }
                    let member = &mut op["members"][id];
                    if flag(member, "lazy") && !truthy(agents[id].get("lazyAccountTransfer")) {
                        return Err(TransferError::Requires(SETTINGS_REQUIRED));
                    }
                    member["phase"] = json!(if flag(member, "lazy") {
                        "lazy"
                    } else if truthy(member.get("result")) {
                        "ready"
                    } else {
                        "waiting"
                    });
                    member["error"] = Value::Null;
                    member["nextCheck"] = json!(0);
                    if truthy(member.get("interruptSubmittedAt")) {
                        remove(
                            member,
                            &[
                                "interruptTurnId",
                                "interruptSubmittedAt",
                                "interruptOutcome",
                            ],
                        )?;
                    }
                }
            }
            Command::Action(Action::Cancel) => {
                for id in &self.order {
                    let m = &op["members"][id];
                    let phase = self.phase(id)?;
                    if flag(m, "lazy")
                        && !phase.terminal()
                        && phase != Phase::Lazy
                        && !(phase == Phase::Blocked
                            && flag(m, "archiveInvalidated")
                            && (!truthy(m.get("submittedAt")) || truthy(m.get("result"))))
                    {
                        return Err(TransferError::Refused(
                            "A native history move has started. Resolve its receipt before cancelling.",
                        ));
                    }
                }
                if self.order.iter().any(|id| {
                    let m = &op["members"][id];
                    truthy(m.get("interruptSubmittedAt")) && m["interruptOutcome"] != "acknowledged"
                }) {
                    return Err(TransferError::Refused(
                        "Retry the unresolved turn interruption before cancelling this transfer",
                    ));
                }
                if self.order.iter().any(|id| {
                    op["members"][id]["interruptOutcome"] == "acknowledged"
                        && flag(&agents[id], "inFlight")
                }) {
                    return Err(TransferError::Refused(
                        "Wait for the active turn interruption to finish before cancelling this transfer",
                    ));
                }
                op["status"] = json!("cancelled");
                for id in &self.order {
                    let a = agents
                        .get_mut(id)
                        .ok_or_else(|| invalid("transfer agent", "/id"))?;
                    let m = &op["members"][id];
                    if a["accountTransferId"] != op["id"] || self.phase(id)? == Phase::Reading {
                        continue;
                    }
                    let lazy = a.get("lazyAccountTransfer").cloned().unwrap_or(Value::Null);
                    if lazy["id"] == op["id"] && flag(m, "lazy") {
                        if !lazy["sourceAccountKey"].is_string() {
                            return Err(invalid(
                                "transfer agent",
                                "/lazyAccountTransfer/sourceAccountKey",
                            ));
                        }
                        a["accountKey"] = lazy["sourceAccountKey"].clone();
                        for field in RESTORED_SETTINGS {
                            if let Some(value) = lazy["sourceState"].get(*field) {
                                if value.is_null() {
                                    remove(a, &[*field])?;
                                } else {
                                    a[*field] = value.clone();
                                }
                            }
                        }
                        remove(a, &["lazyAccountTransfer"])?;
                    }
                    if truthy(m.get("interruptReason"))
                        && flag(m, "continueAfterTransfer")
                        && flag(a, "autoWake")
                    {
                        followups.push(Followup {
                            agent: id.clone(),
                            id: format!(
                                "account-transfer-cancel:{}:{id}",
                                op["id"].as_str().unwrap_or_default()
                            ),
                            text: CANCEL_FOLLOWUP,
                        });
                        a["status"] = json!("queued");
                    }
                    remove(a, &["accountTransferId"])?;
                }
            }
        }
        let mut decision = self.decision(op, agents, followups);
        decision.changed |= matches!(command, Command::Action(_));
        Ok(decision)
    }

    pub fn begin_read(&self, agent: &str) -> Result<Decision, TransferError> {
        let a = self
            .agents
            .get(agent)
            .ok_or(TransferError::Refused("Unknown transfer member"))?;
        let member = &self.operation["members"][agent];
        if !self.pending()
            || !matches!(self.phase(agent)?, Phase::Waiting | Phase::Lazy)
            || a["accountTransferId"] != self.identity.id
            || truthy(a.get("deletedAt"))
            || truthy(member.get("result"))
            || truthy(member.get("submittedAt"))
            || flag(member, "archiveInvalidated")
            || flag(a, "inFlight")
            || matches!(
                a["status"].as_str(),
                Some("running" | "starting" | "approval")
            )
        {
            return Err(TransferError::Refused(
                "The transfer is not ready for history preparation",
            ));
        }
        let mut op = self.operation.clone();
        let member = &mut op["members"][agent];
        member["phase"] = json!("reading");
        member["source"] = json!({});
        for field in ["epoch", "accountKey", "threadId", "cwd"] {
            member["source"][field] = a.get(field).cloned().unwrap_or(Value::Null);
        }
        member["settings"] = settings_snapshot(a);
        for field in ["pendingSettings", "pendingSettingsAccountKey"] {
            member[field] = a.get(field).cloned().unwrap_or(Value::Null);
        }
        member["error"] = Value::Null;
        Ok(self.decision(op, self.agents.clone(), vec![]))
    }

    /// Record a transport ticket before the caller sends. A returned saved result
    /// or an uncertain phase never authorizes dispatch, even after restart.
    pub fn submit(
        &self,
        ticket: &Ticket,
        method: &str,
        params: Value,
    ) -> Result<Decision, TransferError> {
        let id = &ticket.agent;
        self.check_ticket_agent(ticket)?;
        let m = &self.operation["members"][id];
        if !self.pending()
            || self.phase(id)? != Phase::Reading
            || truthy(m.get("submittedAt"))
            || truthy(m.get("result"))
            || m.get("transportTicket").is_some()
        {
            return Err(TransferError::Refused(
                "The transfer is not ready for a new native request",
            ));
        }
        if !matches!(method, "thread/start" | "thread/fork") || !params.is_object() {
            return Err(TransferError::Refused("Invalid native transfer request"));
        }
        let agent = &self.agents[id];
        let source = m
            .get("source")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("account transfer", "/members/source"))?;
        if source.is_empty()
            || source
                .iter()
                .any(|(field, value)| agent.get(field).unwrap_or(&Value::Null) != value)
            || m.get("settings") != Some(&settings_snapshot(agent))
            || agent.get("pendingSettings").unwrap_or(&Value::Null)
                != m.get("pendingSettings").unwrap_or(&Value::Null)
            || agent
                .get("pendingSettingsAccountKey")
                .unwrap_or(&Value::Null)
                != m.get("pendingSettingsAccountKey").unwrap_or(&Value::Null)
        {
            return Err(TransferError::Refused(
                "Agent state changed during transfer. Its original session is preserved.",
            ));
        }
        let mut op = self.operation.clone();
        let m = &mut op["members"][id];
        m["phase"] = json!(if flag(m, "lazy") {
            "lazy_submitted"
        } else {
            "submitted"
        });
        m["transportTicket"] =
            serde_json::to_value(ticket).map_err(|_| invalid("transport ticket", "/"))?;
        m["nativeMethod"] = json!(method);
        m["nativeParams"] = params;
        m["targetConnection"] = json!(ticket.connection);
        Ok(self.decision(op, self.agents.clone(), vec![]))
    }

    /// Receipts can arrive after restart or cancellation. Preserve the success
    /// without adopting it; adoption requires fresh source and archive checks.
    pub fn receive(
        &self,
        ticket: &Ticket,
        result: Option<Value>,
    ) -> Result<Decision, TransferError> {
        let m = &self.operation["members"][&ticket.agent];
        let expected =
            serde_json::to_value(ticket).map_err(|_| invalid("transport ticket", "/"))?;
        if m.get("transportTicket") != Some(&expected) {
            return Err(TransferError::Refused(
                "The transfer receipt belongs to another native request",
            ));
        }
        self.check_ticket_agent(ticket)?;
        if let Some(saved) = m.get("result").filter(|v| !v.is_null()) {
            if result.as_ref() == Some(saved) {
                return Ok(self.decision(self.operation.clone(), self.agents.clone(), vec![]));
            }
            return Err(TransferError::Refused(
                "The transfer receipt conflicts with its saved result",
            ));
        }
        if !matches!(
            self.phase(&ticket.agent)?,
            Phase::Submitted | Phase::LazySubmitted | Phase::Unknown
        ) {
            return Err(TransferError::Refused(
                "The transfer has no pending native receipt",
            ));
        }
        let mut op = self.operation.clone();
        let m = &mut op["members"][&ticket.agent];
        match result {
            Some(result)
                if result
                    .pointer("/thread/id")
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.is_empty()) =>
            {
                m["phase"] = json!("ready");
                m["result"] = result;
                m["error"] = Value::Null;
            }
            _ => {
                m["phase"] = json!("unknown");
                m["error"] = json!("Native transfer returned no thread identity; outcome unknown");
            }
        }
        Ok(self.decision(op, self.agents.clone(), vec![]))
    }
    fn check_ticket_agent(&self, ticket: &Ticket) -> Result<(), TransferError> {
        let a = self
            .agents
            .get(&ticket.agent)
            .ok_or(TransferError::Refused("Unknown transfer member"))?;
        let m = &self.operation["members"][&ticket.agent];
        if self.identity.id != ticket.operation
            || self.identity.target_account_key != ticket.account
            || a["epoch"].as_i64() != Some(ticket.epoch)
            || truthy(a.get("deletedAt"))
            || a.get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default")
                != if flag(m, "lazy") {
                    self.identity.target_account_key.as_str()
                } else {
                    m["sourceAccountKey"].as_str().unwrap_or("default")
                }
            || !m.is_object()
            || ticket.request.is_empty()
            || ticket.connection.is_empty()
            || (a["accountTransferId"] != ticket.operation && self.pending())
        {
            return Err(TransferError::Refused(
                "The transfer receipt identity is stale",
            ));
        }
        Ok(())
    }
}

pub fn before_start(agent: &Value) -> Result<(), TransferError> {
    validate("PersistedAgentRecord", "transfer agent", agent)?;
    if agent.get("accountTransfer").is_some_and(|v| {
        !v.is_null() && !matches!(v["status"].as_str(), Some("completed" | "cancelled"))
    }) || truthy(agent.get("accountTransferId"))
        || truthy(agent.get("lazyAccountTransfer"))
        || agent
            .get("emptyTransferRecovery")
            .is_some_and(|r| !r.is_null() && r["phase"] != "completed")
    {
        return Err(TransferError::Requires("account-transfer/receipt-hold"));
    }
    Ok(())
}
fn flag(value: &Value, key: &str) -> bool {
    value.get(key) == Some(&Value::Bool(true))
}
fn truthy(value: Option<&Value>) -> bool {
    crate::event_write::truthy(value)
}
pub(crate) fn remove(value: &mut Value, keys: &[&str]) -> Result<(), TransferError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("transfer agent", "/"))?;
    for key in keys {
        object.remove(*key);
    }
    Ok(())
}
const RESTORED_SETTINGS: &[&str] = &[
    "provider",
    "model",
    "effort",
    "nativeEffort",
    "fastMode",
    "daybreakEnabled",
    "cyberAccessProgram",
    "workerDefaults",
    "pendingSettings",
    "pendingSettingsAccountKey",
    "claudeOptions",
    "executionSettingsAccountKey",
];
const CANCEL_FOLLOWUP: &str = "The account transfer was cancelled after this turn was interrupted. Continue the existing task on this account from saved context. Preserve completed work and check existing receipts before any command with an unknown outcome.";

#[derive(Deserialize)]
struct MemberOrder {
    #[serde(deserialize_with = "member_order")]
    members: Vec<String>,
}
fn member_order<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("transfer members")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut keys = Vec::new();
            while let Some(key) = map.next_key::<String>()? {
                if keys.contains(&key) {
                    return Err(serde::de::Error::custom("duplicate member"));
                }
                map.next_value::<serde::de::IgnoredAny>()?;
                keys.push(key);
            }
            Ok(keys)
        }
    }
    deserializer.deserialize_map(Visitor)
}

impl Decision {
    /// Atomically maintained renderer summary; a newer operation owns its lead.
    pub fn stamp(&mut self, now: f64, previous_created: Option<f64>) -> Result<(), TransferError> {
        if !self.changed {
            return Ok(());
        }
        if !now.is_finite() {
            return Err(TransferError::Refused("Invalid transfer timestamp"));
        }
        self.operation["updated"] = json!(now);
        let lead_id = self.operation["leadId"]
            .as_str()
            .ok_or_else(|| invalid("account transfer", "/leadId"))?;
        let lead = self
            .agents
            .get_mut(lead_id)
            .ok_or_else(|| invalid("transfer agent", "/id"))?;
        let pointer = lead.get("accountTransferId").filter(|v| !v.is_null());
        let current = lead.get("accountTransfer").cloned().unwrap_or(Value::Null);
        if pointer.is_some_and(|v| v != &self.operation["id"]) {
            return Ok(());
        }
        if current
            .get("id")
            .is_some_and(|v| !v.is_null() && v != &self.operation["id"])
            && pointer != Some(&self.operation["id"])
            && (!matches!(current["status"].as_str(), Some("completed" | "cancelled"))
                || previous_created
                    .is_some_and(|old| self.operation["created"].as_f64().unwrap_or(0.0) <= old))
        {
            return Ok(());
        }
        let mut summary = json!({});
        for key in [
            "id",
            "targetAccountKey",
            "status",
            "updated",
            "scope",
            "finishHistory",
        ] {
            summary[key] = self.operation.get(key).cloned().unwrap_or(Value::Null);
        }
        let counts = progress(&self.operation, &self.order, false)?;
        extend(&mut summary, counts)?;
        summary["finishHistory"] = json!(true);
        summary["waiting"] = self
            .order
            .iter()
            .find_map(|id| {
                let m = &self.operation["members"][id];
                m.get("error")
                    .filter(|v| truthy(Some(v)))
                    .or_else(|| m.get("waiting").filter(|v| truthy(Some(v))))
                    .cloned()
            })
            .unwrap_or(Value::Null);
        lead["accountTransfer"] = summary;
        Ok(())
    }
    pub fn public_view(&self) -> Result<Value, TransferError> {
        public_view(&self.operation, &self.order)
    }
    pub fn encoded(&self) -> Result<String, TransferError> {
        // serde_json maps sort keys; preserve Python's member order explicitly.
        struct Ordered<'a>(&'a Value, &'a [String]);
        impl Serialize for Ordered<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(self.1.len()))?;
                for id in self.1 {
                    map.serialize_entry(id, &self.0[id])?;
                }
                map.end()
            }
        }
        struct Operation<'a>(&'a Value, &'a [String]);
        impl Serialize for Operation<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let fields = self
                    .0
                    .as_object()
                    .ok_or_else(|| serde::ser::Error::custom("operation object required"))?;
                let mut map = serializer.serialize_map(Some(fields.len()))?;
                for (key, value) in fields {
                    if key == "members" {
                        map.serialize_entry(key, &Ordered(value, self.1))?;
                    } else {
                        map.serialize_entry(key, value)?;
                    }
                }
                map.end()
            }
        }
        validate(
            "PersistedAccountTransferRecord",
            "account transfer",
            &self.operation,
        )?;
        let encoded = serde_json::to_string(&Operation(&self.operation, &self.order))
            .map_err(|_| invalid("account transfer", "/"))?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(invalid("account transfer", "/resourceLimit"));
        }
        Ok(encoded)
    }
}
fn extend(target: &mut Value, fields: Value) -> Result<(), TransferError> {
    let fields = fields
        .as_object()
        .ok_or_else(|| invalid("account transfer", "/"))?;
    target
        .as_object_mut()
        .ok_or_else(|| invalid("account transfer", "/"))?
        .extend(fields.clone());
    Ok(())
}
fn public_view(op: &Value, order: &[String]) -> Result<Value, TransferError> {
    let mut output = json!({});
    for key in [
        "id",
        "leadId",
        "targetAccountKey",
        "status",
        "created",
        "updated",
        "scope",
        "targetProvider",
        "finishHistory",
        "requests",
    ] {
        if let Some(value) = op.get(key) {
            output[key] = value.clone();
        }
    }
    output["scope"] = op.get("scope").cloned().unwrap_or(json!("team"));
    let mut members = json!({});
    for id in order {
        members[id] = json!({});
        for field in [
            "phase",
            "sourceAccountKey",
            "sourceThreadId",
            "targetThreadId",
            "provider",
            "name",
            "reason",
            "error",
            "waiting",
            "lazy",
            "interruptReason",
            "interruptOutcome",
            "continueAfterTransfer",
        ] {
            if let Some(value) = op["members"][id].get(field) {
                members[id][field] = value.clone();
            }
        }
    }
    output["members"] = members;
    extend(&mut output, progress(op, order, true)?)?;
    Ok(output)
}
fn progress(op: &Value, order: &[String], public: bool) -> Result<Value, TransferError> {
    let mut completed = 0;
    let mut moved = 0;
    let mut pending = 0;
    let mut moving = 0;
    let mut waiting = 0;
    let mut attention = false;
    let mut retry = false;
    let mut interrupted = vec![];
    let mut left = vec![];
    let mut blocked = vec![];
    for id in order {
        let m = &op["members"][id];
        let phase: Phase = serde_json::from_value(m["phase"].clone())
            .map_err(|_| invalid("account transfer", "/members/phase"))?;
        completed += usize::from(phase.terminal());
        moved += usize::from(flag(m, "lazy") || phase == Phase::Completed);
        pending += usize::from(flag(m, "lazy") && phase != Phase::Completed);
        moving += usize::from(phase.moving());
        waiting += usize::from(
            !phase.terminal()
                && !matches!(
                    phase,
                    Phase::Blocked | Phase::Unknown | Phase::Lazy | Phase::LazySubmitted
                ),
        );
        attention |= matches!(phase, Phase::Blocked | Phase::Unknown);
        retry |= phase == Phase::Blocked && !flag(m, "archiveInvalidated");
        for (include, reason, destination, provider) in [
            (
                truthy(m.get("interruptReason")),
                "interruptReason",
                &mut interrupted,
                false,
            ),
            (phase == Phase::Left, "reason", &mut left, true),
            (
                matches!(phase, Phase::Blocked | Phase::Unknown),
                "error",
                &mut blocked,
                false,
            ),
        ] {
            if !include {
                continue;
            }
            let mut notice = json!({"id":id});
            for field in ["name", "provider", "reason"] {
                if field == "provider" && !public && !provider {
                    continue;
                }
                let value = m.get(if field == "reason" { reason } else { field });
                if public {
                    if let Some(value) = value.filter(|v| v.is_string()) {
                        notice[field] = value.clone();
                    }
                } else {
                    notice[field] = value.cloned().unwrap_or(Value::Null);
                }
            }
            destination.push(notice);
        }
    }
    Ok(
        json!({"total":order.len(), "completed":completed, "moved":moved, "nativeHistoryPending":pending,
        "movingNow":moving, "canFinishHistory":false, "waitingCount":waiting, "needsAttention":attention,
        "canRetry":retry, "interrupted":interrupted, "leftOnSource":left, "blocked":blocked}),
    )
}

fn settings_snapshot(agent: &Value) -> Value {
    let mut result = json!({});
    for field in [
        "model",
        "effort",
        "nativeEffort",
        "fastMode",
        "yoloMode",
        "profileInstructions",
        "role",
        "workerDefaults",
        "claudeOptions",
    ] {
        result[field] = agent.get(field).cloned().unwrap_or(Value::Null);
    }
    for field in ["daybreakEnabled", "cyberAccessProgram"] {
        if let Some(value) = agent.get(field) {
            result[field] = value.clone();
        }
    }
    result["provider"] = agent.get("provider").cloned().unwrap_or(json!("codex"));
    result
}
