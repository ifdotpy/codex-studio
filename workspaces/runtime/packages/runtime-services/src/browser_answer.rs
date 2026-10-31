//! Durable browser-open card answers bridged to the VM guest.

use std::{sync::Arc, time::Duration};

use base64::Engine as _;
use serde_json::{Value, json};
use studio_runtime_schema::approvals::{
    Answer, Approval, ApprovalId, ApprovalState, BrowserCard, BrowserForward, BrowserOutcome,
    BrowserPhase, CloseReason, CommandId, Decision, Principal, Timestamp,
};
use studio_runtime_store::approvals::{self, AnswerOutcome, ApprovalError};
use studio_runtime_store::{RuntimeStore, Transaction};
use thiserror::Error;

use crate::browser_cards::{self, BrowserCardError, waits_for_address};

const BROWSER_ANSWER_TIMEOUT: Duration = Duration::from_secs(15);
const BROWSER_TOKEN_BYTES: usize = 32;
const LOCAL_FORWARD_SECONDS: u64 = 600;
const BROWSER_DECLINED: &str = "The person declined to open the requested page in the browser. The sign-in command keeps waiting until its own timeout.";
/// One relayed pasted redirect may take this long (`Bridge.relay` of the Python server).
const PASTE_RELAY_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest relay body, in bytes (`BODY_LIMIT`).
const PASTE_BODY_LIMIT: usize = 256 * 1024;
/// Guest answers that end a relay without a code reaching the tool (`DEFINITE_CODES`).
const DEFINITE_CODES: [&str; 4] = ["stale", "limit", "invalid_params", "not_found"];
const INVALID_CARD: &str = "The sign-in request is invalid";

#[derive(Debug, Error)]
pub enum BrowserAnswerError {
    #[error("{0}")]
    Invalid(&'static str),
    /// A refusal whose text names the problem, for example a pasted address of another
    /// sign-in.
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("the sign-in command already stopped")]
    Stale,
    #[error("browser bridge outcome is unknown")]
    Unknown,
    #[error(transparent)]
    Guest(#[from] crate::linux_vm_guest::GuestError),
    #[error(transparent)]
    Forward(#[from] crate::browser::BrowserError),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

impl From<ApprovalError> for BrowserAnswerError {
    fn from(error: ApprovalError) -> Self {
        match error {
            ApprovalError::Conflict(_) => {
                Self::Conflict("This request already has a different answer")
            }
            ApprovalError::Closed(_) | ApprovalError::State(_) => {
                Self::Invalid("Request is no longer pending")
            }
            ApprovalError::NotFound(_) => Self::Invalid("Unknown request"),
            ApprovalError::Sql(error) => Self::Sqlite(error),
            _ => Self::Invalid(INVALID_CARD),
        }
    }
}

impl From<BrowserCardError> for BrowserAnswerError {
    fn from(error: BrowserCardError) -> Self {
        match error {
            BrowserCardError::Store(error) => Self::Store(error),
            BrowserCardError::Sqlite(error) => Self::Sqlite(error),
            BrowserCardError::Approval(error) => error.into(),
        }
    }
}

pub struct BrowserAnswerResult {
    pub response: Value,
    pub replayed: bool,
}

pub struct BrowserAnswerContext {
    pub local_window: bool,
    pub guest: Arc<dyn crate::linux_vm_guest::GuestRpc>,
    pub local_forwarder: Option<Arc<crate::browser::BrowserForwarder>>,
    pub now: f64,
    /// Test provider origins that may use http (`browser_policy`). Production passes none.
    pub http_providers: std::collections::BTreeSet<String>,
}

fn decision_answer(decision: Decision) -> Answer {
    Answer::Decision { decision }
}

fn load_card(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<(Approval, BrowserCard), BrowserAnswerError> {
    browser_cards::load(tx, id)?.ok_or(BrowserAnswerError::Invalid("Unknown request"))
}

/// Record the person's answer through the approval commands. The guest already took it, so
/// the delivery is confirmed at once. Returns whether the answer is new.
fn record_answer(
    tx: &Transaction<'_>,
    id: &str,
    decision: Decision,
    now: f64,
) -> Result<bool, BrowserAnswerError> {
    let id = ApprovalId(id.to_owned());
    match approvals::answer(tx, &id, decision_answer(decision), CommandId(id.0.clone()))? {
        AnswerOutcome::Started(_) => {
            approvals::confirm_delivery(tx, &id, Principal::Owner, Timestamp(now))?;
            Ok(true)
        }
        AnswerOutcome::Replayed(_) => Ok(false),
    }
}

/// Replace the phase and publish the card.
fn set_phase(
    tx: &Transaction<'_>,
    id: &str,
    phase: BrowserPhase,
) -> Result<(Approval, BrowserCard), BrowserAnswerError> {
    let (row, mut card) = load_card(tx, id)?;
    card.phase = phase;
    browser_cards::save(tx, row.clone(), card.clone())?;
    Ok((row, card))
}

pub async fn is_host_source(store: &RuntimeStore, id: String) -> Result<bool, BrowserAnswerError> {
    let card = store
        .read_async("runtime-services/browser_answer", move |tx| {
            browser_cards::load(tx, &id)
        })
        .await?;
    Ok(card.is_some_and(|(_, card)| card.source == studio_runtime_schema::approvals::BrowserSource::Host))
}

/// The guest owns the sign-in request. A stable request ID lets its durable
/// receipt resolve an unknown transport outcome without delivering twice.
pub async fn answer(
    store: &RuntimeStore,
    id: &str,
    data: &Value,
    context: BrowserAnswerContext,
) -> Result<Option<BrowserAnswerResult>, BrowserAnswerError> {
    let BrowserAnswerContext {
        local_window,
        guest,
        local_forwarder,
        now,
        http_providers,
    } = context;
    let id_for_read = id.to_owned();
    let Some((row, card)) = store
        .read_async("runtime-services/browser_answer", move |tx| {
            browser_cards::load(tx, &id_for_read)
        })
        .await?
    else {
        return Ok(None);
    };
    let decision = data
        .get("decision")
        .and_then(Value::as_str)
        .ok_or(BrowserAnswerError::Invalid("Choose accept or decline"))?;
    if decision == "answer"
        && let Some(result) = data.get("result").and_then(Value::as_str)
    {
        return paste(
            store,
            id,
            result,
            PasteContext {
                guest,
                now,
                http_providers,
            },
        )
        .await
        .map(Some);
    }
    let wanted = match decision {
        "accept" => Decision::Accept,
        "decline" => Decision::Decline,
        _ => return Err(BrowserAnswerError::Invalid("Choose accept or decline")),
    };
    // Paste mode: any window. Nothing binds a port; the person pastes the redirect.
    let accept = wanted == Decision::Accept;
    let paste_mode = accept && data.get("paste").and_then(Value::as_bool) == Some(true);
    let bridge_id = Some(card.bridge_id.as_str())
        .filter(|value| value.len() == 32)
        .ok_or(BrowserAnswerError::Invalid(INVALID_CARD))?;
    let url = card.url.as_str();
    let has_redirect = card.redirect.is_some();
    let pending = matches!(row.state, ApprovalState::Pending);
    if pending && waits_for_address(&card.phase) {
        if accept {
            if paste_mode {
                return Ok(Some(BrowserAnswerResult {
                    response: json!({"status":"answered","replayed":true,"openUrl":url}),
                    replayed: true,
                }));
            }
            return Err(BrowserAnswerError::Invalid(
                "This sign-in already waits for the address of the page that did not load.",
            ));
        }
        return cancel(store, id, bridge_id, &card.host, guest, now)
            .await
            .map(Some);
    }
    if let ApprovalState::Answering { answer: given, .. }
    | ApprovalState::Answered { answer: given, .. }
    | ApprovalState::Unknown { answer: given, .. } = &row.state
    {
        if *given != decision_answer(wanted) {
            return Err(BrowserAnswerError::Conflict(
                "This request already has a different answer",
            ));
        }
        if matches!(row.state, ApprovalState::Answered { .. }) {
            return Ok(Some(BrowserAnswerResult {
                response: if accept && (local_window || (paste_mode && !has_redirect)) {
                    json!({"status":"answered","replayed":true,"openUrl":url})
                } else {
                    json!({"status":"answered","replayed":true})
                },
                replayed: true,
            }));
        }
        return Err(BrowserAnswerError::Unknown);
    }
    if !pending {
        return Err(BrowserAnswerError::Invalid("Request is no longer pending"));
    }
    let remote = accept
        && !local_window
        && !paste_mode
        && data.get("forward").and_then(Value::as_bool) == Some(true);
    if accept && !local_window && !remote && !paste_mode {
        return Err(BrowserAnswerError::Invalid(
            "Open this chat in a window on the computer that runs its server to sign in, or use the tool's device-code mode.",
        ));
    }

    let rpc_decision = if accept { "approve" } else { "decline" };
    let local_forward = if accept
        && local_window
        && !paste_mode
        && card.source == studio_runtime_schema::approvals::BrowserSource::Vm
    {
        if let Some(redirect) = &card.redirect {
            let forwarder = local_forwarder.ok_or(BrowserAnswerError::Unknown)?;
            let input = crate::browser::ForwardInput::new(
                store
                    .local_server_id()?
                    .ok_or(BrowserAnswerError::Unknown)?,
                bridge_id.to_owned(),
                format!("local-browser-forward:{bridge_id}"),
                redirect.host.clone(),
                redirect.port,
                redirect.path.clone(),
                now + LOCAL_FORWARD_SECONDS as f64,
            );
            forwarder.open(input, now, None).await?;
            Some(forwarder)
        } else {
            None
        }
    } else {
        None
    };
    let guest = Arc::clone(&guest);
    let guest_params = json!({"id":bridge_id,"decision":rpc_decision});
    let guest_request_id = format!("browser-answer:{bridge_id}:{rpc_decision}");
    let call = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/browser_answer_guest_rpc",
        move || {
            guest.call(
                "browser.answer",
                &guest_params,
                Some(&guest_request_id),
                BROWSER_ANSWER_TIMEOUT,
            )
        },
    )
    .await
    .map_err(|_| BrowserAnswerError::Unknown)?;
    let answered = match call {
        Ok(value) => Some(value),
        Err(_error) if !accept => None,
        Err(error) if error.code() == Some("stale") => {
            if let Some(forwarder) = &local_forward {
                drop(forwarder.close(bridge_id).await);
            }
            let id = id.to_owned();
            store
                .transaction_with_error(move |tx| expire(tx, &id, now))
                .await?;
            return Err(BrowserAnswerError::Stale);
        }
        Err(error) => {
            if let Some(forwarder) = &local_forward {
                drop(forwarder.close(bridge_id).await);
            }
            return Err(BrowserAnswerError::Guest(error));
        }
    };
    if paste_mode && has_redirect {
        // Keep the card pending in the waiting phase until a pasted result or the deadline.
        let expires_at = answered
            .as_ref()
            .and_then(|value| value.pointer("/lease/expiresAt"))
            .and_then(Value::as_f64)
            .ok_or(BrowserAnswerError::Unknown)?;
        let id = id.to_owned();
        store
            .transaction_with_error(move |tx| {
                let (row, _) = load_card(tx, &id)?;
                if matches!(row.state, ApprovalState::Pending) {
                    set_phase(
                        tx,
                        &id,
                        BrowserPhase::Waiting {
                            expires_at: Timestamp(expires_at),
                        },
                    )?;
                }
                Ok::<_, BrowserAnswerError>(())
            })
            .await?;
        return Ok(Some(BrowserAnswerResult {
            response: json!({"status":"answered","openUrl":url}),
            replayed: false,
        }));
    }
    let forward = if remote && let Some(redirect) = &card.redirect {
        let expires_at = answered
            .as_ref()
            .and_then(|value| value.pointer("/lease/expiresAt"))
            .and_then(Value::as_f64)
            .ok_or(BrowserAnswerError::Unknown)?;
        let mut bytes = [0_u8; BROWSER_TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| BrowserAnswerError::Unknown)?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let server = store
            .local_server_id()?
            .ok_or(BrowserAnswerError::Unknown)?;
        let state = BrowserForward {
            token_hash: studio_runtime_store::sha256_hex(token.as_bytes()),
            expires_at: Timestamp(expires_at),
        };
        let spec = json!({
            "server":server,
            "bridgeId":bridge_id,
            "host":redirect.host,
            "port":redirect.port,
            "path":redirect.path,
            "expiresAt":expires_at,
            "token":token
        });
        Some((state, spec))
    } else {
        None
    };
    let id = id.to_owned();
    let forward_state = forward.as_ref().map(|(state, _)| state.clone());
    store
        .transaction_with_error(move |tx| finish(tx, &id, wanted, forward_state, now))
        .await?;
    let response = if let Some((_, spec)) = forward {
        json!({"status":"answered","openUrl":url,"forward":spec})
    } else if accept {
        json!({"status":"answered","openUrl":url})
    } else {
        json!({"status":"answered"})
    };
    Ok(Some(BrowserAnswerResult {
        response,
        replayed: false,
    }))
}

fn expire(tx: &Transaction<'_>, id: &str, now: f64) -> Result<(), BrowserAnswerError> {
    let (mut row, card) = load_card(tx, id)?;
    if matches!(row.state, ApprovalState::Pending) {
        row.state = ApprovalState::Closed {
            reason: CloseReason::Expired,
            at: Timestamp(now),
        };
        row.detail.host_close_reason = Some("stale".to_owned());
        browser_cards::save(tx, row, card)?;
    }
    Ok(())
}

/// Decline in the waiting phase: close the guest request; no result can pass.
async fn cancel(
    store: &RuntimeStore,
    id: &str,
    bridge_id: &str,
    host: &str,
    guest: Arc<dyn crate::linux_vm_guest::GuestRpc>,
    now: f64,
) -> Result<BrowserAnswerResult, BrowserAnswerError> {
    browser_cards::close_guest(guest, bridge_id, "cancelled").await;
    let id = id.to_owned();
    let text = format!(
        "The person cancelled the sign-in at {host}. The sign-in command keeps waiting until its own timeout. Stop it, or ask the person before you run it again."
    );
    store
        .transaction_with_error(move |tx| {
            let (row, _) = load_card(tx, &id)?;
            if !matches!(row.state, ApprovalState::Pending) {
                return Err(BrowserAnswerError::Invalid("Request is no longer pending"));
            }
            record_answer(tx, &id, Decision::Decline, now)?;
            let (row, card) = set_phase(
                tx,
                &id,
                BrowserPhase::Finished {
                    outcome: BrowserOutcome::Cancelled,
                    http: None,
                },
            )?;
            if let Ok(agent) = crate::event_write::checked_agent(tx, row.agent.as_str()) {
                crate::event_write::enqueue_event(
                    tx,
                    &agent,
                    format!("browser-declined:{}", card.bridge_id),
                    "browser_declined",
                    text,
                    now,
                )
                .map_err(|_| {
                    BrowserAnswerError::Invalid("The cancelled sign-in could not be recorded")
                })?;
            }
            Ok(())
        })
        .await?;
    Ok(BrowserAnswerResult {
        response: json!({"status":"answered"}),
        replayed: false,
    })
}

struct PasteContext {
    guest: Arc<dyn crate::linux_vm_guest::GuestRpc>,
    now: f64,
    http_providers: std::collections::BTreeSet<String>,
}

/// The status of a relay result that the browser could see (`cleaned` of the Python
/// server): a redirect becomes Studio's own page, so it counts as 200.
fn relay_status(result: &Value) -> Option<u16> {
    let status = result.get("status").and_then(Value::as_u64)?;
    if !(100..=599).contains(&status) {
        return None;
    }
    if (300..400).contains(&status) {
        return Some(200);
    }
    let body = base64::engine::general_purpose::STANDARD
        .decode(
            result
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .ok()?;
    (body.len() <= PASTE_BODY_LIMIT).then_some(status as u16)
}

/// Relay the address that the person pasted after a sign-in in paste mode, once
/// (docs/vm-browser-bridge.md, section 6.3). Studio stores no pasted address.
async fn paste(
    store: &RuntimeStore,
    id: &str,
    text: &str,
    context: PasteContext,
) -> Result<BrowserAnswerResult, BrowserAnswerError> {
    let PasteContext {
        guest,
        now,
        http_providers,
    } = context;
    let read_id = id.to_owned();
    let (row, card) = store
        .read_async("runtime-services/browser_answer_paste", move |tx| {
            load_card(tx, &read_id)
        })
        .await?;
    let waiting = matches!(row.state, ApprovalState::Pending)
        && matches!(card.phase, BrowserPhase::Waiting { .. });
    if !waiting {
        if let BrowserPhase::Finished { outcome, .. } = card.phase
            && outcome.was_pasted()
            && matches!(
                row.state,
                ApprovalState::Answered { .. } | ApprovalState::Unknown { .. }
            )
        {
            return Ok(BrowserAnswerResult {
                response: json!({"status":"answered","replayed":true}),
                replayed: true,
            });
        }
        return Err(BrowserAnswerError::Refused(
            "This sign-in does not wait for a pasted address.".into(),
        ));
    }
    let state = crate::browser_policy::provider(&card.url, &http_providers)
        .map_err(|error| BrowserAnswerError::Refused(error.0))?
        .state;
    let redirect = card
        .redirect
        .as_ref()
        .ok_or(BrowserAnswerError::Invalid(INVALID_CARD))?;
    let callback =
        crate::browser_policy::pasted(text, redirect.port, &redirect.path, state.as_deref())
            .map_err(|error| BrowserAnswerError::Refused(error.0))?;
    let bridge_id = card.bridge_id.clone();
    let mark_id = id.to_owned();
    store
        .transaction_with_error(move |tx| {
            let (row, card) = load_card(tx, &mark_id)?;
            let BrowserPhase::Waiting { expires_at } = card.phase else {
                return Err(BrowserAnswerError::Refused(
                    "Studio already sends a pasted address for this sign-in.".into(),
                ));
            };
            if !matches!(row.state, ApprovalState::Pending) {
                return Err(BrowserAnswerError::Refused(
                    "Studio already sends a pasted address for this sign-in.".into(),
                ));
            }
            // The mark comes before the relay: a second paste never sends a second code.
            set_phase(
                tx,
                &mark_id,
                BrowserPhase::Pasted {
                    expires_at,
                    at: Timestamp(now),
                },
            )?;
            Ok::<_, BrowserAnswerError>(())
        })
        .await?;
    let request_id = format!("browser-callback:{}", uuid::Uuid::new_v4().simple());
    let relayed = browser_cards::guest_call(
        Arc::clone(&guest),
        "browser.callback",
        json!({"id": bridge_id, "target": callback.target, "headers": {}}),
        Some(request_id),
        PASTE_RELAY_TIMEOUT,
    )
    .await;
    if let Err(error) = &relayed
        && error.code() == Some(crate::browser_host::NOT_SENT_CODE)
        && !error.uncertain()
    {
        // The bridge sent nothing to the command, so clear this paste's mark: the
        // card keeps waiting and a retry sends the code once.
        let retry_id = id.to_owned();
        store
            .transaction_with_error(move |tx| {
                let (row, card) = load_card(tx, &retry_id)?;
                if matches!(row.state, ApprovalState::Pending)
                    && let BrowserPhase::Pasted { expires_at, .. } = card.phase
                {
                    set_phase(tx, &retry_id, BrowserPhase::Waiting { expires_at })?;
                }
                Ok::<_, BrowserAnswerError>(())
            })
            .await?;
        return Err(BrowserAnswerError::Refused(
            "Studio could not check the sign-in command now. Nothing was sent. Paste the address again."
                .into(),
        ));
    }
    let (outcome, http_status) = match relayed {
        Ok(result) => match relay_status(&result) {
            Some(status) => (
                if callback.error {
                    BrowserOutcome::ProviderError
                } else if (200..300).contains(&status) {
                    BrowserOutcome::SignedIn
                } else {
                    BrowserOutcome::Refused
                },
                Some(status),
            ),
            None => (BrowserOutcome::Unknown, None),
        },
        Err(error)
            if error
                .code()
                .is_some_and(|code| DEFINITE_CODES.contains(&code)) =>
        {
            (BrowserOutcome::Expired, None)
        }
        Err(_) => (BrowserOutcome::Unknown, None),
    };
    let final_id = id.to_owned();
    store
        .transaction_with_error(move |tx| {
            match outcome {
                BrowserOutcome::Expired => {
                    let (mut row, card) = load_card(tx, &final_id)?;
                    row.state = ApprovalState::Closed {
                        reason: CloseReason::Expired,
                        at: Timestamp(now),
                    };
                    row.detail.host_close_reason = Some("stale".to_owned());
                    browser_cards::save(tx, row, card)?;
                }
                BrowserOutcome::Unknown => {
                    let approval = ApprovalId(final_id.clone());
                    approvals::answer(
                        tx,
                        &approval,
                        decision_answer(Decision::Accept),
                        CommandId(final_id.clone()),
                    )?;
                    approvals::delivery_lost(tx, &approval, Timestamp(now))?;
                }
                _ => {
                    record_answer(tx, &final_id, Decision::Accept, now)?;
                }
            }
            let (row, card) = set_phase(
                tx,
                &final_id,
                BrowserPhase::Finished {
                    outcome,
                    http: http_status,
                },
            )?;
            browser_cards::outcome_event(tx, &row, &card, now);
            Ok::<_, BrowserAnswerError>(())
        })
        .await?;
    match outcome {
        BrowserOutcome::Unknown => {
            browser_cards::close_guest(guest, &bridge_id, "unknown").await;
            Err(BrowserAnswerError::Refused(
                "The result of the sign-in is unknown. Studio does not send it again. Check the command output."
                    .into(),
            ))
        }
        BrowserOutcome::Expired => Err(BrowserAnswerError::Refused(
            "The sign-in command stopped waiting. Ask the agent to run it again.".into(),
        )),
        _ => Ok(BrowserAnswerResult {
            response: json!({"status":"answered"}),
            replayed: false,
        }),
    }
}

fn finish(
    tx: &Transaction<'_>,
    id: &str,
    decision: Decision,
    forward: Option<BrowserForward>,
    now: f64,
) -> Result<(), BrowserAnswerError> {
    if !record_answer(tx, id, decision, now)? {
        return Ok(());
    }
    let (row, mut card) = load_card(tx, id)?;
    card.forward = forward;
    browser_cards::save(tx, row.clone(), card.clone())?;
    if decision == Decision::Decline {
        let agent = crate::event_write::checked_agent(tx, row.agent.as_str())
            .map_err(|_| BrowserAnswerError::Invalid("Unknown request"))?;
        crate::event_write::enqueue_event(
            tx,
            &agent,
            format!("browser-declined:{}", card.bridge_id),
            "browser_declined",
            BROWSER_DECLINED.to_owned(),
            now,
        )
        .map_err(|_| BrowserAnswerError::Invalid("The declined sign-in could not be recorded"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{BrowserAnswerContext, answer};
    use crate::browser_cards::fixture;
    use crate::linux_vm_guest::{GuestError, GuestRpc};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use studio_runtime_schema::approvals::{ApprovalState, BrowserSource};

    struct FakeGuest {
        calls: Mutex<Vec<(String, String)>>,
        fail_unknown_once: Mutex<bool>,
    }

    impl GuestRpc for FakeGuest {
        fn stream(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            _timeout: Duration,
            _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
        ) -> Result<Value, GuestError> {
            let request_id = request_id.unwrap_or_default().to_owned();
            self.calls
                .lock()
                .expect("fake call log lock")
                .push((method.to_owned(), request_id));
            if std::mem::replace(
                &mut *self.fail_unknown_once.lock().expect("fake failure lock"),
                false,
            ) {
                Err(GuestError::Transport {
                    message: "reply lost".to_owned(),
                    uncertain: true,
                })
            } else {
                Ok(json!({"state":"opened","decision":params["decision"]}))
            }
        }
    }

    fn context(guest: Arc<FakeGuest>, now: f64) -> BrowserAnswerContext {
        BrowserAnswerContext {
            local_window: true,
            guest,
            local_forwarder: None,
            now,
            http_providers: Default::default(),
        }
    }

    async fn store_with_browser_request() -> (
        tempfile::TempDir,
        studio_runtime_store::RuntimeStore,
        String,
    ) {
        let dir = tempfile::tempdir().expect("temporary runtime state");
        let store = studio_runtime_store::RuntimeStore::open(Some(dir.path()))
            .await
            .expect("runtime store opens");
        let id = fixture::seed(
            &store,
            "browser-answer-agent",
            fixture::card(fixture::BRIDGE),
        )
        .await;
        (dir, store, id)
    }

    #[tokio::test]
    async fn host_source_follows_the_stored_card() {
        let (_dir, store, id) = store_with_browser_request().await;
        assert!(!super::is_host_source(&store, id.clone()).await.expect("source"));
        let mut card = fixture::card(fixture::BRIDGE);
        card.source = BrowserSource::Host;
        fixture::seed(&store, "browser-answer-agent", card).await;
        assert!(super::is_host_source(&store, id).await.expect("source"));
        assert!(
            !super::is_host_source(&store, "no-such-card".into())
                .await
                .expect("source")
        );
    }

    #[tokio::test]
    async fn browser_answer_uses_guest_receipt_and_replays_without_second_call() {
        let (_dir, store, id) = store_with_browser_request().await;
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_unknown_once: Mutex::new(false),
        });
        let answer_data = json!({"decision":"accept"});
        let first = answer(&store, &id, &answer_data, context(guest.clone(), 10.0))
            .await
            .expect("guest approval succeeds")
            .expect("browser card is handled");
        assert_eq!(first.response["openUrl"], "https://login.example.test/");
        assert!(!first.replayed);
        let replay = answer(&store, &id, &answer_data, context(guest.clone(), 11.0))
            .await
            .expect("answer replay succeeds")
            .expect("browser card is handled");
        assert!(replay.replayed);
        assert_eq!(guest.calls.lock().expect("call log").len(), 1);
        let conflict = answer(
            &store,
            &id,
            &json!({"decision":"decline"}),
            context(guest.clone(), 12.0),
        )
        .await;
        assert!(matches!(
            conflict,
            Err(super::BrowserAnswerError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn unknown_browser_answer_keeps_card_pending_and_retries_same_guest_identity() {
        let (_dir, store, id) = store_with_browser_request().await;
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_unknown_once: Mutex::new(true),
        });
        let answer_data = json!({"decision":"accept"});
        assert!(
            answer(&store, &id, &answer_data, context(guest.clone(), 10.0))
                .await
                .is_err()
        );
        let call_ids = guest.calls.lock().expect("call log").clone();
        assert_eq!(call_ids.len(), 1);
        let request_id = "browser-answer:0123456789abcdef0123456789abcdef:approve";
        assert_eq!(call_ids[0].1, request_id);
        let (row, _) = fixture::read(&store, &id);
        assert!(matches!(row.state, ApprovalState::Pending));
        answer(&store, &id, &answer_data, context(guest.clone(), 11.0))
            .await
            .expect("same guest receipt resolves retry")
            .expect("browser card is handled");
        let call_ids = guest.calls.lock().expect("call log").clone();
        assert_eq!(call_ids.len(), 2);
        assert_eq!(call_ids[0], call_ids[1]);
    }

    #[tokio::test]
    async fn a_declined_card_leaves_the_client_and_tells_the_chat() {
        let (_dir, store, id) = store_with_browser_request().await;
        let shown = id.clone();
        store
            .transaction_with(move |tx| {
                // The pending card is shown to the client.
                let (row, _) = crate::browser_cards::load(tx, &shown).expect("card").expect("card");
                crate::approval_sync::publish(tx, &row)?;
                // The decline tells the chat, so the chat exists.
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    [
                        "browser-answer-agent",
                        &json!({"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"browser-answer-agent","rootId":"browser-answer-agent",
                                 "epoch":1,"status":"idle","autoWake":true})
                        .to_string(),
                    ],
                )
                .map(drop)
                .map_err(studio_runtime_store::StoreError::from)
            })
            .await
            .expect("the chat is seeded");
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_unknown_once: Mutex::new(false),
        });
        answer(
            &store,
            &id,
            &json!({"decision":"decline"}),
            context(guest, 10.0),
        )
        .await
        .expect("the decline succeeds")
        .expect("browser card is handled");
        let (row, _) = fixture::read(&store, &id);
        assert!(matches!(row.state, ApprovalState::Answered { .. }));
        let live = |store: &studio_runtime_store::RuntimeStore| {
            let id = id.clone();
            store
                .test_read_transaction(move |tx| {
                    Ok(tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='request' AND id=?1 AND deleted=0)",
                        [id],
                        |row| row.get::<_, bool>(0),
                    )?)
                })
                .expect("request entity reads")
        };
        assert!(!live(&store), "a declined card leaves the client");
        let events: i64 = store
            .test_reader()
            .expect("read")
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE kind='browser_declined'",
                [],
                |row| row.get(0),
            )
            .expect("events");
        assert_eq!(events, 1);
    }
}

#[cfg(test)]
#[path = "browser_answer_paste_tests.rs"]
mod paste_tests;
