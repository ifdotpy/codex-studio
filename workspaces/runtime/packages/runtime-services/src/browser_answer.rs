//! Durable browser-open card answers bridged to the VM guest.

use std::{sync::Arc, time::Duration};

use base64::Engine as _;
use serde_json::{Value, json};
use studio_runtime_store::{QuestionsWriteRepository, RuntimeStore, Transaction};
use thiserror::Error;

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

pub async fn is_host_source(store: &RuntimeStore, id: String) -> Result<bool, BrowserAnswerError> {
    let raw = store
        .read_async("runtime-services/browser_answer", move |tx| {
            QuestionsWriteRepository::request(tx, &id)
        })
        .await?;
    let Some(raw) = raw else {
        return Ok(false);
    };
    let record: studio_runtime_store::RequestRecord = serde_json::from_str(&raw)
        .map_err(|_| BrowserAnswerError::Invalid("The sign-in request is invalid"))?;
    if record.method != studio_runtime_schema::request_record::RequestMethod::BrowserOpen {
        return Ok(false);
    }
    let params = crate::browser_cards::BrowserParams::read(&record.params).ok_or(
        BrowserAnswerError::Invalid("The sign-in request is invalid"),
    )?;
    Ok(params.source.as_deref() == Some("host"))
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
    let Some(record) = store
        .read_async("runtime-services/browser_answer", move |tx| {
            load_optional(tx, &id_for_read)
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
    if !matches!(decision, "accept" | "decline") {
        return Err(BrowserAnswerError::Invalid("Choose accept or decline"));
    }
    // Paste mode: any window. Nothing binds a port; the person pastes the redirect.
    let paste_mode =
        decision == "accept" && data.get("paste").and_then(Value::as_bool) == Some(true);
    let signature = crate::provider_callbacks::answer_signature(&json!({"decision":decision}));
    let parsed = &record.params;
    let bridge_id = parsed
        .bridge_id
        .as_deref()
        .filter(|value| value.len() == 32)
        .ok_or(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ))?;
    let url = parsed.url.as_deref().ok_or(BrowserAnswerError::Invalid(
        "The sign-in request is invalid",
    ))?;
    let has_redirect = parsed.redirect.is_some();
    if record.record.status == studio_runtime_schema::persistence::RequestCardStatus::Pending
        && parsed.phase == Some(crate::browser_cards::BrowserPhase::Waiting)
    {
        if decision == "accept" {
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
        let host = parsed.host.as_deref().unwrap_or_default().to_owned();
        return cancel(store, id, bridge_id, &host, guest, now)
            .await
            .map(Some);
    }
    if let Some(previous) = record.record.answer_signature.as_deref() {
        if previous != signature {
            return Err(BrowserAnswerError::Conflict(
                "This request already has a different answer",
            ));
        }
        if record.record.status == studio_runtime_schema::persistence::RequestCardStatus::Answered {
            return Ok(Some(BrowserAnswerResult {
                response: if decision == "accept" && (local_window || (paste_mode && !has_redirect))
                {
                    json!({"status":"answered","replayed":true,"openUrl":url})
                } else {
                    json!({"status":"answered","replayed":true})
                },
                replayed: true,
            }));
        }
        return Err(BrowserAnswerError::Unknown);
    }
    if record.record.status != studio_runtime_schema::persistence::RequestCardStatus::Pending {
        return Err(BrowserAnswerError::Invalid("Request is no longer pending"));
    }
    let remote = decision == "accept"
        && !local_window
        && !paste_mode
        && data.get("forward").and_then(Value::as_bool) == Some(true);
    if decision == "accept" && !local_window && !remote && !paste_mode {
        return Err(BrowserAnswerError::Invalid(
            "Open this chat in a window on the computer that runs its server to sign in, or use the tool's device-code mode.",
        ));
    }

    let rpc_decision = if decision == "accept" {
        "approve"
    } else {
        "decline"
    };
    let local_forward = if decision == "accept"
        && local_window
        && !paste_mode
        && parsed.source.as_deref() == Some("vm")
    {
        if let Some(redirect) = parsed.redirect.as_ref() {
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
        Err(_error) if decision == "decline" => None,
        Err(error) if error.code() == Some("stale") => {
            if let Some(forwarder) = &local_forward {
                drop(forwarder.close(bridge_id).await);
            }
            let id = id.to_owned();
            store
                .transaction_with_error(move |tx| expire(tx, &id))
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
                let mut record = load(tx, &id)?;
                if record.record.status
                    == studio_runtime_schema::persistence::RequestCardStatus::Pending
                {
                    record.params.phase = Some(crate::browser_cards::BrowserPhase::Waiting);
                    record.params.waiting =
                        Some(crate::browser_cards::BrowserWaiting { expires_at });
                    put(tx, &mut record)?;
                }
                Ok::<_, BrowserAnswerError>(())
            })
            .await?;
        return Ok(Some(BrowserAnswerResult {
            response: json!({"status":"answered","openUrl":url}),
            replayed: false,
        }));
    }
    let (forward, forward_state) = if remote && let Some(redirect) = parsed.redirect.as_ref() {
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
        let redirect_host = &redirect.host;
        let redirect_port = redirect.port;
        let redirect_path = &redirect.path;
        let forward_state = crate::browser_cards::BrowserForward {
            token_hash: Some(studio_runtime_store::sha256_hex(token.as_bytes())),
            expires_at: Some(expires_at),
            extra: Default::default(),
        };
        (
            Some(json!({
                "server":server,
                "bridgeId":bridge_id,
                "host":redirect_host,
                "port":redirect_port,
                "path":redirect_path,
                "expiresAt":expires_at,
                "token":token
            })),
            Some(forward_state),
        )
    } else {
        (None, None)
    };
    let id = id.to_owned();
    let signature = signature.to_owned();
    let decision = decision.to_owned();
    let decision_for_write = decision.clone();
    store
        .transaction_with_error(move |tx| {
            finish(
                tx,
                &id,
                &signature,
                &decision_for_write,
                forward_state.as_ref(),
                now,
            )
        })
        .await?;
    let response = if let Some(forward) = forward {
        json!({"status":"answered","openUrl":url,"forward":forward})
    } else if decision == "accept" {
        json!({"status":"answered","openUrl":url})
    } else {
        json!({"status":"answered"})
    };
    Ok(Some(BrowserAnswerResult {
        response,
        replayed: false,
    }))
}

fn load(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<crate::browser_cards::BrowserCard, BrowserAnswerError> {
    load_optional(tx, id)?.ok_or(BrowserAnswerError::Invalid("Unknown request"))
}
fn load_optional(
    tx: &Transaction<'_>,
    id: &str,
) -> Result<Option<crate::browser_cards::BrowserCard>, BrowserAnswerError> {
    let Some(raw) = QuestionsWriteRepository::request(tx, id)? else {
        return Ok(None);
    };
    let record: studio_runtime_store::RequestRecord = serde_json::from_str(&raw)
        .map_err(|_| BrowserAnswerError::Invalid("The sign-in request is invalid"))?;
    if record.id != id {
        return Err(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ));
    }
    if record.method != studio_runtime_schema::request_record::RequestMethod::BrowserOpen {
        return Ok(None);
    }
    let params = crate::browser_cards::BrowserParams::read(&record.params).ok_or(
        BrowserAnswerError::Invalid("The sign-in request is invalid"),
    )?;
    Ok(Some(crate::browser_cards::BrowserCard { record, params }))
}
fn put(
    tx: &Transaction<'_>,
    card: &mut crate::browser_cards::BrowserCard,
) -> Result<(), BrowserAnswerError> {
    crate::browser_cards::put_request(tx, card)
        .map_err(|_| BrowserAnswerError::Invalid("The sign-in request is invalid"))
}
fn expire(tx: &Transaction<'_>, id: &str) -> Result<(), BrowserAnswerError> {
    let mut card = load(tx, id)?;
    if card.record.status == studio_runtime_schema::persistence::RequestCardStatus::Pending {
        card.record.status = studio_runtime_schema::persistence::RequestCardStatus::Expired;
        card.record.close_reason = Some("stale".into());
        put(tx, &mut card)?;
    }
    Ok(())
}
fn mark_answered(
    card: &mut crate::browser_cards::BrowserCard,
    decision: studio_runtime_schema::request_record::RequestDecision,
    now: f64,
) {
    card.record.answer_signature = Some(crate::provider_callbacks::answer_signature(
        &json!({"decision":decision}),
    ));
    card.record.answered_by = Some("user".into());
    card.record.answered_at = Some(now);
    card.record.decision = Some(decision);
    card.record.deferred = Some(false);
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
    crate::browser_cards::close_guest(guest, bridge_id, "cancelled").await;
    let id = id.to_owned();
    let text = format!(
        "The person cancelled the sign-in at {host}. The sign-in command keeps waiting until its own timeout. Stop it, or ask the person before you run it again."
    );
    store
        .transaction_with_error(move |tx| {
            let mut record = load(tx, &id)?;
            if record.record.status
                != studio_runtime_schema::persistence::RequestCardStatus::Pending
            {
                return Err(BrowserAnswerError::Invalid("Request is no longer pending"));
            }
            record.record.status = studio_runtime_schema::persistence::RequestCardStatus::Answered;
            record.params.outcome = Some("cancelled".into());
            mark_answered(
                &mut record,
                studio_runtime_schema::request_record::RequestDecision::Decline,
                now,
            );
            put(tx, &mut record)?;
            if let Ok(agent) = crate::event_write::checked_agent(tx, &record.record.agent) {
                let bridge_id = record.params.bridge_id.as_deref().unwrap_or_default();
                crate::event_write::enqueue_event(
                    tx,
                    &agent,
                    format!("browser-declined:{bridge_id}"),
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
    let record = store
        .read_async("runtime-services/browser_answer_paste", move |tx| {
            load(tx, &read_id)
        })
        .await?;
    let params = &record.params;
    let waiting = record.record.status
        == studio_runtime_schema::persistence::RequestCardStatus::Pending
        && params.phase == Some(crate::browser_cards::BrowserPhase::Waiting);
    if !waiting {
        if params.paste.is_some()
            && matches!(
                record.record.status,
                studio_runtime_schema::persistence::RequestCardStatus::Answered
                    | studio_runtime_schema::persistence::RequestCardStatus::Uncertain
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
    let state =
        crate::browser_policy::provider(params.url.as_deref().unwrap_or_default(), &http_providers)
            .map_err(|error| BrowserAnswerError::Refused(error.0))?
            .state;
    let redirect = params.redirect.as_ref().ok_or(BrowserAnswerError::Invalid(
        "The sign-in request is invalid",
    ))?;
    let port = redirect.port;
    let path = redirect.path.as_str();
    let callback = crate::browser_policy::pasted(text, port, path, state.as_deref())
        .map_err(|error| BrowserAnswerError::Refused(error.0))?;
    let bridge_id = params
        .bridge_id
        .as_deref()
        .ok_or(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ))?
        .to_owned();
    let mark_id = id.to_owned();
    store
        .transaction_with_error(move |tx| {
            let mut record = load(tx, &mark_id)?;
            let params = &record.params;
            if record.record.status
                != studio_runtime_schema::persistence::RequestCardStatus::Pending
                || params.phase != Some(crate::browser_cards::BrowserPhase::Waiting)
                || params.paste.is_some()
            {
                return Err(BrowserAnswerError::Refused(
                    "Studio already sends a pasted address for this sign-in.".into(),
                ));
            }
            // The mark comes before the relay: a second paste never sends a second code.
            record.params.paste = Some(crate::browser_cards::BrowserPaste { at: now });
            put(tx, &mut record)
        })
        .await?;
    let request_id = format!("browser-callback:{}", uuid::Uuid::new_v4().simple());
    let relayed = crate::browser_cards::guest_call(
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
                let mut record = load(tx, &retry_id)?;
                if record.record.status
                    == studio_runtime_schema::persistence::RequestCardStatus::Pending
                {
                    record.params.paste = None;
                    put(tx, &mut record)?;
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
                    "provider-error"
                } else if (200..300).contains(&status) {
                    "signed-in"
                } else {
                    "refused"
                },
                Some(status),
            ),
            None => ("unknown", None),
        },
        Err(error)
            if error
                .code()
                .is_some_and(|code| DEFINITE_CODES.contains(&code)) =>
        {
            ("expired", None)
        }
        Err(_) => ("unknown", None),
    };
    let final_id = id.to_owned();
    store
        .transaction_with_error(move |tx| {
            let mut record = load(tx, &final_id)?;
            record.params.outcome = Some(outcome.into());
            record.params.outcome_http = http_status;
            if outcome == "expired" {
                record.record.status =
                    studio_runtime_schema::persistence::RequestCardStatus::Expired;
                record.record.close_reason = Some("stale".into());
            } else {
                record.record.status = if outcome == "unknown" {
                    studio_runtime_schema::persistence::RequestCardStatus::Uncertain
                } else {
                    studio_runtime_schema::persistence::RequestCardStatus::Answered
                };
                mark_answered(
                    &mut record,
                    studio_runtime_schema::request_record::RequestDecision::Answer,
                    now,
                );
            }
            put(tx, &mut record)?;
            crate::browser_cards::outcome_event(tx, &record, now);
            Ok::<_, BrowserAnswerError>(())
        })
        .await?;
    match outcome {
        "unknown" => {
            crate::browser_cards::close_guest(guest, &bridge_id, "unknown").await;
            Err(BrowserAnswerError::Refused(
                "The result of the sign-in is unknown. Studio does not send it again. Check the command output."
                    .into(),
            ))
        }
        "expired" => Err(BrowserAnswerError::Refused(
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
    signature: &str,
    decision: &str,
    forward: Option<&crate::browser_cards::BrowserForward>,
    now: f64,
) -> Result<(), BrowserAnswerError> {
    let mut card = load(tx, id)?;
    if let Some(previous) = card.record.answer_signature.as_deref() {
        if previous != signature {
            return Err(BrowserAnswerError::Conflict(
                "This request already has a different answer",
            ));
        }
        return Ok(());
    }
    if card.record.status != studio_runtime_schema::persistence::RequestCardStatus::Pending {
        return Err(BrowserAnswerError::Invalid("Request is no longer pending"));
    }
    card.record.status = studio_runtime_schema::persistence::RequestCardStatus::Answered;
    card.record.answer_signature = Some(signature.into());
    card.record.answered_by = Some("user".into());
    card.record.answered_at = Some(now);
    card.record.decision = Some(
        decision
            .parse()
            .map_err(|_| BrowserAnswerError::Invalid("Invalid decision"))?,
    );
    card.record.deferred = Some(false);
    if let Some(forward) = forward {
        card.params.forward = Some(forward.clone());
    }
    put(tx, &mut card)?;
    if decision == "decline" {
        let agent = crate::event_write::checked_agent(tx, &card.record.agent)
            .map_err(|_| BrowserAnswerError::Invalid("Unknown request"))?;
        let bridge_id = card.params.bridge_id.as_deref().unwrap_or_default();
        crate::event_write::enqueue_event(
            tx,
            &agent,
            format!("browser-declined:{bridge_id}"),
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
    use crate::linux_vm_guest::{GuestError, GuestRpc};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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

    async fn store_with_browser_request() -> (
        tempfile::TempDir,
        studio_runtime_store::RuntimeStore,
        String,
    ) {
        let dir = tempfile::tempdir().expect("temporary runtime state");
        let store = studio_runtime_store::RuntimeStore::open(Some(dir.path()))
            .await
            .expect("runtime store opens");
        let id = "browser-answer-test".to_owned();
        let request_id = id.clone();
        store
            .transaction_with(move |tx| {
                studio_runtime_store::QuestionsWriteRepository::put_request(
                    tx,
                    &request_id,
                    &studio_runtime_store::python_json_utf8(&json!({
                        "id":request_id,
                        "agent":"browser-answer-agent",
                        "method":"browser/open",
                        "status":"pending", "createdAt":1.0,
                        "params":{"bridgeId":"0123456789abcdef0123456789abcdef","url":"https://login.example.test/"}
                    })),
                )
            })
            .await
            .expect("browser request is seeded");
        (dir, store, id)
    }

    #[tokio::test]
    async fn host_source_rejects_invalid_and_duplicate_params() {
        let (_dir, store, id) = store_with_browser_request().await;
        for (params, expected) in [
            ("null", None),
            (r#"{"source":false}"#, None),
            (r#"{"source":"vm","source":"host"}"#, None),
            (r#"{"source":"host","unknown":{}}"#, Some(true)),
            (r#"{"source":"vm"}"#, Some(false)),
        ] {
            let key = id.clone();
            let raw = format!(
                r#"{{"id":"browser-answer-test","agent":"agent","method":"browser/open","status":"pending","createdAt":1,"params":{params}}}"#
            );
            store
                .transaction_with(move |tx| {
                    studio_runtime_store::QuestionsWriteRepository::put_request(tx, &key, &raw)
                })
                .await
                .expect("request");
            let actual = super::is_host_source(&store, id.clone()).await.ok();
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn browser_answer_uses_guest_receipt_and_replays_without_second_call() {
        let (_dir, store, id) = store_with_browser_request().await;
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_unknown_once: Mutex::new(false),
        });
        let answer_data = json!({"decision":"accept"});
        let first = answer(
            &store,
            &id,
            &answer_data,
            BrowserAnswerContext {
                local_window: true,
                guest: guest.clone(),
                local_forwarder: None,
                now: 10.0,
                http_providers: Default::default(),
            },
        )
        .await
        .expect("guest approval succeeds")
        .expect("browser card is handled");
        assert_eq!(first.response["openUrl"], "https://login.example.test/");
        assert!(!first.replayed);
        let replay = answer(
            &store,
            &id,
            &answer_data,
            BrowserAnswerContext {
                local_window: true,
                guest: guest.clone(),
                local_forwarder: None,
                now: 11.0,
                http_providers: Default::default(),
            },
        )
        .await
        .expect("answer replay succeeds")
        .expect("browser card is handled");
        assert!(replay.replayed);
        assert_eq!(guest.calls.lock().expect("call log").len(), 1);
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
            answer(
                &store,
                &id,
                &answer_data,
                BrowserAnswerContext {
                    local_window: true,
                    guest: guest.clone(),
                    local_forwarder: None,
                    now: 10.0,
                    http_providers: Default::default(),
                },
            )
            .await
            .is_err()
        );
        let call_ids = guest.calls.lock().expect("call log").clone();
        assert_eq!(call_ids.len(), 1);
        let request_id = "browser-answer:0123456789abcdef0123456789abcdef:approve";
        assert_eq!(call_ids[0].1, request_id);
        let request_id = id.to_owned();
        let row = store
            .read_async("runtime-services/tests/browser_answer", move |tx| {
                studio_runtime_store::QuestionsWriteRepository::request(tx, &request_id)
            })
            .await
            .expect("request state reads")
            .expect("request exists");
        let row: Value = serde_json::from_str(&row).expect("request JSON");
        assert_eq!(row["status"], "pending");
        assert!(row.get("answerSignature").is_none());
        answer(
            &store,
            &id,
            &answer_data,
            BrowserAnswerContext {
                local_window: true,
                guest: guest.clone(),
                local_forwarder: None,
                now: 11.0,
                http_providers: Default::default(),
            },
        )
        .await
        .expect("same guest receipt resolves retry")
        .expect("browser card is handled");
        let call_ids = guest.calls.lock().expect("call log").clone();
        assert_eq!(call_ids.len(), 2);
        assert_eq!(call_ids[0], call_ids[1]);
    }

    /// Whether the card has a live client entity, as the renderer pulls it.
    fn card_shown(store: &studio_runtime_store::RuntimeStore, id: &str) -> bool {
        let id = id.to_owned();
        store
            .test_read_transaction(move |tx| {
                Ok(tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='request' AND id=?1 AND deleted=0)",
                    [id],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .expect("request entity reads")
    }

    #[tokio::test]
    async fn a_declined_card_leaves_the_client() {
        let (_dir, store, id) = store_with_browser_request().await;
        let shown_id = id.clone();
        store
            .transaction_with(move |tx| {
                // The decline tells the chat, so the chat exists.
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    [
                        "browser-answer-agent",
                        &json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"browser-answer-agent","rootId":"browser-answer-agent",
                                 "epoch":1,"status":"idle","autoWake":true})
                        .to_string(),
                    ],
                )
                .map_err(studio_runtime_store::StoreError::from)?;
                let raw = studio_runtime_store::QuestionsWriteRepository::request(tx, &shown_id)?
                    .expect("seeded card");
                let record: Value = serde_json::from_str(&raw).expect("card JSON");
                let view = crate::sync_views::project_persisted_record("request", &record)
                    .expect("card view");
                studio_runtime_store::SyncDocumentsRepository::put_entity(
                    tx, "request", &shown_id, view, false,
                )?;
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("the pending card is shown");
        assert!(card_shown(&store, &id));
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_unknown_once: Mutex::new(false),
        });
        answer(
            &store,
            &id,
            &json!({"decision":"decline"}),
            BrowserAnswerContext {
                local_window: true,
                guest,
                local_forwarder: None,
                now: 10.0,
                http_providers: Default::default(),
            },
        )
        .await
        .expect("the decline succeeds")
        .expect("browser card is handled");
        assert!(
            !card_shown(&store, &id),
            "a declined card leaves the client"
        );
    }
}

#[cfg(test)]
#[path = "browser_answer_paste_tests.rs"]
mod paste_tests;
