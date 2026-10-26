//! The account auth gate before a turn start and the release of held turns,
//! the same for every provider.
//!
//! A turn whose account is not Ready (signed out, changed, an auth error or a
//! keychain latch) is held durably: its input returns to `pending` and the
//! agent shows `failed` with the hold reason. Nothing polls a held turn. When
//! the account registry publishes a change (a status read that saw new
//! credentials, a login or a logout) or a cached auth status ends (a provider
//! auth error), [`run_release`] requeues the held turns of every account that
//! is Ready again, and the account scheduler claims each turn once.
use crate::account_driver::{AccountDriver, AccountStatus, CHANGED_ERROR};
use crate::continuation::{ContinuationError, Result};
use crate::provider_adapter::adapter;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use studio_runtime_providers::ProviderKind;
use studio_runtime_store::{
    AgentId, AgentRepository, AgentStatus, RuntimeStore, SchedulerRepository, StoreError,
};
use tokio::sync::{Notify, watch};

/// Account fields whose change means a session no longer runs the account it
/// selected.
const PIN_FIELDS: [&str; 6] = [
    "id",
    "provider",
    "home",
    "claudeOptions",
    "accountId",
    "_credentialIdentity",
];

/// Why a start on an account with `status` is held, or `None` when it may
/// start. `selected` is the account record the session launched with and
/// `current` the registry's record now.
fn hold_reason(
    kind: ProviderKind,
    status: AccountStatus,
    selected: &Value,
    current: &Value,
) -> Option<&'static str> {
    match status {
        AccountStatus::Changed => Some(CHANGED_ERROR),
        AccountStatus::Ready if same_selected_account(selected, current) => None,
        AccountStatus::Ready => Some(adapter(kind).account_changed_message()),
        _ => Some(adapter(kind).sign_in_message()),
    }
}

/// Every hold reason; a failed agent with one of them waits for its account.
fn hold_reasons() -> Vec<&'static str> {
    let mut reasons = vec![CHANGED_ERROR];
    for kind in ProviderKind::ALL {
        reasons.extend([
            adapter(kind).sign_in_message(),
            adapter(kind).account_changed_message(),
        ]);
    }
    reasons
}

/// Only the account readiness owner can release these holds.
pub(crate) fn is_hold_reason(reason: &str) -> bool {
    hold_reasons().contains(&reason)
}

fn same_selected_account(selected: &Value, current: &Value) -> bool {
    PIN_FIELDS.iter().all(|key| {
        let old = selected.get(*key).unwrap_or(&Value::Null);
        let new = current.get(*key).unwrap_or(&Value::Null);
        if *key == "_credentialIdentity" && selected.get("provider") == Some(&json!("claude")) {
            old == new
                || old.as_str().zip(new.as_str()).is_some_and(|(old, new)| {
                    crate::account_driver::claude_identity::accepts(old, new)
                })
        } else {
            old == new
        }
    })
}

/// Status and record of `account`, read through the cached auth status.
async fn account_state(
    driver: Arc<AccountDriver>,
    account: String,
) -> Result<(AccountStatus, Value)> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_auth_gate",
        move || {
            Ok::<_, crate::account_driver::AccountDriverError>((
                driver.get(&account)?,
                driver.internal_record(&account)?,
            ))
        },
    )
    .await
    .map_err(|_| ContinuationError::Unavailable("account-auth/status"))?
    .map_err(|_| ContinuationError::Unavailable("account-auth/status"))
}

/// Gate one turn start. A start that may not run is held with its reason in
/// the start-outcome transaction, and this returns `false`.
pub async fn require_before_start(
    store: Arc<RuntimeStore>,
    driver: Arc<AccountDriver>,
    selected_pin: Value,
    intent: &crate::scheduler::TurnIntent,
) -> Result<bool> {
    let (status, current) = account_state(driver, intent.account_key.clone()).await?;
    let Some(reason) = hold_reason(intent.provider, status, &selected_pin, &current) else {
        return Ok(true);
    };
    let identity = intent.identity();
    crate::scheduler::lifecycle_transaction_with(&store, move |tx| {
        SchedulerRepository::settle_start_outcome(tx, &identity, "held", Some(reason))?;
        Ok::<_, ContinuationError>(())
    })
    .await?;
    Ok(false)
}

/// The hold reason of an account whose provider launch `error` refused for a
/// reason that only a sign-in clears, or `None` for any other launch failure.
pub fn launch_hold_reason(error: &crate::account_worker::WorkerError) -> Option<&'static str> {
    match error {
        crate::account_worker::WorkerError::PreparationRefused(code)
            if code == crate::provider_adapter::CLAUDE_ACCOUNT_IDENTITY_REFUSAL =>
        {
            Some(adapter(ProviderKind::Claude).sign_in_message())
        }
        _ => None,
    }
}

/// Hold the queued starts of `agents` (agent, epoch) whose account cannot
/// launch its provider until it signs in, with `reason`, as a gated start is
/// held: the agent shows `failed` with the reason and keeps its pending input,
/// and [`release_held`] requeues it once the account is Ready. An agent that
/// moved on (another epoch, a claim, a stop or a delete) is left unchanged.
/// Returns the held agents.
pub async fn hold_unstarted(
    store: &RuntimeStore,
    agents: Vec<(String, i64)>,
    reason: &'static str,
) -> Result<Vec<String>> {
    Ok(
        crate::scheduler::lifecycle_transaction_with(store, move |tx| {
            let mut held = Vec::new();
            for (agent_id, epoch) in agents {
                let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                else {
                    continue;
                };
                if agent.status != AgentStatus::Queued
                    || agent.epoch != epoch
                    || agent.extra.get("autoWake") != Some(&json!(true))
                    || studio_runtime_store::truthy(agent.extra.get("inFlight"))
                    || agent.extra.contains_key("startAttempt")
                    || studio_runtime_store::truthy(agent.extra.get("deletedAt"))
                {
                    continue;
                }
                agent.status = AgentStatus::Failed;
                agent.extra.insert("error".into(), json!(reason));
                agent.extra.remove(crate::account_start_gate::WAIT_FIELD);
                AgentRepository::put(tx, &agent)?;
                held.push(agent_id);
            }
            Ok::<_, StoreError>(held)
        })
        .await?,
    )
}

/// A held turn: its agent, the epoch of its pending input and its hold reason.
struct Held {
    agent: String,
    epoch: i64,
    reason: String,
}

/// Requeue the held turns of every account that is Ready again, oldest input
/// first. Returns the requeued `(agent, epoch)` pairs; the account scheduler
/// claims each one. A turn whose account is still not Ready stays held, and
/// no record is written for it.
pub async fn release_held(
    store: &RuntimeStore,
    driver: &Arc<AccountDriver>,
) -> Result<Vec<(String, i64)>> {
    let reasons = json!(hold_reasons()).to_string();
    let held = store
        .read_async("runtime-services/account_auth_gate", move |db| {
            let mut statement = db.prepare_cached(
                "SELECT a.id,json_extract(a.record,'$.epoch'), \
                   COALESCE(json_extract(a.record,'$.accountKey'),'default'), \
                   json_extract(a.record,'$.error'), \
                   (SELECT MIN(e.created) FROM runtime_events e WHERE e.agent=a.id \
                      AND e.epoch=json_extract(a.record,'$.epoch') AND e.status='pending') AS first \
                 FROM runtime_agents a \
                 WHERE json_extract(a.record,'$.status')='failed' \
                   AND json_extract(a.record,'$.autoWake')=1 \
                   AND COALESCE(json_extract(a.record,'$.inFlight'),0)=0 \
                   AND json_extract(a.record,'$.deletedAt') IS NULL \
                   AND json_extract(a.record,'$.error') IN (SELECT value FROM json_each(?1)) \
                   AND first IS NOT NULL \
                 ORDER BY first,a.rowid",
            )?;
            let rows = statement
                .query_map([reasons], |row| {
                    Ok((
                        row.get::<_, String>(2)?,
                        Held {
                            agent: row.get(0)?,
                            epoch: row.get(1)?,
                            reason: row.get(3)?,
                        },
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok::<_, StoreError>(rows)
        })
        .await?;
    let mut ready = BTreeMap::new();
    let mut release = Vec::new();
    for (account, turn) in held {
        if !ready.contains_key(&account) {
            let state = account_state(Arc::clone(driver), account.clone()).await?;
            ready.insert(account.clone(), state.0 == AccountStatus::Ready);
        }
        if ready[&account] {
            release.push(turn);
        }
    }
    if release.is_empty() {
        return Ok(Vec::new());
    }
    Ok(
        crate::scheduler::lifecycle_transaction_with(store, move |tx| {
            let mut requeued = Vec::new();
            for turn in release {
                let Some(mut agent) =
                    AgentRepository::get(tx, &AgentId::from(turn.agent.as_str()))?
                else {
                    continue;
                };
                // Recheck under the writer: a concurrent release or a new message
                // already requeued it, or the agent moved on.
                if agent.status != AgentStatus::Failed
                    || agent.epoch != turn.epoch
                    || agent.extra.get("error").and_then(Value::as_str)
                        != Some(turn.reason.as_str())
                {
                    continue;
                }
                agent.status = AgentStatus::Queued;
                agent.extra.insert("error".into(), Value::Null);
                AgentRepository::put(tx, &agent)?;
                requeued.push((turn.agent, turn.epoch));
            }
            Ok::<_, StoreError>(requeued)
        })
        .await?,
    )
}

/// Release held turns now and after every account or auth status change,
/// until `shutdown`. Each release that requeued a turn notifies `wake`, the
/// account scheduler's wake. It never wakes on a timer.
pub async fn run_release(
    store: Arc<RuntimeStore>,
    driver: Arc<AccountDriver>,
    wake: Arc<Notify>,
    shutdown: watch::Receiver<bool>,
) {
    release_loop(store, driver, shutdown, |released| {
        if released.is_some_and(|count| count > 0) {
            wake.notify_one();
        }
    })
    .await;
}

/// [`run_release`] with `pass` called with `None` when a release pass starts
/// and with the number of requeued turns when it ends.
pub(crate) async fn release_loop(
    store: Arc<RuntimeStore>,
    driver: Arc<AccountDriver>,
    mut shutdown: watch::Receiver<bool>,
    mut pass: impl FnMut(Option<usize>),
) {
    let mut accounts = driver.change_signal().subscribe();
    let mut auth = crate::account_driver::auth_status_changes();
    loop {
        pass(None);
        let released = release_held(&store, &driver).await.unwrap_or_else(|error| {
            eprintln!("held turn release failed: {error}");
            Vec::new()
        });
        pass(Some(released.len()));
        let open = tokio::select! {
            changed = accounts.changed() => changed.is_ok(),
            changed = auth.changed() => changed.is_ok(),
            changed = shutdown.changed() => changed.is_ok() && !*shutdown.borrow(),
        };
        if !open {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hold_reason, same_selected_account};
    use crate::account_driver::{AccountStatus, CHANGED_ERROR};
    use serde_json::json;
    use studio_runtime_providers::ProviderKind;

    #[test]
    fn claude_identity_upgrade_preserves_admission_but_changed_org_holds() {
        let selected = json!({"provider":"claude", "_credentialIdentity":"claude:a@example.test"});
        let mut current = selected.clone();
        current["_credentialIdentity"] = json!(crate::account_driver::claude_identity::identity(
            "a@example.test",
            Some("org-a"),
            Some("account-a")
        ));
        assert!(super::same_selected_account(&selected, &current));
        let pinned = current.clone();
        current["_credentialIdentity"] = json!(crate::account_driver::claude_identity::identity(
            "a@example.test",
            Some("org-b"),
            Some("account-a")
        ));
        assert!(!super::same_selected_account(&pinned, &current));
        assert!(!super::same_selected_account(&pinned, &selected));
    }

    #[test]
    fn selected_account_comparison_treats_missing_and_null_alike() {
        let current = json!({"id":"claude-local","provider":"claude","home":"/fixture/claude",
            "accountId":"claude:fixture@example.test","_credentialIdentity":"claude:fixture@example.test"});
        let mut explicit_null = current.clone();
        explicit_null["claudeOptions"] = json!(null);
        assert!(same_selected_account(&current, &explicit_null));
        assert!(same_selected_account(&explicit_null, &current));
    }

    #[test]
    fn every_identity_field_change_holds_the_start_for_both_providers() {
        for kind in ProviderKind::ALL {
            let selected = json!({"id":"a","provider":kind.key(),"home":"/fixture/home",
                "claudeOptions":{"binaryPath":"/fixture/bin"},"accountId":"x","_credentialIdentity":"x"});
            assert_eq!(
                hold_reason(kind, AccountStatus::Ready, &selected, &selected),
                None
            );
            for field in super::PIN_FIELDS {
                let mut current = selected.clone();
                current[field] = json!("changed");
                assert_eq!(
                    hold_reason(kind, AccountStatus::Ready, &selected, &current),
                    Some(crate::provider_adapter::adapter(kind).account_changed_message()),
                    "{field}"
                );
            }
            assert_eq!(
                hold_reason(kind, AccountStatus::Changed, &selected, &selected),
                Some(CHANGED_ERROR)
            );
            for status in [
                AccountStatus::SignedOut,
                AccountStatus::Pending,
                AccountStatus::Error,
                AccountStatus::Duplicate,
                AccountStatus::Unavailable,
            ] {
                assert_eq!(
                    hold_reason(kind, status, &selected, &selected),
                    Some(crate::provider_adapter::adapter(kind).sign_in_message())
                );
            }
        }
    }
}
