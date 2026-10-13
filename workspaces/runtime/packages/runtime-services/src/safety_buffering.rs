//! Pure safety-retry decisions. RPC execution belongs to the turn driver.

use serde_json::{Map, Value, json};
use thiserror::Error;

const ACTIVE_STAGES: &[SafetyRetryStage] = &[
    SafetyRetryStage::Turns,
    SafetyRetryStage::Items,
    SafetyRetryStage::Interrupt,
    SafetyRetryStage::VerifyTurns,
    SafetyRetryStage::VerifyItems,
    SafetyRetryStage::Fork,
    SafetyRetryStage::Start,
    SafetyRetryStage::Unknown,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafetyRetryStage {
    Turns,
    Items,
    Interrupt,
    VerifyTurns,
    VerifyItems,
    Fork,
    Start,
    Running,
    Unknown,
    Failed,
    Cancelled,
}

impl SafetyRetryStage {
    fn parse(value: &Value) -> Result<Self, SafetyBufferingError> {
        match value.as_str() {
            Some("turns") => Ok(Self::Turns),
            Some("items") => Ok(Self::Items),
            Some("interrupt") => Ok(Self::Interrupt),
            Some("verify_turns") => Ok(Self::VerifyTurns),
            Some("verify_items") => Ok(Self::VerifyItems),
            Some("fork") => Ok(Self::Fork),
            Some("start") => Ok(Self::Start),
            Some("running") => Ok(Self::Running),
            Some("unknown") => Ok(Self::Unknown),
            Some("failed") => Ok(Self::Failed),
            Some("cancelled") => Ok(Self::Cancelled),
            _ => Err(SafetyBufferingError::InvalidStoredState),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::Items => "items",
            Self::Interrupt => "interrupt",
            Self::VerifyTurns => "verify_turns",
            Self::VerifyItems => "verify_items",
            Self::Fork => "fork",
            Self::Start => "start",
            Self::Running => "running",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SafetyBufferingError {
    #[error("Invalid stored safety retry state")]
    InvalidStoredState,
    #[error("This retry belongs to an earlier account or agent state")]
    StaleRetry,
    #[error("This safety check is no longer active")]
    Inactive,
    #[error("Choose wait or retry")]
    InvalidChoice,
    #[error("No model change exists for this turn")]
    NoRetryToCancel,
    #[error("The retry was already submitted. Wait for its receipt or stop the turn.")]
    StartAlreadySubmitted,
    #[error("The native thread is not open")]
    ThreadNotOpen,
    #[error("Codex did not offer another model")]
    MissingFasterModel,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafetySnapshot {
    agent_id: String,
    account_key: String,
    epoch: i64,
    thread_id: String,
    turn_id: String,
    buffering_thread_id: String,
    buffering_turn_id: String,
    connection_id: String,
    in_flight: bool,
    auto_wake: bool,
    account_transfer: bool,
    response_started: bool,
    show_buffering_ui: bool,
    native_response_is_turn: bool,
    thread_open: bool,
    connection_current: bool,
    faster_model: Option<String>,
}

impl SafetySnapshot {
    /// Build the decision snapshot from records read from storage and the current connection.
    pub fn from_agent(
        agent: &Value,
        connection_current: bool,
        thread_open: bool,
    ) -> Result<Self, SafetyBufferingError> {
        let string = |name: &str| {
            agent
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or(SafetyBufferingError::InvalidStoredState)
        };
        let epoch = agent
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(SafetyBufferingError::InvalidStoredState)?;
        let account_key = match agent.get("accountKey") {
            None => "default".to_owned(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => return Err(SafetyBufferingError::InvalidStoredState),
        };
        let buffering = agent.get("nativeSafetyBuffering").unwrap_or(&Value::Null);
        Ok(Self {
            agent_id: string("id")?,
            account_key,
            epoch,
            thread_id: string("threadId")?,
            turn_id: string("turnId")?,
            buffering_thread_id: buffering
                .get("threadId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(SafetyBufferingError::InvalidStoredState)?
                .to_owned(),
            buffering_turn_id: buffering
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(SafetyBufferingError::InvalidStoredState)?
                .to_owned(),
            connection_id: buffering
                .get("connectionId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or(SafetyBufferingError::InvalidStoredState)?
                .to_owned(),
            in_flight: agent.get("inFlight").and_then(Value::as_bool) == Some(true),
            auto_wake: agent.get("autoWake").and_then(Value::as_bool) == Some(true),
            account_transfer: agent
                .get("accountTransferId")
                .is_some_and(|value| !value.is_null()),
            response_started: buffering.get("responseStarted").and_then(Value::as_bool)
                == Some(true),
            show_buffering_ui: buffering.get("showBufferingUi").and_then(Value::as_bool)
                == Some(true),
            native_response_is_turn: agent.get("nativeResponseTurn").and_then(Value::as_str)
                == agent.get("turnId").and_then(Value::as_str),
            thread_open,
            connection_current,
            faster_model: buffering
                .get("fasterModel")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        })
    }

    fn is_current_buffering(&self, requested_turn: &str) -> bool {
        self.in_flight
            && self.auto_wake
            && !self.account_transfer
            && self.turn_id == requested_turn
            && self.buffering_turn_id == requested_turn
            && self.buffering_thread_id == self.thread_id
            && self.show_buffering_ui
            && !self.response_started
            && !self.native_response_is_turn
            && self.connection_current
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SafetyDecision {
    Waiting { agent_id: String, turn_id: String },
    Cancelled { receipt: Value },
    BeginRetry { receipt: Value },
    Replay { receipt: Value },
}

/// Decide the UI action. `identity` and `attempt_id` are generated by the effect adapter.
pub fn decide_action(
    snapshot: &SafetySnapshot,
    choice: &str,
    requested_turn: &str,
    existing: Option<&Value>,
    identity: &str,
    attempt_id: &str,
    created: f64,
) -> Result<SafetyDecision, SafetyBufferingError> {
    if !matches!(choice, "wait" | "retry" | "cancel") {
        return Err(SafetyBufferingError::InvalidChoice);
    }
    if let Some(receipt) = existing {
        let stage = SafetyRetryStage::parse(
            receipt
                .get("stage")
                .ok_or(SafetyBufferingError::InvalidStoredState)?,
        )?;
        if receipt.get("accountKey").and_then(Value::as_str) != Some(&snapshot.account_key)
            || receipt.get("epoch").and_then(Value::as_i64) != Some(snapshot.epoch)
            || receipt.get("agent").and_then(Value::as_str) != Some(&snapshot.agent_id)
            || receipt.get("id").and_then(Value::as_str) != Some(identity)
            || receipt.get("turnId").and_then(Value::as_str) != Some(requested_turn)
        {
            return Err(SafetyBufferingError::StaleRetry);
        }
        if choice == "cancel" && ACTIVE_STAGES.contains(&stage) {
            if receipt.get("rpcMethod").and_then(Value::as_str) == Some("turn/start") {
                return Err(SafetyBufferingError::StartAlreadySubmitted);
            }
            let mut updated = receipt
                .as_object()
                .cloned()
                .ok_or(SafetyBufferingError::InvalidStoredState)?;
            updated.insert(
                "stage".into(),
                Value::String(SafetyRetryStage::Cancelled.as_str().into()),
            );
            updated.insert("error".into(), Value::Null);
            updated.insert("updated".into(), json!(created));
            return Ok(SafetyDecision::Cancelled {
                receipt: public_receipt(&Value::Object(updated))?,
            });
        }
        return Ok(SafetyDecision::Replay {
            receipt: public_receipt(receipt)?,
        });
    }
    if choice == "cancel" {
        return Err(SafetyBufferingError::NoRetryToCancel);
    }
    if !snapshot.is_current_buffering(requested_turn) {
        return Err(SafetyBufferingError::Inactive);
    }
    if !snapshot.thread_open {
        return Err(SafetyBufferingError::ThreadNotOpen);
    }
    if choice == "wait" {
        return Ok(SafetyDecision::Waiting {
            agent_id: snapshot.agent_id.clone(),
            turn_id: snapshot.turn_id.clone(),
        });
    }
    let model = snapshot
        .faster_model
        .as_ref()
        .ok_or(SafetyBufferingError::MissingFasterModel)?;
    let mut receipt = Map::new();
    receipt.insert("id".into(), Value::String(identity.to_owned()));
    receipt.insert("agent".into(), Value::String(snapshot.agent_id.clone()));
    receipt.insert(
        "accountKey".into(),
        Value::String(snapshot.account_key.clone()),
    );
    receipt.insert(
        "connectionId".into(),
        Value::String(snapshot.connection_id.clone()),
    );
    receipt.insert("epoch".into(), json!(snapshot.epoch));
    receipt.insert("threadId".into(), Value::String(snapshot.thread_id.clone()));
    receipt.insert(
        "sourceThreadId".into(),
        Value::String(snapshot.thread_id.clone()),
    );
    receipt.insert("turnId".into(), Value::String(snapshot.turn_id.clone()));
    receipt.insert("model".into(), Value::String(model.clone()));
    receipt.insert(
        "stage".into(),
        Value::String(SafetyRetryStage::Turns.as_str().into()),
    );
    receipt.insert("attemptId".into(), Value::String(attempt_id.to_owned()));
    receipt.insert("created".into(), json!(created));
    receipt.insert("updated".into(), json!(created));
    Ok(SafetyDecision::BeginRetry {
        receipt: Value::Object(receipt),
    })
}

/// Whether the persisted receipt still blocks normal turn completion.
pub fn active(agent: &Value) -> Result<bool, SafetyBufferingError> {
    let Some(receipt) = agent
        .get("nativeSafetyRetry")
        .filter(|value| !value.is_null())
    else {
        return Ok(false);
    };
    let stage = SafetyRetryStage::parse(
        receipt
            .get("stage")
            .ok_or(SafetyBufferingError::InvalidStoredState)?,
    )?;
    let account = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    Ok(ACTIVE_STAGES.contains(&stage)
        && receipt.get("epoch").and_then(Value::as_i64)
            == agent.get("epoch").and_then(Value::as_i64)
        && receipt.get("accountKey").and_then(Value::as_str) == Some(account))
}

/// Restart never reissues a native mutation; it converts the matching live receipt into a hold.
pub fn decide_restart(
    agent: &Value,
    stored_receipt: Option<&Value>,
    finished_at: f64,
) -> Result<Option<Value>, SafetyBufferingError> {
    if !active(agent)? {
        return Ok(None);
    }
    let receipt = agent
        .get("nativeSafetyRetry")
        .ok_or(SafetyBufferingError::InvalidStoredState)?;
    let fallback = || {
        let mut value = receipt.as_object().cloned().unwrap_or_default();
        value.insert(
            "id".into(),
            receipt.get("id").cloned().unwrap_or(Value::Null),
        );
        value.insert(
            "agent".into(),
            agent.get("id").cloned().unwrap_or(Value::Null),
        );
        value.insert(
            "accountKey".into(),
            Value::String(
                agent
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .into(),
            ),
        );
        value.insert(
            "epoch".into(),
            agent.get("epoch").cloned().unwrap_or(Value::Null),
        );
        Value::Object(value)
    };
    let mut operation = match stored_receipt {
        Some(value) if value.get("id") == receipt.get("id") => value.clone(),
        _ => fallback(),
    };
    if operation.get("agent").and_then(Value::as_str) != agent.get("id").and_then(Value::as_str)
        || operation.get("epoch").and_then(Value::as_i64)
            != agent.get("epoch").and_then(Value::as_i64)
        || operation.get("accountKey").and_then(Value::as_str)
            != Some(
                agent
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default"),
            )
    {
        return Ok(None);
    }
    let stage = SafetyRetryStage::parse(
        operation
            .get("stage")
            .ok_or(SafetyBufferingError::InvalidStoredState)?,
    )?;
    if matches!(
        stage,
        SafetyRetryStage::Running | SafetyRetryStage::Failed | SafetyRetryStage::Cancelled
    ) {
        return Ok(None);
    }
    let reason = format!(
        "Safety retry stopped at restart during {}. Studio did not repeat the native operation. Review the saved retry receipt.",
        stage.as_str()
    );
    let operation = operation
        .as_object_mut()
        .ok_or(SafetyBufferingError::InvalidStoredState)?;
    operation.insert(
        "stage".into(),
        Value::String(SafetyRetryStage::Failed.as_str().into()),
    );
    operation.insert("terminalHold".into(), Value::Bool(true));
    operation.insert("error".into(), Value::String(reason));
    operation.insert("finished".into(), json!(finished_at));
    Ok(Some(Value::Object(operation.clone())))
}

fn public_receipt(receipt: &Value) -> Result<Value, SafetyBufferingError> {
    let object = receipt
        .as_object()
        .ok_or(SafetyBufferingError::InvalidStoredState)?;
    let keys = [
        "id",
        "stage",
        "model",
        "turnId",
        "created",
        "updated",
        "epoch",
        "accountKey",
        "error",
        "newThreadId",
        "acceptedTurnId",
        "requestId",
        "rpcMethod",
    ];
    Ok(Value::Object(
        keys.into_iter()
            .map(|key| {
                (
                    key.to_owned(),
                    object.get(key).cloned().unwrap_or(Value::Null),
                )
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        SafetyBufferingError, SafetyDecision, SafetySnapshot, active, decide_action, decide_restart,
    };
    use serde_json::json;

    fn agent() -> serde_json::Value {
        json!({"id":"agent","epoch":4,"threadId":"thread","turnId":"turn","accountKey":"acct","inFlight":true,"autoWake":true,"nativeSafetyBuffering":{"turnId":"turn","threadId":"thread","connectionId":"conn","showBufferingUi":true,"fasterModel":"model"}})
    }

    #[test]
    fn retry_decision_binds_agent_account_epoch_and_connection() {
        let agent = agent();
        let snapshot = SafetySnapshot::from_agent(&agent, true, true).expect("snapshot");
        let SafetyDecision::BeginRetry { receipt } = decide_action(
            &snapshot,
            "retry",
            "turn",
            None,
            "agent:turn",
            "attempt",
            1.0,
        )
        .expect("retry decision") else {
            panic!("retry decision")
        };
        assert_eq!(receipt["agent"], "agent");
        assert_eq!(receipt["id"], "agent:turn");
        assert_eq!(receipt["connectionId"], "conn");
        assert_eq!(receipt["epoch"], 4);
        assert_eq!(receipt["threadId"], "thread");
        assert_eq!(receipt["sourceThreadId"], "thread");
        assert_eq!(receipt["turnId"], "turn");
        assert_eq!(receipt["model"], "model");
        assert_eq!(receipt["stage"], "turns");
        assert_eq!(receipt["attemptId"], "attempt");
        assert_eq!(receipt["created"], 1.0);
    }

    #[test]
    fn already_started_rpc_cannot_be_cancelled_or_replayed_as_a_new_retry() {
        let agent = agent();
        let snapshot = SafetySnapshot::from_agent(&agent, true, true).expect("snapshot");
        let receipt = json!({"id":"agent:turn","agent":"agent","accountKey":"acct","epoch":4,"turnId":"turn","stage":"start","rpcMethod":"turn/start"});
        assert_eq!(
            decide_action(
                &snapshot,
                "cancel",
                "turn",
                Some(&receipt),
                "agent:turn",
                "",
                2.0
            ),
            Err(SafetyBufferingError::StartAlreadySubmitted)
        );
        assert!(matches!(
            decide_action(
                &snapshot,
                "retry",
                "turn",
                Some(&receipt),
                "agent:turn",
                "",
                2.0
            ),
            Ok(SafetyDecision::Replay { .. })
        ));
    }

    #[test]
    fn response_started_refuses_retry_and_restart_records_terminal_hold_without_resend() {
        let mut agent = agent();
        agent["nativeSafetyBuffering"]["responseStarted"] = json!(true);
        let snapshot = SafetySnapshot::from_agent(&agent, true, true).expect("snapshot");
        assert_eq!(
            decide_action(&snapshot, "retry", "turn", None, "", "", 1.0),
            Err(SafetyBufferingError::Inactive)
        );
        agent["nativeSafetyBuffering"]
            .as_object_mut()
            .expect("buffering")
            .remove("responseStarted");
        agent["nativeSafetyRetry"] =
            json!({"id":"agent:turn","stage":"fork","epoch":4,"accountKey":"acct"});
        assert!(active(&agent).expect("active"));
        let saved =
            json!({"id":"agent:turn","agent":"agent","epoch":4,"accountKey":"acct","stage":"fork"});
        let hold = decide_restart(&agent, Some(&saved), 3.0)
            .expect("restart")
            .expect("hold");
        assert_eq!(hold["stage"], "failed");
        assert_eq!(hold["terminalHold"], true);
        assert!(
            hold["error"]
                .as_str()
                .expect("message")
                .contains("did not repeat")
        );
    }

    #[test]
    fn retry_requires_every_current_buffering_condition() {
        let stored = agent();
        let base = SafetySnapshot::from_agent(&stored, true, true).expect("snapshot");
        let mut inactive = Vec::new();
        let mut snapshot = base.clone();
        snapshot.in_flight = false;
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.auto_wake = false;
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.account_transfer = true;
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.turn_id = "other".into();
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.buffering_turn_id = "other".into();
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.buffering_thread_id = "other".into();
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.show_buffering_ui = false;
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.response_started = true;
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.native_response_is_turn = true;
        inactive.push(snapshot);
        let mut snapshot = base.clone();
        snapshot.connection_current = false;
        inactive.push(snapshot);

        for snapshot in inactive {
            assert_eq!(
                decide_action(&snapshot, "retry", "turn", None, "", "", 1.0),
                Err(SafetyBufferingError::Inactive)
            );
        }
        let SafetyDecision::BeginRetry { receipt } =
            decide_action(&base, "retry", "turn", None, "", "", 1.0).expect("active retry")
        else {
            panic!("expected retry receipt");
        };
        assert_eq!(receipt["model"], "model");
    }

    #[test]
    fn retry_requires_open_thread_model_and_valid_choice() {
        let stored = agent();
        let open = SafetySnapshot::from_agent(&stored, true, true).expect("snapshot");
        let closed = SafetySnapshot::from_agent(&stored, true, false).expect("snapshot");
        assert_eq!(
            decide_action(&closed, "wait", "turn", None, "", "", 1.0),
            Err(SafetyBufferingError::ThreadNotOpen)
        );
        assert_eq!(
            decide_action(&open, "again", "turn", None, "", "", 1.0),
            Err(SafetyBufferingError::InvalidChoice)
        );
        assert_eq!(
            decide_action(&open, "cancel", "turn", None, "", "", 1.0),
            Err(SafetyBufferingError::NoRetryToCancel)
        );
        let mut no_model = stored;
        no_model["nativeSafetyBuffering"]
            .as_object_mut()
            .expect("buffering")
            .remove("fasterModel");
        let no_model = SafetySnapshot::from_agent(&no_model, true, true).expect("snapshot");
        assert_eq!(
            decide_action(&no_model, "retry", "turn", None, "", "", 1.0),
            Err(SafetyBufferingError::MissingFasterModel)
        );
        assert_eq!(
            decide_action(&open, "wait", "turn", None, "", "", 1.0),
            Ok(SafetyDecision::Waiting {
                agent_id: "agent".into(),
                turn_id: "turn".into()
            })
        );
    }

    #[test]
    fn absent_account_key_defaults_but_present_invalid_values_are_rejected() {
        let mut missing = agent();
        missing.as_object_mut().expect("agent").remove("accountKey");
        let snapshot = SafetySnapshot::from_agent(&missing, true, true).expect("default account");
        assert_eq!(snapshot.account_key, "default");

        for value in [serde_json::Value::Null, json!(7), json!(false)] {
            let mut malformed = agent();
            malformed["accountKey"] = value;
            assert_eq!(
                SafetySnapshot::from_agent(&malformed, true, true),
                Err(SafetyBufferingError::InvalidStoredState)
            );
        }
    }

    #[test]
    fn existing_receipts_require_matching_identity_and_cancel_only_unsent_active_stages() {
        let stored = agent();
        let snapshot = SafetySnapshot::from_agent(&stored, true, true).expect("snapshot");
        let receipt = json!({"id":"agent:turn","agent":"agent","accountKey":"acct","epoch":4,"turnId":"turn","stage":"fork","private":"keep","model":"fast"});
        for stale in [
            json!({"id":"agent:turn","agent":"other","accountKey":"acct","epoch":4,"turnId":"turn","stage":"fork"}),
            json!({"id":"agent:turn","agent":"agent","accountKey":"other","epoch":4,"turnId":"turn","stage":"fork"}),
            json!({"id":"agent:turn","agent":"agent","accountKey":"acct","epoch":3,"turnId":"turn","stage":"fork"}),
            json!({"id":"agent:other","agent":"agent","accountKey":"acct","epoch":4,"turnId":"turn","stage":"fork"}),
            json!({"id":"agent:turn","agent":"agent","accountKey":"acct","epoch":4,"turnId":"other","stage":"fork"}),
        ] {
            assert_eq!(
                decide_action(
                    &snapshot,
                    "retry",
                    "turn",
                    Some(&stale),
                    "agent:turn",
                    "",
                    1.0
                ),
                Err(SafetyBufferingError::StaleRetry)
            );
            assert_eq!(
                decide_action(
                    &snapshot,
                    "cancel",
                    "turn",
                    Some(&stale),
                    "agent:turn",
                    "",
                    1.0
                ),
                Err(SafetyBufferingError::StaleRetry)
            );
        }
        let SafetyDecision::Cancelled { receipt: cancelled } = decide_action(
            &snapshot,
            "cancel",
            "turn",
            Some(&receipt),
            "agent:turn",
            "",
            1.0,
        )
        .expect("cancel unsent active retry") else {
            panic!("expected cancellation");
        };
        assert_eq!(cancelled["stage"], "cancelled");
        assert_eq!(cancelled["error"], serde_json::Value::Null);
        assert!(cancelled.get("private").is_none());
        let SafetyDecision::Replay { receipt: public } = decide_action(
            &snapshot,
            "retry",
            "turn",
            Some(&receipt),
            "agent:turn",
            "",
            1.0,
        )
        .expect("replay existing receipt") else {
            panic!("expected receipt replay");
        };
        assert_eq!(public["model"], "fast");
        assert!(public.get("private").is_none());
        let terminal = json!({"id":"agent:turn","agent":"agent","accountKey":"acct","epoch":4,"turnId":"turn","stage":"failed"});
        assert!(matches!(
            decide_action(
                &snapshot,
                "cancel",
                "turn",
                Some(&terminal),
                "agent:turn",
                "",
                1.0
            ),
            Ok(SafetyDecision::Replay { .. })
        ));
    }
}
