//! Durable browser-open card answers bridged to the VM guest.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use studio_runtime_store::{QuestionsWriteRepository, RuntimeStore, Transaction};
use thiserror::Error;

const BROWSER_ANSWER_TIMEOUT: Duration = Duration::from_secs(15);
const BROWSER_TOKEN_BYTES: usize = 32;
const LOCAL_FORWARD_SECONDS: u64 = 600;
const BROWSER_DECLINED: &str = "The person declined to open the requested page in the browser. The sign-in command keeps waiting until its own timeout.";

#[derive(Debug, Error)]
pub enum BrowserAnswerError {
    #[error("{0}")]
    Invalid(&'static str),
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

pub fn is_host_source(store: &RuntimeStore, id: &str) -> Result<bool, BrowserAnswerError> {
    let raw = store.read_transaction_with_error(|tx| QuestionsWriteRepository::request(tx, id))?;
    let Some(raw) = raw else {
        return Ok(false);
    };
    let record: Value = serde_json::from_str(&raw)
        .map_err(|_| BrowserAnswerError::Invalid("The sign-in request is invalid"))?;
    Ok(record.pointer("/params/source").and_then(Value::as_str) == Some("host"))
}

/// The guest owns the sign-in request. A stable request ID lets its durable
/// receipt resolve an unknown transport outcome without delivering twice.
pub fn answer(
    store: &RuntimeStore,
    id: &str,
    data: &Value,
    local_window: bool,
    guest: &dyn crate::linux_vm_guest::GuestRpc,
    local_forwarder: Option<&crate::browser::BrowserForwarder>,
    now: f64,
    validate: impl Fn(&Value) -> bool,
) -> Result<Option<BrowserAnswerResult>, BrowserAnswerError> {
    let Some(record) = store.read_transaction_with_error(|tx| load_optional(tx, id, &validate))?
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
    if !matches!(decision, "accept" | "decline") {
        return Err(BrowserAnswerError::Invalid("Choose accept or decline"));
    }
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
    if let Some(previous) = record.get("answerSignature").and_then(Value::as_str) {
        if previous != signature {
            return Err(BrowserAnswerError::Conflict(
                "This request already has a different answer",
            ));
        }
        if record.get("status").and_then(Value::as_str) == Some("answered") {
            return Ok(Some(BrowserAnswerResult {
                response: if decision == "accept" && local_window {
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
        && data.get("forward").and_then(Value::as_bool) == Some(true);
    if decision == "accept" && !local_window && !remote {
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
            let handle =
                tokio::runtime::Handle::try_current().map_err(|_| BrowserAnswerError::Unknown)?;
            handle.block_on(forwarder.open(input, now, None))?;
            Some((forwarder, handle))
        } else {
            None
        }
    } else {
        None
    };
    let call = guest.call(
        "browser.answer",
        &json!({"id":bridge_id,"decision":rpc_decision}),
        Some(&format!("browser-answer:{bridge_id}:{rpc_decision}")),
        BROWSER_ANSWER_TIMEOUT,
    );
    let answered = match call {
        Ok(value) => Some(value),
        Err(_error) if decision == "decline" => None,
        Err(error) if error.code() == Some("stale") => {
            if let Some((forwarder, handle)) = &local_forward {
                let _ = handle.block_on(forwarder.close(bridge_id));
            }
            store.transaction_with_error(|tx| expire(tx, id, &validate))?;
            return Err(BrowserAnswerError::Stale);
        }
        Err(error) => {
            if let Some((forwarder, handle)) = &local_forward {
                let _ = handle.block_on(forwarder.close(bridge_id));
            }
            return Err(BrowserAnswerError::Guest(error));
        }
    };
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
    store.transaction_with_error(|tx| {
        finish(
            tx,
            id,
            &signature,
            decision,
            forward
                .as_ref()
                .and_then(|value| value.get("_forwardState")),
            now,
            &validate,
        )
    })?;
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
    use super::answer;
    use crate::linux_vm_guest::{GuestError, GuestRpc};
    use serde_json::{Value, json};
    use std::sync::Mutex;
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

    fn store_with_browser_request() -> (
        tempfile::TempDir,
        studio_runtime_store::RuntimeStore,
        String,
    ) {
        let dir = tempfile::tempdir().expect("temporary runtime state");
        let store = studio_runtime_store::RuntimeStore::open(Some(dir.path()))
            .expect("runtime store opens");
        let id = "browser-answer-test".to_owned();
        store
            .transaction_with(|tx| {
                studio_runtime_store::QuestionsWriteRepository::put_request(
                    tx,
                    &id,
                    &studio_runtime_store::python_json_utf8(&json!({
                        "id":id,
                        "agent":"browser-answer-agent",
                        "method":"browser/open",
                        "status":"pending",
                        "params":{"bridgeId":"0123456789abcdef0123456789abcdef","url":"https://login.example.test/"}
                    })),
                )
            })
            .expect("browser request is seeded");
        (dir, store, id)
    }

    #[test]
    fn browser_answer_uses_guest_receipt_and_replays_without_second_call() {
        let (_dir, store, id) = store_with_browser_request();
        let guest = FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_unknown_once: Mutex::new(false),
        };
        let answer_data = json!({"decision":"accept"});
        let first = answer(&store, &id, &answer_data, true, &guest, None, 10.0, |_| {
            true
        })
        .expect("guest approval succeeds")
        .expect("browser card is handled");
        assert_eq!(first.response["openUrl"], "https://login.example.test/");
        assert!(!first.replayed);
        let replay = answer(&store, &id, &answer_data, true, &guest, None, 11.0, |_| {
            true
        })
        .expect("answer replay succeeds")
        .expect("browser card is handled");
        assert!(replay.replayed);
        assert_eq!(guest.calls.lock().expect("call log").len(), 1);
    }

    #[test]
    fn unknown_browser_answer_keeps_card_pending_and_retries_same_guest_identity() {
        let (_dir, store, id) = store_with_browser_request();
        let guest = FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_unknown_once: Mutex::new(true),
        };
        let answer_data = json!({"decision":"accept"});
        assert!(
            answer(&store, &id, &answer_data, true, &guest, None, 10.0, |_| {
                true
            })
            .is_err()
        );
        let call_ids = guest.calls.lock().expect("call log").clone();
        assert_eq!(call_ids.len(), 1);
        let request_id = "browser-answer:0123456789abcdef0123456789abcdef:approve";
        assert_eq!(call_ids[0].1, request_id);
        let row = store
            .read_transaction_with_error(|tx| {
                studio_runtime_store::QuestionsWriteRepository::request(tx, &id)
            })
            .expect("request state reads")
            .expect("request exists");
        let row: Value = serde_json::from_str(&row).expect("request JSON");
        assert_eq!(row["status"], "pending");
        assert!(row.get("answerSignature").is_none());
        answer(&store, &id, &answer_data, true, &guest, None, 11.0, |_| {
            true
        })
        .expect("same guest receipt resolves retry")
        .expect("browser card is handled");
        let call_ids = guest.calls.lock().expect("call log").clone();
        assert_eq!(call_ids.len(), 2);
        assert_eq!(call_ids[0], call_ids[1]);
    }
}
