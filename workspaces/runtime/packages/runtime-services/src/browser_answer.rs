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

pub struct BrowserAnswerContext<V> {
    pub local_window: bool,
    pub guest: Arc<dyn crate::linux_vm_guest::GuestRpc>,
    pub local_forwarder: Option<Arc<crate::browser::BrowserForwarder>>,
    pub now: f64,
    pub validate: V,
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
    let record: Value = serde_json::from_str(&raw)
        .map_err(|_| BrowserAnswerError::Invalid("The sign-in request is invalid"))?;
    Ok(record.pointer("/params/source").and_then(Value::as_str) == Some("host"))
}

/// The guest owns the sign-in request. A stable request ID lets its durable
/// receipt resolve an unknown transport outcome without delivering twice.
pub async fn answer<V: Fn(&Value) -> bool + Send + Sync + 'static>(
    store: &RuntimeStore,
    id: &str,
    data: &Value,
    context: BrowserAnswerContext<V>,
) -> Result<Option<BrowserAnswerResult>, BrowserAnswerError> {
    let BrowserAnswerContext {
        local_window,
        guest,
        local_forwarder,
        now,
        validate,
        http_providers,
    } = context;
    let validate = Arc::new(validate);
    let validate_for_read = Arc::clone(&validate);
    let id_for_read = id.to_owned();
    let Some(record) = store
        .read_async("runtime-services/browser_answer", move |tx| {
            load_optional(tx, &id_for_read, validate_for_read.as_ref())
        })
        .await?
    else {
        return Ok(None);
    };
    if record.get("method").and_then(Value::as_str) != Some("browser/open") {
        return Ok(None);
    }
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
                validate,
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
    let params =
        record
            .get("params")
            .and_then(Value::as_object)
            .ok_or(BrowserAnswerError::Invalid(
                "The sign-in request is invalid",
            ))?;
    let bridge_id = params
        .get("bridgeId")
        .and_then(Value::as_str)
        .filter(|value| value.len() == 32)
        .ok_or(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ))?;
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .ok_or(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ))?;
    let has_redirect = params.get("redirect").is_some_and(Value::is_object);
    if record.get("status").and_then(Value::as_str) == Some("pending")
        && params.get("phase").and_then(Value::as_str) == Some(crate::browser_cards::WAITING)
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
        let host = params
            .get("host")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return cancel(store, id, bridge_id, &host, guest, now, validate)
            .await
            .map(Some);
    }
    if let Some(previous) = record.get("answerSignature").and_then(Value::as_str) {
        if previous != signature {
            return Err(BrowserAnswerError::Conflict(
                "This request already has a different answer",
            ));
        }
        if record.get("status").and_then(Value::as_str) == Some("answered") {
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
    if record.get("status").and_then(Value::as_str) != Some("pending") {
        return Err(BrowserAnswerError::Invalid("Request is no longer pending"));
    }
    let remote = decision == "accept"
        && !local_window
        && !paste_mode
        && data.get("forward").and_then(Value::as_bool) == Some(true);
    if decision == "accept" && !local_window && !remote && !paste_mode {
        return Err(BrowserAnswerError::Invalid(
            "Open this chat in the window of the Mac that runs its server to sign in, or use the tool's device-code mode.",
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
        && params.get("source").and_then(Value::as_str) == Some("vm")
    {
        if let Some(redirect) = params.get("redirect").filter(|value| value.is_object()) {
            let forwarder = local_forwarder.ok_or(BrowserAnswerError::Unknown)?;
            let input = crate::browser::ForwardInput::new(
                store
                    .local_server_id()?
                    .ok_or(BrowserAnswerError::Unknown)?,
                bridge_id.to_owned(),
                format!("local-browser-forward:{bridge_id}"),
                redirect
                    .get("host")
                    .and_then(Value::as_str)
                    .ok_or(BrowserAnswerError::Invalid(
                        "The sign-in request is invalid",
                    ))?
                    .to_owned(),
                redirect
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|port| u16::try_from(port).ok())
                    .ok_or(BrowserAnswerError::Invalid(
                        "The sign-in request is invalid",
                    ))?,
                redirect
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or(BrowserAnswerError::Invalid(
                        "The sign-in request is invalid",
                    ))?
                    .to_owned(),
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
            let validate = Arc::clone(&validate);
            store
                .transaction_with_error(move |tx| expire(tx, &id, validate.as_ref()))
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
                let mut record = load(tx, &id, validate.as_ref())?;
                if record.get("status").and_then(Value::as_str) == Some("pending") {
                    record["params"]["phase"] = json!(crate::browser_cards::WAITING);
                    record["params"]["waiting"] = json!({"expiresAt": expires_at});
                    put(tx, &id, &record, validate.as_ref())?;
                }
                Ok::<_, BrowserAnswerError>(())
            })
            .await?;
        return Ok(Some(BrowserAnswerResult {
            response: json!({"status":"answered","openUrl":url}),
            replayed: false,
        }));
    }
    let forward = if remote
        && let Some(redirect) = params.get("redirect").filter(|value| value.is_object())
    {
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
        let redirect_host =
            redirect
                .get("host")
                .and_then(Value::as_str)
                .ok_or(BrowserAnswerError::Invalid(
                    "The sign-in request is invalid",
                ))?;
        let redirect_port =
            redirect
                .get("port")
                .and_then(Value::as_u64)
                .ok_or(BrowserAnswerError::Invalid(
                    "The sign-in request is invalid",
                ))?;
        let redirect_path =
            redirect
                .get("path")
                .and_then(Value::as_str)
                .ok_or(BrowserAnswerError::Invalid(
                    "The sign-in request is invalid",
                ))?;
        let forward_state = json!({
            "tokenHash":studio_runtime_store::sha256_hex(token.as_bytes()),
            "expiresAt":expires_at
        });
        Some(json!({
            "server":server,
            "bridgeId":bridge_id,
            "host":redirect_host,
            "port":redirect_port,
            "path":redirect_path,
            "expiresAt":expires_at,
            "token":token,
            "_forwardState":forward_state
        }))
    } else {
        None
    };
    let id = id.to_owned();
    let signature = signature.to_owned();
    let decision = decision.to_owned();
    let forward_state = forward
        .as_ref()
        .and_then(|value| value.get("_forwardState"))
        .cloned();
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
                validate.as_ref(),
            )
        })
        .await?;
    let response = if let Some(forward) = forward {
        let mut spec = forward;
        spec.as_object_mut()
            .expect("forward spec object")
            .remove("_forwardState");
        json!({"status":"answered","openUrl":url,"forward":spec})
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
    validate: &impl Fn(&Value) -> bool,
) -> Result<Value, BrowserAnswerError> {
    load_optional(tx, id, validate)?.ok_or(BrowserAnswerError::Invalid("Unknown request"))
}

fn load_optional(
    tx: &Transaction<'_>,
    id: &str,
    validate: &impl Fn(&Value) -> bool,
) -> Result<Option<Value>, BrowserAnswerError> {
    let raw = QuestionsWriteRepository::request(tx, id)?
        .ok_or(BrowserAnswerError::Invalid("Unknown request"));
    let raw = match raw {
        Ok(raw) => raw,
        Err(_) => return Ok(None),
    };
    let record: Value = serde_json::from_str(&raw)
        .map_err(|_| BrowserAnswerError::Invalid("The sign-in request is invalid"))?;
    if record.get("id").and_then(Value::as_str) != Some(id) || !validate(&record) {
        return Err(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ));
    }
    Ok(Some(record))
}

fn expire(
    tx: &Transaction<'_>,
    id: &str,
    validate: &impl Fn(&Value) -> bool,
) -> Result<(), BrowserAnswerError> {
    let mut record = load(tx, id, validate)?;
    if record.get("status").and_then(Value::as_str) == Some("pending") {
        record["status"] = json!("expired");
        record["closeReason"] = json!("stale");
        put(tx, id, &record, validate)?;
    }
    Ok(())
}

fn mark_answered(record: &mut Value, decision: &str, now: f64) {
    record["answerSignature"] = json!(crate::provider_callbacks::answer_signature(
        &json!({"decision":decision})
    ));
    record["answeredBy"] = json!("user");
    record["answeredAt"] = json!(now);
    record["decision"] = json!(decision);
    record["deferred"] = json!(false);
}

/// Decline in the waiting phase: close the guest request; no result can pass.
async fn cancel<V: Fn(&Value) -> bool + Send + Sync + 'static>(
    store: &RuntimeStore,
    id: &str,
    bridge_id: &str,
    host: &str,
    guest: Arc<dyn crate::linux_vm_guest::GuestRpc>,
    now: f64,
    validate: Arc<V>,
) -> Result<BrowserAnswerResult, BrowserAnswerError> {
    crate::browser_cards::close_guest(guest, bridge_id, "cancelled").await;
    let id = id.to_owned();
    let text = format!(
        "The person cancelled the sign-in at {host}. The sign-in command keeps waiting until its own timeout. Stop it, or ask the person before you run it again."
    );
    store
        .transaction_with_error(move |tx| {
            let mut record = load(tx, &id, validate.as_ref())?;
            if record.get("status").and_then(Value::as_str) != Some("pending") {
                return Err(BrowserAnswerError::Invalid("Request is no longer pending"));
            }
            record["status"] = json!("answered");
            record["params"]["outcome"] = json!("cancelled");
            mark_answered(&mut record, "decline", now);
            put(tx, &id, &record, validate.as_ref())?;
            if let Some(agent_id) = record.get("agent").and_then(Value::as_str)
                && let Ok(agent) = crate::event_write::checked_agent(tx, agent_id)
            {
                let bridge_id = record
                    .pointer("/params/bridgeId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
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

struct PasteContext<V> {
    guest: Arc<dyn crate::linux_vm_guest::GuestRpc>,
    now: f64,
    validate: Arc<V>,
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
async fn paste<V: Fn(&Value) -> bool + Send + Sync + 'static>(
    store: &RuntimeStore,
    id: &str,
    text: &str,
    context: PasteContext<V>,
) -> Result<BrowserAnswerResult, BrowserAnswerError> {
    let PasteContext {
        guest,
        now,
        validate,
        http_providers,
    } = context;
    let read_id = id.to_owned();
    let read_validate = Arc::clone(&validate);
    let record = store
        .read_async("runtime-services/browser_answer_paste", move |tx| {
            load(tx, &read_id, read_validate.as_ref())
        })
        .await?;
    let params = &record["params"];
    let waiting = record.get("status").and_then(Value::as_str) == Some("pending")
        && params.get("phase").and_then(Value::as_str) == Some(crate::browser_cards::WAITING);
    if !waiting {
        if params.get("paste").is_some_and(|mark| !mark.is_null())
            && matches!(
                record.get("status").and_then(Value::as_str),
                Some("answered" | "uncertain")
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
    let state = crate::browser_policy::provider(
        params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        &http_providers,
    )
    .map_err(|error| BrowserAnswerError::Refused(error.0))?
    .state;
    let port = params
        .pointer("/redirect/port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ))?;
    let path = params
        .pointer("/redirect/path")
        .and_then(Value::as_str)
        .ok_or(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ))?;
    let callback = crate::browser_policy::pasted(text, port, path, state.as_deref())
        .map_err(|error| BrowserAnswerError::Refused(error.0))?;
    let bridge_id = params
        .get("bridgeId")
        .and_then(Value::as_str)
        .ok_or(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ))?
        .to_owned();
    let mark_id = id.to_owned();
    let mark_validate = Arc::clone(&validate);
    store
        .transaction_with_error(move |tx| {
            let mut record = load(tx, &mark_id, mark_validate.as_ref())?;
            let params = &record["params"];
            if record.get("status").and_then(Value::as_str) != Some("pending")
                || params.get("phase").and_then(Value::as_str)
                    != Some(crate::browser_cards::WAITING)
                || params.get("paste").is_some_and(|mark| !mark.is_null())
            {
                return Err(BrowserAnswerError::Refused(
                    "Studio already sends a pasted address for this sign-in.".into(),
                ));
            }
            // The mark comes before the relay: a second paste never sends a second code.
            record["params"]["paste"] = json!({"at": now});
            put(tx, &mark_id, &record, mark_validate.as_ref())
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
            let mut record = load(tx, &final_id, validate.as_ref())?;
            record["params"]["outcome"] = json!(outcome);
            record["params"]["outcomeHttp"] = json!(http_status);
            if outcome == "expired" {
                record["status"] = json!("expired");
                record["closeReason"] = json!("stale");
            } else {
                record["status"] = json!(if outcome == "unknown" {
                    "uncertain"
                } else {
                    "answered"
                });
                mark_answered(&mut record, "answer", now);
            }
            put(tx, &final_id, &record, validate.as_ref())?;
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
    forward: Option<&Value>,
    now: f64,
    validate: &impl Fn(&Value) -> bool,
) -> Result<(), BrowserAnswerError> {
    let mut record = load(tx, id, validate)?;
    if let Some(previous) = record.get("answerSignature").and_then(Value::as_str) {
        if previous != signature {
            return Err(BrowserAnswerError::Conflict(
                "This request already has a different answer",
            ));
        }
        return Ok(());
    }
    if record.get("status").and_then(Value::as_str) != Some("pending") {
        return Err(BrowserAnswerError::Invalid("Request is no longer pending"));
    }
    record["status"] = json!("answered");
    record["answerSignature"] = json!(signature);
    record["answeredBy"] = json!("user");
    record["answeredAt"] = json!(now);
    record["decision"] = json!(decision);
    record["deferred"] = json!(false);
    if let Some(forward) = forward {
        record["params"]["forward"] = forward.clone();
    }
    put(tx, id, &record, validate)?;
    if decision == "decline"
        && let Some(agent_id) = record.get("agent").and_then(Value::as_str)
    {
        let agent = crate::event_write::checked_agent(tx, agent_id)
            .map_err(|_| BrowserAnswerError::Invalid("Unknown request"))?;
        let bridge_id = record
            .pointer("/params/bridgeId")
            .and_then(Value::as_str)
            .unwrap_or_default();
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

fn put(
    tx: &Transaction<'_>,
    id: &str,
    record: &Value,
    validate: &impl Fn(&Value) -> bool,
) -> Result<(), BrowserAnswerError> {
    if !validate(record) {
        return Err(BrowserAnswerError::Invalid(
            "The sign-in request is invalid",
        ));
    }
    QuestionsWriteRepository::put_request(tx, id, &studio_runtime_store::python_json_utf8(record))?;
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
                        "status":"pending",
                        "params":{"bridgeId":"0123456789abcdef0123456789abcdef","url":"https://login.example.test/"}
                    })),
                )
            })
            .await
            .expect("browser request is seeded");
        (dir, store, id)
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
                validate: |_: &serde_json::Value| true,
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
                validate: |_: &serde_json::Value| true,
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
                    validate: |_: &serde_json::Value| true,
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
                validate: |_: &serde_json::Value| true,
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
}

#[cfg(test)]
#[path = "browser_answer_paste_tests.rs"]
mod paste_tests;
