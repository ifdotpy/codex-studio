//! Exactly-once reset-credit redemption over the retained account session.

use crate::account_session_runtime::{
    AccountSessionError, AccountSessionPort, ProviderRequest, ProviderResponse, RequestCertainty,
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use studio_runtime_providers::Method;
use studio_runtime_store::{RuntimeStore, StoreError};
use thiserror::Error;
use uuid::Uuid;

const RESET_OPERATION_PREFIX: &str = "limit-reset:";
const RESET_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const INITIAL_TRANSPORT_ATTEMPTS: u64 = 0;
static RESET_REQUEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Error)]
pub enum ResetError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Provider(String),
    #[error("reset-credit persistence failed")]
    Store(#[from] StoreError),
    #[error("reset-credit persistence failed")]
    Sql(#[from] rusqlite::Error),
    #[error("reset-credit persistence failed")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Attempt {
    id: String,
    account_id: String,
    account_key: String,
    credit_id: String,
    idempotency_key: String,
    #[serde(default = "initial_transport_attempts")]
    transport_attempts: u64,
    status: String,
    #[serde(default)]
    certainty: Option<String>,
    outcome: Option<String>,
    error: Option<String>,
    #[serde(default)]
    created: Option<f64>,
    #[serde(default)]
    updated: Option<f64>,
}

fn initial_transport_attempts() -> u64 {
    INITIAL_TRANSPORT_ATTEMPTS
}

fn now() -> Result<f64, ResetError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| ResetError::Invalid("The system clock is unavailable"))
}

fn required(data: &Value, name: &'static str, message: &'static str) -> Result<String, ResetError> {
    data.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.chars().count() <= 1024)
        .map(str::to_owned)
        .ok_or(ResetError::Invalid(message))
}

fn from_record(value: Value) -> Result<Attempt, ResetError> {
    serde_json::from_value(value)
        .map_err(|_| ResetError::Invalid("The reset attempt record is invalid"))
}

fn read_attempts(
    store: &RuntimeStore,
    request_id: &str,
    account: &str,
    credit: &str,
) -> Result<Option<Attempt>, ResetError> {
    store.transaction_with(|tx| {
        let alias = tx
            .query_row(
                "SELECT attempt_id FROM runtime_limit_reset_requests WHERE id=?1",
                [request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let mut statement = tx.prepare("SELECT record FROM runtime_limit_reset_attempts")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut attempts = Vec::new();
        for row in rows {
            attempts.push(from_record(serde_json::from_str(&row?)?)?);
        }
        if let Some(alias) = alias {
            if let Some(attempt) = attempts.iter().find(|attempt| attempt.id == alias) {
                if attempt.account_id != account || attempt.credit_id != credit {
                    return Err(ResetError::Invalid(
                        "This reset request belongs to a different account or credit",
                    ));
                }
                return Ok(Some(attempt.clone()));
            }
        }
        let attempt = attempts.into_iter().rev().find(|attempt| {
            attempt.account_id == account
                && attempt.credit_id == credit
                && (matches!(attempt.status.as_str(), "pending" | "uncertain")
                    || matches!(
                        attempt.outcome.as_deref(),
                        Some("reset" | "alreadyRedeemed")
                    ))
        });
        if let Some(attempt) = attempt {
            // A public request id is permanently bound even when it reuses an
            // existing account/credit attempt. This prevents later rebinding.
            tx.execute(
                "INSERT OR IGNORE INTO runtime_limit_reset_requests (id,attempt_id) VALUES (?1,?2)",
                (request_id, &attempt.id),
            )?;
            return Ok(Some(attempt));
        }
        Ok(None)
    })
}

fn save_attempt(
    store: &RuntimeStore,
    attempt: &Attempt,
    request_id: &str,
) -> Result<(), ResetError> {
    let mut attempt = attempt.clone();
    attempt.updated = Some(now()?);
    let record = serde_json::to_string(&attempt)
        .map_err(|_| ResetError::Invalid("The reset attempt record is invalid"))?;
    store.transaction_with(|tx| {
        tx.execute(
            "INSERT INTO runtime_limit_reset_attempts (id,record) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            (&attempt.id, record),
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO runtime_limit_reset_requests (id,attempt_id) VALUES (?1,?2)",
            (request_id, &attempt.id),
        )?;
        Ok(())
    })
}

/// Atomically bind a new public request to the one attempt for this account and credit.
/// The store serializes this check and insert; provider I/O happens after commit.
fn reserve_attempt(
    store: &RuntimeStore,
    candidate: Attempt,
    request_id: &str,
) -> Result<Attempt, ResetError> {
    store.transaction_with(|tx| {
        let alias = tx
            .query_row(
                "SELECT attempt_id FROM runtime_limit_reset_requests WHERE id=?1",
                [request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let mut statement = tx.prepare("SELECT record FROM runtime_limit_reset_attempts")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut attempts = Vec::new();
        for row in rows {
            attempts.push(from_record(serde_json::from_str(&row?)?)?);
        }
        if let Some(alias) = alias {
            let attempt = attempts
                .into_iter()
                .find(|attempt| attempt.id == alias)
                .ok_or(ResetError::Invalid("The reset request receipt is invalid"))?;
            if attempt.account_id != candidate.account_id
                || attempt.credit_id != candidate.credit_id
            {
                return Err(ResetError::Invalid(
                    "This reset request belongs to a different account or credit",
                ));
            }
            return Ok(attempt);
        }
        if let Some(attempt) = attempts.into_iter().rev().find(|attempt| {
            attempt.account_id == candidate.account_id
                && attempt.credit_id == candidate.credit_id
                && (matches!(attempt.status.as_str(), "pending" | "uncertain")
                    || matches!(
                        attempt.outcome.as_deref(),
                        Some("reset" | "alreadyRedeemed")
                    ))
        }) {
            tx.execute(
                "INSERT OR IGNORE INTO runtime_limit_reset_requests (id,attempt_id) VALUES (?1,?2)",
                (request_id, &attempt.id),
            )?;
            return Ok(attempt);
        }
        let record = serde_json::to_string(&candidate)?;
        tx.execute(
            "INSERT INTO runtime_limit_reset_attempts (id,record) VALUES (?1,?2)",
            (&candidate.id, record),
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO runtime_limit_reset_requests (id,attempt_id) VALUES (?1,?2)",
            (request_id, &candidate.id),
        )?;
        Ok(candidate)
    })
}

fn account_id(limits: &Value) -> Option<&str> {
    limits.get("data")?.get("accountId")?.as_str()
}

fn has_available_credit(limits: &Value, credit: &str, now: f64) -> bool {
    limits
        .get("data")
        .and_then(|data| data.get("rateLimitResetCredits"))
        .and_then(|summary| summary.get("credits"))
        .and_then(Value::as_array)
        .is_some_and(|credits| {
            credits.iter().any(|row| {
                row.get("id").and_then(Value::as_str) == Some(credit)
                    && row.get("status").and_then(Value::as_str) == Some("available")
                    && row.get("resetType").and_then(Value::as_str) == Some("codexRateLimits")
                    && row.get("expiresAt").is_none_or(|expires| {
                        expires.is_null() || expires.as_f64().is_some_and(|value| value > now)
                    })
            })
        })
}

fn response(attempt: &Attempt, limits: Value) -> Value {
    json!({
        "outcome":attempt.outcome.as_deref().unwrap_or("uncertain"),
        "request_id":attempt.id,
        "credit_id":attempt.credit_id,
        "account_id":attempt.account_id,
        "account_key":attempt.account_key,
        "limits":limits,
        "error":attempt.error,
    })
}

/// Persist the provider charge identity before dispatch. Retries of pending or uncertain
/// attempts retain that key while advancing the durable retained-worker transport sequence.
pub fn consume_reset(
    store: &RuntimeStore,
    session: &dyn AccountSessionPort,
    data: &Value,
) -> Result<Value, ResetError> {
    let account_key = data
        .get("account_key")
        .and_then(Value::as_str)
        .unwrap_or("default");
    if account_key.is_empty() {
        return Err(ResetError::Invalid("Supply an account key"));
    }
    // Python codex_limit_resets.py serializes limit refresh and redemption. Keep
    // concurrent retries from racing their durable outcome transitions.
    let _guard = RESET_REQUEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let account = required(
        data,
        "account_id",
        "Supply the account id from current usage",
    )?;
    let credit = required(data, "credit_id", "Supply an exact reset credit id")?;
    let request_id = match data.get("request_id").and_then(Value::as_str) {
        Some(value) => Uuid::parse_str(value)
            .map_err(|_| ResetError::Invalid("Supply a UUID request_id"))?
            .to_string(),
        None => Uuid::new_v4().to_string(),
    };
    let mut attempt = read_attempts(store, &request_id, &account, &credit)?;
    let limits = session
        .read_limits(account_key)
        .map_err(|error| ResetError::Provider(error.to_string()))?;
    if account_id(&limits) != Some(account.as_str()) {
        return Err(ResetError::Invalid(
            "The account changed. Reload usage before using a reset credit",
        ));
    }
    let current_time = now()?;
    if attempt.is_none() && !has_available_credit(&limits, &credit, current_time) {
        return Err(ResetError::Invalid(
            "This exact reset credit is no longer available. Reload usage",
        ));
    }
    let mut attempt = match attempt.take() {
        Some(attempt) => attempt,
        None => reserve_attempt(
            store,
            Attempt {
                id: request_id.clone(),
                account_id: account.clone(),
                account_key: account_key.to_owned(),
                credit_id: credit.clone(),
                idempotency_key: Uuid::new_v4().to_string(),
                transport_attempts: INITIAL_TRANSPORT_ATTEMPTS,
                status: "pending".to_owned(),
                certainty: None,
                outcome: None,
                error: None,
                created: Some(current_time),
                updated: None,
            },
            &request_id,
        )?,
    };
    if attempt.status == "completed" {
        return Ok(response(&attempt, limits));
    }
    attempt.certainty = None;
    save_attempt(store, &attempt, &request_id)?;
    let scope = match session.ensure_session(account_key) {
        Ok(scope) => scope,
        Err(error) => {
            attempt.status = "pending".to_owned();
            attempt.certainty = Some("not_sent".to_owned());
            attempt.error = Some(error.to_string());
            save_attempt(store, &attempt, &request_id)?;
            return Err(ResetError::Provider(error.to_string()));
        }
    };
    // The provider key identifies the charge. The persisted sequence identifies
    // each retained-worker try so a lost response can be recovered through the
    // provider's idempotent replay instead of the worker's permanent Unknown.
    attempt.transport_attempts =
        attempt
            .transport_attempts
            .checked_add(1)
            .ok_or(ResetError::Invalid(
                "The reset transport attempt limit was reached",
            ))?;
    save_attempt(store, &attempt, &request_id)?;
    let operation = format!(
        "{RESET_OPERATION_PREFIX}{}:{}",
        attempt.idempotency_key, attempt.transport_attempts
    );
    let receipt = session.provider_request(ProviderRequest {
        account: account_key.to_owned(),
        operation: operation.clone(),
        actor: None,
        expected_scope: Some(scope.clone()),
        deleted_actor_session: None,
        method: Method::AccountRateLimitResetCreditConsume,
        params: json!({"creditId":credit,"idempotencyKey":attempt.idempotency_key}),
        deadline: std::time::Instant::now() + RESET_REQUEST_TIMEOUT,
    });
    match receipt {
        Ok(receipt)
            if receipt.operation == operation
                && receipt.scope == scope
                && receipt.certainty == RequestCertainty::NotSent =>
        {
            attempt.status = "pending".to_owned();
            attempt.certainty = Some("not_sent".to_owned());
            attempt.error =
                Some("The reset request was not sent; retry the same request_id".to_owned());
        }
        Ok(receipt)
            if receipt.operation == operation
                && receipt.scope == scope
                && receipt.certainty == RequestCertainty::Written
                && receipt.ticket.is_some() =>
        {
            let outcome = receipt.result.and_then(|result| match result {
                ProviderResponse::Reply(value) => value
                    .get("outcome")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                ProviderResponse::Rejected(_) => None,
            });
            if let Some(outcome) = outcome.filter(|value| {
                matches!(
                    value.as_str(),
                    "reset" | "nothingToReset" | "noCredit" | "alreadyRedeemed"
                )
            }) {
                attempt.status = "completed".to_owned();
                attempt.certainty = Some("written".to_owned());
                attempt.outcome = Some(outcome);
                attempt.error = None;
            } else {
                attempt.status = "uncertain".to_owned();
                attempt.certainty = Some("written".to_owned());
                attempt.error =
                    Some("The reset result is unknown. Retry the same attempt".to_owned());
            }
        }
        Ok(_) => {
            attempt.status = "uncertain".to_owned();
            attempt.certainty = Some("unknown".to_owned());
            attempt.error = Some("The reset result is unknown. Retry the same attempt".to_owned());
        }
        Err(AccountSessionError::Unknown) => {
            attempt.status = "uncertain".to_owned();
            attempt.certainty = Some("unknown".to_owned());
            attempt.error = Some("The reset result is unknown. Retry the same attempt".to_owned());
        }
        Err(error) => {
            attempt.status = "uncertain".to_owned();
            attempt.certainty = Some("unknown".to_owned());
            attempt.error = Some(error.to_string());
        }
    }
    save_attempt(store, &attempt, &request_id)?;
    let limits = session.read_limits(account_key).unwrap_or_else(|_| {
        session.limits_snapshot(account_key).unwrap_or_else(|_| json!({"accountKey":account_key,"data":null,"error":"Account limits are unavailable"}))
    });
    Ok(response(&attempt, limits))
}

#[cfg(test)]
mod tests {
    use super::{
        Attempt, RESET_OPERATION_PREFIX, RESET_REQUEST_LOCK, consume_reset, read_attempts,
        save_attempt,
    };
    use crate::account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    };
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use studio_runtime_providers::{Method, RequestId, RequestTicket};
    use studio_runtime_store::RuntimeStore;

    enum Reply {
        NotSent,
        Unknown,
        Outcome(&'static str),
    }

    struct FakeProvider {
        limits: Value,
        scope: ConnectionScope,
        replies: Mutex<VecDeque<Reply>>,
        requests: Mutex<Vec<(String, Value)>>,
        reads: Mutex<usize>,
        assert_reset_lock: bool,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl FakeProvider {
        fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
            Self {
                limits: json!({
                    "accountKey":"default",
                    "data":{"accountId":"account-A","rateLimitResetCredits":{"credits":[{
                        "id":"credit-A","status":"available","resetType":"codexRateLimits"
                    }]}}
                }),
                scope: ConnectionScope {
                    account: "default".into(),
                    connection: "fixture-connection".into(),
                    generation: 1,
                },
                replies: Mutex::new(replies.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
                reads: Mutex::new(0),
                assert_reset_lock: false,
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
            }
        }
    }

    impl AccountSessionPort for FakeProvider {
        fn ensure_session(&self, account: &str) -> Result<ConnectionScope, AccountSessionError> {
            assert_eq!(account, "default");
            Ok(self.scope.clone())
        }

        fn read_limits(&self, account: &str) -> Result<Value, AccountSessionError> {
            assert_eq!(account, "default");
            *self.reads.lock().unwrap() += 1;
            Ok(self.limits.clone())
        }

        fn provider_request(
            &self,
            request: ProviderRequest,
        ) -> Result<SessionReceipt, AccountSessionError> {
            assert_eq!(request.method, Method::AccountRateLimitResetCreditConsume);
            let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
            if self.assert_reset_lock {
                assert!(
                    RESET_REQUEST_LOCK.try_lock().is_err(),
                    "the reset serialization lock must cover provider I/O"
                );
            }
            self.requests
                .lock()
                .unwrap()
                .push((request.operation.clone(), request.params.clone()));
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Reply::Unknown);
            let (certainty, result, ticket) = match reply {
                Reply::NotSent => (RequestCertainty::NotSent, None, None),
                Reply::Unknown => {
                    self.in_flight.fetch_sub(1, Ordering::SeqCst);
                    return Err(AccountSessionError::Unknown);
                }
                Reply::Outcome(outcome) => (
                    RequestCertainty::Written,
                    Some(ProviderResponse::Reply(json!({"outcome":outcome}))),
                    Some(RequestTicket {
                        generation: self.scope.generation,
                        id: RequestId::String(request.operation.clone()),
                        method: request.method,
                    }),
                ),
            };
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(SessionReceipt {
                scope: self.scope.clone(),
                operation: request.operation,
                ticket,
                certainty,
                result,
                not_sent_reason: None,
                replayed: false,
            })
        }
    }

    fn store() -> (tempfile::TempDir, RuntimeStore) {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).unwrap();
        (root, store)
    }

    fn request(request_id: &str) -> Value {
        json!({"account_key":"default","account_id":"account-A","credit_id":"credit-A","request_id":request_id})
    }

    #[test]
    fn not_sent_retry_changes_transport_identity_and_preserves_charge_identity() {
        let (_root, store) = store();
        let provider = FakeProvider::new([Reply::NotSent, Reply::Outcome("reset")]);
        let request_id = "11000000-0000-4000-8000-000000000001";
        let first = consume_reset(&store, &provider, &request(request_id)).unwrap();
        let second = consume_reset(&store, &provider, &request(request_id)).unwrap();
        let calls = provider.requests.lock().unwrap();

        assert_eq!(first["outcome"], "uncertain");
        assert_eq!(second["outcome"], "reset");
        assert_eq!(second["request_id"], request_id);
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].0,
            format!(
                "{RESET_OPERATION_PREFIX}{}:1",
                calls[0].1["idempotencyKey"].as_str().unwrap()
            )
        );
        assert_eq!(
            calls[1].0,
            format!(
                "{RESET_OPERATION_PREFIX}{}:2",
                calls[1].1["idempotencyKey"].as_str().unwrap()
            )
        );
        assert_eq!(calls[0].1, calls[1].1);
        assert_eq!(calls[0].1["creditId"], "credit-A");
        assert_eq!(calls[0].1["idempotencyKey"], calls[1].1["idempotencyKey"]);
    }

    #[test]
    fn unknown_retry_uses_new_transport_identity_and_same_provider_key() {
        let (_root, store) = store();
        let provider = FakeProvider::new([Reply::Unknown, Reply::Outcome("reset")]);
        let request_id = "11000000-0000-4000-8000-000000000002";
        let first = consume_reset(&store, &provider, &request(request_id)).unwrap();
        let retry = consume_reset(&store, &provider, &request(request_id)).unwrap();

        assert_eq!(first["outcome"], "uncertain");
        assert_eq!(retry["outcome"], "reset");
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0.split(':').next_back(), Some("1"));
        assert_eq!(calls[1].0.split(':').next_back(), Some("2"));
        assert_eq!(calls[0].1, calls[1].1);
        assert_eq!(calls[0].1["idempotencyKey"], calls[1].1["idempotencyKey"]);
    }

    #[test]
    fn concurrent_retries_keep_one_provider_call_in_flight_per_attempt() {
        let (_root, store) = store();
        let mut provider = FakeProvider::new([Reply::Unknown, Reply::Outcome("reset")]);
        provider.assert_reset_lock = true;
        let provider = Arc::new(provider);
        let store = Arc::new(store);
        let request_id = "11000000-0000-4000-8000-000000000016";
        let start = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let provider = Arc::clone(&provider);
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            workers.push(std::thread::spawn(move || {
                start.wait();
                consume_reset(&store, &*provider, &request(request_id)).unwrap()
            }));
        }
        start.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        assert!(
            results
                .iter()
                .any(|result| result["outcome"] == "uncertain")
        );
        assert!(results.iter().any(|result| result["outcome"] == "reset"));
        assert_eq!(provider.max_in_flight.load(Ordering::SeqCst), 1);
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0.split(':').next_back(), Some("1"));
        assert_eq!(calls[1].0.split(':').next_back(), Some("2"));
        assert_eq!(calls[0].1, calls[1].1);
        assert_eq!(calls[0].1["idempotencyKey"], calls[1].1["idempotencyKey"]);
    }

    #[test]
    fn recovered_pending_attempt_advances_persisted_transport_sequence() {
        let (_root, store) = store();
        let request_id = "11000000-0000-4000-8000-000000000014";
        let stored_key = "11000000-0000-4000-8000-000000000015";
        let pending = Attempt {
            id: request_id.into(),
            account_id: "account-A".into(),
            account_key: "default".into(),
            credit_id: "credit-A".into(),
            idempotency_key: stored_key.into(),
            // The prior process committed attempt one, then stopped before dispatch.
            transport_attempts: 1,
            status: "pending".into(),
            certainty: None,
            outcome: None,
            error: Some("interrupted before the result was saved".into()),
            created: Some(1.0),
            updated: Some(2.0),
        };
        save_attempt(&store, &pending, request_id).unwrap();
        let provider = FakeProvider::new([Reply::Outcome("reset")]);

        let result = consume_reset(&store, &provider, &request(request_id)).unwrap();

        assert_eq!(result["outcome"], "reset");
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["idempotencyKey"], stored_key);
        assert_eq!(
            calls[0].0,
            format!("{RESET_OPERATION_PREFIX}{stored_key}:2")
        );
    }

    #[test]
    fn request_identity_cannot_be_rebound_and_exact_credit_is_required() {
        let (_root, store) = store();
        let provider = FakeProvider::new([Reply::Outcome("noCredit")]);
        let request_id = "11000000-0000-4000-8000-000000000003";
        assert_eq!(
            consume_reset(&store, &provider, &request(request_id)).unwrap()["outcome"],
            "noCredit"
        );
        let mut rebound = request(request_id);
        rebound["credit_id"] = Value::String("credit-B".into());
        assert!(consume_reset(&store, &provider, &rebound).is_err());
        let mut absent = request("11000000-0000-4000-8000-000000000004");
        absent["credit_id"] = Value::String("absent".into());
        assert!(consume_reset(&store, &provider, &absent).is_err());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn reused_request_identity_is_bound_to_the_existing_credit_attempt() {
        let (_root, store) = store();
        let provider = FakeProvider::new([Reply::Outcome("reset")]);
        let first_id = "11000000-0000-4000-8000-000000000010";
        let alias_id = "11000000-0000-4000-8000-000000000011";
        assert_eq!(
            consume_reset(&store, &provider, &request(first_id)).unwrap()["outcome"],
            "reset"
        );
        assert_eq!(
            consume_reset(&store, &provider, &request(alias_id)).unwrap()["outcome"],
            "reset"
        );
        let mut rebound = request(alias_id);
        rebound["credit_id"] = Value::String("credit-B".into());
        assert!(consume_reset(&store, &provider, &rebound).is_err());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn settled_attempt_followed_by_new_action_gets_a_new_idempotency_key() {
        let (_root, store) = store();
        let provider = FakeProvider::new([Reply::Outcome("noCredit"), Reply::Outcome("reset")]);
        let first_id = "11000000-0000-4000-8000-000000000012";
        let second_id = "11000000-0000-4000-8000-000000000013";
        assert_eq!(
            consume_reset(&store, &provider, &request(first_id)).unwrap()["outcome"],
            "noCredit"
        );
        assert_eq!(
            consume_reset(&store, &provider, &request(second_id)).unwrap()["outcome"],
            "reset"
        );
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].1["idempotencyKey"], calls[1].1["idempotencyKey"]);
        let first = read_attempts(&store, first_id, "account-A", "credit-A")
            .unwrap()
            .unwrap();
        let second = read_attempts(&store, second_id, "account-A", "credit-A")
            .unwrap()
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_ne!(first.idempotency_key, second.idempotency_key);
    }
}
