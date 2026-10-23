//! Ordered limits persistence. The journal identity and the snapshot commit together.
use super::*;
use rusqlite::OptionalExtension;
use studio_runtime_store::{StoreError, SyncDocumentsRepository};

pub(super) enum LimitsWrite {
    Snapshot(Value),
    Notification(LimitsUpdate),
    ReadFailure {
        scope: Option<ConnectionScope>,
        before: Value,
        value: Value,
    },
    Read {
        scope: ConnectionScope,
        before: Value,
        value: Value,
    },
}

impl LimitsWrite {
    fn scope(&self) -> Option<&ConnectionScope> {
        match self {
            Self::Snapshot(_) => None,
            Self::Notification(update) => Some(&update.scope),
            Self::Read { scope, .. } => Some(scope),
            Self::ReadFailure { scope, .. } => scope.as_ref(),
        }
    }
}

use studio_runtime_store::account_state::limits_observation_key as watermark_key;

// Fresh observations are durable even when their public content has not changed.
#[derive(serde::Serialize)]
struct ObservationReceipt<'a> {
    scope: &'a Value,
    cursor: u64,
    snapshot: &'a Value,
}

pub(super) async fn persist(
    store: Arc<studio_runtime_store::RuntimeStore>,
    cache: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Value>>>,
    account: String,
    write: LimitsWrite,
    wake: Option<Arc<Notify>>,
    session: Option<Arc<dyn RetainedAccountSession>>,
) -> Result<bool, AccountSessionError> {
    let now = timestamp()?;
    let cache_account = account.to_owned();
    let result = store
        .transaction_with_post_commit(
            move |tx| commit(tx, &account, write, now, session.as_deref()),
            move |(value, changed)| {
                if let Some(value) = value {
                    cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(cache_account, value.clone());
                }
                if *changed && let Some(wake) = wake {
                    wake.notify_one();
                }
            },
        )
        .await
        .map_err(|_| AccountSessionError::Persistence)?;
    Ok(result.0.1)
}

fn commit(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    write: LimitsWrite,
    now: f64,
    session: Option<&dyn RetainedAccountSession>,
) -> Result<(Option<Value>, bool), StoreError> {
    // Check at the writer boundary as the session may retire while this write queues.
    if let Some(scope) = write.scope()
        && session
            .as_ref()
            .and_then(|s| s.current_scope(account))
            .as_ref()
            != Some(scope)
    {
        if matches!(
            &write,
            LimitsWrite::Read { .. } | LimitsWrite::ReadFailure { .. }
        ) {
            return Err(StoreError::SchemaMismatch(
                "limits read belongs to a retired session".into(),
            ));
        }
        return Ok((None, false));
    }
    let key = watermark_key(account);
    let raw_stamp: Option<String> = tx
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [&key],
            |row| row.get(0),
        )
        .optional()?;
    let mut stamp: Value = raw_stamp
        .map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
        .unwrap_or(Value::Null);
    let old = match stamp.get_mut("snapshot") {
        Some(snapshot) => Some(snapshot.take()),
        None => {
            let raw: Option<String> = tx.query_row(
                "SELECT payload FROM sync_entities WHERE collection='rateLimit' AND id=?1 AND deleted=0",
                [account], |row| row.get(0),
            ).optional()?;
            raw.map(|raw| serde_json::from_str::<Value>(&raw))
                .transpose()
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
                .and_then(|mut entity| entity.get_mut("value").map(Value::take))
        }
    };
    let mut next_stamp = None;
    let value = match write {
        LimitsWrite::Snapshot(value) => value,
        LimitsWrite::ReadFailure { before, value, .. } => {
            if old.as_ref().is_some_and(|old| *old != before) {
                return Ok((old, false));
            }
            value
        }
        LimitsWrite::Read {
            scope,
            before,
            value,
        } => {
            // An event committed during this read owns all provider extensions,
            // including overage permission. Do not merge a stale read over it.
            if old.as_ref().is_some_and(|old| *old != before) {
                return Ok((old, false));
            }
            let cursor = if stamp["scope"] == serde_json::json!(scope) {
                stamp["cursor"].as_u64().unwrap_or(0)
            } else {
                0
            };
            next_stamp = Some(serde_json::json!({"scope":scope,"cursor":cursor}));
            value
        }
        LimitsWrite::Notification(update) => {
            let scope = serde_json::json!(update.scope);
            if stamp["scope"] == scope
                && stamp["cursor"]
                    .as_u64()
                    .is_some_and(|cursor| cursor >= update.cursor)
            {
                return Ok((old, false));
            }
            if stamp["scope"]["connection"] == scope["connection"]
                && stamp["scope"]["generation"]
                    .as_u64()
                    .is_some_and(|generation| generation > update.scope.generation)
            {
                return Ok((old, false));
            }
            next_stamp = Some(serde_json::json!({"scope":scope,"cursor":update.cursor}));
            // Old queued notifications cannot replace a more recent provider read.
            if old
                .as_ref()
                .and_then(|old| old["at"].as_f64())
                .is_some_and(|at| at > update.observed_at)
            {
                return Ok((old, false));
            }
            let mut value = old.clone().unwrap_or_else(|| serde_json::json!({}));
            let mut data = value
                .get("data")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let incoming = update.data.as_object().ok_or_else(|| {
                StoreError::SchemaMismatch("limits notification data must be an object".into())
            })?;
            let mut buckets = data
                .get("rateLimitsByLimitId")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(incoming_buckets) = incoming
                .get("rateLimitsByLimitId")
                .and_then(Value::as_object)
            {
                buckets.extend(incoming_buckets.clone());
            }
            if let Some(bucket) = incoming
                .get("rateLimits")
                .filter(|bucket| bucket.is_object())
            {
                let id = bucket["limitId"].as_str().unwrap_or("codex");
                buckets.insert(id.to_owned(), bucket.clone());
            }
            data.extend(incoming.clone());
            data.insert("rateLimitsByLimitId".into(), Value::Object(buckets));
            value["data"] = Value::Object(data);
            value["accountKey"] = Value::String(account.to_owned());
            value["at"] = serde_json::json!(update.observed_at);
            value["error"] = Value::Null;
            // A successful observation clears a previous read failure.
            if let Some(object) = value.as_object_mut() {
                object.remove("stale");
            }
            value
        }
    };
    let value = LimitsSnapshot::read(account, value)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
        .into_value();
    let content_changed = old.as_ref().is_none_or(|old| {
        old["data"] != value["data"]
            || old["error"] != value["error"]
            || old["stale"] != value["stale"]
    });
    let stamp = next_stamp.as_ref().unwrap_or(&stamp);
    let receipt = serde_json::to_string(&ObservationReceipt {
        scope: &stamp["scope"],
        cursor: stamp["cursor"].as_u64().unwrap_or(0),
        snapshot: &value,
    })?;
    tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'limits-observation',?2) ON CONFLICT(id) DO UPDATE SET result=excluded.result", rusqlite::params![key,receipt])?;
    if old.as_ref() == Some(&value) {
        return Ok((Some(value), false));
    }
    if content_changed {
        SyncDocumentsRepository::put_entity(tx, "rateLimit", account, value.clone(), false)?;
    }
    let previous_due = earliest_due(tx, account)?;
    crate::continuation_runtime::stage_limits_changed_for_account(tx, account, &value, now)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    let schedule_advanced = earliest_due(tx, account)?
        .is_some_and(|due| previous_due.is_none_or(|previous| due < previous));
    Ok((Some(value), content_changed || schedule_advanced))
}

fn earliest_due(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
) -> Result<Option<f64>, StoreError> {
    tx.query_row("SELECT MIN(CAST(json_extract(resume.record,'$.dueAt') AS REAL)) FROM runtime_usage_resumes AS resume JOIN runtime_agents AS owner ON owner.id=resume.agent AND json_extract(owner.record,'$.usageResume.id')=resume.id WHERE json_extract(resume.record,'$.status')='scheduled' AND json_extract(resume.record,'$.accountKey')=?1",[account],|row|row.get(0)).map_err(Into::into)
}

pub(super) async fn read_for_resume(
    runtime: &AccountSessionRuntime,
    account: &str,
    after: f64,
) -> Result<Value, AccountSessionError> {
    if let (Some(scope), Some(store)) = (runtime.current_scope(account), runtime.store.as_ref()) {
        let key = account.to_owned();
        let expected = scope.clone();
        let candidate = store
            .read_async("account-session/resume-limits", move |db| {
                let stamp: Option<String> = db
                    .query_row(
                        "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                        [watermark_key(&key)],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(stamp) = stamp else {
                    return Ok(None);
                };
                let stamp: Value = serde_json::from_str(&stamp)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                if stamp["scope"] != serde_json::json!(expected) {
                    return Ok(None);
                }
                let candidate = stamp.get("snapshot").cloned();
                Ok::<_, StoreError>(candidate)
            })
            .await
            .map_err(|_| AccountSessionError::Persistence)?;
        let now = timestamp()?;
        if let Some(value) = candidate
            && runtime.current_scope(account).as_ref() == Some(&scope)
            && crate::usage_resume::LimitsObservation::from_record(value.clone(), None).is_ok_and(
                |observation| {
                    observation.account_key() == account && observation.fresh_after(after, now)
                },
            )
        {
            return Ok(value);
        }
    }
    runtime.read_limits(account).await
}
