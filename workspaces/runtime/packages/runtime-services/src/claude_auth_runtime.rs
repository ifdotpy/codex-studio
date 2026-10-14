//! Store and process adapter for Claude's retained auth probe waits.
//!
//! Callers capture before probing, stage a timeout in the start transaction,
//! and invoke `probe_due` from recovery/scheduler work. Probe I/O is always
//! outside the store writer; applying a result rechecks the saved ticket.
use crate::{
    claude_auth_wait::{self, Capture, ProbeTicket},
    continuation::{ContinuationError, Result, Snapshot},
};
use serde_json::{Value, json};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use studio_runtime_store::{ContinuationRepository, ContinuationTable, RuntimeStore, Transaction};

pub const PROBE_INTERVAL: Duration = Duration::from_secs(crate::continuation::POLL_SECONDS as u64);
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(180);
const TABLE: ContinuationTable = ContinuationTable::Usage;

fn snapshot(tx: &Transaction<'_>, agent: &str, record: Option<&str>) -> Result<Snapshot> {
    Snapshot::from_records(ContinuationRepository::snapshot(tx, agent, TABLE, record)?)
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

/// Claim a due wait durably, run the provider credential probe without a transaction,
/// then apply only if the exact receipt and account/credential pin still match.
/// `probe` and `read_current_pin` are external ports and are never called under the writer.
pub async fn probe_due<F, Fut>(
    store: &RuntimeStore,
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
    let attempt_id = store.transaction_with(|tx| {
        let state = snapshot(tx, agent, None)?;
        let attempt = state
            .agent
            .get("startAttempt")
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok::<_, ContinuationError>(attempt.to_owned())
    })?;
    if attempt_id.is_empty() {
        return Ok(false);
    }
    let receipt_id = format!("claude-auth-probe:{attempt_id}");
    let claimed: Option<ProbeTicket> = store.transaction_with(|tx| {
        let state = snapshot(tx, agent, Some(&receipt_id))?;
        let (decision, ticket) = claude_auth_wait::claim_probe(&state, settings, account_pin, now)?;
        persist(tx, agent, &decision)?;
        Ok::<_, ContinuationError>(ticket)
    })?;
    let Some(ticket) = claimed else {
        return Ok(false);
    };

    let observed = tokio::time::timeout(PROBE_TIMEOUT, probe(account_pin))
        .await
        .unwrap_or_else(|_| json!({"status":"timeout"}));
    let current_pin = read_current_pin();
    let applied = store.transaction_with(|tx| {
        let state = snapshot(tx, agent, Some(&receipt_id))?;
        let decision =
            claude_auth_wait::apply_probe(&state, &ticket, settings, &current_pin, &observed, now)?;
        let completed = decision
            .agent
            .as_ref()
            .is_some_and(|updated| updated.get("status") == Some(&json!("queued")));
        persist(tx, agent, &decision)?;
        Ok::<_, ContinuationError>(completed)
    })?;
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
