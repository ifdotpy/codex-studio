//! Atomic work-item writes that need no worker or filesystem driver.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Connection, EventWriteRepository,
    ReviewerArchiveRepository, RuntimeStore, Transaction, WorkItemsRepository,
};

use crate::event_write::{self, EventWriteError as Error};
use thiserror::Error;

static GENERATED_WORK_ID: AtomicU64 = AtomicU64::new(0);
/// Maximum accepted archive receipts handled by one scheduler pass.
pub const ACCEPTED_ARCHIVE_PAGE_SIZE: usize = 16;
const ACCEPTED_ARCHIVE_BASE_DELAY_SECONDS: u64 = 2;
const ACCEPTED_ARCHIVE_MAX_DELAY_SECONDS: u64 = 60;
const ACCEPTED_ARCHIVE_MAX_BACKOFF_EXPONENT: u64 = 6;
// Reviewer archive retry policy; delays and persisted deadlines use seconds.
const REVIEW_ARCHIVE_BASE_DELAY_SECONDS: f64 = 1.0;
const REVIEW_ARCHIVE_MAX_DELAY_SECONDS: f64 = 60.0;
// Capped exponent bounds the seconds-based doubling before the 60-second cap.
const REVIEW_ARCHIVE_MAX_BACKOFF_EXPONENT: u32 = 6;

/// Check whether a submitted commit is on local main without contacting a remote.
/// Repositories with an origin are kept for the separately scheduled remote check.
pub fn accepted_revision_is_on_local_main(repository: &str, revision: &str) -> bool {
    use std::ffi::OsStr;
    use std::time::Duration;

    if !(7..=64).contains(&revision.len()) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    let run_git = |args: &[&str]| {
        crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
            program: OsStr::new("git"),
            cwd: OsStr::new(repository),
            args,
            environment: &[],
            input: None,
            stdout_limit: 256,
            stderr_limit: 256,
            timeout: Duration::from_secs(2),
        })
        .ok()
        .map(|output| output.stdout.bytes)
    };
    let submitted_ref = format!("{revision}^{{commit}}");
    let Some(submitted) = run_git(&["rev-parse", "--verify", "--end-of-options", &submitted_ref])
    else {
        return false;
    };
    let submitted = String::from_utf8_lossy(&submitted).trim().to_owned();
    let Some(remotes) = run_git(&["remote"]) else {
        return false;
    };
    if String::from_utf8_lossy(&remotes)
        .lines()
        .any(|remote| remote == "origin")
    {
        return false;
    }
    let Some(main) = run_git(&["rev-parse", "--verify", "refs/heads/main^{commit}"]) else {
        return false;
    };
    let main = String::from_utf8_lossy(&main).trim().to_owned();
    run_git(&["merge-base", "--is-ancestor", &submitted, &main]).is_some()
}

async fn read_in_blocking<T, F>(store: Arc<RuntimeStore>, read: F) -> Result<T, WorkWriteError>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, WorkWriteError> + Send + 'static,
{
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/work_write",
        move || {
            let connection = store.read()?;
            read(&connection)
        },
    )
    .await
    .map_err(|_| WorkWriteError::PreparationWorker)?
}

/// Identify repositories whose accepted-result main check needs remote transport.
pub fn repository_has_origin(repository: &str) -> bool {
    use std::ffi::OsStr;
    use std::time::Duration;

    crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: OsStr::new("git"),
        cwd: OsStr::new(repository),
        args: &["remote"],
        environment: &[],
        input: None,
        stdout_limit: 256,
        stderr_limit: 256,
        timeout: Duration::from_secs(2),
    })
    .ok()
    .is_some_and(|output| {
        String::from_utf8_lossy(&output.stdout.bytes)
            .lines()
            .any(|remote| remote == "origin")
    })
}

/// Retry accepted-result archives whose Python-compatible retry deadline has
/// elapsed. `now` is injected so scheduling and exponential backoff are
/// deterministic in tests. Pending/unknown receipts are reconciled against the
/// target before any retry; a known retryable response may be retried with the
/// same intent identity and target.
pub async fn accepted_archive_tick(
    store: Arc<RuntimeStore>,
    now: f64,
) -> Result<usize, WorkWriteError> {
    reviewer_archive_tick(Arc::clone(&store), now).await?;
    let due = read_in_blocking(Arc::clone(&store), move |connection| {
        accepted_archive_due_records(connection, now)
    })
    .await?;
    let mut handled = 0;
    for (task_id, encoded_intent) in due {
        let intent: Value = serde_json::from_str(&encoded_intent)
            .map_err(studio_runtime_store::StoreError::from)?;
        let task_id_for_read = task_id.clone();
        let Some(encoded_work) = read_in_blocking(Arc::clone(&store), move |connection| {
            Ok(WorkItemsRepository::work_record(
                connection,
                &task_id_for_read,
            )?)
        })
        .await?
        else {
            continue;
        };
        let work = encoded_work;
        if work["status"] != "accepted" {
            continue;
        }
        let intent_id = intent["intentId"].as_str().unwrap_or_default().to_owned();
        let owner_id = work["owner"].as_str().unwrap_or_default().to_owned();
        let root_id = work["rootId"].as_str().unwrap_or_default().to_owned();
        if intent_id.is_empty() || owner_id.is_empty() || root_id.is_empty() {
            continue;
        }
        let receipt_id = format!("work-archive:{intent_id}");
        let intent_id_for_claim = intent_id.clone();
        let receipt_id_for_claim = receipt_id.clone();
        let task_id_for_claim = task_id.clone();
        let intent_for_claim = intent.clone();
        let owner_for_claim = owner_id.clone();
        let root_for_claim = root_id.clone();
        let (should_dispatch, dispatch_actor_epoch, prior_status) =
            store.transaction_with_error(move |tx| -> Result<(bool, i64, String), WorkWriteError> {
                let mut dispatch_actor_epoch = 0_i64;
                let mut prior_status = String::new();
                let current = WorkItemsRepository::work_record(tx, &task_id_for_claim)?
                    .ok_or(Error::InvalidRequest("Unknown work item"))?;
                if current["status"] != "accepted"
                    || current["owner"] != owner_for_claim
                    || current["rootId"] != root_for_claim
                    || !matches!(
                        intent_for_claim["status"].as_str(),
                        Some("pending" | "retryable" | "unknown")
                    )
                    || intent_for_claim["nextAttemptAt"].as_f64().unwrap_or(0.0) > now
                {
                    return Ok((false, 0, String::new()));
                }
                let receipt = tx
                    .query_row(
                        "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                        [&receipt_id_for_claim],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(studio_runtime_store::StoreError::from)?;
                let mut receipt = if let Some(receipt) = receipt {
                    let receipt: Value = serde_json::from_str(&receipt)
                        .map_err(studio_runtime_store::StoreError::from)?;
                    if !matches!(
                        receipt["status"].as_str(),
                        Some("pending" | "retryable" | "unknown")
                    ) || receipt["nextAttemptAt"].as_f64().unwrap_or(0.0) > now
                    {
                        return Ok((false, 0, String::new()));
                    }
                    prior_status = receipt["status"].as_str().unwrap_or_default().to_owned();
                    dispatch_actor_epoch = receipt["actorEpoch"].as_i64().unwrap_or_default();
                    receipt
                } else {
                    json!({"status":"pending","taskId":task_id_for_claim,"intentId":intent_id_for_claim,
                        "actor":root_for_claim,"target":owner_for_claim,"attempts":0,"created":now})
                };
                let actor_epoch = if dispatch_actor_epoch == 0 {
                    AgentRepository::get(tx, &AgentId::from(root_for_claim.as_str()))?
                        .map(|actor| actor.epoch)
                        .unwrap_or_default()
                } else {
                    dispatch_actor_epoch
                };
                dispatch_actor_epoch = actor_epoch;
                receipt["status"] = json!("unknown");
                receipt["actorEpoch"] = json!(actor_epoch);
                receipt["attemptStartedAt"] = json!(now);
                tx.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3) \
                 ON CONFLICT(id) DO UPDATE SET result=excluded.result",
                    rusqlite::params![receipt_id_for_claim, "accepted-archive-v1", receipt.to_string()],
                )
                .map_err(studio_runtime_store::StoreError::from)?;
                Ok((true, dispatch_actor_epoch, prior_status))
            }).await?;
        if !should_dispatch {
            continue;
        }

        let task_for_preflight = task_id.clone();
        let work_for_preflight = work.clone();
        let intent_for_preflight = intent.clone();
        let owner_for_preflight = owner_id.clone();
        let root_for_preflight = root_id.clone();
        let preflight = read_in_blocking(Arc::clone(&store), move |connection| {
            accepted_archive_preflight(
                connection,
                &task_for_preflight,
                &work_for_preflight,
                &intent_for_preflight,
                &owner_for_preflight,
                &root_for_preflight,
                dispatch_actor_epoch,
            )
        })
        .await?;
        let outcome = match preflight {
            Some(value) => Ok(value),
            None if prior_status == "unknown" => Ok(json!({
                "status":"kept",
                "reason":"The accepted archive outcome is unconfirmed",
            })),
            None => {
                crate::agent_management_store::execute(
                    Arc::clone(&store),
                    &crate::agent_management::Action::Archive {
                        actor: root_id.clone(),
                        epoch: dispatch_actor_epoch,
                        target: owner_id.clone(),
                        reason: "Accepted task result is on main".to_owned(),
                        unassign_work: false,
                    },
                    now,
                )
                .await
            }
        };
        let (receipt_status, archive, retryable) = match outcome {
            Ok(value) => {
                let retryable = value["retryable"] == true
                    || value["status"] == "blocked"
                    || value["cleanupPending"] == true;
                let receipt_status = if retryable {
                    "retryable"
                } else if value["status"] == "archived" {
                    "complete"
                } else {
                    "terminal"
                };
                (receipt_status, value, retryable)
            }
            Err(error) => (
                "unknown",
                json!({"status":"kept","reason":format!("The archive check failed: {error}")}),
                false,
            ),
        };
        let task_id_for_settlement = task_id.clone();
        let intent_for_settlement = intent.clone();
        let intent_id_for_settlement = intent_id.clone();
        let owner_for_settlement = owner_id.clone();
        let root_for_settlement = root_id.clone();
        let receipt_for_settlement = receipt_id.clone();
        store.transaction_with_error(move |tx| -> Result<(), WorkWriteError> {
            let Some(mut current) = WorkItemsRepository::work_record(tx, &task_id_for_settlement)? else {
                return Ok(());
            };
            if current["status"] != "accepted"
                || current["owner"] != owner_for_settlement
                || current["rootId"] != root_for_settlement
            {
                return Ok(());
            }
            let attempts = intent_for_settlement["attempts"].as_u64().unwrap_or_default();
            let next_attempt = attempts + 1;
            let delay = if retryable {
                ACCEPTED_ARCHIVE_BASE_DELAY_SECONDS
                    .pow(next_attempt.min(ACCEPTED_ARCHIVE_MAX_BACKOFF_EXPONENT) as u32)
                    .min(ACCEPTED_ARCHIVE_MAX_DELAY_SECONDS)
            } else {
                ACCEPTED_ARCHIVE_MAX_DELAY_SECONDS
            };
            if receipt_status != "unknown" {
                current["archive"] = archive.clone();
            }
            if retryable {
                let mut row_intent = current.get("archiveIntent").cloned().unwrap_or_else(|| {
                    json!({
                        "id":intent_id_for_settlement,"status":"pending","owner":owner_for_settlement,"attempts":0,
                        "created":intent_for_settlement.get("created").and_then(Value::as_f64).unwrap_or(now),
                    })
                });
                row_intent["status"] = json!("pending");
                row_intent["attempts"] = json!(next_attempt);
                row_intent["nextAttemptAt"] = json!(now + delay as f64);
                row_intent["lastOutcome"] = archive.clone();
                row_intent["updated"] = json!(now);
                current["archiveIntent"] = row_intent;
            } else if receipt_status == "unknown" {
                if let Some(mut row_intent) = current.get("archiveIntent").cloned() {
                    row_intent["nextAttemptAt"] =
                        json!(now + ACCEPTED_ARCHIVE_MAX_DELAY_SECONDS as f64);
                    current["archiveIntent"] = row_intent;
                }
            } else if let Some(mut row_intent) = current.get("archiveIntent").cloned() {
                row_intent["status"] = json!(if receipt_status == "complete" {
                    "complete"
                } else {
                    "terminal"
                });
                row_intent["updated"] = json!(now);
                row_intent["outcome"] = archive.clone();
                current["archiveIntent"] = row_intent;
            }
            WorkItemsRepository::update_work(tx, &task_id_for_settlement, &current)?;
            let mut final_intent = intent_for_settlement.clone();
            final_intent["status"] = json!(if retryable { "pending" } else { receipt_status });
            final_intent["attempts"] = json!(next_attempt);
            final_intent["nextAttemptAt"] = json!(now + delay as f64);
            final_intent["lastOutcome"] = archive.clone();
            final_intent["updated"] = json!(now);
            final_intent["actorEpoch"] = json!(dispatch_actor_epoch);
            if !retryable {
                final_intent["outcome"] = archive.clone();
            }
            tx.execute(
                "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
                rusqlite::params![final_intent.to_string(), receipt_for_settlement],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            Ok(())
        }).await?;
        handled += 1;
    }
    Ok(handled)
}

/// Schedule and retry archiving completed reviewers after their child result
/// has been delivered. The event ID stays in the agent record as the durable
/// identity for every retry; scheduling commits before the archive operation.
async fn reviewer_archive_tick(store: Arc<RuntimeStore>, now: f64) -> Result<(), WorkWriteError> {
    store
        .transaction_with_error(move |tx| -> Result<(), WorkWriteError> {
            let candidates = ReviewerArchiveRepository::candidates(tx)?;
            for (child_id, encoded) in candidates {
                let mut record: Value = serde_json::from_str(&encoded)
                    .map_err(studio_runtime_store::StoreError::from)?;
                if event_write::truthy(record.get("agentArchive"))
                    || event_write::truthy(record.get("reviewArchiveError"))
                    || record
                        .get("reviewArchiveScheduled")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty())
                {
                    continue;
                }
                let Some(parent_id) = record.get("parentId").and_then(Value::as_str) else {
                    continue;
                };
                let event_id =
                    ReviewerArchiveRepository::child_result_id(tx, parent_id, &child_id)?;
                let Some(event_id) = event_id else {
                    continue;
                };
                record["reviewArchiveScheduled"] = json!(event_id);
                let child = AgentRecord::from_value(record).map_err(|_| {
                    studio_runtime_store::StoreError::InvalidStoredObject {
                        table: "runtime_agents",
                        id: child_id.to_owned(),
                    }
                })?;
                AgentRepository::put(tx, &child)?;
            }
            Ok(())
        })
        .await?;

    let due = read_in_blocking(Arc::clone(&store), move |connection| {
        Ok(ReviewerArchiveRepository::due_retries(connection, now)?)
    })
    .await?;

    for (child_id, encoded) in due {
        let record: Value =
            serde_json::from_str(&encoded).map_err(studio_runtime_store::StoreError::from)?;
        let Some(event_id) = record.get("reviewArchiveScheduled").and_then(Value::as_str) else {
            continue;
        };
        let Some(parent_id) = record.get("parentId").and_then(Value::as_str) else {
            continue;
        };
        let Some(root_id) = record.get("rootId").and_then(Value::as_str) else {
            continue;
        };
        if record.get("role").and_then(Value::as_str) != Some("reviewer")
            || record.get("status").and_then(Value::as_str) != Some("completed")
            || event_write::truthy(record.get("agentArchive"))
        {
            continue;
        }
        let event_id_for_read = event_id.to_owned();
        let parent_id_for_read = parent_id.to_owned();
        let child_id_for_read = child_id.clone();
        let event_matches = read_in_blocking(Arc::clone(&store), move |connection| {
            let event: Option<(String, String)> = connection
                .query_row(
                    "SELECT agent,text FROM runtime_events \
                     WHERE id=?1 AND kind='child_result' AND status='delivered'",
                    [event_id_for_read],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(studio_runtime_store::StoreError::from)?;
            Ok(event.is_some_and(|(event_parent, text)| {
                event_parent == parent_id_for_read
                    && serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|payload| {
                            payload
                                .get("agent_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .as_deref()
                        == Some(child_id_for_read.as_str())
            }))
        })
        .await?;
        if !event_matches {
            continue;
        }

        let actor_epoch = {
            let connection = store.read()?;
            AgentRepository::get(&connection, &AgentId::from(root_id))?
                .map(|agent| agent.epoch)
                .unwrap_or_default()
        };
        let outcome = crate::agent_management_store::execute(
            Arc::clone(&store),
            &crate::agent_management::Action::Archive {
                actor: root_id.to_owned(),
                epoch: actor_epoch,
                target: child_id.clone(),
                reason: "Reviewer result delivered to parent".to_owned(),
                unassign_work: false,
            },
            now,
        )
        .await;
        let (error, retryable) = match outcome {
            Ok(value) if value["status"] == "archived" => continue,
            Ok(value) => {
                let blockers = value.get("blockers").and_then(Value::as_array);
                let retryable = blockers.is_some_and(|items| !items.is_empty());
                let error = if retryable {
                    Some(value["blockers"].to_string())
                } else {
                    Some(
                        value
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("Archive was not completed")
                            .to_owned(),
                    )
                };
                (error, retryable)
            }
            Err(error) => (Some(error.to_string()), false),
        };
        let Some(error) = error else { continue };
        let child_for_settlement = child_id.clone();
        let event_for_settlement = event_id.to_owned();
        store
            .transaction_with_error(move |tx| -> Result<(), WorkWriteError> {
                let Some(mut child) =
                    AgentRepository::get(tx, &AgentId::from(child_for_settlement.as_str()))?
                else {
                    return Ok(());
                };
                let mut record = child.value();
                if record.get("reviewArchiveScheduled").and_then(Value::as_str)
                    != Some(event_for_settlement.as_str())
                {
                    return Ok(());
                }
                record["reviewArchiveError"] = json!(error.chars().take(180).collect::<String>());
                if retryable {
                    let attempts = record
                        .get("reviewArchiveAttempts")
                        .and_then(Value::as_u64)
                        .unwrap_or_default()
                        .saturating_add(1);
                    let exponent = attempts
                        .saturating_sub(1)
                        .min(u64::from(REVIEW_ARCHIVE_MAX_BACKOFF_EXPONENT));
                    let delay = (REVIEW_ARCHIVE_BASE_DELAY_SECONDS * 2_f64.powi(exponent as i32))
                        .min(REVIEW_ARCHIVE_MAX_DELAY_SECONDS);
                    record["reviewArchiveAttempts"] = json!(attempts);
                    record["reviewArchiveNextAt"] = json!(now + delay);
                } else {
                    record
                        .as_object_mut()
                        .map(|value| value.remove("reviewArchiveScheduled"));
                }
                child = AgentRecord::from_value(record).map_err(|_| {
                    studio_runtime_store::StoreError::InvalidStoredObject {
                        table: "runtime_agents",
                        id: child_for_settlement.clone(),
                    }
                })?;
                AgentRepository::put(tx, &child)?;
                Ok(())
            })
            .await?;
    }
    Ok(())
}

/// Find the next durable archive retry deadline. New reviewer candidates are
/// discovered on startup and after commits; only persisted retries need a timer.
pub fn next_archive_retry_at(
    store: &RuntimeStore,
    now: f64,
    accepted_batch_full: bool,
) -> Result<Option<f64>, WorkWriteError> {
    if accepted_batch_full {
        return Ok(Some(now));
    }
    Ok(store.read_transaction(|connection| {
        let accepted: Option<f64> = connection.query_row(
            "SELECT MIN(CAST(json_extract(result,'$.nextAttemptAt') AS REAL)) \
             FROM runtime_operation_receipts \
             WHERE signature='accepted-archive-v1' \
               AND json_extract(result,'$.status') IN ('pending','retryable','unknown') \
               AND json_type(result,'$.nextAttemptAt') IN ('integer','real') \
               AND CAST(json_extract(result,'$.nextAttemptAt') AS REAL)>?1",
            [now],
            |row| row.get(0),
        )?;
        let reviewer = ReviewerArchiveRepository::next_retry_at(connection, now)?;
        Ok(accepted.into_iter().chain(reviewer).reduce(f64::min))
    })?)
}

/// Persist uncertainty before the synchronous accept route attempts an archive
/// effect. A later tick reconciles the target before replaying the same tuple.
pub async fn begin_accepted_archive_attempt(
    store: &RuntimeStore,
    task_id: &str,
    now: f64,
) -> Result<(), WorkWriteError> {
    let task_id = task_id.to_owned();
    store.transaction_with_error(move |tx| -> Result<(), WorkWriteError> {
        let work = WorkItemsRepository::work_record(tx, &task_id)?
            .ok_or(Error::InvalidRequest("Unknown work item"))?;
        if work["status"] != "accepted" {
            return Err(Error::InvalidRequest("Work item is not accepted").into());
        }
        let result_id = work["decisions"]
            .as_array()
            .and_then(|items| items.last())
            .and_then(|decision| decision["resultId"].as_str())
            .ok_or(Error::InvalidRequest("Accepted result is missing"))?;
        let receipt_id = format!("work-archive:{task_id}:{result_id}");
        let raw = tx
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1 AND signature='accepted-archive-v1'",
                [&receipt_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        let mut intent: Value = serde_json::from_str(&raw)
            .map_err(studio_runtime_store::StoreError::from)?;
        if !matches!(intent["status"].as_str(), Some("pending" | "retryable")) {
            return Err(Error::InvalidRequest("Archive attempt is not dispatchable").into());
        }
        intent["status"] = json!("unknown");
        intent["attemptStartedAt"] = json!(now);
        intent["nextAttemptAt"] = json!(now + ACCEPTED_ARCHIVE_MAX_DELAY_SECONDS as f64);
        tx.execute(
            "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2",
            rusqlite::params![intent.to_string(), receipt_id],
        )
        .map_err(studio_runtime_store::StoreError::from)?;
        Ok(())
    }).await?;
    Ok(())
}

/// Reconcile the target and accepted result before replaying a lifecycle archive.
/// A canonical archive marker proves the prior effect committed; cleanupPending
/// or a conflicting marker is held without repeating cleanup effects.
fn accepted_archive_preflight(
    connection: &Connection,
    task_id: &str,
    work: &Value,
    intent: &Value,
    owner_id: &str,
    root_id: &str,
    actor_epoch: i64,
) -> Result<Option<Value>, WorkWriteError> {
    let Some(actor) = AgentRepository::get(connection, &AgentId::from(root_id))? else {
        return Ok(Some(
            json!({"status":"kept","reason":"The archive actor changed"}),
        ));
    };
    if actor.epoch != actor_epoch
        || actor
            .extra
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
    {
        return Ok(Some(
            json!({"status":"kept","reason":"The archive actor changed"}),
        ));
    }
    let Some(owner) = AgentRepository::get(connection, &AgentId::from(owner_id))? else {
        return Ok(Some(
            json!({"status":"kept","reason":"The accepted task owner changed"}),
        ));
    };
    let owner_value = owner.value();
    if owner_value
        .get("remoteWorker")
        .is_some_and(|value| event_write::truthy(Some(value)))
    {
        return Ok(Some(
            json!({"status":"kept","reason":"The remote worker workspace requires review on its own server"}),
        ));
    }
    let Some(result) = work["results"].as_array().and_then(|items| items.last()) else {
        return Ok(Some(
            json!({"status":"kept","reason":"The accepted task result is missing"}),
        ));
    };
    let Some(result_id) = result["id"].as_str() else {
        return Ok(Some(
            json!({"status":"kept","reason":"The accepted task result is missing"}),
        ));
    };
    if intent["intentId"] != format!("{task_id}:{result_id}")
        || work["owner"] != owner_id
        || work["rootId"] != root_id
    {
        return Ok(Some(
            json!({"status":"kept","reason":"The accepted task owner or result changed"}),
        ));
    }

    let archive_marker = owner_value.get("agentArchive").unwrap_or(&Value::Null);
    if !archive_marker.is_null() {
        let canonical = archive_marker["by"] == root_id
            && archive_marker["reason"] == "Accepted task result is on main"
            && archive_marker["epoch"] == owner.epoch
            && archive_marker.get("at") == owner_value.get("deletedAt")
            && archive_marker["at"].as_f64().unwrap_or_default()
                >= intent["created"].as_f64().unwrap_or_default()
            && archive_marker["cleanupPending"] == false
            && !event_write::truthy(archive_marker.get("cleanupPending"));
        if !canonical {
            return Ok(Some(json!({
                "status":"kept",
                "reason":"The accepted archive outcome is unconfirmed",
                "cleanupPending":event_write::truthy(archive_marker.get("cleanupPending")),
            })));
        }
        let cleaned = owner_value
            .get("cleanedWorktree")
            .filter(|value| !value.is_null());
        let worktree = if cleaned.is_some_and(|value| event_write::truthy(value.get("missing"))) {
            json!({"state":"missing","reason":cleaned.and_then(|v| v.get("note")),"bytes":0})
        } else if let Some(cleaned) = cleaned {
            json!({"state":"removed","bytes":cleaned.get("bytes")})
        } else if event_write::truthy(owner_value.get("worktreeReady")) {
            json!({"state":"kept","reason":"The archived worktree remains","bytes":0})
        } else {
            json!({"state":"none","bytes":0})
        };
        return Ok(Some(json!({"status":"archived","worktree":worktree})));
    }

    let mut statement = connection
        .prepare(
            "SELECT id FROM runtime_work WHERE json_extract(record,'$.owner')=?1 \
             AND id<>?2 AND json_extract(record,'$.status') NOT IN ('accepted','cancelled') \
             ORDER BY id LIMIT 20",
        )
        .map_err(studio_runtime_store::StoreError::from)?;
    let open_tasks = statement
        .query_map([owner_id, task_id], |row| row.get::<_, String>(0))
        .map_err(studio_runtime_store::StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(studio_runtime_store::StoreError::from)?;
    if !open_tasks.is_empty() {
        return Ok(Some(
            json!({"status":"kept","reason":"The owner has another open task",
            "tasks":open_tasks,"retryable":true}),
        ));
    }

    if owner_value.get("executionMode").and_then(Value::as_str) == Some("vm") {
        if !vm_merge_confirmed(
            connection,
            task_id,
            result_id,
            result["revision"].as_str().unwrap_or_default(),
        )? {
            return Ok(Some(
                json!({"status":"kept","reason":"The layr result merge is not confirmed"}),
            ));
        }
    } else {
        let revision = result["revision"].as_str().unwrap_or_default();
        let cwd = owner_value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if cwd.is_empty() || !accepted_revision_is_on_local_main(cwd, revision) {
            let remote = !cwd.is_empty() && repository_has_origin(cwd);
            let reason = if remote {
                "Remote-origin main verification requires cross-server/remote-transport"
            } else {
                "The result commit is not reachable from local main"
            };
            return Ok(Some(json!({"status":"kept","reason":reason})));
        }
    }
    Ok(None)
}

fn accepted_archive_due_records(
    connection: &Connection,
    now: f64,
) -> Result<Vec<(String, String)>, WorkWriteError> {
    let mut statement = connection
        .prepare(
            "SELECT json_extract(result,'$.taskId'),result FROM runtime_operation_receipts \
         WHERE signature='accepted-archive-v1' \
           AND json_extract(result,'$.status') IN ('pending','retryable','unknown') \
           AND COALESCE(json_extract(result,'$.nextAttemptAt'),0)<=?1 \
         ORDER BY COALESCE(json_extract(result,'$.nextAttemptAt'),0),id LIMIT ?2",
        )
        .map_err(studio_runtime_store::StoreError::from)?;
    Ok(statement
        .query_map(
            rusqlite::params![now, ACCEPTED_ARCHIVE_PAGE_SIZE as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(studio_runtime_store::StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(studio_runtime_store::StoreError::from)?)
}

pub struct CreateWork {
    pub actor_id: String,
    pub title: String,
    pub description: String,
    pub owner: Option<String>,
    pub status: Option<String>,
    pub dependencies: Vec<String>,
}

pub enum WorkActorWrite {
    Create(CreateWork),
    Mutate(WorkMutation),
}

pub enum WorkMutation {
    Update {
        task_id: String,
        version: Option<i64>,
        title: Option<String>,
        description: Option<String>,
        owner: Option<Option<String>>,
        status: Option<String>,
        dependencies: Option<Vec<String>>,
    },
    Reject {
        task_id: String,
        version: Option<i64>,
        reason: String,
    },
    Cancel {
        task_id: String,
        version: Option<i64>,
        reason: String,
    },
}

#[derive(Clone)]
pub struct SubmitWork {
    pub task_id: String,
    pub version: Option<i64>,
    pub result: String,
    pub checks: String,
    pub revision: String,
    pub files: Vec<String>,
    pub state_dir: PathBuf,
    pub vm_evidence: Option<Value>,
    pub vm_evidence_request_id: Option<String>,
}

pub struct VmSubmissionCall {
    pub owner_id: String,
    pub revision: String,
    pub request_id: String,
}

fn vm_intent_id(kind: &str, task_id: &str) -> String {
    format!("work-write-vm-intent:{kind}:{task_id}")
}

fn read_vm_intent(
    connection: &Connection,
    kind: &str,
    task_id: &str,
) -> Result<Option<Value>, WorkWriteError> {
    let encoded = connection
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [vm_intent_id(kind, task_id)],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(studio_runtime_store::StoreError::from)?;
    let decoded = encoded
        .map(|value| serde_json::from_str(&value).map_err(studio_runtime_store::StoreError::from))
        .transpose()?;
    Ok(decoded)
}

fn write_vm_intent(
    tx: &Transaction<'_>,
    kind: &str,
    task_id: &str,
    intent: &Value,
) -> Result<(), WorkWriteError> {
    tx.execute(
        "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3) \
         ON CONFLICT(id) DO UPDATE SET signature=excluded.signature,result=excluded.result",
        rusqlite::params![
            vm_intent_id(kind, task_id),
            "work-write-vm-intent-v1",
            intent.to_string()
        ],
    )
    .map_err(studio_runtime_store::StoreError::from)?;
    Ok(())
}

pub enum VmSubmissionPreparation {
    NotVm,
    Replay,
    Evidence { evidence: Value, request_id: String },
    Call(VmSubmissionCall),
}

/// Persist an exact layr evidence request before contacting the guest. A pending
/// or unknown request is never sent again; completed evidence can be replayed.
pub async fn prepare_vm_submission(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: Option<&str>,
    signature: &str,
    input: &SubmitWork,
) -> Result<VmSubmissionPreparation, WorkWriteError> {
    let agent_id = agent_id.to_owned();
    let request_id = request_id.map(str::to_owned);
    let signature = signature.to_owned();
    let input = input.clone();
    store.transaction_with_error(move |tx| {
        let agent = event_write::checked_agent(tx, &agent_id)?;
        if event_write::read_receipt(
            tx,
            request_id.as_deref(),
            &signature,
            "runtime_work",
            request_id.as_deref().unwrap_or(&agent_id),
            |value| {
                value
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| WorkItemsRepository::validate_work(tx, id, value).is_ok())
            },
        )?
        .is_some()
        {
            return Ok(VmSubmissionPreparation::Replay);
        }
        let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
        if !matches!(work["status"].as_str(), Some("running" | "ready" | "blocked" | "review")) {
            return Err(Error::InvalidRequest("This result is already accepted").into());
        }
        if !blocked_by(&work, &dependency_statuses(tx, &work)?).is_empty() {
            return Err(Error::InvalidRequest("Dependencies have not been accepted").into());
        }
        text(&input.result, false)?;
        text(&input.checks, false)?;
        let revision = python_bounded_text(&input.revision, 200)?;
        if input.files.len() > 50 || input.files.iter().any(|path| path.chars().count() > 4096) {
            return Err(Error::InvalidRequest("Supply up to 50 file paths").into());
        }
        let owner_id = work["owner"].as_str().unwrap_or(&agent.id.0);
        let owner = event_write::checked_agent(tx, owner_id)?;
        validate_submission_files(&owner, &input.files)?;
        if owner.extra.get("executionMode").and_then(Value::as_str) != Some("vm") {
            return Ok(VmSubmissionPreparation::NotVm);
        }
        let rpc_id = format!("layr-evidence:{owner_id}:{revision}");
        if let Some(intent) = read_vm_intent(tx, "submission", &input.task_id)? {
            match intent.get("status").and_then(Value::as_str) {
                Some("pending" | "unknown") => {
                    return Err(Error::Conflict(
                        "A prior layr evidence request has an unknown outcome; inspect it before retrying",
                    )
                    .into());
                }
                _ if intent.get("requestId").and_then(Value::as_str)
                    == Some(rpc_id.as_str()) =>
                {
                    if intent.get("status").and_then(Value::as_str) == Some("completed")
                        && intent.get("owner").and_then(Value::as_str) == Some(owner_id)
                        && intent.get("version") == Some(&work["version"])
                        && intent.get("revision").and_then(Value::as_str)
                            == Some(revision.as_str())
                        && let Some(evidence) = intent.get("evidence")
                    {
                        return Ok(VmSubmissionPreparation::Evidence {
                            evidence: evidence.clone(),
                            request_id: rpc_id.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        let intent = json!({
            "status":"pending",
            "requestId":rpc_id,
            "taskId":input.task_id,
            "owner":owner_id,
            "revision":revision,
            "version":work["version"],
            "created":event_write::unix_time()?,
        });
        write_vm_intent(tx, "submission", &input.task_id, &intent)?;
        Ok(VmSubmissionPreparation::Call(VmSubmissionCall {
            owner_id: owner_id.to_owned(),
            revision,
            request_id: rpc_id,
        }))
    }).await
}

/// Record the guest's terminal response before the normal work submission write.
/// A lost response is persisted as unknown and can never trigger an automatic resend.
pub async fn finish_vm_submission(
    store: &RuntimeStore,
    task_id: &str,
    request_id: &str,
    outcome: Result<Value, bool>,
) -> Result<Option<Value>, WorkWriteError> {
    let task_id = task_id.to_owned();
    let request_id = request_id.to_owned();
    store
        .transaction_with_error(move |tx| {
            WorkItemsRepository::work_record(tx, &task_id)?
                .ok_or(Error::InvalidRequest("Unknown work item"))?;
            let mut intent = read_vm_intent(tx, "submission", &task_id)?
                .filter(|value| {
                    value.get("requestId").and_then(Value::as_str) == Some(request_id.as_str())
                })
                .ok_or(Error::Conflict("The layr evidence intent changed"))?;
            match outcome {
                Ok(evidence) => {
                    intent["status"] = json!("completed");
                    intent["evidence"] = evidence.clone();
                    write_vm_intent(tx, "submission", &task_id, &intent)?;
                    Ok(Some(evidence))
                }
                Err(uncertain) => {
                    intent["status"] = json!(if uncertain { "unknown" } else { "not_sent" });
                    write_vm_intent(tx, "submission", &task_id, &intent)?;
                    Ok(None)
                }
            }
        })
        .await
}

pub struct ClaimWork {
    pub task_id: String,
    pub version: Option<i64>,
    pub claimant_id: String,
}

pub struct AcceptWork {
    pub task_id: String,
    pub version: Option<i64>,
    pub reason: String,
    pub merged_result_id: Option<String>,
}

pub struct VmMergeCall {
    pub lead_id: String,
    pub owner_id: String,
    pub result_id: String,
    pub revision: String,
    pub request_id: String,
}

pub enum VmAcceptPreparation {
    NotVm,
    Merged(String),
    Call(VmMergeCall),
}

/// Persist a merge intent before calling the VM guest. Pending or unknown
/// identities are never resent; a completed exact merge can be replayed.
pub async fn prepare_vm_accept(
    store: &RuntimeStore,
    agent_id: &str,
    task_id: &str,
    version: Option<i64>,
    request_id: Option<&str>,
    signature: &str,
    reason: &str,
) -> Result<VmAcceptPreparation, WorkWriteError> {
    let agent_id = agent_id.to_owned();
    let task_id = task_id.to_owned();
    let request_id = request_id.map(str::to_owned);
    let signature = signature.to_owned();
    let reason = reason.to_owned();
    store
        .transaction_with_error(move |tx| {
            let lead = event_write::checked_agent(tx, &agent_id)?;
            if lead.extra.get("executionMode").and_then(Value::as_str) != Some("vm") {
                return Ok(VmAcceptPreparation::NotVm);
            }
            if lead.extra.get("isLead") != Some(&Value::Bool(true)) || lead.root_id.0 != agent_id {
                return Err(
                    Error::InvalidRequest("Only the active lead can accept a layr result").into(),
                );
            }
            let work = load_work(tx, &lead.root_id.0, &task_id, version)?;
            if work["status"] != "review" {
                return Ok(VmAcceptPreparation::NotVm);
            }
            let owner_id = work["owner"].as_str().unwrap_or_default().to_owned();
            let owner = event_write::checked_agent(tx, &owner_id)?;
            if owner.extra.get("executionMode").and_then(Value::as_str) != Some("vm") {
                return Ok(VmAcceptPreparation::NotVm);
            }
            // Validate the acceptance and all dependent recipients before the guest
            // merge. Python's WorkMixin refuses unsupported remote recipients while
            // applying accept; the VM merge must not happen before that refusal.
            let statuses = dependency_statuses(tx, &work)?;
            let now = event_write::unix_time()?;
            decide_accept(
                WorkSnapshot {
                    record: &work,
                    dependency_statuses: &statuses,
                },
                &reason,
                &agent_id,
                now,
            )?;
            for row in WorkItemsRepository::dependent_work(tx, &lead.root_id.0, &task_id)? {
                let dependent = WorkItemsRepository::work_record(tx, &row.id)?
                    .ok_or_else(|| invalid("runtime_work", &row.id))?;
                let mut dependent_statuses = dependency_statuses(tx, &dependent)?;
                dependent_statuses.insert(task_id.clone(), "accepted".to_owned());
                if blocked_by(&dependent, &dependent_statuses).is_empty()
                    && let Some(dependent_owner) = dependent["owner"].as_str()
                {
                    let recipient = AgentRepository::get(tx, &AgentId::from(dependent_owner))?
                        .ok_or_else(|| invalid("runtime_agents", dependent_owner))?;
                    event_write::refuse_remote(&recipient)?;
                }
            }
            if let Some(request_id) = request_id.as_deref().filter(|id| !id.is_empty()) {
                // Reserve the canonical HTTP identity so every work mutation sees
                // this in-flight VM action before any other request can reuse it.
                let reservation_id = request_id;
                if let Some(receipt) = EventWriteRepository::receipt(tx, reservation_id)? {
                    if receipt.signature != signature {
                        return Err(Error::Conflict("This request id has different content").into());
                    }
                    let reservation: Value = serde_json::from_str(&receipt.result)
                        .map_err(|_| invalid("runtime_operation_receipts", reservation_id))?;
                    if reservation["vmAcceptReservation"] != true
                        || reservation["taskId"] != task_id
                        || reservation["agentId"] != agent_id
                    {
                        return Err(Error::Conflict("This request id has different content").into());
                    }
                } else {
                    EventWriteRepository::put_receipt(
                        tx,
                        reservation_id,
                        &signature,
                        &json!({
                            "vmAcceptReservation":true,
                            "taskId":task_id,
                            "agentId":agent_id,
                            "status":"reserved"
                        })
                        .to_string(),
                    )?;
                }
            }
            if lead.extra.get("layrProjectId") != owner.extra.get("layrProjectId") {
                return Err(
                    Error::InvalidRequest("The result belongs to another layr project").into(),
                );
            }
            let result = work["results"]
                .as_array()
                .and_then(|items| items.last())
                .ok_or_else(|| invalid("runtime_work", &task_id))?;
            let result_id = result["id"]
                .as_str()
                .ok_or_else(|| invalid("runtime_work", &task_id))?
                .to_owned();
            let revision = result["revision"]
                .as_str()
                .ok_or_else(|| invalid("runtime_work", &task_id))?
                .to_owned();
            let rpc_id = format!("layr-accept:{result_id}");
            if let Some(intent) = read_vm_intent(tx, "merge", &task_id)? {
                if matches!(
                    intent.get("status").and_then(Value::as_str),
                    Some("pending" | "unknown")
                ) {
                    return Err(Error::Conflict(
                        "The prior layr merge outcome is unknown; inspect it before retrying",
                    )
                    .into());
                }
                if intent.get("requestId").and_then(Value::as_str) == Some(rpc_id.as_str())
                    && intent.get("status").and_then(Value::as_str) == Some("completed")
                {
                    return Ok(VmAcceptPreparation::Merged(result_id.to_owned()));
                }
            }
            let intent = json!({
                "status":"pending",
                "requestId":rpc_id,
                "taskId":task_id.as_str(),
                "resultId":result_id.as_str(),
                "revision":revision.as_str(),
                "owner":owner_id.as_str(),
                "created":event_write::unix_time()?,
            });
            write_vm_intent(tx, "merge", &task_id, &intent)?;
            Ok(VmAcceptPreparation::Call(VmMergeCall {
                lead_id: lead.id.0,
                owner_id,
                result_id,
                revision,
                request_id: rpc_id,
            }))
        })
        .await
}

pub async fn finish_vm_accept(
    store: &RuntimeStore,
    task_id: &str,
    request_id: &str,
    uncertain: Option<bool>,
) -> Result<(), WorkWriteError> {
    let task_id = task_id.to_owned();
    let request_id = request_id.to_owned();
    store
        .transaction_with_error(move |tx| {
            WorkItemsRepository::work_record(tx, &task_id)?
                .ok_or(Error::InvalidRequest("Unknown work item"))?;
            let mut intent = read_vm_intent(tx, "merge", &task_id)?
                .filter(|value| {
                    value.get("requestId").and_then(Value::as_str) == Some(request_id.as_str())
                })
                .ok_or(Error::Conflict("The layr merge intent changed"))?;
            if let Some(uncertain) = uncertain {
                intent["status"] = json!(if uncertain { "unknown" } else { "not_sent" });
            } else {
                intent["status"] = json!("completed");
            }
            write_vm_intent(tx, "merge", &task_id, &intent)?;
            Ok(())
        })
        .await
}

/// Verify the exact accepted result has a durable successful guest merge.
pub fn vm_merge_confirmed(
    connection: &Connection,
    task_id: &str,
    result_id: &str,
    revision: &str,
) -> Result<bool, WorkWriteError> {
    let Some(intent) = read_vm_intent(connection, "merge", task_id)? else {
        return Ok(false);
    };
    Ok(intent["status"] == "completed"
        && intent["resultId"] == result_id
        && intent["revision"] == revision)
}

pub enum WorkWrite {
    Create(CreateWork),
    Mutate(WorkMutation),
    Claim(ClaimWork),
    Submit(SubmitWork),
    Accept(AcceptWork),
}

pub struct WorkWriteResult {
    pub record: Value,
    pub event_inserted: bool,
    pub artifact: Option<ResultArtifact>,
}

pub struct WorkActorResult {
    pub record: Value,
    pub event_inserted: bool,
}

pub struct ResultArtifact {
    state_dir: PathBuf,
    path: PathBuf,
    body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseFailedWorkResult {
    pub task_ids: Vec<String>,
    pub event_inserted: bool,
}

#[derive(Debug, Error)]
pub enum WorkWriteError {
    #[error(transparent)]
    Event(#[from] Error),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error("Not implemented: requires {0}")]
    NotImplemented(&'static str),
    #[error("work-write preparation blocking task failed")]
    PreparationWorker,
}

/// Read and validate an idempotency receipt before an external VM side effect.
pub fn read_work_receipt(
    store: &RuntimeStore,
    agent_id: &str,
    request_id: Option<&str>,
    signature: &str,
) -> Result<Option<Value>, WorkWriteError> {
    let Some(request_id) = request_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };
    let connection = store.read()?;
    let agent = AgentRepository::get(&connection, &AgentId::from(agent_id))?
        .ok_or(Error::InvalidRequest("Invalid request"))?;
    if agent.id.0 != agent_id || event_write::truthy(agent.extra.get("deletedAt")) {
        return Err(Error::InvalidRequest("Invalid request").into());
    }
    let Some(receipt) = EventWriteRepository::receipt(&connection, request_id)? else {
        return Ok(None);
    };
    if receipt.signature != signature {
        return Err(Error::Conflict("This request id has different content").into());
    }
    let value: Value = serde_json::from_str(&receipt.result)
        .map_err(|_| invalid("runtime_operation_receipts", request_id))?;
    if value.get("vmAcceptReservation") == Some(&Value::Bool(true)) {
        // The matching VM accept continues through preparation and replaces
        // this reservation with its normal work receipt after the guest reply.
        return Ok(None);
    }
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return Err(invalid("runtime_work", request_id).into());
    };
    if value.get("rootId").and_then(Value::as_str) != Some(agent.root_id.0.as_str()) {
        return Err(Error::InvalidRequest("Invalid request").into());
    }
    WorkItemsRepository::validate_work(&connection, id, &value).map_err(WorkWriteError::Store)?;
    Ok(Some(value))
}

pub fn write(
    tx: &Transaction<'_>,
    agent_id: &str,
    request_id: Option<&str>,
    signature: &str,
    command: WorkWrite,
) -> Result<WorkWriteResult, WorkWriteError> {
    write_internal(tx, agent_id, None, request_id, signature, command)
}

fn write_internal(
    tx: &Transaction<'_>,
    agent_id: &str,
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
    command: WorkWrite,
) -> Result<WorkWriteResult, WorkWriteError> {
    #[cfg(not(unix))]
    if matches!(&command, WorkWrite::Submit(_)) {
        return Err(WorkWriteError::NotImplemented(
            "workspace-state/submission-files",
        ));
    }

    let agent = event_write::checked_agent(tx, agent_id)?;
    let actor = if let Some(actor_id) = actor_id {
        let actor = event_write::checked_agent(tx, actor_id)?;
        if actor.root_id != agent.root_id {
            return Err(Error::InvalidRequest("Actor belongs to another team").into());
        }
        if !event_write::truthy(actor.extra.get("autoWake")) {
            return Err(Error::InvalidRequest("The caller is stopped").into());
        }
        Some(actor)
    } else {
        None
    };
    let reserved_vm_accept = if let Some(request_id) = request_id.filter(|id| !id.is_empty()) {
        if let Some(receipt) = EventWriteRepository::receipt(tx, request_id)? {
            let value: Value = serde_json::from_str(&receipt.result)
                .map_err(|_| invalid("runtime_operation_receipts", request_id))?;
            if value.get("vmAcceptReservation") == Some(&Value::Bool(true)) {
                if receipt.signature != signature {
                    return Err(Error::Conflict("This request id has different content").into());
                }
                let matches_accept = match &command {
                    WorkWrite::Accept(input) => {
                        value.get("taskId").and_then(Value::as_str) == Some(input.task_id.as_str())
                            && value.get("agentId").and_then(Value::as_str)
                                == Some(agent.id.0.as_str())
                    }
                    _ => false,
                };
                if !matches_accept {
                    return Err(Error::Conflict("This request id has different content").into());
                }
                tx.execute(
                    "DELETE FROM runtime_operation_receipts WHERE id=?1 AND signature=?2",
                    rusqlite::params![request_id, signature],
                )
                .map_err(studio_runtime_store::StoreError::from)?;
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    if !reserved_vm_accept
        && let Some(record) = event_write::read_receipt(
            tx,
            request_id,
            signature,
            "runtime_work",
            request_id.unwrap_or(agent_id),
            |value| {
                value
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| WorkItemsRepository::validate_work(tx, id, value).is_ok())
            },
        )?
    {
        return Ok(WorkWriteResult {
            artifact: if matches!(&command, WorkWrite::Submit(_)) {
                let WorkWrite::Submit(input) = &command else {
                    return Err(WorkWriteError::Event(Error::InvalidRequest(
                        "Invalid request",
                    )));
                };
                Some(result_artifact(&record, &input.state_dir)?)
            } else {
                None
            },
            record,
            event_inserted: false,
        });
    }
    match command {
        WorkWrite::Create(input) => {
            if let Some(actor) = &actor
                && (actor.id != actor.root_id || agent.id != agent.root_id)
            {
                return Err(Error::InvalidRequest("Only the lead can create work").into());
            }
            create(
                tx,
                &agent,
                input,
                actor_id.unwrap_or(&agent.id.0),
                request_id,
                signature,
            )
            .map_err(Into::into)
        }
        WorkWrite::Mutate(input) => {
            if let Some(actor) = &actor {
                authorize_mutation(tx, &agent, actor, &input)?;
            }
            mutate(tx, &agent, input, actor_id, request_id, signature).map_err(Into::into)
        }
        WorkWrite::Claim(input) => claim(tx, &agent, input, actor_id, request_id, signature),
        WorkWrite::Submit(input) => submit(tx, &agent, input, actor_id, request_id, signature),
        WorkWrite::Accept(input) => accept(tx, &agent, input, actor_id, request_id, signature),
    }
}

/// Execute a work action with a persisted agent as the acting principal.
pub fn write_as_agent(
    tx: &Transaction<'_>,
    agent_id: &str,
    actor_id: &str,
    request_id: Option<&str>,
    signature: &str,
    command: WorkActorWrite,
) -> Result<WorkActorResult, Error> {
    let command = match command {
        WorkActorWrite::Create(input) => WorkWrite::Create(input),
        WorkActorWrite::Mutate(input) => WorkWrite::Mutate(input),
    };
    let result = write_internal(tx, agent_id, Some(actor_id), request_id, signature, command)
        .map_err(|error| match error {
            WorkWriteError::Event(error) => error,
            WorkWriteError::Store(error) => Error::Store(error),
            WorkWriteError::NotImplemented(_) => Error::InvalidRequest("Invalid request"),
            // This synchronous transaction helper cannot create the blocking-worker variant.
            WorkWriteError::PreparationWorker => Error::InvalidRequest("Invalid request"),
        })?;
    Ok(WorkActorResult {
        record: result.record,
        event_inserted: result.event_inserted,
    })
}

/// Apply caller provenance to all work actions introduced by this slice.
pub fn write_action_as_agent(
    tx: &Transaction<'_>,
    agent_id: &str,
    actor_id: &str,
    request_id: Option<&str>,
    signature: &str,
    command: WorkWrite,
) -> Result<WorkWriteResult, WorkWriteError> {
    write_internal(tx, agent_id, Some(actor_id), request_id, signature, command)
}

fn authorize_mutation(
    tx: &Transaction<'_>,
    selected: &AgentRecord,
    actor: &AgentRecord,
    mutation: &WorkMutation,
) -> Result<(), Error> {
    match mutation {
        WorkMutation::Update { .. } | WorkMutation::Reject { .. } => {
            if actor.id != actor.root_id {
                return Err(Error::InvalidRequest(
                    "Only the lead can change work assignments",
                ));
            }
        }
        WorkMutation::Cancel { task_id, .. } => {
            let work = WorkItemsRepository::work_record(tx, task_id)?
                .ok_or(Error::InvalidRequest("Unknown work item"))?;
            if actor.id != actor.root_id
                && work.get("createdBy").and_then(Value::as_str) != Some(actor.id.0.as_str())
            {
                return Err(Error::InvalidRequest(
                    "Only the lead or task creator can cancel this task",
                ));
            }
        }
    }
    if selected.root_id != actor.root_id {
        return Err(Error::InvalidRequest("Actor belongs to another team"));
    }
    Ok(())
}

/// Run the filesystem half after the transaction commits. A same-identity retry
/// recreates a missing artifact and rejects a conflicting file.
pub fn write_result_artifact(artifact: &ResultArtifact) -> Result<(), std::io::Error> {
    #[cfg(not(unix))]
    {
        let _ = artifact;
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe result artifact writes require directory-relative filesystem operations",
        ));
    }
    #[cfg(unix)]
    write_result_artifact_at(artifact)
}

#[cfg(unix)]
fn write_result_artifact_at(artifact: &ResultArtifact) -> Result<(), std::io::Error> {
    use rustix::fs::{AtFlags, Mode, OFlags, linkat, mkdirat, openat};

    let root = fs::canonicalize(&artifact.state_dir)?;
    let relative = artifact
        .path
        .strip_prefix(&artifact.state_dir)
        .map_err(|_| std::io::Error::other("invalid result artifact path"))?;
    let parent_path = relative
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid result artifact path"))?;
    let mut parent = fs::File::open("/")?;
    for component in root.components() {
        let std::path::Component::Normal(name) = component else {
            if matches!(component, std::path::Component::RootDir) {
                continue;
            }
            return Err(std::io::Error::other("invalid result artifact root"));
        };
        parent = openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(fs::File::from)
        .map_err(std::io::Error::from)?;
    }
    for component in parent_path.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::other("invalid result artifact path"));
        };
        let open_directory = || {
            openat(
                &parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(fs::File::from)
        };
        match open_directory() {
            Ok(directory) => parent = directory,
            Err(error) if error == rustix::io::Errno::NOENT => {
                match mkdirat(&parent, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                parent = open_directory().map_err(std::io::Error::from)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let file_name = relative
        .file_name()
        .ok_or_else(|| std::io::Error::other("invalid result artifact path"))?;
    let mut temporary_file = None;
    let mut temporary_name = String::new();
    for _ in 0..32 {
        temporary_name = format!(
            ".result-{}-{}.tmp",
            std::process::id(),
            GENERATED_WORK_ID.fetch_add(1, Ordering::Relaxed)
        );
        match openat(
            &parent,
            temporary_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => {
                temporary_file = Some(fs::File::from(file));
                break;
            }
            Err(error) if error == rustix::io::Errno::EXIST => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let mut temporary_file = temporary_file
        .ok_or_else(|| std::io::Error::other("could not allocate result artifact temporary"))?;
    let mut temporary_guard = TemporaryArtifact {
        parent: &parent,
        name: Some(temporary_name.clone()),
    };
    temporary_file.write_all(artifact.body.as_bytes())?;
    temporary_file.sync_all()?;
    drop(temporary_file);
    let result = match linkat(
        &parent,
        temporary_name.as_str(),
        &parent,
        file_name,
        AtFlags::empty(),
    ) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::EXIST => {
            let existing = openat(
                &parent,
                file_name,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(fs::File::from)
            .map_err(std::io::Error::from)?;
            if !existing.metadata()?.is_file()
                || existing.metadata()?.len() > artifact.body.len() as u64
            {
                Err(std::io::Error::other("saved task result file is invalid"))
            } else {
                let mut saved = Vec::with_capacity(artifact.body.len());
                existing
                    .take(artifact.body.len() as u64 + 1)
                    .read_to_end(&mut saved)?;
                if saved == artifact.body.as_bytes() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(
                        "saved task result file differs from its committed record",
                    ))
                }
            }
        }
        Err(error) => Err(error.into()),
    };
    result?;
    temporary_guard.cleanup()?;
    Ok(())
}

#[cfg(unix)]
struct TemporaryArtifact<'a> {
    parent: &'a fs::File,
    name: Option<String>,
}

#[cfg(unix)]
impl TemporaryArtifact<'_> {
    fn cleanup(&mut self) -> Result<(), std::io::Error> {
        let Some(name) = self.name.as_deref() else {
            return Ok(());
        };
        rustix::fs::unlinkat(self.parent, name, rustix::fs::AtFlags::empty())
            .map_err(std::io::Error::from)?;
        self.name.take();
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for TemporaryArtifact<'_> {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            match rustix::fs::unlinkat(self.parent, name.as_str(), rustix::fs::AtFlags::empty()) {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

/// Release persisted claims for failed or deleted workers. Callers own cadence and
/// invoke this in the same transaction that commits the resulting recovery events.
pub fn release_failed_work(
    tx: &Transaction<'_>,
    agents: &[AgentRecord],
    now: f64,
) -> Result<ReleaseFailedWorkResult, Error> {
    let mut released_ids = Vec::new();
    let mut event_inserted = false;
    for snapshot in agents {
        let Some(worker) = AgentRepository::get(tx, &snapshot.id)? else {
            continue;
        };
        if worker.root_id != snapshot.root_id
            || worker.epoch != snapshot.epoch
            || worker.status != snapshot.status
        {
            continue;
        }
        let deleted = worker
            .extra
            .get("deletedAt")
            .is_some_and(|value| event_write::truthy(Some(value)));
        if worker.id == worker.root_id
            || (!deleted && worker.status.as_str() != "failed")
            || (!deleted && continuation_work_pending(&worker))
        {
            continue;
        }
        let reason = if deleted {
            "The worker was deleted".to_owned()
        } else {
            worker
                .extra
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("The worker failed")
                .to_owned()
        };
        for released in WorkItemsRepository::release_failed_work(tx, &worker.id.0, &reason, now)? {
            let id = released["id"]
                .as_str()
                .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
            let root = AgentRepository::get(tx, &worker.root_id)?;
            if let Some(root) = root.filter(|root| {
                !root
                    .extra
                    .get("deletedAt")
                    .is_some_and(|value| event_write::truthy(Some(value)))
            }) {
                let owner = released["releases"]
                    .as_array()
                    .and_then(|history| history.last())
                    .and_then(|release| release.get("agent"))
                    .and_then(Value::as_str)
                    .unwrap_or(&worker.id.0);
                let reason = released["releases"]
                    .as_array()
                    .and_then(|history| history.last())
                    .and_then(|release| release.get("reason"))
                    .cloned()
                    .unwrap_or_else(|| json!("The worker failed"));
                event_inserted |= event_write::enqueue_recovery_event(
                    tx,
                    &root,
                    format!(
                        "work-released:{id}:{}",
                        released["version"].as_i64().unwrap_or_default()
                    ),
                    "work_released",
                    json!({"task":id,"title":released["title"],"agent":owner,"reason":reason})
                        .to_string(),
                    now,
                )?;
            }
            released_ids.push(id.to_owned());
        }
    }
    Ok(ReleaseFailedWorkResult {
        task_ids: released_ids,
        event_inserted,
    })
}

fn continuation_work_pending(agent: &AgentRecord) -> bool {
    if !agent
        .extra
        .get("autoWake")
        .is_some_and(|value| event_write::truthy(Some(value)))
        || agent
            .extra
            .get("deletedAt")
            .is_some_and(|value| event_write::truthy(Some(value)))
    {
        return false;
    }
    let default_account = json!("default");
    let account = agent.extra.get("accountKey").unwrap_or(&default_account);
    let thread = agent.extra.get("threadId").and_then(Value::as_str);
    let epoch = agent.epoch;
    let last_turn = agent.extra.get("lastCompletedTurn").and_then(Value::as_str);

    let matches = |record: &Value, statuses: &[&str], turn_field: &str| {
        let record_account = record.get("accountKey").unwrap_or(&default_account);
        record
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| statuses.contains(&status))
            && record_account == account
            && record.get("threadId").and_then(Value::as_str) == thread
            && thread.is_some_and(|thread| !thread.is_empty())
            && record.get("epoch").and_then(Value::as_i64) == Some(epoch)
            && record.get(turn_field).and_then(Value::as_str) == last_turn
            && last_turn.is_some_and(|turn| !turn.is_empty())
    };

    let capacity = agent.extra.get("capacityRetry").unwrap_or(&Value::Null);
    let usage = agent.extra.get("usageResume").unwrap_or(&Value::Null);
    let restart = agent.extra.get("restartRecovery").unwrap_or(&Value::Null);
    matches(capacity, &["scheduled", "starting", "unknown"], "turnId")
        || matches(usage, &["scheduled", "started", "unknown"], "turnId")
        || (restart.get("stage").and_then(Value::as_str) == Some("pending")
            && restart
                .get("autoWake")
                .is_some_and(|value| event_write::truthy(Some(value)))
            && restart.get("accountKey").unwrap_or(&default_account) == account
            && restart.get("threadId").and_then(Value::as_str) == thread
            && thread.is_some_and(|thread| !thread.is_empty())
            && restart.get("epoch").and_then(Value::as_i64) == Some(epoch))
}

struct WorkSnapshot<'a> {
    record: &'a Value,
    dependency_statuses: &'a BTreeMap<String, String>,
}

fn decide_claim(snapshot: WorkSnapshot<'_>, claimant: &str, now: f64) -> Result<Value, Error> {
    let record = snapshot.record;
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let status = record["status"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    if !matches!(status, "ready" | "running")
        || !blocked_by(record, snapshot.dependency_statuses).is_empty()
    {
        return Err(Error::InvalidRequest("This work item is not ready"));
    }
    if record["owner"]
        .as_str()
        .is_some_and(|owner| owner != claimant)
    {
        return Err(Error::InvalidRequest("Another agent owns this work item"));
    }
    let mut updated = record.clone();
    updated["owner"] = json!(claimant);
    updated["status"] = json!("running");
    advance(&mut updated, now)?;
    Ok(updated)
}

fn decide_submit(
    snapshot: WorkSnapshot<'_>,
    submitter: &AgentRecord,
    input: &SubmitWork,
    identity: Option<(String, Option<String>)>,
    now: f64,
    result_id: String,
) -> Result<(Value, ResultArtifact), Error> {
    let record = snapshot.record;
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    if !matches!(
        record["status"].as_str(),
        Some("running" | "ready" | "blocked" | "review")
    ) {
        return Err(Error::InvalidRequest("This result is already accepted"));
    }
    if !blocked_by(record, snapshot.dependency_statuses).is_empty() {
        return Err(Error::InvalidRequest("Dependencies have not been accepted"));
    }
    let title = record["title"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let submitter_id = submitter.id.0.as_str();
    let path = result_path(&input.state_dir, id, &result_id)?;
    let result_text = python_bounded_text(&input.result, 32000)?;
    let checks_text = python_bounded_text(&input.checks, 32000)?;
    let revision_text = python_bounded_text(&input.revision, 200)?;
    let mut result = json!({
        "id": result_id,
        "agent": submitter_id,
        "text": result_text,
        "checks": checks_text,
        "revision": revision_text,
        "files": input.files,
        "created": now,
        "resultFile": path.to_string_lossy(),
    });
    if let Some(evidence) = &input.vm_evidence {
        let normalized_revision = python_bounded_text(&input.revision, 200)?;
        if evidence["layrStateId"] != normalized_revision
            || evidence["layrProjectId"].as_str().is_none()
            || evidence["layrLine"].as_str().is_none()
        {
            return Err(Error::InvalidRequest("Submit the exact layr state ID"));
        }
        result["layrProjectId"] = evidence["layrProjectId"].clone();
        result["layrLine"] = evidence["layrLine"].clone();
        result["layrStateId"] = evidence["layrStateId"].clone();
    }
    if let Some(identity) = identity {
        result["runId"] = json!(identity.0);
        result["attemptId"] = json!(identity.1);
    }
    let mut updated = record.clone();
    updated["results"]
        .as_array_mut()
        .ok_or_else(|| invalid("runtime_work", id))?
        .push(result.clone());
    updated["status"] = json!("review");
    advance(&mut updated, now)?;
    let body = result_artifact_body(title, &result)?;
    Ok((
        updated,
        ResultArtifact {
            state_dir: input.state_dir.clone(),
            path,
            body,
        },
    ))
}

fn decide_accept(
    snapshot: WorkSnapshot<'_>,
    reason: &str,
    by: &str,
    now: f64,
) -> Result<Value, Error> {
    let record = snapshot.record;
    if record["status"] == "accepted" {
        return Err(Error::InvalidRequest(
            "This work is already accepted. Create a follow-up task with this task ID and the new evidence",
        ));
    }
    let result_id = record["results"]
        .as_array()
        .and_then(|results| results.last())
        .and_then(|result| result.get("id"))
        .and_then(Value::as_str)
        .ok_or(Error::InvalidRequest("Submit a result before review"))?;
    if record["status"] != "review" {
        return Err(Error::InvalidRequest("Submit a result before review"));
    }
    let mut updated = record.clone();
    if crate::event_write::python_strip(reason).is_empty() {
        return Err(Error::InvalidRequest("Field must not be empty"));
    }
    push_decision(
        &mut updated,
        json!({"resultId":result_id,"decision":"accept","reason":text(reason, false)?,"by":by,"created":now}),
    )?;
    updated["status"] = json!("accepted");
    updated["archive"] =
        json!({"status":"skipped","reason":"The task owner is the lead or is unassigned"});
    advance(&mut updated, now)?;
    let _ = snapshot.dependency_statuses;
    Ok(updated)
}

fn advance(record: &mut Value, now: f64) -> Result<(), Error> {
    let id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let version = record["version"]
        .as_i64()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let next = version
        .checked_add(1)
        .ok_or_else(|| invalid("runtime_work", id))?;
    record["version"] = json!(next);
    record["updated"] = json!(now);
    Ok(())
}

fn blocked_by(record: &Value, statuses: &BTreeMap<String, String>) -> Vec<String> {
    record["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|dependency| statuses.get(*dependency).map(String::as_str) != Some("accepted"))
        .map(str::to_owned)
        .collect()
}

fn result_path(state_dir: &Path, task_id: &str, result_id: &str) -> Result<PathBuf, Error> {
    if !task_id
        .chars()
        .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
        || !result_id
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
    {
        return Err(invalid("runtime_work", task_id));
    }
    Ok(state_dir
        .join("results")
        .join(task_id)
        .join(format!("{result_id}.md")))
}

fn result_artifact(record: &Value, state_dir: &Path) -> Result<ResultArtifact, Error> {
    let result = record["results"]
        .as_array()
        .and_then(|results| results.last())
        .ok_or_else(|| invalid("runtime_work", record["id"].as_str().unwrap_or("<invalid>")))?;
    let task_id = record["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let result_id = result["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", task_id))?;
    let path = result["resultFile"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", task_id))?;
    let expected_path = result_path(state_dir, task_id, result_id)?;
    if Path::new(path) != expected_path {
        return Err(invalid("runtime_work", task_id));
    }
    Ok(ResultArtifact {
        state_dir: state_dir.to_owned(),
        path: PathBuf::from(path),
        body: result_artifact_body(
            record["title"]
                .as_str()
                .ok_or_else(|| invalid("runtime_work", task_id))?,
            result,
        )?,
    })
}

fn result_artifact_body(title: &str, result: &Value) -> Result<String, Error> {
    let id = result["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let text = result["text"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let checks = result["checks"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let revision = result["revision"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let files = result["files"]
        .as_array()
        .ok_or_else(|| invalid("runtime_work", id))?
        .iter()
        .map(|value| value.as_str().map(|value| format!("- `{value}`")))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid("runtime_work", id))?;
    Ok(format!(
        "# {title}\n\nResult ID: `{id}`\n\n## Result\n\n{text}\n\n## Checks\n\n{checks}\n\n## Revision\n\n{revision}\n\n## Files\n\n{}\n",
        if files.is_empty() {
            "None".to_owned()
        } else {
            files.join("\n")
        },
    ))
}

fn create(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: CreateWork,
    actor_id: &str,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, Error> {
    let title = text(&input.title, false)?;
    let description = text(&input.description, true)?;
    let status = input.status.as_deref().unwrap_or("ready");
    if status == "accepted" {
        return Err(Error::InvalidRequest(
            "Cannot edit accepted work. Create a follow-up task with this task ID and the new evidence",
        ));
    }
    if status == "review" {
        return Err(Error::InvalidRequest(
            "Work under review cannot be reassigned; reject its result before editing",
        ));
    }
    if !matches!(status, "ready" | "blocked") {
        return Err(Error::InvalidRequest("Choose ready or blocked"));
    }
    let owner = input.owner.as_deref();
    check_owner(tx, agent, owner)?;
    let id = work_id(event_write::unix_time()?);
    let dependencies = unique(input.dependencies);
    validate_dependencies(tx, &agent.root_id.0, &id, &dependencies)?;
    let work = json!({
        "id": id, "rootId": agent.root_id.0, "title": title, "description": description,
        "owner": owner, "dependencies": dependencies, "status": status,
        "created": event_write::unix_time()?, "updated": event_write::unix_time()?, "version": 1,
        "results": [], "decisions": [], "createdBy": actor_id,
    });
    let view = single_view(tx, &work)?;
    if !WorkItemsRepository::put_work(tx, &id, &work)? {
        return Err(Error::Conflict("Work item already exists"));
    }
    let mut event_inserted = false;
    if let Some(owner) = owner.filter(|owner| *owner != agent.root_id.0)
        && is_ready(&view)
    {
        let recipient = AgentRepository::get(tx, &AgentId::from(owner))?
            .ok_or_else(|| invalid("runtime_agents", owner))?;
        event_inserted = queue_assignment(tx, &recipient, &work)?;
    }
    event_write::save_receipt(tx, request_id, signature, &view)?;
    Ok(WorkWriteResult {
        record: view,
        event_inserted,
        artifact: None,
    })
}

fn mutate(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    mutation: WorkMutation,
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, Error> {
    let (task_id, expected) = match &mutation {
        WorkMutation::Update {
            task_id, version, ..
        }
        | WorkMutation::Cancel {
            task_id, version, ..
        }
        | WorkMutation::Reject {
            task_id, version, ..
        } => (task_id.clone(), *version),
    };
    let mut work = WorkItemsRepository::work_record(tx, &task_id)?
        .ok_or(Error::InvalidRequest("Unknown work item"))?;
    if work["rootId"] != agent.root_id.0 {
        return Err(invalid("runtime_work", &task_id));
    }
    let version = work["version"]
        .as_i64()
        .ok_or_else(|| invalid("runtime_work", &task_id))?;
    if expected.is_some_and(|expected| expected != version) {
        return Err(Error::Conflict(
            "This work item changed. Reload before editing",
        ));
    }
    let status = work["status"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", &task_id))?;
    let owner = work["owner"].as_str().map(str::to_owned);
    let previous_ready = is_ready(&single_view(tx, &work)?);
    let mut wake = false;
    let mut assignment_owner = None;

    match mutation {
        WorkMutation::Update {
            title,
            description,
            owner: requested_owner,
            status: requested_status,
            dependencies,
            ..
        } => {
            if status == "accepted" {
                return Err(Error::InvalidRequest(
                    "Cannot edit accepted work. Create a follow-up task with this task ID and the new evidence",
                ));
            }
            if status == "review" {
                return Err(Error::InvalidRequest(
                    "Work under review cannot be reassigned; reject its result before editing",
                ));
            }
            if let Some(title) = title {
                work["title"] = json!(text(&title, false)?);
            }
            if let Some(description) = description {
                work["description"] = json!(text(&description, true)?);
            }
            if let Some(status) = requested_status {
                if !matches!(status.as_str(), "ready" | "blocked") {
                    return Err(Error::InvalidRequest("Choose ready or blocked"));
                }
                work["status"] = json!(status);
            }
            if let Some(dependencies) = dependencies {
                let dependencies = unique(dependencies);
                validate_dependencies(tx, &agent.root_id.0, &task_id, &dependencies)?;
                work["dependencies"] = json!(dependencies);
            }
            let next_owner = requested_owner.map(|owner| owner.unwrap_or_default());
            if let Some(owner) = next_owner {
                let owner = (!owner.is_empty()).then_some(owner);
                check_owner(tx, agent, owner.as_deref())?;
                work["owner"] = owner.map_or(Value::Null, Value::String);
            }
            if let Some(owner) = work["owner"].as_str()
                && owner != agent.root_id.0
            {
                event_write::assert_delegation(&root_agent(tx, agent)?)?;
            }
            let view = single_view(tx, &work)?;
            let current_owner = work["owner"].as_str();
            let changed = current_owner != owner.as_deref();
            let ready = is_ready(&view);
            if ready
                && (changed || !previous_ready)
                && current_owner.is_some_and(|owner| owner != agent.root_id.0)
            {
                assignment_owner = current_owner.map(str::to_owned);
            }
            if let Some(previous_owner) = owner.as_deref()
                && (changed || previous_ready != ready)
            {
                WorkItemsRepository::retire_pending_work_ready(
                    tx,
                    previous_owner,
                    &task_id,
                    "The work assignment or readiness changed before delivery",
                )?;
            }
        }
        WorkMutation::Cancel { reason, .. } => {
            let reason = text(&reason, false)?;
            if status == "cancelled" {
                let view = single_view(tx, &work)?;
                event_write::save_receipt(tx, request_id, signature, &view)?;
                return Ok(WorkWriteResult {
                    record: view,
                    event_inserted: false,
                    artifact: None,
                });
            }
            if status == "accepted" {
                return Err(Error::InvalidRequest("Cannot cancel accepted work"));
            }
            push_decision(
                &mut work,
                json!({"decision":"cancel", "reason":reason, "by":actor_id.unwrap_or(&agent.id.0), "owner":owner, "created":event_write::unix_time()?}),
            )?;
            work["status"] = json!("cancelled");
            work["owner"] = Value::Null;
            if let Some(owner_id) = owner.as_deref() {
                WorkItemsRepository::retire_pending_work_ready(
                    tx,
                    owner_id,
                    &task_id,
                    "The task was cancelled before delivery",
                )?;
                if let Some(recipient) = AgentRepository::get(tx, &AgentId::from(owner_id))?
                    && live_recipient(&recipient)
                {
                    wake |= event_write::enqueue_recovery_event(
                        tx,
                        &recipient,
                        format!("work-decision:{task_id}:cancel"),
                        "work_decision",
                        json!({"task":task_id,"decision":"cancel","reason":reason}).to_string(),
                        event_write::unix_time()?,
                    )?;
                }
            }
        }
        WorkMutation::Reject { reason, .. } => {
            if status == "accepted" {
                return Err(Error::InvalidRequest(
                    "This work is already accepted. Create a follow-up task with this task ID and the new evidence",
                ));
            }
            if status != "review" {
                return Err(Error::InvalidRequest("Submit a result before review"));
            }
            let reason = text(&reason, false)?;
            let result_id = work["results"]
                .as_array()
                .and_then(|v| v.last())
                .and_then(|v| v["id"].as_str())
                .ok_or_else(|| invalid("runtime_work", &task_id))?
                .to_owned();
            if let Some(owner_id) = owner.as_deref() {
                if owner_id != agent.root_id.0 {
                    event_write::assert_delegation(&root_agent(tx, agent)?)?;
                }
                let recipient = AgentRepository::get(tx, &AgentId::from(owner_id))?
                    .ok_or_else(|| invalid("runtime_agents", owner_id))?;
                if actor_id != Some(owner_id) {
                    wake |= event_write::enqueue_recovery_event(
                        tx,
                        &recipient,
                        format!("work-decision:{task_id}:{version}"),
                        "work_decision",
                        json!({"task":task_id,"decision":"reject","reason":reason}).to_string(),
                        event_write::unix_time()?,
                    )?;
                }
            }
            WorkItemsRepository::close_review_event(tx, &agent.root_id.0, &result_id)?;
            push_decision(
                &mut work,
                json!({"resultId":result_id,"decision":"reject","reason":reason,"by":actor_id.unwrap_or("user"),"created":event_write::unix_time()?}),
            )?;
            work["status"] = json!("ready");
        }
    }

    work["version"] = json!(version + 1);
    work["updated"] = json!(event_write::unix_time()?);
    WorkItemsRepository::update_work(tx, &task_id, &work)?;
    if let Some(owner_id) = assignment_owner {
        let recipient = AgentRepository::get(tx, &AgentId::from(owner_id.as_str()))?
            .ok_or_else(|| invalid("runtime_agents", &owner_id))?;
        wake |= queue_assignment(tx, &recipient, &work)?;
    }
    let view = single_view(tx, &work)?;
    event_write::save_receipt(tx, request_id, signature, &view)?;
    Ok(WorkWriteResult {
        record: view,
        event_inserted: wake,
        artifact: None,
    })
}

fn claim(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: ClaimWork,
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
    let claimant = event_write::checked_agent(tx, actor_id.unwrap_or(&input.claimant_id))?;
    if claimant.root_id != agent.root_id {
        return Err(Error::InvalidRequest("Claimant belongs to another team").into());
    }
    if claimant.id != agent.root_id && work["owner"].as_str() != Some(claimant.id.0.as_str()) {
        event_write::assert_delegation(&root_agent(tx, agent)?)?;
    }
    let statuses = dependency_statuses(tx, &work)?;
    let updated = decide_claim(
        WorkSnapshot {
            record: &work,
            dependency_statuses: &statuses,
        },
        &claimant.id.0,
        event_write::unix_time()?,
    )?;
    WorkItemsRepository::update_work(tx, &input.task_id, &updated)?;
    let record = single_view(tx, &updated)?;
    event_write::save_receipt(tx, request_id, signature, &record)?;
    Ok(WorkWriteResult {
        record,
        event_inserted: false,
        artifact: None,
    })
}

fn submit(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: SubmitWork,
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    if input.files.len() > 50 || input.files.iter().any(|path| path.chars().count() > 4096) {
        return Err(Error::InvalidRequest("Supply up to 50 file paths").into());
    }
    let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
    if actor_id.is_some_and(|actor| work["owner"].as_str() != Some(actor)) {
        return Err(Error::InvalidRequest("Only the assigned worker can submit its result").into());
    }
    let submitter_id = work["owner"].as_str().unwrap_or(&agent.id.0);
    let submitter = event_write::checked_agent(tx, submitter_id)?;
    if submitter.root_id != agent.root_id {
        return Err(invalid("runtime_agents", submitter_id).into());
    }
    validate_submission_files(&submitter, &input.files)?;
    if submitter.extra.get("executionMode").and_then(Value::as_str) == Some("vm") {
        let Some(request_id) = input.vm_evidence_request_id.as_deref() else {
            return Err(Error::Conflict(
                "Layr submission evidence must be checked before saving the result",
            )
            .into());
        };
        let Some(intent) = read_vm_intent(tx, "submission", &input.task_id)? else {
            return Err(
                Error::Conflict("Layr submission evidence changed before it was saved").into(),
            );
        };
        let revision = python_bounded_text(&input.revision, 200)?;
        if input.vm_evidence.is_none()
            || intent.get("status").and_then(Value::as_str) != Some("completed")
            || intent.get("requestId").and_then(Value::as_str) != Some(request_id)
            || intent.get("owner").and_then(Value::as_str) != Some(submitter_id)
            || intent.get("version") != Some(&work["version"])
            || intent.get("revision").and_then(Value::as_str) != Some(revision.as_str())
            || intent.get("evidence") != input.vm_evidence.as_ref()
        {
            return Err(
                Error::Conflict("Layr submission evidence changed before it was saved").into(),
            );
        }
    }
    event_write::refuse_remote(&submitter)?;
    let statuses = dependency_statuses(tx, &work)?;
    let identity = WorkItemsRepository::submission_identity(tx, &submitter)?;
    let result_id = new_id()?;
    let now = event_write::unix_time()?;
    let (updated, artifact) = decide_submit(
        WorkSnapshot {
            record: &work,
            dependency_statuses: &statuses,
        },
        &submitter,
        &input,
        identity,
        now,
        result_id,
    )?;
    let result = updated["results"]
        .as_array()
        .and_then(|results| results.last())
        .cloned()
        .ok_or_else(|| invalid("runtime_work", &input.task_id))?;
    let mut event_inserted = false;
    if actor_id != Some(agent.root_id.0.as_str()) {
        let lead = AgentRepository::get(tx, &agent.root_id)?
            .ok_or_else(|| invalid("runtime_agents", &agent.root_id.0))?;
        let result_id = result["id"]
            .as_str()
            .ok_or_else(|| invalid("runtime_work", &input.task_id))?;
        event_inserted |= event_write::enqueue_recovery_event(
            tx,
            &lead,
            format!("work-result:{result_id}"),
            "work_review",
            json!({"task":input.task_id,"title":work["title"],"result":result}).to_string(),
            now,
        )?;
    }
    WorkItemsRepository::update_work(tx, &input.task_id, &updated)?;
    let record = single_view(tx, &updated)?;
    event_write::save_receipt(tx, request_id, signature, &record)?;
    Ok(WorkWriteResult {
        record,
        event_inserted,
        artifact: Some(artifact),
    })
}

fn accept(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    input: AcceptWork,
    actor_id: Option<&str>,
    request_id: Option<&str>,
    signature: &str,
) -> Result<WorkWriteResult, WorkWriteError> {
    if actor_id.is_some_and(|actor| actor != agent.root_id.0) {
        return Err(Error::InvalidRequest("Only the lead can accept or reject a result").into());
    }
    let work = load_work(tx, &agent.root_id.0, &input.task_id, input.version)?;
    if let Some(owner_id) = work["owner"].as_str()
        && owner_id != agent.root_id.0
    {
        let owner = event_write::checked_agent(tx, owner_id)?;
        if owner.extra.get("executionMode").and_then(Value::as_str) == Some("vm") {
            let latest_result_id = work["results"]
                .as_array()
                .and_then(|items| items.last())
                .and_then(|result| result["id"].as_str())
                .ok_or_else(|| invalid("runtime_work", &input.task_id))?;
            if input.merged_result_id.as_deref() != Some(latest_result_id) {
                return Err(Error::Conflict(
                    "The layr result must be merged before it can be accepted",
                )
                .into());
            }
        }
    }
    let statuses = dependency_statuses(tx, &work)?;
    let now = event_write::unix_time()?;
    if crate::event_write::python_strip(&input.reason).is_empty() {
        return Err(Error::InvalidRequest("Field must not be empty").into());
    }
    let root = AgentRepository::get(tx, &agent.root_id)?
        .ok_or_else(|| invalid("runtime_agents", &agent.root_id.0))?;
    let updated = decide_accept(
        WorkSnapshot {
            record: &work,
            dependency_statuses: &statuses,
        },
        &input.reason,
        actor_id.unwrap_or("user"),
        now,
    )?;
    let result_id = updated["decisions"]
        .as_array()
        .and_then(|decisions| decisions.last())
        .and_then(|decision| decision.get("resultId"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("runtime_work", &input.task_id))?
        .to_owned();
    let mut ready_dependents = Vec::new();
    for row in WorkItemsRepository::dependent_work(tx, &agent.root_id.0, &input.task_id)? {
        let dependent = WorkItemsRepository::work_record(tx, &row.id)?
            .ok_or_else(|| invalid("runtime_work", &row.id))?;
        let mut dep_statuses = dependency_statuses(tx, &dependent)?;
        dep_statuses.insert(input.task_id.clone(), "accepted".to_owned());
        if !blocked_by(&dependent, &dep_statuses).is_empty() {
            continue;
        }
        let Some(owner_id) = dependent["owner"].as_str() else {
            continue;
        };
        let recipient = AgentRepository::get(tx, &AgentId::from(owner_id))?
            .ok_or_else(|| invalid("runtime_agents", owner_id))?;
        event_write::refuse_remote(&recipient)?;
        ready_dependents.push((recipient, dependent));
    }
    WorkItemsRepository::close_review_event(tx, &agent.root_id.0, &result_id)?;
    let mut event_inserted = false;
    if let Some(owner_id) = work["owner"].as_str()
        && owner_id != actor_id.unwrap_or("")
        && (actor_id.is_some() || owner_id == agent.root_id.0)
    {
        event_inserted |= event_write::enqueue_recovery_event(
            tx,
            &root,
            format!(
                "work-decision:{}:{}",
                input.task_id,
                work["version"].as_i64().unwrap_or_default()
            ),
            "work_decision",
            json!({"task":input.task_id,"decision":"accept","reason":input.reason}).to_string(),
            now,
        )?;
    }
    for (recipient, dependent) in ready_dependents {
        event_inserted |= event_write::enqueue_recovery_event(
            tx,
            &recipient,
            format!(
                "work-ready:{}:{}",
                dependent["id"].as_str().unwrap_or_default(),
                input.task_id
            ),
            "work_ready",
            json!({
                "task":dependent["id"],"title":dependent["title"],
                "description":dependent.get("description").cloned().unwrap_or_else(|| json!(""))
            })
            .to_string(),
            now,
        )?;
    }
    let archive_owner = work["owner"]
        .as_str()
        .filter(|owner| *owner != agent.root_id.0);
    if let Some(owner) = archive_owner {
        let archive_intent_id = format!("{}:{result_id}", input.task_id);
        tx.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3) \
             ON CONFLICT(id) DO NOTHING",
            rusqlite::params![
                format!("work-archive:{archive_intent_id}"),
                "accepted-archive-v1",
                json!({"status":"pending","taskId":input.task_id,
                    "intentId":archive_intent_id,"actor":agent.root_id.0,"actorEpoch":root.epoch,
                    "target":owner,"attempts":0,"created":now,
                    "nextAttemptAt":now + ACCEPTED_ARCHIVE_MAX_DELAY_SECONDS as f64})
                .to_string()
            ],
        )
        .map_err(studio_runtime_store::StoreError::from)?;
    }
    WorkItemsRepository::update_work(tx, &input.task_id, &updated)?;
    let mut record = single_view(tx, &updated)?;
    record["archivePending"] = json!(archive_owner.is_some());
    event_write::save_receipt(tx, request_id, signature, &record)?;
    Ok(WorkWriteResult {
        record,
        event_inserted,
        artifact: None,
    })
}

fn load_work(
    tx: &Transaction<'_>,
    root: &str,
    task_id: &str,
    expected_version: Option<i64>,
) -> Result<Value, Error> {
    let work = WorkItemsRepository::work_record(tx, task_id)?
        .ok_or(Error::InvalidRequest("Unknown work item"))?;
    if work["rootId"] != root {
        return Err(invalid("runtime_work", task_id));
    }
    let version = work["version"]
        .as_i64()
        .ok_or_else(|| invalid("runtime_work", task_id))?;
    if expected_version.is_some_and(|expected| expected != version) {
        return Err(Error::Conflict(
            "This work item changed. Reload before editing",
        ));
    }
    Ok(work)
}

fn dependency_statuses(
    tx: &Transaction<'_>,
    work: &Value,
) -> Result<BTreeMap<String, String>, Error> {
    let id = work["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let status = work["status"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let mut statuses = BTreeMap::from([(id.to_owned(), status.to_owned())]);
    for dependency in work["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(value) = WorkItemsRepository::work_record(tx, dependency)?
            && value["rootId"] == work["rootId"]
        {
            let status = value["status"]
                .as_str()
                .ok_or_else(|| invalid("runtime_work", dependency))?;
            statuses.insert(dependency.to_owned(), status.to_owned());
        }
    }
    Ok(statuses)
}

fn new_id() -> Result<String, Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        Error::Store(studio_runtime_store::StoreError::StateDirectory(
            std::io::Error::other("secure random generator failed"),
        ))
    })?;
    Ok(uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string())
}

fn push_decision(work: &mut Value, decision: Value) -> Result<(), Error> {
    let id = work["id"].as_str().unwrap_or("<work>").to_owned();
    work["decisions"]
        .as_array_mut()
        .ok_or_else(|| invalid("runtime_work", &id))?
        .push(decision);
    Ok(())
}

pub fn work_item_view(
    connection: &Connection,
    work: &Value,
) -> studio_runtime_store::StoreResult<Value> {
    let mut statuses = BTreeMap::new();
    if let (Some(id), Some(status)) = (work["id"].as_str(), work["status"].as_str()) {
        statuses.insert(id.to_owned(), status.to_owned());
    }
    for dependency in work["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(value) = WorkItemsRepository::work_record(connection, dependency)?
            && value["rootId"] == work["rootId"]
            && let (Some(id), Some(status)) = (value["id"].as_str(), value["status"].as_str())
        {
            statuses.insert(id.to_owned(), status.to_owned());
        }
    }
    Ok(crate::work_policy::work_item_view(work, &statuses))
}

fn single_view(tx: &Transaction<'_>, work: &Value) -> Result<Value, Error> {
    let statuses = dependency_statuses(tx, work)?;
    Ok(crate::work_policy::work_item_view(work, &statuses))
}

fn validate_dependencies(
    tx: &Transaction<'_>,
    root: &str,
    task: &str,
    deps: &[String],
) -> Result<(), Error> {
    if deps.iter().any(|id| id == task) {
        return Err(Error::InvalidRequest("Task dependencies contain a cycle"));
    }
    for id in deps {
        let item = WorkItemsRepository::work_record(tx, id)?
            .ok_or(Error::InvalidRequest("Dependency is outside this team"))?;
        if item["rootId"] != root {
            return Err(Error::InvalidRequest("Dependency is outside this team"));
        }
    }
    let mut active = vec![task.to_owned()];
    let mut visited = BTreeMap::new();
    for id in deps {
        dependency_visit(tx, root, id, &mut active, &mut visited)?;
    }
    Ok(())
}

fn dependency_visit(
    tx: &Transaction<'_>,
    root: &str,
    task: &str,
    active: &mut Vec<String>,
    visited: &mut BTreeMap<String, ()>,
) -> Result<(), Error> {
    if active.iter().any(|id| id == task) {
        return Err(Error::InvalidRequest("Task dependencies contain a cycle"));
    }
    if visited.contains_key(task) {
        return Ok(());
    }
    let item = WorkItemsRepository::work_record(tx, task)?
        .ok_or(Error::InvalidRequest("Dependency is outside this team"))?;
    if item["rootId"] != root {
        return Err(Error::InvalidRequest("Dependency is outside this team"));
    }
    active.push(task.to_owned());
    for dependency in item["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        dependency_visit(tx, root, dependency, active, visited)?;
    }
    active.pop();
    visited.insert(task.to_owned(), ());
    Ok(())
}

fn unique(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeMap::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone(), ()).is_none())
        .collect()
}

fn check_owner(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    owner: Option<&str>,
) -> Result<(), Error> {
    if let Some(owner) = owner {
        let candidate = event_write::checked_agent(tx, owner)?;
        if candidate.root_id != agent.root_id {
            return Err(Error::InvalidRequest("Owner belongs to another team"));
        }
        if owner != agent.root_id.0 {
            event_write::assert_delegation(&root_agent(tx, agent)?)?;
        }
    }
    Ok(())
}

fn queue_assignment(
    tx: &Transaction<'_>,
    recipient: &AgentRecord,
    work: &Value,
) -> Result<bool, Error> {
    let id = work["id"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", "<invalid>"))?;
    let version = work["version"]
        .as_i64()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let title = work["title"]
        .as_str()
        .ok_or_else(|| invalid("runtime_work", id))?;
    let updated = work["updated"]
        .as_f64()
        .ok_or_else(|| invalid("runtime_work", id))?;
    event_write::enqueue_recovery_event(
        tx,
        recipient,
        format!("work-ready:{id}:assignment:{version}"),
        "work_ready",
        json!({
            "task":id,
            "title":title,
            "description":work.get("description").cloned().unwrap_or_else(|| json!(""))
        })
        .to_string(),
        updated,
    )
}

fn root_agent(tx: &Transaction<'_>, agent: &AgentRecord) -> Result<AgentRecord, Error> {
    if agent.id == agent.root_id {
        return Ok(agent.clone());
    }
    AgentRepository::get(tx, &agent.root_id)?.ok_or(Error::InvalidRequest("Invalid request"))
}

fn live_recipient(agent: &AgentRecord) -> bool {
    !event_write::truthy(agent.extra.get("deletedAt"))
        && !matches!(
            agent.status,
            studio_runtime_store::AgentStatus::Completed
                | studio_runtime_store::AgentStatus::Failed
        )
}

fn is_ready(view: &Value) -> bool {
    matches!(view["status"].as_str(), Some("ready" | "running"))
        && view["blockedBy"].as_array().is_some_and(Vec::is_empty)
}

fn text(value: &str, empty: bool) -> Result<String, Error> {
    let value = value.trim();
    if !empty && value.is_empty() {
        return Err(Error::InvalidRequest("Invalid request"));
    }
    Ok(value.to_owned())
}

fn validate_submission_files(agent: &AgentRecord, files: &[String]) -> Result<(), Error> {
    if files.len() > 50 || files.iter().any(|path| path.chars().count() > 4096) {
        return Err(Error::InvalidRequest("Supply up to 50 file paths"));
    }
    if files.is_empty() {
        return Ok(());
    }
    let cwd = agent
        .extra
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .ok_or_else(|| invalid("runtime_agents", &agent.id.0))?;
    for raw_path in files {
        let path = python_bounded_text(raw_path, 4096)?;
        let expanded = if path == "~" || path.starts_with("~/") {
            let home = std::env::var_os("HOME").ok_or(Error::InvalidRequest("Invalid request"))?;
            PathBuf::from(home).join(
                path.strip_prefix('~')
                    .unwrap_or_default()
                    .trim_start_matches('/'),
            )
        } else {
            PathBuf::from(path)
        };
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            Path::new(cwd).join(expanded)
        };
        weakly_canonicalize(&candidate).map_err(|_| Error::InvalidRequest("Invalid request"))?;
    }
    Ok(())
}

fn weakly_canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(name) = path.file_name() else {
                return Err(error);
            };
            let parent = path
                .parent()
                .ok_or_else(|| std::io::Error::other("missing parent"))?;
            Ok(weakly_canonicalize(parent)?.join(name))
        }
        Err(error) => Err(error),
    }
}

fn python_bounded_text(value: &str, maximum: usize) -> Result<String, Error> {
    let value = event_write::python_strip(value);
    if value.is_empty() || value.chars().count() > maximum {
        return Err(Error::InvalidRequest("Invalid request"));
    }
    Ok(value.to_owned())
}

fn work_id(created: f64) -> String {
    format!(
        "work-{}-{}",
        (created * 1_000_000_000.0) as u128,
        GENERATED_WORK_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn invalid(object: &'static str, id: &str) -> Error {
    Error::InvalidStoredObject {
        object,
        id: id.to_owned(),
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::{ResultArtifact, write_result_artifact};
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn replay_of_fifo_is_bounded_and_does_not_leave_a_temporary_file() {
        use std::sync::mpsc;
        use std::time::Duration;

        let temporary = tempfile::tempdir().expect("temporary root opens");
        let state = temporary.path().join("state");
        let parent = state.join("results/task");
        fs::create_dir_all(&parent).expect("artifact parent creates");
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(parent.join("result.md"))
            .status()
            .expect("FIFO creation starts");
        assert!(status.success(), "FIFO artifact creates");
        let artifact = ResultArtifact {
            state_dir: state,
            path: parent.join("result.md"),
            body: "committed result".to_owned(),
        };
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            drop(sender.send(write_result_artifact(&artifact)));
        });
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("FIFO replay returns without blocking")
            .expect_err("FIFO is not a valid result artifact");
        let entries: Vec<_> = fs::read_dir(&parent)
            .expect("artifact parent lists")
            .map(|entry| entry.expect("directory entry reads").file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("result.md")]);
    }

    #[cfg(unix)]
    #[test]
    fn failed_artifact_replay_removes_its_temporary_file() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root opens");
        let state = temporary.path().join("state");
        let parent = state.join("results/task");
        let outside = temporary.path().join("outside.md");
        fs::create_dir_all(&parent).expect("artifact parent creates");
        fs::write(&outside, "foreign content").expect("outside file writes");
        symlink(&outside, parent.join("result.md")).expect("artifact symlink creates");
        let artifact = ResultArtifact {
            state_dir: state,
            path: parent.join("result.md"),
            body: "committed result".to_owned(),
        };

        assert!(write_result_artifact(&artifact).is_err());
        let entries: Vec<_> = fs::read_dir(&parent)
            .expect("artifact parent lists")
            .map(|entry| entry.expect("directory entry reads").file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("result.md")]);
    }

    #[cfg(unix)]
    #[test]
    fn result_artifact_writer_is_anchored_to_open_directory_handles() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root opens");
        let state = temporary.path().join("state");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&state).expect("state directory creates");
        fs::create_dir_all(&outside).expect("outside directory creates");
        let artifact_path = state.join("results/task/result-1.md");
        let artifact = ResultArtifact {
            state_dir: state.clone(),
            path: artifact_path.clone(),
            body: "committed result".to_owned(),
        };
        write_result_artifact(&artifact).expect("safe artifact writes");
        assert_eq!(
            fs::read(&artifact_path).expect("artifact reads"),
            b"committed result"
        );

        fs::remove_dir_all(state.join("results")).expect("result tree removes");
        symlink(&outside, state.join("results")).expect("outside symlink creates");
        let escaped = ResultArtifact {
            state_dir: state,
            path: artifact_path,
            body: "must stay contained".to_owned(),
        };
        assert!(write_result_artifact(&escaped).is_err());
        assert!(!outside.join("task/result-1.md").exists());
    }
}

#[cfg(test)]
mod actor_tests {
    #[cfg(not(unix))]
    use super::SubmitWork;
    use super::{
        AcceptWork, ClaimWork, CreateWork, WorkActorWrite, WorkMutation, WorkWrite, WorkWriteError,
        accepted_archive_due_records, accepted_archive_tick, next_archive_retry_at,
        write_action_as_agent, write_as_agent,
    };
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use studio_runtime_store::{
        AgentId, AgentRecord, AgentRepository, RuntimeStore, WorkItemsRepository,
    };

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection.execute_batch(
            "CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
             CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
             CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);",
        ).expect("work tables create");
        for agent in [
            json!({"id":"lead","rootId":"lead","epoch":1,"status":"running","autoWake":true,"inFlight":true,"agentMode":"multi","concurrency":8}),
            json!({"id":"worker","rootId":"lead","epoch":2,"status":"running","autoWake":true,"inFlight":true,"agentMode":"multi","concurrency":8}),
        ] {
            connection
                .execute(
                    "INSERT INTO runtime_agents VALUES (?1,?2)",
                    rusqlite::params![agent["id"].as_str().expect("id"), agent.to_string()],
                )
                .expect("agent row inserts");
        }
        connection
    }

    #[cfg(not(unix))]
    #[test]
    fn submit_refuses_before_database_access_without_directory_relative_operations() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        let tx = connection.transaction().expect("transaction starts");
        let result = super::write(
            &tx,
            "lead",
            None,
            "submit-unsupported-platform",
            WorkWrite::Submit(SubmitWork {
                task_id: "task".to_owned(),
                version: Some(1),
                result: "result".to_owned(),
                checks: "checks".to_owned(),
                revision: "abc1234".to_owned(),
                files: vec![],
                state_dir: std::env::temp_dir(),
                vm_evidence: None,
                vm_evidence_request_id: None,
            }),
        );
        assert!(matches!(
            result,
            Err(WorkWriteError::NotImplemented(
                "workspace-state/submission-files"
            ))
        ));
    }

    fn insert_work(connection: &Connection, work: &Value) {
        connection
            .execute(
                "INSERT INTO runtime_work VALUES (?1,?2)",
                rusqlite::params![work["id"].as_str().expect("work id"), work.to_string()],
            )
            .expect("work row inserts");
    }

    #[test]
    fn accepted_archive_due_selection_uses_injected_clock() {
        let connection = database();
        let pending = work("archive-due", Some("worker"), "accepted", json!([]));
        insert_work(&connection, &pending);
        connection
            .execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3)",
                rusqlite::params![
                    "work-archive:archive-due:result-1",
                    "accepted-archive-v1",
                    json!({"taskId":"archive-due","intentId":"archive-due:result-1",
                    "status":"pending","nextAttemptAt":120.0})
                    .to_string()
                ],
            )
            .expect("archive intent receipt inserts");

        assert!(
            accepted_archive_due_records(&connection, 119.999)
                .expect("before deadline query succeeds")
                .is_empty()
        );
        let due =
            accepted_archive_due_records(&connection, 120.0).expect("deadline query succeeds");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "archive-due");
        assert_eq!(
            serde_json::from_str::<Value>(&due[0].1).expect("receipt JSON")["intentId"],
            "archive-due:result-1"
        );
    }

    #[tokio::test]
    async fn archive_scheduler_uses_earliest_persisted_retry_deadline() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        assert_eq!(
            next_archive_retry_at(&store, 100.0, false).expect("empty deadline query"),
            None
        );
        store
            .transaction(|tx| {
                for (id, due) in [("later", 88.0), ("earlier", 42.0)] {
                    tx.execute(
                        "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,'accepted-archive-v1',?2)",
                    rusqlite::params![id, json!({"status":"retryable","nextAttemptAt":due + 100.0}).to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("archive receipts commit");
        assert_eq!(
            next_archive_retry_at(&store, 100.0, false).expect("deadline query"),
            Some(142.0)
        );
        assert_eq!(
            next_archive_retry_at(&store, 100.0, true).expect("full batch deadline"),
            Some(100.0)
        );
    }

    #[tokio::test]
    async fn empty_archive_maintenance_does_not_commit_or_self_wake() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        let commits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let commits = std::sync::Arc::clone(&commits);
            store.add_commit_observer(move || {
                commits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        assert_eq!(
            accepted_archive_tick(Arc::clone(&store), 100.0)
                .await
                .expect("maintenance pass"),
            0
        );
        assert_eq!(
            accepted_archive_tick(Arc::clone(&store), 100.0)
                .await
                .expect("second maintenance pass"),
            0
        );
        assert_eq!(commits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn accepted_archive_tick_persists_terminal_kept_outcome_for_remote_worker() {
        let root = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store"),
        );
        store
            .transaction(|tx| {
                for agent in [
                    json!({"id":"lead","rootId":"lead","epoch":1,"status":"running"}),
                    json!({"id":"worker","rootId":"lead","epoch":2,"status":"paused",
                        "remoteWorker":{"server":"remote","link":"worker-link"}}),
                ] {
                    AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?;
                }
                let work = json!({
                    "id":"accepted-task","rootId":"lead","owner":"worker",
                    "status":"accepted","version":3,"title":"Accepted result",
                    "description":"","dependencies":[],"results":[{"id":"result-1",
                        "text":"verified","revision":"abc1234"}],"decisions":[],
                    "archiveIntent":{"id":"accepted-task:result-1","owner":"worker",
                        "status":"pending","attempts":0,"created":10.0}
                });
                tx.execute(
                    "INSERT INTO runtime_work(id,record) VALUES (?1,?2)",
                    rusqlite::params!["accepted-task", work.to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3)",
                    rusqlite::params![
                        "work-archive:accepted-task:result-1",
                        "accepted-archive-v1",
                        json!({"taskId":"accepted-task","intentId":"accepted-task:result-1",
                            "actor":"lead","actorEpoch":1,"target":"worker","attempts":0,
                            "created":10.0,"status":"pending","nextAttemptAt":100.0})
                        .to_string(),
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("archive intent state commits");

        assert_eq!(
            accepted_archive_tick(Arc::clone(&store), 100.0)
                .await
                .expect("tick succeeds"),
            1
        );

        let connection = store.read().expect("read tick result");
        let encoded_receipt: String = connection
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                ["work-archive:accepted-task:result-1"],
                |row| row.get(0),
            )
            .expect("archive outcome receipt");
        let receipt: Value = serde_json::from_str(&encoded_receipt).expect("receipt JSON");
        assert_eq!(receipt["status"], "terminal");
        assert_eq!(receipt["outcome"]["status"], "kept");
        assert_eq!(
            receipt["outcome"]["reason"],
            "The remote worker workspace requires review on its own server"
        );
        let encoded_work: String = connection
            .query_row(
                "SELECT record FROM runtime_work WHERE id=?1",
                ["accepted-task"],
                |row| row.get(0),
            )
            .expect("accepted work record");
        let work: Value = serde_json::from_str(&encoded_work).expect("work JSON");
        assert_eq!(work["archive"]["status"], "kept");
        assert_eq!(work["archiveIntent"]["status"], "terminal");
    }

    async fn reviewer_archive_store(role: &str) -> (Arc<RuntimeStore>, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store"),
        );
        let role = role.to_owned();
        store
            .transaction(move |tx| {
                for agent in [
                    json!({"id":"lead","rootId":"lead","parentId":null,
                        "epoch":1,"status":"running","autoWake":true,"isLead":true}),
                    json!({"id":"child","rootId":"lead","parentId":"lead",
                        "epoch":2,"status":"completed","autoWake":false,"inFlight":false,
                        "role":role}),
                ] {
                    AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?;
                }
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                     VALUES (?1,?2,'child_result',?3,'delivered',1,1,'lead-turn',NULL)",
                    rusqlite::params![
                        "child:result-1",
                        "lead",
                        json!({"agent_id":"child","result":"review complete"}).to_string()
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("reviewer fixture commits");
        (store, root)
    }

    fn reviewer_record(store: &RuntimeStore) -> Value {
        let connection = store.read().expect("runtime store reads");
        AgentRepository::get(&connection, &AgentId::from("child"))
            .expect("reviewer record loads")
            .expect("reviewer exists")
            .value()
    }

    #[tokio::test]
    async fn reviewer_archive_delivered_child_result_schedules_and_archives() {
        let (store, _root) = reviewer_archive_store("reviewer").await;

        accepted_archive_tick(Arc::clone(&store), 10.0)
            .await
            .expect("maintenance tick succeeds");

        let reviewer = reviewer_record(&store);
        assert_eq!(reviewer["reviewArchiveScheduled"], "child:result-1");
        assert_eq!(
            reviewer["agentArchive"]["reason"],
            "Reviewer result delivered to parent"
        );
        assert_eq!(reviewer["agentArchive"]["by"], "lead");
    }

    #[tokio::test]
    async fn reviewer_archive_uses_earliest_matching_parent_event_id() {
        let (store, _root) = reviewer_archive_store("reviewer").await;
        store
            .transaction(|tx| {
                for (id, parent) in [
                    ("child:z-result", "lead"),
                    ("child:a-result", "lead"),
                    ("child:0-wrong-parent", "other-lead"),
                ] {
                    tx.execute(
                        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                         VALUES (?1,?2,'child_result',?3,'delivered',2,1,'lead-turn',NULL)",
                        rusqlite::params![
                            id,
                            parent,
                            json!({"agent_id":"child","result":"review complete"}).to_string()
                        ],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("review events insert");

        accepted_archive_tick(Arc::clone(&store), 10.0)
            .await
            .expect("maintenance tick succeeds");

        let reviewer = reviewer_record(&store);
        assert_eq!(reviewer["reviewArchiveScheduled"], "child:a-result");
        assert_eq!(
            reviewer["agentArchive"]["reason"],
            "Reviewer result delivered to parent"
        );
    }

    #[tokio::test]
    async fn reviewer_archive_skips_existing_archive_markers_before_event_lookup() {
        for marker in [
            json!({"reviewArchiveError":"previous terminal error"}),
            json!({"reviewArchiveScheduled":"child:existing-result"}),
            json!({"agentArchive":{"at":1.0,"epoch":2,"reason":"already archived"}}),
        ] {
            let (store, _root) = reviewer_archive_store("reviewer").await;
            let marker_for_write = marker.clone();
            store
                .transaction(move |tx| {
                    let mut child =
                        AgentRepository::get(tx, &AgentId::from("child"))?.expect("reviewer loads");
                    let mut record = child.value();
                    for (key, value) in marker_for_write.as_object().expect("marker object") {
                        record[key] = value.clone();
                    }
                    child = AgentRecord::from_value(record)?;
                    AgentRepository::put(tx, &child)?;
                    Ok(())
                })
                .await
                .expect("marker inserts");

            accepted_archive_tick(Arc::clone(&store), 10.0)
                .await
                .expect("maintenance tick succeeds");

            let reviewer = reviewer_record(&store);
            for (key, value) in marker.as_object().expect("marker object") {
                assert_eq!(reviewer.get(key), Some(value));
            }
            assert_eq!(
                reviewer.get("reviewArchiveScheduled"),
                marker.get("reviewArchiveScheduled")
            );
        }
    }

    #[tokio::test]
    async fn reviewer_archive_failure_records_error_and_retries_with_event_identity() {
        let (store, _root) = reviewer_archive_store("reviewer").await;
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_work(id,record) VALUES (?1,?2)",
                    rusqlite::params![
                        "open-work",
                        json!({"id":"open-work","rootId":"lead","owner":"child",
                            "status":"ready","version":1,"releases":[]})
                        .to_string()
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("archive blocker inserts");

        accepted_archive_tick(Arc::clone(&store), 10.0)
            .await
            .expect("first tick succeeds");
        let first = reviewer_record(&store);
        assert_eq!(first["reviewArchiveScheduled"], "child:result-1");
        assert_eq!(first["reviewArchiveAttempts"], 1);
        assert_eq!(first["reviewArchiveNextAt"], 11.0);
        assert!(
            first["reviewArchiveError"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        );
        assert!(first.get("agentArchive").is_none());

        accepted_archive_tick(Arc::clone(&store), 11.0)
            .await
            .expect("second tick succeeds");
        let second = reviewer_record(&store);
        assert_eq!(second["reviewArchiveScheduled"], "child:result-1");
        assert_eq!(second["reviewArchiveAttempts"], 2);
        assert_eq!(second["reviewArchiveNextAt"], 13.0);

        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_work WHERE id='open-work'", [])?;
                Ok(())
            })
            .await
            .expect("archive blocker clears");
        accepted_archive_tick(Arc::clone(&store), 12.999)
            .await
            .expect("early retry tick succeeds");
        assert!(reviewer_record(&store).get("agentArchive").is_none());
        accepted_archive_tick(Arc::clone(&store), 13.0)
            .await
            .expect("due retry tick succeeds");
        let archived = reviewer_record(&store);
        assert_eq!(archived["reviewArchiveScheduled"], "child:result-1");
        assert_eq!(
            archived["agentArchive"]["reason"],
            "Reviewer result delivered to parent"
        );
    }

    #[tokio::test]
    async fn reviewer_archive_non_reviewer_child_result_is_not_archived() {
        let (store, _root) = reviewer_archive_store("worker").await;

        accepted_archive_tick(Arc::clone(&store), 10.0)
            .await
            .expect("maintenance tick succeeds");

        let worker = reviewer_record(&store);
        assert!(worker.get("reviewArchiveScheduled").is_none());
        assert!(worker.get("agentArchive").is_none());
    }

    #[test]
    fn assigning_ready_work_persists_a_stable_worker_notification() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction starts");
        let created = write_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "create-assigned-work",
            WorkActorWrite::Create(CreateWork {
                actor_id: "lead".to_owned(),
                title: "Assigned task".to_owned(),
                description: "Do the task".to_owned(),
                owner: Some("worker".to_owned()),
                status: None,
                dependencies: vec![],
            }),
        )
        .expect("assigned task creates");
        assert_eq!(created.record["version"], 1);
        assert!(created.event_inserted);
        let task_id = created.record["id"].as_str().expect("task ID");
        assert_eq!(
            events(&tx),
            vec![(
                format!("work-ready:{task_id}:assignment:1"),
                "worker".to_owned(),
                "work_ready".to_owned(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn assigning_existing_ready_work_queues_the_new_owner() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("reassign", Some("lead"), "ready", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let updated = write_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "reassign-ready-work",
            WorkActorWrite::Mutate(WorkMutation::Update {
                task_id: "reassign".to_owned(),
                version: Some(2),
                title: None,
                description: None,
                owner: Some(Some("worker".to_owned())),
                status: None,
                dependencies: None,
            }),
        )
        .expect("task updates");
        assert_eq!(updated.record["version"], 3);
        assert!(updated.event_inserted);
        assert_eq!(
            events(&tx),
            vec![(
                "work-ready:reassign:assignment:3".to_owned(),
                "worker".to_owned(),
                "work_ready".to_owned(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    fn events(connection: &Connection) -> Vec<(String, String, String)> {
        let mut statement = connection
            .prepare("SELECT id,agent,kind FROM runtime_events ORDER BY id")
            .expect("events query prepares");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("events query runs")
            .map(|row| row.expect("event row reads"))
            .collect()
    }

    fn work(id: &str, owner: Option<&str>, status: &str, results: Value) -> Value {
        json!({"id":id,"rootId":"lead","owner":owner,"status":status,"title":id,
            "description":"","created":1.0,"updated":1.0,"version":2,
            "dependencies":[],"results":results,"decisions":[],"createdBy":"worker"})
    }

    #[test]
    fn vm_evidence_is_rejected_when_assignment_changes_before_commit() {
        let mut connection = database();
        for id in ["worker", "replacement-worker"] {
            let record = json!({
                "id":id,"rootId":"lead","epoch":2,"status":"running","autoWake":true,
                "inFlight":true,"agentMode":"multi","concurrency":8,"executionMode":"vm",
                "layrProjectId":"project","layrLine":id
            });
            connection
                .execute(
                    "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                    rusqlite::params![record.to_string(), id],
                )
                .expect("agent becomes a VM worker");
            if id == "replacement-worker" {
                connection
                    .execute(
                        "INSERT INTO runtime_agents VALUES (?1,?2)",
                        rusqlite::params![id, record.to_string()],
                    )
                    .expect("replacement worker inserts");
            }
        }
        let mut current = work(
            "vm-owner-race",
            Some("replacement-worker"),
            "running",
            json!([]),
        );
        current["version"] = json!(3);
        insert_work(&connection, &current);
        let request_id = "layr-evidence:worker:state-1";
        let intent = json!({
            "status":"completed","requestId":request_id,"taskId":"vm-owner-race",
            "owner":"worker","revision":"state-1","version":2,
            "evidence":{"layrProjectId":"project","layrLine":"worker","layrStateId":"state-1"}
        });
        connection
            .execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3)",
                rusqlite::params![
                    super::vm_intent_id("submission", "vm-owner-race"),
                    "work-write-vm-intent-v1",
                    intent.to_string()
                ],
            )
            .expect("completed guest evidence intent inserts");

        let tx = connection.transaction().expect("transaction starts");
        let result = super::write(
            &tx,
            "lead",
            Some("vm-owner-race-submit"),
            "request-signature",
            WorkWrite::Submit(super::SubmitWork {
                task_id: "vm-owner-race".to_owned(),
                version: None,
                result: "result".to_owned(),
                checks: "checks".to_owned(),
                revision: "state-1".to_owned(),
                files: vec![],
                state_dir: std::env::temp_dir(),
                vm_evidence: Some(intent["evidence"].clone()),
                vm_evidence_request_id: Some(request_id.to_owned()),
            }),
        );
        assert!(matches!(result, Err(WorkWriteError::Event(_))));
        let saved = WorkItemsRepository::work_record(&tx, "vm-owner-race")
            .expect("work row reads")
            .expect("work row remains");
        assert_eq!(saved["owner"], "replacement-worker");
        assert!(
            saved["results"]
                .as_array()
                .expect("results array")
                .is_empty()
        );
        tx.rollback().expect("transaction rolls back");
    }

    #[test]
    fn actor_claim_overrides_the_selected_payload_claimant() {
        let mut connection = database();
        insert_work(&connection, &work("claim", None, "ready", json!([])));
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "worker",
            None,
            "claim-as-worker",
            WorkWrite::Claim(ClaimWork {
                task_id: "claim".to_owned(),
                version: Some(2),
                claimant_id: "lead".to_owned(),
            }),
        )
        .expect("actor claim succeeds");
        assert_eq!(result.record["owner"], "worker");
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn actor_create_rejects_a_nonlead_selected_agent_without_writing() {
        let mut connection = database();
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "worker",
            "lead",
            None,
            "create-with-worker-selected",
            WorkWrite::Create(CreateWork {
                actor_id: "lead".to_owned(),
                title: "must be refused".to_owned(),
                description: String::new(),
                owner: None,
                status: None,
                dependencies: Vec::new(),
            }),
        );
        assert!(matches!(result, Err(WorkWriteError::Event(_))));
        let count: i64 = tx
            .query_row("SELECT count(*) FROM runtime_work", [], |row| row.get(0))
            .expect("work count reads");
        assert_eq!(count, 0);
        tx.rollback().expect("transaction rolls back");
    }

    #[test]
    fn nonlead_actor_cannot_change_assignments_through_actor_api() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("unauthorized-update", Some("worker"), "running", json!([])),
        );
        let before = WorkItemsRepository::work_record(&connection, "unauthorized-update")
            .expect("stored work reads")
            .expect("work exists");
        let tx = connection.transaction().expect("transaction starts");
        let result = write_as_agent(
            &tx,
            "lead",
            "worker",
            None,
            "worker-assignment-update",
            WorkActorWrite::Mutate(WorkMutation::Update {
                task_id: "unauthorized-update".to_owned(),
                version: Some(2),
                title: None,
                description: None,
                owner: Some(Some("lead".to_owned())),
                status: None,
                dependencies: None,
            }),
        );
        assert!(result.is_err());
        let after = WorkItemsRepository::work_record(&tx, "unauthorized-update")
            .expect("stored work reads")
            .expect("work exists");
        assert_eq!(after, before);
        tx.rollback().expect("transaction rolls back");
    }

    #[test]
    fn actor_owner_cancel_notifies_the_owner_itself() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("cancel", Some("worker"), "running", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "worker",
            None,
            "cancel-as-owner",
            WorkWrite::Mutate(WorkMutation::Cancel {
                task_id: "cancel".to_owned(),
                version: Some(2),
                reason: "stop".to_owned(),
            }),
        )
        .expect("owner cancellation succeeds");
        assert_eq!(result.record["status"], "cancelled");
        assert_eq!(
            events(&tx),
            vec![(
                "work-decision:cancel:cancel".to_owned(),
                "worker".to_owned(),
                "work_decision".to_owned(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_cancelling_worker_owned_work_notifies_that_worker() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("lead-cancel", Some("worker"), "running", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "cancel-worker-as-lead",
            WorkWrite::Mutate(WorkMutation::Cancel {
                task_id: "lead-cancel".to_owned(),
                version: Some(2),
                reason: "stop".to_owned(),
            }),
        )
        .expect("lead cancellation succeeds");
        assert_eq!(result.record["decisions"][0]["by"], "lead");
        assert_eq!(
            events(&tx),
            vec![(
                "work-decision:lead-cancel:cancel".into(),
                "worker".into(),
                "work_decision".into(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_accepting_its_own_result_does_not_self_notify() {
        let mut connection = database();
        insert_work(
            &connection,
            &work(
                "accept",
                Some("lead"),
                "review",
                json!([
                    {"id":"result-1","agent":"lead","text":"done","checks":"checked","revision":"abc1234","files":[]}
                ]),
            ),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "accept-as-lead",
            WorkWrite::Accept(AcceptWork {
                task_id: "accept".to_owned(),
                version: Some(2),
                reason: "verified".to_owned(),
                merged_result_id: None,
            }),
        )
        .expect("lead acceptance succeeds");
        assert_eq!(result.record["status"], "accepted");
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE kind='work_decision'",
                [],
                |row| row.get(0),
            )
            .expect("decision event count reads");
        assert_eq!(count, 0);
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_rejecting_its_own_result_does_not_self_notify() {
        let mut connection = database();
        insert_work(
            &connection,
            &work(
                "reject-own",
                Some("lead"),
                "review",
                json!([{"id":"result-own","agent":"lead","text":"done","checks":"checked","revision":"abc1234","files":[]}]),
            ),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "reject-own-as-lead",
            WorkWrite::Mutate(WorkMutation::Reject {
                task_id: "reject-own".to_owned(),
                version: Some(2),
                reason: "needs changes".to_owned(),
            }),
        )
        .expect("lead rejection succeeds");
        assert_eq!(result.record["decisions"][0]["by"], "lead");
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE kind='work_decision'",
                [],
                |row| row.get(0),
            )
            .expect("decision event count reads");
        assert_eq!(count, 0);
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_rejecting_worker_result_notifies_worker() {
        let mut connection = database();
        insert_work(
            &connection,
            &work(
                "reject-worker",
                Some("worker"),
                "review",
                json!([{"id":"result-worker","agent":"worker","text":"done","checks":"checked","revision":"abc1234","files":[]}]),
            ),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "reject-worker-as-lead",
            WorkWrite::Mutate(WorkMutation::Reject {
                task_id: "reject-worker".to_owned(),
                version: Some(2),
                reason: "needs changes".to_owned(),
            }),
        )
        .expect("lead rejection succeeds");
        assert_eq!(result.record["decisions"][0]["by"], "lead");
        assert_eq!(
            events(&tx),
            vec![(
                "work-decision:reject-worker:2".into(),
                "worker".into(),
                "work_decision".into(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn actor_submit_records_worker_and_notifies_the_lead() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("submit", Some("worker"), "running", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "worker",
            None,
            "submit-as-worker",
            WorkWrite::Submit(super::SubmitWork {
                task_id: "submit".to_owned(),
                version: Some(2),
                result: "done".to_owned(),
                checks: "checked".to_owned(),
                revision: "abc1234".to_owned(),
                files: vec![],
                state_dir: std::env::temp_dir(),
                vm_evidence: None,
                vm_evidence_request_id: None,
            }),
        )
        .expect("worker submission succeeds");
        assert_eq!(result.record["results"][0]["agent"], "worker");
        assert_eq!(
            events(&tx),
            vec![(
                format!(
                    "work-result:{}",
                    result.record["results"][0]["id"]
                        .as_str()
                        .expect("result ID")
                ),
                "lead".to_owned(),
                "work_review".to_owned(),
            )]
        );
        tx.commit().expect("transaction commits");
    }

    #[test]
    fn lead_actor_submitting_its_own_work_does_not_create_a_review_event() {
        let mut connection = database();
        insert_work(
            &connection,
            &work("lead-submit", Some("lead"), "running", json!([])),
        );
        let tx = connection.transaction().expect("transaction starts");
        let result = write_action_as_agent(
            &tx,
            "lead",
            "lead",
            None,
            "submit-as-lead",
            WorkWrite::Submit(super::SubmitWork {
                task_id: "lead-submit".to_owned(),
                version: Some(2),
                result: "done".to_owned(),
                checks: "checked".to_owned(),
                revision: "abc1234".to_owned(),
                files: vec![],
                state_dir: std::env::temp_dir(),
                vm_evidence: None,
                vm_evidence_request_id: None,
            }),
        )
        .expect("lead submission succeeds");
        assert_eq!(result.record["results"][0]["agent"], "lead");
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM runtime_events WHERE kind='work_review'",
                [],
                |row| row.get(0),
            )
            .expect("review event count reads");
        assert_eq!(count, 0);
        tx.commit().expect("transaction commits");
    }
}

#[cfg(test)]
mod release_tests {
    use super::{continuation_work_pending, release_failed_work};
    use rusqlite::Connection;
    use serde_json::{Value, json};
    use studio_runtime_store::AgentRecord;

    #[test]
    fn continuation_identity_only_preserves_an_exact_pending_retry() {
        let agent = AgentRecord::from_value(json!({
            "id":"worker","rootId":"lead","epoch":7,"status":"failed",
            "autoWake":true,"accountKey":"acct","threadId":"thread",
            "lastCompletedTurn":"turn-3",
            "capacityRetry":{"status":"starting","accountKey":"acct","threadId":"thread","epoch":7,"turnId":"turn-3"}
        }))
        .expect("worker record parses");
        assert!(continuation_work_pending(&agent));

        let mut unrelated = agent.value();
        unrelated["capacityRetry"]["turnId"] = json!("older-turn");
        let unrelated = AgentRecord::from_value(unrelated).expect("worker record parses");
        assert!(!continuation_work_pending(&unrelated));

        let mut explicit_null = agent.value();
        explicit_null["accountKey"] = Value::Null;
        explicit_null["capacityRetry"]
            .as_object_mut()
            .expect("capacity retry is an object")
            .remove("accountKey");
        let explicit_null = AgentRecord::from_value(explicit_null).expect("worker record parses");
        assert!(!continuation_work_pending(&explicit_null));
    }

    #[test]
    fn service_releases_failed_worker_claim_and_emits_recovery_event() {
        let mut connection = Connection::open_in_memory().expect("memory SQLite opens");
        connection.execute_batch(
            "CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
             CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);",
        ).expect("service tables create");
        for agent in [
            json!({"id":"lead","rootId":"lead","epoch":1,"status":"paused","autoWake":false}),
            json!({"id":"worker","rootId":"lead","epoch":2,"status":"failed","parentId":"lead","autoWake":false,"error":"worker exited"}),
        ] {
            connection
                .execute(
                    "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                    rusqlite::params![agent["id"].as_str().unwrap_or_default(), agent.to_string()],
                )
                .expect("agent inserts");
        }
        let work = json!({
            "id":"task","rootId":"lead","owner":"worker","status":"running",
            "title":"Task","created":1.0,"updated":1.0,"version":2,
            "dependencies":[],"results":[],"decisions":[]
        });
        let blocked_work = json!({
            "id":"blocked-task","rootId":"lead","owner":"worker","status":"blocked",
            "title":"Blocked task","created":1.0,"updated":1.0,"version":2,
            "dependencies":[],"results":[],"decisions":[]
        });
        connection
            .execute(
                "INSERT INTO runtime_work(id,record) VALUES ('task',?1),('blocked-task',?2)",
                rusqlite::params![work.to_string(), blocked_work.to_string()],
            )
            .expect("work items insert");
        let failed = AgentRecord::from_value(json!({
            "id":"worker","rootId":"lead","epoch":2,"status":"failed",
            "parentId":"lead","autoWake":false,"error":"worker exited"
        }))
        .expect("worker record parses");

        let tx = connection.transaction().expect("transaction starts");
        let released = release_failed_work(&tx, &[failed], 10.0).expect("claims release");
        let mut released_ids = released.task_ids.clone();
        released_ids.sort();
        assert_eq!(released_ids, ["blocked-task", "task"]);
        assert!(released.event_inserted);
        let stored: Value = serde_json::from_str(
            &tx.query_row(
                "SELECT record FROM runtime_work WHERE id='task'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("released record reads"),
        )
        .expect("released JSON parses");
        assert_eq!(stored["owner"], Value::Null);
        assert_eq!(stored["status"], "ready");
        assert_eq!(stored["releases"][0]["reason"], "worker exited");
        let blocked: Value = serde_json::from_str(
            &tx.query_row(
                "SELECT record FROM runtime_work WHERE id='blocked-task'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("blocked release reads"),
        )
        .expect("blocked work JSON parses");
        assert_eq!(blocked["owner"], Value::Null);
        assert_eq!(blocked["status"], "blocked");
        assert_eq!(blocked["version"], 3);
        let event = tx
            .query_row(
                "SELECT kind,text FROM runtime_events WHERE id='work-released:task:3'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("recovery event is persisted");
        assert_eq!(event.0, "work_released");
        assert!(event.1.contains("worker exited"));
        tx.commit().expect("transaction commits");
    }
}
