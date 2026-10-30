//! Sign-in request cards from the browser bridges (docs/vm-browser-bridge.md, sections 2(b)
//! and 6). The Rust port of `codex_browser_bridge.Bridge.poll` and `apply`.
//!
//! One poll runs for each source: the guest service of the Linux VM on this Mac (`vm`) and
//! the host bridge of commands on this server (`host`, `browser_host`). It long-polls `browser.wait`,
//! writes one typed `BrowserOpen` approval for each new pending link, refuses links that no chat
//! can own, keeps the card's deadline equal to the guest request's, and expires a pending
//! card when its guest request ends. A card that nobody answered tells its chat
//! (`unanswered_event`), so the agent does not wait for a result that cannot come.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use studio_runtime_schema::approvals::{
    AgentId, Answer, Approval, ApprovalId, ApprovalState, BrowserCard, BrowserOutcome,
    BrowserPhase, BrowserRedirect, BrowserSource, CloseReason, Decision, Detail, Principal,
    ProviderObject, Request, Timestamp,
};
use studio_runtime_store::approvals::{self, ApprovalError, NewApproval};
use studio_runtime_store::{RuntimeStore, Transaction};
use tokio::sync::{Notify, watch};

use crate::browser::BrowserForwarder;
use crate::linux_vm_guest::{GuestError, GuestRpc};

/// The method name of the old request records. Readers outside the browser cluster
/// still query it until they move to the approvals table.
pub const METHOD: &str = "browser/open";

/// The long poll of `browser.wait`, in seconds (`POLL_SECONDS`).
const POLL_SECONDS: u64 = 20;
/// Guest request states that end a card (`FINAL_STATES`).
pub const FINAL_STATES: [&str; 6] = [
    "declined", "stale", "expired", "closed", "replaced", "opened",
];
/// The `closeReason` of a card that startup recovery expired (`monitor_runtime`). The
/// bridge keeps its request across the restart, so the poll reopens the card while that
/// request is open, and tells the chat when it ended meanwhile without an answer.
pub const RESTART_CLOSE_REASON: &str = "restart";
const CLOSE_TIMEOUT: Duration = Duration::from_secs(15);

/// The chat event text of each outcome after a paste (`OUTCOME_TEXT` of `codex_browser_bridge.py`).
fn outcome_text(outcome: BrowserOutcome) -> Option<&'static str> {
    Some(match outcome {
        BrowserOutcome::SignedIn => {
            "The person signed in at {host}. Studio sent the result to the command on port {port}; the command answered HTTP {http}."
        }
        BrowserOutcome::Refused => {
            "The person signed in at {host}, but the command refused the result (HTTP {http})."
        }
        BrowserOutcome::ProviderError => {
            "The sign-in at {host} returned an error. Studio sent it to the command on port {port}."
        }
        BrowserOutcome::Unknown => {
            "The result of the sign-in at {host} is unknown. Studio does not send it again. Check the command output."
        }
        BrowserOutcome::Expired => {
            "The sign-in at {host} ended without a result: the request expired or the command stopped."
        }
        BrowserOutcome::Cancelled => return None,
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
    #[error(transparent)]
    Approval(#[from] ApprovalError),
}

/// The stored sign-in card with this id, or none. Decoding is strict: a damaged row is an error.
pub(crate) fn load(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<Option<(Approval, BrowserCard)>, BrowserCardError> {
    Ok(approvals::get(tx, &ApprovalId(id.to_owned()))?.and_then(|row| {
        let Request::BrowserOpen(card) = &row.request else {
            return None;
        };
        let card = card.clone();
        Some((row, card))
    }))
}

/// Store a changed card and publish its client view in the same transaction. The renderer
/// shows a card only while it is pending; a closed card loses its entity.
pub(crate) fn save(
    tx: &Transaction<'_>,
    mut row: Approval,
    card: BrowserCard,
) -> Result<(), BrowserCardError> {
    row.request = Request::BrowserOpen(card);
    approvals::put(tx, &row)?;
    crate::approval_sync::publish(tx, &row)?;
    Ok(())
}

fn optional_text<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "None".into(), |value| value.to_string())
}

/// Tell the chat how a sign-in that the person opened ended (section 6.2). Returns whether an
/// event was queued.
pub(crate) fn outcome_event(
    tx: &Transaction<'_>,
    row: &Approval,
    card: &BrowserCard,
    now: f64,
) -> bool {
    let BrowserPhase::Finished { outcome, http } = &card.phase else {
        return false;
    };
    let Some(text) = outcome_text(*outcome) else {
        return false;
    };
    let Ok(agent) = crate::event_write::checked_agent(tx, row.agent.as_str()) else {
        return false;
    };
    let text = text
        .replace("{host}", &card.host)
        .replace(
            "{port}",
            &optional_text(card.redirect.as_ref().map(|redirect| redirect.port)),
        )
        .replace("{http}", &optional_text(*http));
    match crate::event_write::enqueue_event(
        tx,
        &agent,
        format!("browser-outcome:{}", card.bridge_id),
        "browser_outcome",
        text,
        now,
    ) {
        Ok(_) => true,
        Err(error) => {
            eprintln!("browser outcome event for {} failed: {error}", card.bridge_id);
            false
        }
    }
}

/// Tell the chat that its pending card closed without an answer. Returns whether an event was
/// queued. The event ID names the guest request, so a repeated final state queues nothing.
fn unanswered_event(tx: &Transaction<'_>, row: &Approval, card: &BrowserCard, state: &str, now: f64) -> bool {
    let Some(text) = unanswered_text(state) else {
        return false;
    };
    let Ok(agent) = crate::event_write::checked_agent(tx, row.agent.as_str()) else {
        return false;
    };
    match crate::event_write::enqueue_event(
        tx,
        &agent,
        format!("browser-unanswered:{}", card.bridge_id),
        "browser_unanswered",
        text.replace("{host}", &card.host),
        now,
    ) {
        Ok(queued) => queued,
        Err(error) => {
            eprintln!("browser unanswered event for {} failed: {error}", card.bridge_id);
            false
        }
    }
}

/// The chat event of a joined request whose sign-in finished on another chat's card.
const JOINED_SIGNED_IN_TEXT: &str = "The person signed in at {host} on the sign-in card of another chat. Your own sign-in command did not get that result. Stop it if it still waits, then run it again: the sign-in session is now active.";

/// Add a chat that waits on a card to the card's `joined` list, or take it away.
fn join_card(
    tx: &Transaction<'_>,
    leader: &str,
    agent: &str,
    waits: bool,
) -> Result<(), BrowserCardError> {
    let Some((row, mut card)) = load(tx, &card_id(leader))? else {
        return Ok(());
    };
    if !matches!(row.state, ApprovalState::Pending) {
        return Ok(());
    }
    let before = card.joined.clone();
    card.joined.retain(|chat| chat.as_str() != agent);
    if waits {
        card.joined.push(AgentId(agent.to_owned()));
    }
    if card.joined != before {
        save(tx, row, card)?;
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
fn restart_expired(row: &Approval) -> bool {
    matches!(
        row.state,
        ApprovalState::Closed { reason: CloseReason::Expired, .. }
    ) && row.detail.host_close_reason.as_deref() == Some(RESTART_CLOSE_REASON)
}

/// Whether the person opened the link in paste mode and the card waits for the address.
pub(crate) fn waits_for_address(phase: &BrowserPhase) -> bool {
    matches!(phase, BrowserPhase::Waiting { .. } | BrowserPhase::Pasted { .. })
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
            let mut applied = Applied {
                refused: None,
                finished,
                event: false,
            };
            match load(tx, &key)? {
                None if state == "pending" => {
                    applied.refused = refusal(tx, &source, &item)?;
                    if applied.refused.is_none() {
                        match new_card(&source, &item, &bridge_id) {
                            Some((agent, card)) => {
                                let row = approvals::create(
                                    tx,
                                    NewApproval {
                                        id: ApprovalId(key.clone()),
                                        agent,
                                        turn: None,
                                        epoch: None,
                                        request: Request::BrowserOpen(card),
                                        delivery: None,
                                        detail: Detail {
                                            params: ProviderObject(serde_json::Map::new()),
                                            preview: None,
                                            answer_error: None,
                                            host_close_reason: None,
                                        },
                                        fire_at: None,
                                    },
                                    Timestamp(now),
                                )?;
                                crate::approval_sync::publish(tx, &row)?;
                            }
                            None => applied.refused = Some("The card is invalid".into()),
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
                Some((mut row, mut card)) if finished => {
                    // A pasted result in flight writes its own outcome (browser_answer).
                    let pasting = matches!(card.phase, BrowserPhase::Pasted { .. });
                    let local = item.get("closeReason").and_then(Value::as_str)
                        == Some(crate::browser_host::LOCAL_CLOSE_REASON);
                    // A card that startup recovery expired ends as an open card does: the
                    // request ended while the server was down, and nobody answered it.
                    let pending = matches!(row.state, ApprovalState::Pending) || restart_expired(&row);
                    if pending && !pasting && local {
                        // The browser of this Mac had the page and the command stopped
                        // listening: it ended its sign-in there, and nobody waits for the card.
                        row.state = ApprovalState::Answered {
                            answer: Answer::Decision { decision: Decision::Accept },
                            by: Principal::Owner,
                            at: Timestamp(now),
                        };
                        row.detail.host_close_reason =
                            Some(crate::browser_host::LOCAL_CLOSE_REASON.to_owned());
                        save(tx, row, card)?;
                    } else if pending && !pasting {
                        let waiting = waits_for_address(&card.phase);
                        row.state = ApprovalState::Closed {
                            reason: CloseReason::Expired,
                            at: Timestamp(now),
                        };
                        row.detail.host_close_reason = Some(state.clone());
                        if waiting {
                            card.phase = BrowserPhase::Finished {
                                outcome: BrowserOutcome::Expired,
                                http: None,
                            };
                        }
                        applied.event = if waiting {
                            outcome_event(tx, &row, &card, now)
                        } else {
                            unanswered_event(tx, &row, &card, &state, now)
                        };
                        save(tx, row, card)?;
                    }
                }
                // An open request whose card startup recovery expired gets its card back: the
                // pending link, or the waiting phase of a phone sign-in.
                Some((mut row, mut card)) if state == "pending" || state == "approved" => {
                    let mut changed = false;
                    if restart_expired(&row)
                        && (state == "pending" || waits_for_address(&card.phase))
                    {
                        row.state = ApprovalState::Pending;
                        row.detail.host_close_reason = None;
                        changed = true;
                    }
                    // A listening command moved the deadline of its open request (`check`).
                    let deadline = item.get("expiresAt").and_then(Value::as_f64).map(Timestamp);
                    if state == "pending"
                        && matches!(row.state, ApprovalState::Pending)
                        && let Some(deadline) = deadline
                        && card.expires_at != Some(deadline)
                    {
                        card.expires_at = Some(deadline);
                        changed = true;
                    }
                    if changed {
                        save(tx, row, card)?;
                    }
                }
                _ => {}
            }
            Ok(applied)
        })
        .await
}

/// The owner chat and card of a new pending link. The link was checked by `refusal`.
fn new_card(source: &CardSource, item: &Value, bridge_id: &str) -> Option<(AgentId, BrowserCard)> {
    let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
    let url = text("url")?;
    let user_code = crate::browser_policy::provider(&url, &source.http_providers)
        .ok()
        .and_then(|checked| checked.user_code);
    let redirect = match item.get("redirect") {
        None | Some(Value::Null) => None,
        Some(value) => Some(serde_json::from_value::<BrowserRedirect>(value.clone()).ok()?),
    };
    Some((
        AgentId(text("agentId")?),
        BrowserCard {
            url,
            bridge_id: bridge_id.to_owned(),
            host: text("host")?,
            source: if source.source == "host" {
                BrowserSource::Host
            } else {
                BrowserSource::Vm
            },
            redirect,
            client_id: text("clientId"),
            scope: text("scope"),
            linux_user: text("linuxUser"),
            user_code,
            expires_at: item.get("expiresAt").and_then(Value::as_f64).map(Timestamp),
            joined: Vec::new(),
            forward: None,
            phase: BrowserPhase::Link,
        },
    ))
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

/// Typed rows for the tests of the browser cluster.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    pub const BRIDGE: &str = "0123456789abcdef0123456789abcdef";

    pub fn card(bridge: &str) -> BrowserCard {
        BrowserCard {
            url: "https://login.example.test/".into(),
            bridge_id: bridge.into(),
            host: "login.example.test".into(),
            source: BrowserSource::Vm,
            redirect: None,
            client_id: None,
            scope: None,
            linux_user: None,
            user_code: None,
            expires_at: None,
            joined: Vec::new(),
            forward: None,
            phase: BrowserPhase::Link,
        }
    }

    pub fn row(id: &str, agent: &str, card: BrowserCard) -> Approval {
        Approval {
            id: ApprovalId(id.into()),
            agent: AgentId(agent.into()),
            turn: None,
            created: Timestamp(1.0),
            epoch: None,
            request: Request::BrowserOpen(card),
            delivery: None,
            detail: Detail {
                params: ProviderObject(serde_json::Map::new()),
                preview: None,
                answer_error: None,
                host_close_reason: None,
            },
            state: ApprovalState::Pending,
            deferral: None,
            fire_at: None,
            hidden: None,
        }
    }

    /// Store a pending card for the guest request `card.bridge_id`.
    pub async fn seed(store: &RuntimeStore, agent: &str, card: BrowserCard) -> String {
        let id = card_id(&card.bridge_id);
        let stored = row(&id, agent, card);
        store
            .transaction_with_error(move |tx| approvals::put(tx, &stored).map_err(BrowserCardError::from))
            .await
            .expect("card is seeded");
        id
    }

    pub fn read_optional(store: &RuntimeStore, id: &str) -> Option<(Approval, BrowserCard)> {
        let id = id.to_owned();
        store
            .test_read_transaction(move |tx| {
                load(tx, &id).map_err(|error| {
                    studio_runtime_store::StoreError::SchemaMismatch(error.to_string())
                })
            })
            .expect("card reads")
    }

    pub fn read(store: &RuntimeStore, id: &str) -> (Approval, BrowserCard) {
        read_optional(store, id).expect("card exists")
    }
}

#[cfg(test)]
#[path = "browser_cards_tests.rs"]
mod tests;
