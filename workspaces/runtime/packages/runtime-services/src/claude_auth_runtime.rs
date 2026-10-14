//! Store and process adapter for Claude's retained auth probe waits.
//!
//! Callers capture before probing, stage a timeout in the start transaction,
//! and invoke `probe_due` from recovery/scheduler work. Probe I/O is always
//! outside the store writer; applying a result rechecks the saved ticket.
use crate::{
    claude_auth_wait::{self, Capture, ProbeTicket},
    continuation::{ContinuationError, Result, Snapshot},
};
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    future::Future,
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use studio_runtime_store::{
    ContinuationRepository, ContinuationTable, RuntimeStore, SchedulerRepository, StoreError,
    Transaction,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const PROBE_INTERVAL: Duration = Duration::from_secs(crate::continuation::POLL_SECONDS as u64);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(180);
const TABLE: ContinuationTable = ContinuationTable::Usage;
static PROBE_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub async fn acquire_probe_slot() -> Result<OwnedSemaphorePermit> {
    PROBE_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(claude_auth_wait::MAX_PROFILES)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ContinuationError::Unavailable("claude-auth/probe-slots"))
}

async fn blocking_store<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| ContinuationError::Unavailable("claude-auth/store-worker"))?
}

pub fn preparation_settings(agent: &Value) -> Value {
    let mut settings = serde_json::Map::new();
    for key in [
        "model",
        "effort",
        "nativeEffort",
        "fastMode",
        "yoloMode",
        "profileInstructions",
        "role",
    ] {
        settings.insert(key.into(), agent.get(key).cloned().unwrap_or(Value::Null));
    }
    for key in ["daybreakEnabled", "cyberAccessProgram"] {
        if let Some(value) = agent.get(key) {
            settings.insert(key.into(), value.clone());
        }
    }
    Value::Object(settings)
}

fn snapshot(tx: &Transaction<'_>, agent: &str, record: Option<&str>) -> Result<Snapshot> {
    Snapshot::from_records(ContinuationRepository::snapshot(tx, agent, TABLE, record)?)
}

/// Bounded due waits for one retained Claude account. The worker calls this on
/// startup and on its shared 180-second auth-probe schedule.
pub fn due_waits(
    store: &RuntimeStore,
    account: &str,
    now: f64,
) -> Result<Vec<(String, i64, Value)>> {
    store.transaction_with(|tx| {
        let mut statement = tx
            .prepare(
                "SELECT agent FROM runtime_usage_resumes \
             WHERE json_extract(record,'$.status')='auth_probe_wait' \
               AND json_extract(record,'$.accountKey')=?1 \
               AND CAST(json_extract(record,'$.dueAt') AS REAL)<=?2 \
             ORDER BY CAST(json_extract(record,'$.dueAt') AS REAL),rowid LIMIT 16",
            )
            .map_err(StoreError::from)?;
        let agents = statement
            .query_map(params![account, now], |row| row.get::<_, String>(0))
            .map_err(StoreError::from)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        let mut result = Vec::with_capacity(agents.len());
        for agent in agents {
            let state = snapshot(tx, &agent, None)?;
            if state.agent.get("provider") != Some(&json!("claude"))
                || state.agent.get("accountKey").and_then(Value::as_str) != Some(account)
            {
                continue;
            }
            let Some(epoch) = state.agent.get("epoch").and_then(Value::as_i64) else {
                continue;
            };
            result.push((agent, epoch, preparation_settings(&state.agent)));
        }
        Ok::<_, ContinuationError>(result)
    })
}

fn persist(
    tx: &Transaction<'_>,
    agent: &str,
    decision: &crate::continuation::Decision,
) -> Result<()> {
    if let Some(record) = &decision.record {
        ContinuationRepository::put_record(tx, TABLE, agent, record)?;
    }
    if let Some(agent) = &decision.agent {
        ContinuationRepository::put_agent(tx, agent)?;
    }
    Ok(())
}

/// Capture exact input/account identity before the local auth probe. No I/O occurs here.
pub fn capture_start(
    tx: &Transaction<'_>,
    agent: &str,
    settings: &Value,
    account_pin: &Value,
) -> Result<Option<Capture>> {
    let state = snapshot(tx, agent, None)?;
    Ok(claude_auth_wait::capture(&state, settings, account_pin))
}

/// Called inside the original start-result transaction only after an auth timeout.
/// This records one wait bound to that unsubmitted start attempt and returns whether
/// the transaction should retain the original input for later recovery.
pub fn stage_start_timeout(
    tx: &Transaction<'_>,
    agent: &str,
    capture: &Capture,
    settings: &Value,
    current_account_pin: &Value,
    now: f64,
) -> Result<bool> {
    let state = snapshot(tx, agent, None)?;
    let decision =
        claude_auth_wait::record_timeout(&state, capture, settings, current_account_pin, now)?;
    let recorded = decision.record.is_some();
    persist(tx, agent, &decision)?;
    Ok(recorded)
}

/// Settle a timed-out local auth probe and save its wait in the same writer transaction.
pub fn settle_start_timeout(
    store: &RuntimeStore,
    intent: &crate::scheduler::TurnIntent,
    capture: &Capture,
    settings: &Value,
    account_pin: &Value,
    now: f64,
) -> Result<bool> {
    crate::scheduler::lifecycle_transaction_with(store, |tx| {
        SchedulerRepository::settle_start_outcome(
            tx,
            &intent.identity(),
            "held",
            Some(claude_auth_wait::AUTH_TIMEOUT_ERROR),
        )?;
        let state = snapshot(tx, &intent.agent_id, None)?;
        let decision =
            claude_auth_wait::record_timeout(&state, capture, settings, account_pin, now)?;
        let recorded = decision.record.is_some();
        persist(tx, &intent.agent_id, &decision)?;
        Ok::<_, ContinuationError>(recorded)
    })
}

fn settle_blocked(
    store: &RuntimeStore,
    intent: &crate::scheduler::TurnIntent,
    reason: &'static str,
) -> Result<()> {
    crate::scheduler::lifecycle_transaction_with(store, |tx| {
        SchedulerRepository::settle_start_outcome(tx, &intent.identity(), "held", Some(reason))?;
        Ok::<_, ContinuationError>(())
    })
}

/// Probe the selected Claude profile before input preparation. Timeout waits and
/// ordinary refusals settle the start; the only accepted error latch is verified
/// through this account worker's retained bridge.
pub async fn require_before_start<F, Fut>(
    store: Arc<RuntimeStore>,
    driver: Arc<crate::account_driver::AccountDriver>,
    selected_pin: Value,
    intent: &crate::scheduler::TurnIntent,
    read_account: F,
) -> Result<bool>
where
    F: FnOnce(crate::account_worker::retained::RequestActor, Instant) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let account_key = intent.account_key.clone();
    let driver_copy = driver.clone();
    let key_copy = account_key.clone();
    let before = tokio::task::spawn_blocking(move || driver_copy.internal_record(&key_copy))
        .await
        .map_err(|_| ContinuationError::Unavailable("claude-auth/account-probe"))?
        .map_err(|_| ContinuationError::Unavailable("claude-auth/account-probe"))?;
    if !same_selected_account(&selected_pin, &before) {
        let store = store.clone();
        let intent = intent.clone();
        blocking_store(move || {
            settle_blocked(
                &store,
                &intent,
                "The Claude account changed during authentication",
            )
        })
        .await?;
        return Ok(false);
    }
    let store_copy = store.clone();
    let agent = intent.agent_id.clone();
    let capture_pin = before.clone();
    let (settings, capture) = blocking_store(move || {
        store_copy.transaction_with(|tx| {
            let state = snapshot(tx, &agent, None)?;
            let settings = preparation_settings(&state.agent);
            let capture = claude_auth_wait::capture(&state, &settings, &capture_pin);
            Ok::<_, ContinuationError>((settings, capture))
        })
    })
    .await?;

    let driver_copy = driver.clone();
    let key_copy = account_key.clone();
    let status = tokio::task::spawn_blocking(move || driver_copy.get(&key_copy))
        .await
        .map_err(|_| ContinuationError::Unavailable("claude-auth/account-probe"))?
        .map_err(|_| ContinuationError::Unavailable("claude-auth/account-probe"))?;
    let driver_copy = driver.clone();
    let after = tokio::task::spawn_blocking(move || driver_copy.internal_record(&account_key))
        .await
        .map_err(|_| ContinuationError::Unavailable("claude-auth/account-probe"))?
        .map_err(|_| ContinuationError::Unavailable("claude-auth/account-probe"))?;
    if claude_auth_wait::account_pin(&before) != claude_auth_wait::account_pin(&after) {
        let store = store.clone();
        let intent = intent.clone();
        blocking_store(move || {
            settle_blocked(
                &store,
                &intent,
                "The Claude account changed during authentication",
            )
        })
        .await?;
        return Ok(false);
    }
    if status == crate::account_driver::AccountStatus::Ready {
        return Ok(true);
    }
    let error_kind = after
        .get("_authErrorKind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status == crate::account_driver::AccountStatus::Error && error_kind == "timeout" {
        if let Some(capture) = capture {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ContinuationError::Invalid("/now".into()))?
                .as_secs_f64();
            let store = store.clone();
            let intent = intent.clone();
            let settings = settings.clone();
            let after = after.clone();
            let _recorded = blocking_store(move || {
                settle_start_timeout(&store, &intent, &capture, &settings, &after, now)
            })
            .await?;
        } else {
            let store = store.clone();
            let intent = intent.clone();
            blocking_store(move || {
                settle_blocked(&store, &intent, claude_auth_wait::AUTH_TIMEOUT_ERROR)
            })
            .await?;
        }
        return Ok(false);
    }

    let email = after
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let identity = format!("claude:{email}");
    let accepted_latch = status == crate::account_driver::AccountStatus::Error
        && error_kind == "keychain"
        && after.get("provider") == Some(&json!("claude"))
        && !email.is_empty()
        && after.get("accountId").and_then(Value::as_str) == Some(identity.as_str())
        && after.get("_credentialIdentity").and_then(Value::as_str) == Some(identity.as_str())
        && !["deleted", "disconnected", "duplicateOf"]
            .iter()
            .any(|field| crate::event_write::truthy(after.get(*field)));
    if accepted_latch {
        let actor = crate::account_worker::retained::RequestActor {
            agent: intent.agent_id.clone(),
            epoch: intent.epoch,
        };
        if let Ok(proof) = read_account(actor, Instant::now() + Duration::from_secs(65)).await {
            let driver_copy = driver.clone();
            let key_copy = intent.account_key.clone();
            let observed = after.clone();
            let confirmed = tokio::task::spawn_blocking(move || {
                driver_copy.confirm_native_claude_auth(&key_copy, &observed, &proof)
            })
            .await;
            if confirmed.is_ok_and(|result| result.is_ok()) {
                return Ok(true);
            }
        }
    }
    let store = store.clone();
    let intent = intent.clone();
    blocking_store(move || settle_blocked(&store, &intent, "Sign in with claude auth login first"))
        .await?;
    Ok(false)
}

fn same_selected_account(selected: &Value, current: &Value) -> bool {
    [
        "id",
        "provider",
        "home",
        "claudeOptions",
        "accountId",
        "_credentialIdentity",
    ]
    .iter()
    .all(|key| selected.get(*key) == current.get(*key))
}

/// Claim a due wait durably, run the provider credential probe without a transaction,
/// then apply only if the exact receipt and account/credential pin still match.
/// `probe` and `read_current_pin` are external ports and are never called under the writer.
pub async fn probe_due<F, Fut>(
    store: Arc<RuntimeStore>,
    agent: &str,
    settings: &Value,
    account_pin: &Value,
    now: f64,
    probe: F,
    read_current_pin: impl FnOnce() -> Value,
) -> Result<bool>
where
    F: FnOnce(&Value) -> Fut,
    Fut: Future<Output = Value>,
{
    let store_copy = store.clone();
    let agent_id = agent.to_owned();
    let attempt_id = blocking_store(move || {
        store_copy.transaction_with(|tx| {
            let state = snapshot(tx, &agent_id, None)?;
            let attempt = state
                .agent
                .get("startAttempt")
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok::<_, ContinuationError>(attempt.to_owned())
        })
    })
    .await?;
    if attempt_id.is_empty() {
        return Ok(false);
    }
    let receipt_id = format!("claude-auth-probe:{attempt_id}");
    let store_copy = store.clone();
    let agent_id = agent.to_owned();
    let receipt = receipt_id.clone();
    let settings_copy = settings.clone();
    let pin_copy = account_pin.clone();
    let claimed: Option<ProbeTicket> = blocking_store(move || {
        store_copy.transaction_with(|tx| {
            let state = snapshot(tx, &agent_id, Some(&receipt))?;
            let (decision, ticket) =
                claude_auth_wait::claim_probe(&state, &settings_copy, &pin_copy, now)?;
            persist(tx, &agent_id, &decision)?;
            Ok::<_, ContinuationError>(ticket)
        })
    })
    .await?;
    let Some(ticket) = claimed else {
        return Ok(false);
    };

    let observed = tokio::time::timeout(PROBE_TIMEOUT, probe(account_pin))
        .await
        .unwrap_or_else(|_| json!({"status":"timeout"}));
    let current_pin = read_current_pin();
    let store_copy = store.clone();
    let agent_id = agent.to_owned();
    let receipt = receipt_id.clone();
    let settings_copy = settings.clone();
    let applied = blocking_store(move || {
        store_copy.transaction_with(|tx| {
            let state = snapshot(tx, &agent_id, Some(&receipt))?;
            let decision = claude_auth_wait::apply_probe(
                &state,
                &ticket,
                &settings_copy,
                &current_pin,
                &observed,
                now,
            )?;
            let completed = decision
                .agent
                .as_ref()
                .is_some_and(|updated| updated.get("status") == Some(&json!("queued")));
            persist(tx, &agent_id, &decision)?;
            Ok::<_, ContinuationError>(completed)
        })
    })
    .await?;
    Ok(applied)
}

/// Account proof requests are read-only and get a fresh retained-session operation identity.
/// The account worker passes its already-owned session; this helper never launches a provider.
pub async fn account_read(
    port: &crate::account_worker::retained::RetainedSessionPort,
    actor: crate::account_worker::retained::RequestActor,
    deadline: Instant,
) -> Result<Value> {
    use crate::account_worker::retained::{RequestResult, SessionRequest};
    use studio_runtime_providers::Method;

    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| ContinuationError::Unavailable("claude-auth/account-read-identity"))?;
    let operation = format!(
        "claude-account-read:{}",
        studio_runtime_store::sha256_hex(&nonce)
    );
    let receipt = port
        .request(SessionRequest {
            scope: port.scope().clone(),
            operation,
            actor,
            method: Method::AccountRead,
            params: json!({}),
            deadline,
        })
        .await
        .map_err(|_| ContinuationError::Unavailable("claude-auth/account-read"))?;
    match receipt.result {
        RequestResult::Reply(proof) => Ok(proof),
        _ => Err(ContinuationError::Unavailable("claude-auth/account-read")),
    }
}

/// Match Python `accounts.confirm_native_auth`: the retained bridge must prove the
/// configured Claude identity and a nonempty first-party subscription plan.
pub fn confirm_account_read(account_pin: &Value, proof: &Value) -> Result<Value> {
    let account = proof.get("account").ok_or(ContinuationError::Unavailable(
        "claude-auth/account-read-proof",
    ))?;
    let account_id = account_pin
        .get("accountId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected_credential = account_pin
        .get("_credentialIdentity")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(email) = account_id.strip_prefix("claude:") else {
        return Err(ContinuationError::Unavailable(
            "claude-auth/account-read-proof",
        ));
    };
    if account.get("type") != Some(&json!("claude"))
        || email.is_empty()
        || account_pin.get("email").and_then(Value::as_str) != Some(email)
        || account.get("email").and_then(Value::as_str) != Some(email)
        || account
            .get("planType")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || account_pin.get("provider") != Some(&json!("claude"))
        || expected_credential != account_id
        || ["deleted", "disconnected", "duplicateOf"]
            .iter()
            .any(|field| crate::event_write::truthy(account_pin.get(*field)))
    {
        return Err(ContinuationError::Unavailable(
            "claude-auth/account-read-proof",
        ));
    }
    Ok(
        json!({"status":"ready","accountId":account_id,"_credentialIdentity":expected_credential,
        "email":email,"plan":account["planType"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::confirm_account_read;
    use serde_json::json;

    #[test]
    fn account_read_requires_the_pinned_first_party_subscription_identity() {
        let pin = json!({"provider":"claude","email":"a@example.test",
            "accountId":"claude:a@example.test","_credentialIdentity":"claude:a@example.test"});
        let proof = json!({"account":{"type":"claude","email":"a@example.test","planType":"max"},
            "requiresOpenaiAuth":false});
        assert_eq!(
            confirm_account_read(&pin, &proof).unwrap()["status"],
            "ready"
        );
        let wrong =
            json!({"account":{"type":"claude","email":"other@example.test","planType":"max"}});
        assert!(confirm_account_read(&pin, &wrong).is_err());
    }
}
