//! Sign-in request cards from the browser bridges (docs/vm-browser-bridge.md, sections 2(b)
//! and 6). The Rust port of `codex_browser_bridge.Bridge.poll` and `apply`.
//!
//! One poll runs for each source: the guest service of the Linux VM on this Mac (`vm`) and
//! the host bridge of commands on this server (`host`, `browser_host`). It long-polls `browser.wait`,
//! writes one `browser/open` request for each new pending link, refuses links that no chat
//! can own, keeps the card's deadline equal to the guest request's, and expires a pending
//! card when its guest request ends. A card that nobody answered tells its chat
//! (`unanswered_event`), so the agent does not wait for a result that cannot come.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use studio_runtime_store::{
    QuestionsWriteRepository, RequestRecord, RuntimeStore, Table, Transaction,
};
use tokio::sync::{Notify, watch};

use crate::browser::BrowserForwarder;
use crate::linux_vm_guest::{GuestError, GuestRpc};

pub const METHOD: &str = "browser/open";

/// Browser parameters with opaque provider extensions retained byte-for-byte.
#[derive(Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<BrowserPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<BrowserRedirect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward: Option<BrowserForward>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joined: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste: Option<BrowserPaste>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<BrowserWaiting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_http: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linux_user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrowserRedirect {
    pub host: String,
    pub port: u16,
    pub path: String,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserWaiting {
    pub expires_at: f64,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BrowserPaste {
    pub at: f64,
}
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserForward {
    pub token_hash: Option<String>,
    pub expires_at: Option<f64>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>,
}
fn canonical_field<T>(field: Option<studio_runtime_schema::queue_command::Field<T>>) -> Option<T> {
    match field {
        Some(studio_runtime_schema::queue_command::Field::Value(value)) => Some(value),
        None
        | Some(
            studio_runtime_schema::queue_command::Field::Null
            | studio_runtime_schema::queue_command::Field::Missing,
        ) => None,
    }
}
#[derive(Default)]
struct ForwardFields {
    token_hash: Option<studio_runtime_schema::queue_command::Field<String>>,
    expires_at: Option<studio_runtime_schema::queue_command::Field<f64>>,
}
impl<'de> studio_runtime_schema::raw_object::RawObjectFields<'de> for ForwardFields {
    type Output = BrowserForward;
    fn field<M: serde::de::MapAccess<'de>>(
        &mut self,
        name: &str,
        map: &mut M,
    ) -> Result<bool, M::Error> {
        match name {
            "tokenHash" => {
                if self.token_hash.is_some() {
                    return Err(serde::de::Error::duplicate_field("tokenHash"));
                }
                self.token_hash = Some(map.next_value()?);
            }
            "expiresAt" => {
                if self.expires_at.is_some() {
                    return Err(serde::de::Error::duplicate_field("expiresAt"));
                }
                self.expires_at = Some(map.next_value()?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    fn finish<E: serde::de::Error>(
        self,
        extra: std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>,
    ) -> Result<BrowserForward, E> {
        Ok(BrowserForward {
            token_hash: canonical_field(self.token_hash),
            expires_at: canonical_field(self.expires_at),
            extra,
        })
    }
}
impl<'de> serde::Deserialize<'de> for BrowserForward {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        studio_runtime_schema::raw_object::deserialize::<D, ForwardFields>(deserializer)
    }
}
#[derive(Default)]
struct BrowserFields {
    bridge_id: Option<studio_runtime_schema::queue_command::Field<String>>,
    url: Option<studio_runtime_schema::queue_command::Field<String>>,
    host: Option<studio_runtime_schema::queue_command::Field<String>>,
    source: Option<studio_runtime_schema::queue_command::Field<String>>,
    phase: Option<studio_runtime_schema::queue_command::Field<BrowserPhase>>,
    redirect: Option<studio_runtime_schema::queue_command::Field<BrowserRedirect>>,
    forward: Option<studio_runtime_schema::queue_command::Field<BrowserForward>>,
    joined: Option<studio_runtime_schema::queue_command::Field<Vec<String>>>,
    paste: Option<studio_runtime_schema::queue_command::Field<BrowserPaste>>,
    waiting: Option<studio_runtime_schema::queue_command::Field<BrowserWaiting>>,
    outcome: Option<studio_runtime_schema::queue_command::Field<String>>,
    outcome_http: Option<studio_runtime_schema::queue_command::Field<u16>>,
    expires_at: Option<studio_runtime_schema::queue_command::Field<f64>>,
    client_id: Option<studio_runtime_schema::queue_command::Field<String>>,
    scope: Option<studio_runtime_schema::queue_command::Field<String>>,
    linux_user: Option<studio_runtime_schema::queue_command::Field<String>>,
    user_code: Option<studio_runtime_schema::queue_command::Field<String>>,
}
impl<'de> studio_runtime_schema::raw_object::RawObjectFields<'de> for BrowserFields {
    type Output = BrowserParams;
    fn field<M: serde::de::MapAccess<'de>>(
        &mut self,
        name: &str,
        map: &mut M,
    ) -> Result<bool, M::Error> {
        macro_rules! field {
            ($name:literal, $member:ident) => {
                if name == $name {
                    if self.$member.is_some() {
                        return Err(serde::de::Error::duplicate_field($name));
                    }
                    self.$member = Some(map.next_value()?);
                    return Ok(true);
                }
            };
        }
        field!("bridgeId", bridge_id);
        field!("url", url);
        field!("host", host);
        field!("source", source);
        field!("phase", phase);
        field!("redirect", redirect);
        field!("forward", forward);
        field!("joined", joined);
        field!("paste", paste);
        field!("waiting", waiting);
        field!("outcome", outcome);
        field!("outcomeHttp", outcome_http);
        field!("expiresAt", expires_at);
        field!("clientId", client_id);
        field!("scope", scope);
        field!("linuxUser", linux_user);
        field!("userCode", user_code);
        Ok(false)
    }
    fn finish<E: serde::de::Error>(
        self,
        extra: std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>,
    ) -> Result<BrowserParams, E> {
        Ok(BrowserParams {
            bridge_id: canonical_field(self.bridge_id),
            url: canonical_field(self.url),
            host: canonical_field(self.host),
            source: canonical_field(self.source),
            phase: canonical_field(self.phase),
            redirect: canonical_field(self.redirect),
            forward: canonical_field(self.forward),
            joined: canonical_field(self.joined),
            paste: canonical_field(self.paste),
            waiting: canonical_field(self.waiting),
            outcome: canonical_field(self.outcome),
            outcome_http: canonical_field(self.outcome_http),
            expires_at: canonical_field(self.expires_at),
            client_id: canonical_field(self.client_id),
            scope: canonical_field(self.scope),
            linux_user: canonical_field(self.linux_user),
            user_code: canonical_field(self.user_code),
            extra,
        })
    }
}
impl<'de> serde::Deserialize<'de> for BrowserParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        studio_runtime_schema::raw_object::deserialize::<D, BrowserFields>(deserializer)
    }
}
impl BrowserParams {
    pub(crate) fn read(raw: &serde_json::value::RawValue) -> Option<Self> {
        serde_json::from_str(raw.get()).ok()
    }
}
/// The long poll of `browser.wait`, in seconds (`POLL_SECONDS`).
const POLL_SECONDS: u64 = 20;
/// Guest request states that end a card (`FINAL_STATES`).
pub const FINAL_STATES: [&str; 6] = [
    "declined", "stale", "expired", "closed", "replaced", "opened",
];
/// The waiting phase of a link opened in paste mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BrowserPhase {
    Waiting,
}
/// The `closeReason` of a card that startup recovery expired (`monitor_runtime`). The
/// bridge keeps its request across the restart, so the poll reopens the card while that
/// request is open, and tells the chat when it ended meanwhile without an answer.
pub const RESTART_CLOSE_REASON: &str = "restart";
const CLOSE_TIMEOUT: Duration = Duration::from_secs(15);

/// The chat event text of each final outcome (`OUTCOME_TEXT` of `codex_browser_bridge.py`).
fn outcome_text(outcome: &str) -> Option<&'static str> {
    Some(match outcome {
        "signed-in" => {
            "The person signed in at {host}. Studio sent the result to the command on port {port}; the command answered HTTP {http}."
        }
        "refused" => {
            "The person signed in at {host}, but the command refused the result (HTTP {http})."
        }
        "provider-error" => {
            "The sign-in at {host} returned an error. Studio sent it to the command on port {port}."
        }
        "unknown" => {
            "The result of the sign-in at {host} is unknown. Studio does not send it again. Check the command output."
        }
        "expired" => {
            "The sign-in at {host} ended without a result: the request expired or the command stopped."
        }
        _ => return None,
    })
}

/// The chat event text when a pending card ends before anyone answered it, by the guest state
/// that ended its request (docs/vm-browser-bridge.md, section 2(b)). `replaced` has none: the
/// chat gave a newer link.
fn unanswered_text(state: &str) -> Option<&'static str> {
    Some(match state {
        "stale" => {
            "Nobody answered the sign-in card for {host}, and it closed because the sign-in command stopped waiting. The command got no sign-in result from this card. Do not wait for it: start the sign-in again when the person can answer, and tell the person in one short message."
        }
        "expired" => {
            "Nobody answered the sign-in card for {host} in time, and Studio closed it. The command gets no sign-in result from this card. Do not wait for it: stop the command if it still waits, start the sign-in again when the person can answer, and tell the person in one short message."
        }
        _ => return None,
    })
}

/// Python `str.format` of one JSON value: text as is, null as `None`.
fn python_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(other) => other.to_string(),
    }
}

fn python_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

/// One card for each guest request, also after a lost reply or a restart.
pub fn card_id(bridge_id: &str) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("studio-browser-open:{bridge_id}").as_bytes(),
    )
    .to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserCardError {
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("the sign-in request record is invalid")]
    Invalid,
}

pub(crate) struct BrowserCard {
    pub record: RequestRecord,
    pub params: BrowserParams,
}
impl BrowserCard {
    pub fn decode(raw: &str) -> Result<Self, BrowserCardError> {
        let record: RequestRecord =
            serde_json::from_str(raw).map_err(|_| BrowserCardError::Invalid)?;
        if record.method != studio_runtime_schema::request_record::RequestMethod::BrowserOpen {
            return Err(BrowserCardError::Invalid);
        }
        let params = BrowserParams::read(&record.params).ok_or(BrowserCardError::Invalid)?;
        Ok(Self { record, params })
    }
}
pub(crate) fn put_request(
    tx: &Transaction<'_>,
    card: &mut BrowserCard,
) -> Result<(), BrowserCardError> {
    card.record.params =
        serde_json::value::to_raw_value(&card.params).map_err(|_| BrowserCardError::Invalid)?;
    Table::<RequestRecord>::new(tx).put(&card.record)?;
    publish_request(tx, &card.record.id)?;
    Ok(())
}

/// Publish the client view of a stored card in the writer's transaction. The renderer shows
/// a card only through its `request` entity: a pending card has one, and a closed card loses it.
pub(crate) fn publish_request(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<(), studio_runtime_store::StoreError> {
    let changes = crate::sync_policy::EntityChanges {
        requests: BTreeSet::from([id.to_owned()]),
        ..Default::default()
    };
    let server_id = RuntimeStore::transaction_local_server_id(tx)?;
    crate::sync_policy::publish_entity_changes(tx, &changes, server_id.as_deref(), &|_, _, _, _| {
        true
    })
}

/// Tell the chat how a sign-in that the person opened ended (section 6.2). Returns whether an
/// event was queued.
pub(crate) fn outcome_event(tx: &Transaction<'_>, card: &BrowserCard, now: f64) -> bool {
    let Some(text) = card.params.outcome.as_deref().and_then(outcome_text) else {
        return false;
    };
    let Ok(agent) = crate::event_write::checked_agent(tx, &card.record.agent) else {
        return false;
    };
    let text = text
        .replace("{host}", card.params.host.as_deref().unwrap_or("None"))
        .replace(
            "{port}",
            &card
                .params
                .redirect
                .as_ref()
                .map_or_else(|| "None".into(), |r| r.port.to_string()),
        )
        .replace(
            "{http}",
            &card
                .params
                .outcome_http
                .map_or_else(|| "None".into(), |v| v.to_string()),
        );
    let bridge_id = card.params.bridge_id.as_deref().unwrap_or("None");
    match crate::event_write::enqueue_event(
        tx,
        &agent,
        format!("browser-outcome:{bridge_id}"),
        "browser_outcome",
        text,
        now,
    ) {
        Ok(_) => true,
        Err(error) => {
            eprintln!("browser outcome event for {bridge_id} failed: {error}");
            false
        }
    }
}
fn unanswered_event(tx: &Transaction<'_>, card: &BrowserCard, state: &str, now: f64) -> bool {
    let Some(text) = unanswered_text(state) else {
        return false;
    };
    let Ok(agent) = crate::event_write::checked_agent(tx, &card.record.agent) else {
        return false;
    };
    let bridge_id = card.params.bridge_id.as_deref().unwrap_or("None");
    match crate::event_write::enqueue_event(
        tx,
        &agent,
        format!("browser-unanswered:{bridge_id}"),
        "browser_unanswered",
        text.replace("{host}", card.params.host.as_deref().unwrap_or("None")),
        now,
    ) {
        Ok(queued) => queued,
        Err(error) => {
            eprintln!("browser unanswered event for {bridge_id} failed: {error}");
            false
        }
    }
}

/// The chat event of a joined request whose sign-in finished on another chat's card.
const JOINED_SIGNED_IN_TEXT: &str = "The person signed in at {host} on the sign-in card of another chat. Your own sign-in command did not get that result. Stop it if it still waits, then run it again: the sign-in session is now active.";

/// Add a chat that waits on a card to the card's `params.joined`, or take it away.
fn join_card(
    tx: &Transaction<'_>,
    leader: &str,
    agent: &str,
    waits: bool,
) -> Result<(), BrowserCardError> {
    let key = card_id(leader);
    let Some(raw) = QuestionsWriteRepository::request(tx, &key)? else {
        return Ok(());
    };
    let mut card = BrowserCard::decode(&raw)?;
    if card.record.status != studio_runtime_schema::persistence::RequestCardStatus::Pending {
        return Ok(());
    }
    let mut joined = card.params.joined.take().unwrap_or_default();
    let before = joined.clone();
    joined.retain(|chat| chat != agent);
    if waits {
        joined.push(agent.to_owned());
    }
    if joined != before {
        card.params.joined = Some(joined);
        put_request(tx, &mut card)?;
    }
    Ok(())
}

/// Tell a joined chat that the person signed in on the card that it waited on.
fn joined_event(tx: &Transaction<'_>, item: &Value, now: f64) -> bool {
    let Some(agent_id) = item.get("agentId").and_then(Value::as_str) else {
        return false;
    };
    let Ok(agent) = crate::event_write::checked_agent(tx, agent_id) else {
        return false;
    };
    let bridge_id = python_text(item.get("id"));
    match crate::event_write::enqueue_event(
        tx,
        &agent,
        format!("browser-joined:{bridge_id}"),
        "browser_joined",
        JOINED_SIGNED_IN_TEXT.replace("{host}", &python_text(item.get("host"))),
        now,
    ) {
        Ok(queued) => queued,
        Err(error) => {
            eprintln!("browser joined event for {bridge_id} failed: {error}");
            false
        }
    }
}

/// Whether startup recovery expired this card while nobody had answered it.
fn restart_expired(card: &BrowserCard) -> bool {
    card.record.status == studio_runtime_schema::persistence::RequestCardStatus::Expired
        && card.record.close_reason.as_deref() == Some(RESTART_CLOSE_REASON)
        && card.record.decision.is_none()
}

/// A bridge that the server polls.
#[derive(Clone)]
pub struct CardSource {
    /// `vm`: links of VM chats from the guest. `host`: links of commands that run on this
    /// server: native chats, and `host_exec` commands of VM chats.
    pub source: &'static str,
    pub guest: Arc<dyn GuestRpc>,
    /// The forwards that this server holds for the source's approvals.
    pub forwarder: Option<Arc<BrowserForwarder>>,
    /// Test provider origins that may use http. Production passes none.
    pub http_providers: BTreeSet<String>,
}

/// Why a queued link gets no card, or None (`Bridge.refusal`).
fn refusal(
    tx: &Transaction<'_>,
    source: &CardSource,
    item: &Value,
) -> Result<Option<String>, BrowserCardError> {
    let agent_id = item
        .get("agentId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| row.get(0),
        )
        .optional_row()?;
    let Some(raw) = raw else {
        return Ok(Some("The chat does not exist".into()));
    };
    let agent: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    if python_truthy(agent.get("deletedAt")) {
        return Ok(Some("The chat was deleted".into()));
    }
    // The host bridge also serves a VM chat's `host_exec` command, which runs on this Mac.
    let vm = agent.get("executionMode").and_then(Value::as_str) == Some("vm");
    if source.source == "vm" && !vm {
        return Ok(Some("The chat is not an active VM chat".into()));
    }
    if let Some(project) = agent
        .get("layrProjectId")
        .filter(|value| python_truthy(Some(value)))
        && item.get("projectId") != Some(project)
    {
        return Ok(Some("The chat belongs to another project".into()));
    }
    let checked = match crate::browser_policy::provider(
        item.get("url").and_then(Value::as_str).unwrap_or_default(),
        &source.http_providers,
    ) {
        Ok(checked) => checked,
        Err(error) => return Ok(Some(error.0)),
    };
    let redirect = checked
        .redirect
        .as_ref()
        .map(
            |redirect| json!({"host": redirect.host, "port": redirect.port, "path": redirect.path}),
        )
        .unwrap_or(Value::Null);
    if Some(&redirect) != item.get("redirect").or(Some(&Value::Null))
        || Some(checked.host.as_str()) != item.get("host").and_then(Value::as_str)
    {
        return Ok(Some("The link differs from its checked redirect".into()));
    }
    Ok(None)
}

trait OptionalRow<T> {
    fn optional_row(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalRow<T> for Result<T, rusqlite::Error> {
    fn optional_row(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// What applying one guest item did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The link got no card; the guest request must close with `refused`.
    pub refused: Option<String>,
    /// The guest request reached a final state.
    pub finished: bool,
    /// A chat event was queued.
    pub event: bool,
}

/// Write or expire the card of one guest item (`Bridge.apply`).
pub async fn apply(
    store: &RuntimeStore,
    source: &CardSource,
    item: Value,
    now: f64,
) -> Result<Applied, BrowserCardError> {
    let bridge_id = python_text(item.get("id"));
    let state = item
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let finished = FINAL_STATES.contains(&state.as_str());
    let key = card_id(&bridge_id);
    let source = source.clone();
    store
        .transaction_with_error(move |tx| {
            let row = QuestionsWriteRepository::request(tx, &key)?;
            let mut applied = Applied {
                refused: None,
                finished,
                event: false,
            };
            match row {
                None if state == "pending" => {
                    applied.refused = refusal(tx, &source, &item)?;
                    if applied.refused.is_none() {
                        let user_code = crate::browser_policy::provider(
                            item.get("url").and_then(Value::as_str).unwrap_or_default(),
                            &source.http_providers,
                        )
                        .ok()
                        .and_then(|checked| checked.user_code);
                        let params = BrowserParams {
                            bridge_id: Some(bridge_id.clone()),
                            url: item["url"].as_str().map(str::to_owned),
                            host: item["host"].as_str().map(str::to_owned),
                            source: Some(source.source.to_owned()),
                            redirect: item
                                .get("redirect")
                                .filter(|v| !v.is_null())
                                .map(|v| {
                                    serde::Deserialize::deserialize(v)
                                        .map_err(|_| BrowserCardError::Invalid)
                                })
                                .transpose()?,
                            client_id: item
                                .get("clientId")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            scope: item.get("scope").and_then(Value::as_str).map(str::to_owned),
                            linux_user: item
                                .get("linuxUser")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            user_code,
                            expires_at: item.get("expiresAt").and_then(Value::as_f64),
                            ..BrowserParams::default()
                        };
                        let raw = serde_json::value::to_raw_value(&params)
                            .map_err(|_| BrowserCardError::Invalid)?;
                        let mut card = BrowserCard {
                            record: RequestRecord::new(
                                key.clone(),
                                item["agentId"]
                                    .as_str()
                                    .ok_or(BrowserCardError::Invalid)?
                                    .into(),
                                studio_runtime_schema::request_record::RequestMethod::BrowserOpen,
                                studio_runtime_schema::persistence::RequestCardStatus::Pending,
                                now,
                                raw,
                            ),
                            params,
                        };
                        if let Err(error) = put_request(tx, &mut card) {
                            applied.refused = Some(format!("The card is invalid: {error}"));
                        }
                    }
                }
                // A request of another chat that waits on an open card (one card for one
                // sign-in): no card of its own; the card names it.
                None if state == crate::browser_host::JOINED => {
                    applied.refused = refusal(tx, &source, &item)?;
                    if applied.refused.is_none()
                        && let (Some(leader), Some(agent)) = (
                            item.get("joinedTo").and_then(Value::as_str),
                            item.get("agentId").and_then(Value::as_str),
                        )
                    {
                        join_card(tx, leader, agent, true)?;
                    }
                }
                // A joined request ended: its chat hears a sign-in on the card it waited on.
                None if finished => {
                    if let (Some(leader), Some(agent)) = (
                        item.get("joinedTo").and_then(Value::as_str),
                        item.get("agentId").and_then(Value::as_str),
                    ) {
                        join_card(tx, leader, agent, false)?;
                    }
                    if item.get("closeReason").and_then(Value::as_str)
                        == Some(crate::browser_host::JOINED_SIGNED_IN_REASON)
                    {
                        applied.event = joined_event(tx, &item, now);
                    }
                }
                Some(raw) if finished => {
                    use studio_runtime_schema::persistence::RequestCardStatus as Status;
                    let mut card = BrowserCard::decode(&raw)?;
                    let pasting = card.params.paste.is_some();
                    let local = item.get("closeReason").and_then(Value::as_str)
                        == Some(crate::browser_host::LOCAL_CLOSE_REASON);
                    let pending = card.record.status == Status::Pending || restart_expired(&card);
                    if pending && !pasting && local {
                        card.record.status = Status::Answered;
                        card.record.answered_by = Some("browser".into());
                        card.record.answered_at = Some(now);
                        card.record.decision =
                            Some(studio_runtime_schema::request_record::RequestDecision::Accept);
                        card.record.close_reason =
                            Some(crate::browser_host::LOCAL_CLOSE_REASON.into());
                        put_request(tx, &mut card)?;
                    } else if pending && !pasting {
                        let waiting = card.params.phase == Some(BrowserPhase::Waiting);
                        card.record.status = Status::Expired;
                        card.record.close_reason = Some(state.clone());
                        if waiting {
                            card.params.outcome = Some("expired".into());
                        }
                        put_request(tx, &mut card)?;
                        applied.event = if waiting {
                            outcome_event(tx, &card, now)
                        } else {
                            unanswered_event(tx, &card, &state, now)
                        };
                    }
                }
                Some(raw) if state == "pending" || state == "approved" => {
                    use studio_runtime_schema::persistence::RequestCardStatus as Status;
                    let mut card = BrowserCard::decode(&raw)?;
                    let waiting = card.params.phase == Some(BrowserPhase::Waiting);
                    let mut changed = false;
                    if restart_expired(&card) && (state == "pending" || waiting) {
                        card.record.status = Status::Pending;
                        card.record.close_reason = None;
                        changed = true;
                    }
                    if state == "pending"
                        && card.record.status == Status::Pending
                        && let Some(deadline) = item.get("expiresAt").and_then(Value::as_f64)
                        && card.params.expires_at != Some(deadline)
                    {
                        card.params.expires_at = Some(deadline);
                        changed = true;
                    }
                    if changed {
                        put_request(tx, &mut card)?;
                    }
                }
                _ => {}
            }
            Ok(applied)
        })
        .await
}

/// Run one blocking guest call on its own thread, so a long poll never holds the runtime's
/// blocking pool or its shutdown.
pub async fn guest_call(
    guest: Arc<dyn GuestRpc>,
    method: &'static str,
    params: Value,
    request_id: Option<String>,
    timeout: Duration,
) -> Result<Value, GuestError> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("studio-browser-guest".into())
        .spawn(move || {
            drop(sender.send(guest.call(method, &params, request_id.as_deref(), timeout)));
        });
    if spawned.is_err() {
        return Err(GuestError::Transport {
            message: "could not start the browser bridge call".into(),
            uncertain: false,
        });
    }
    receiver.await.unwrap_or(Err(GuestError::Transport {
        message: "the browser bridge call ended without a result".into(),
        uncertain: true,
    }))
}

/// Close a guest request. A failure leaves it to end at its own deadline.
pub async fn close_guest(guest: Arc<dyn GuestRpc>, bridge_id: &str, reason: &str) {
    drop(
        guest_call(
            guest,
            "browser.close",
            json!({"id": bridge_id, "reason": reason}),
            Some(format!("browser-close:{bridge_id}")),
            CLOSE_TIMEOUT,
        )
        .await,
    );
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or_default()
}

/// Read one batch of guest changes and apply it. Returns the next cursor.
pub async fn poll_once(
    store: &RuntimeStore,
    source: &CardSource,
    after: u64,
    wait_seconds: u64,
    event_wake: Option<&Notify>,
) -> Result<u64, PollError> {
    let result = guest_call(
        Arc::clone(&source.guest),
        "browser.wait",
        json!({"after": after, "waitSeconds": wait_seconds}),
        None,
        Duration::from_secs(wait_seconds + 15),
    )
    .await?;
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .ok_or(PollError::Invalid)?;
    let next = result
        .get("next")
        .and_then(Value::as_u64)
        .ok_or(PollError::Invalid)?;
    for item in items {
        let bridge_id = python_text(item.get("id"));
        let applied = apply(store, source, item, now_seconds()).await?;
        if let Some(reason) = &applied.refused {
            eprintln!("browser bridge refused link {bridge_id}: {reason}");
            close_guest(Arc::clone(&source.guest), &bridge_id, "refused").await;
        }
        if applied.finished
            && let Some(forwarder) = &source.forwarder
        {
            drop(forwarder.close(&bridge_id).await);
        }
        if applied.event
            && let Some(wake) = event_wake
        {
            wake.notify_one();
        }
    }
    Ok(next)
}

#[derive(Debug, thiserror::Error)]
pub enum PollError {
    #[error(transparent)]
    Guest(#[from] GuestError),
    #[error(transparent)]
    Card(#[from] BrowserCardError),
    #[error("the browser.wait result is invalid")]
    Invalid,
}

/// Keep one source's poll running until shutdown. An older guest refuses the unknown method
/// with invalid_params; a stopped VM has no guest socket.
pub async fn run(
    store: Arc<RuntimeStore>,
    source: CardSource,
    event_wake: Option<Arc<Notify>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut after = 0;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let poll = poll_once(&store, &source, after, POLL_SECONDS, event_wake.as_deref());
        let delay = tokio::select! {
            _ = shutdown.wait_for(|stopped| *stopped) => return,
            result = poll => match result {
                Ok(next) => {
                    after = next;
                    continue;
                }
                Err(PollError::Guest(error))
                    if matches!(error.code(), Some("invalid_params" | "invalid_request")) => 120,
                Err(PollError::Guest(_)) => 15,
                Err(error) => {
                    eprintln!("browser bridge poll ({}): {error}", source.source);
                    15
                }
            },
        };
        tokio::select! {
            _ = shutdown.wait_for(|stopped| *stopped) => return,
            () = tokio::time::sleep(Duration::from_secs(delay)) => {}
        }
    }
}

#[cfg(test)]
#[path = "browser_cards_tests.rs"]
mod tests;
