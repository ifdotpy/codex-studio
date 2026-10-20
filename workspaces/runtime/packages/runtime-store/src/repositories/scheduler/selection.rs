use super::*;
use crate::CachedSql;

const ACCOUNT_WAKE_INDEX: &str = "runtime_agent_account_wake_candidates_v3";
/// Earlier index names: the first lacked busy steer candidates, and `_v2`
/// limited them to Claude turns.
const RETIRED_ACCOUNT_WAKE_INDEXES: [&str; 2] = [
    "runtime_agent_account_wake_candidates",
    "runtime_agent_account_wake_candidates_v2",
];

/// Owns the agent-only superset of all rows the account wake page may return.
/// Event existence and invalid-agent hold receipts are checked after this
/// indexed candidate selection because SQLite partial indexes cannot use them.
fn account_wake_candidate_predicate(record: &str) -> String {
    format!(
        "({} OR {} OR {} OR {})",
        account_wake_queued_candidate(record),
        account_wake_steer_candidate(record),
        account_wake_capacity_candidate(record),
        account_wake_recovery(record)
    )
}

fn account_wake_index_eligibility(record: &str) -> String {
    format!(
        "CASE WHEN json_valid({record}) THEN ( \
           json_extract({record},'$.deletedAt') IS NULL \
           AND COALESCE(json_extract({record},'$.remoteWorker'),0)=0 \
           AND COALESCE(json_extract({record},'$.remoteAnchor'),0)=0 \
           AND {} ) ELSE 0 END",
        account_wake_candidate_predicate(record)
    )
}

fn account_wake_queued_candidate(record: &str) -> String {
    format!(
        "(json_extract({record},'$.status')='queued' \
         AND json_extract({record},'$.autoWake')=1 \
         AND COALESCE(json_extract({record},'$.inFlight'),0)=0 \
         AND COALESCE(json_extract({record},'$.nativeFailureHold'),0)=0)"
    )
}

/// A running turn that may receive queued user input by `turn/steer`.
/// `SchedulerRepository::claim_steer` owns the exact eligibility check.
fn account_wake_steer_candidate(record: &str) -> String {
    format!(
        "(json_extract({record},'$.status')='running' \
         AND json_extract({record},'$.autoWake')=1 \
         AND json_extract({record},'$.inFlight')=1 \
         AND json_type({record},'$.turnId')='text' \
         AND COALESCE(json_extract({record},'$.steerRejectedTurnId'),'')<>json_extract({record},'$.turnId') \
         AND COALESCE(json_extract({record},'$.nativeFailureHold'),0)=0 \
         AND COALESCE(json_extract({record},'$.executionMode'),'')<>'vm')"
    )
}

fn account_wake_capacity_candidate(record: &str) -> String {
    format!(
        "(json_extract({record},'$.startAttempt.action')='capacity' \
         AND json_extract({record},'$.startAttempt.submitted')=0 \
         AND json_extract({record},'$.capacityRetry.id')= \
             json_extract({record},'$.startAttempt.capacityRetryId') \
         AND json_extract({record},'$.status')='starting')"
    )
}

/// Index over pending child results by creation time, for the release deadline.
const CHILD_RESULT_PENDING_INDEX: &str = "runtime_event_child_result_pending";
/// Index over agents by parent, for the active-sibling check.
const AGENT_PARENT_INDEX: &str = "runtime_agent_valid_parent";

/// The indexed parent key of an agent record; malformed JSON has no parent.
fn agent_parent_key(record: &str) -> String {
    format!("CASE WHEN json_valid({record}) THEN json_extract({record},'$.parentId') END")
}

/// SQL predicate: the pending event aliased `event` is a `child_result` that
/// still waits for an active sibling child of the same parent. `held_after`
/// binds the Unix time before which results are no longer held (now minus the
/// effective hold), so the comparison stays on the indexed `created` column.
/// The unary `+` drops the column's text affinity so the parent expression
/// index can seek on the correlated agent id.
fn child_result_held(event: &str, held_after: &str) -> String {
    format!(
        "({event}.kind='child_result' AND {event}.created>{held_after} \
         AND EXISTS(SELECT 1 FROM runtime_agents c WHERE {}=+{event}.agent \
           AND CASE WHEN json_valid(c.record) THEN ( \
             json_extract(c.record,'$.deletedAt') IS NULL \
             AND json_extract(c.record,'$.status') IN ('queued','starting','running')) \
           ELSE 0 END))",
        agent_parent_key("c.record")
    )
}

/// The earliest `created` of a held child result; `?1` is the `held_after` bound.
fn child_result_release_query() -> String {
    format!(
        "SELECT MIN(e.created) FROM runtime_events e INDEXED BY {CHILD_RESULT_PENDING_INDEX} \
         WHERE e.kind='child_result' AND e.status='pending' AND {}",
        child_result_held("e", "?1")
    )
}

/// The `held_after` bound of [`child_result_held`] at Unix time `now`, from
/// the stored orchestration setting read on every call.
fn child_result_held_after(connection: &Connection, now: f64) -> StoreResult<f64> {
    Ok(now
        - crate::OrchestrationSettingsRepository::child_result_coalesce(connection)?.as_secs_f64())
}

/// SQL predicate: agent `agent` has a pending event of `epoch` that may start
/// a turn; `held_after` is the bound of [`child_result_held`].
fn deliverable_pending(agent: &str, epoch: &str, held_after: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM runtime_events e WHERE e.agent={agent} \
         AND e.epoch={epoch} AND e.status='pending' AND NOT {})",
        child_result_held("e", held_after)
    )
}

/// A queued native review child whose first turn is its stored review. Python
/// `codex_agent_review.claim` starts it without a queued input event.
fn account_wake_native_review(record: &str) -> String {
    format!(
        "({} AND json_extract({record},'$.nativeReview.status')='pending')",
        account_wake_queued_candidate(record)
    )
}

fn account_wake_pending(record: &str, held_after: &str) -> String {
    format!(
        "(({} AND {}) \
         OR ({} AND EXISTS(SELECT 1 FROM runtime_events e WHERE e.agent=a.id \
             AND e.epoch=json_extract({record},'$.epoch') AND e.status='pending' \
             AND e.kind='user')) \
         OR {} OR {})",
        account_wake_queued_candidate(record),
        deliverable_pending(
            "a.id",
            &format!("json_extract({record},'$.epoch')"),
            held_after,
        ),
        account_wake_steer_candidate(record),
        account_wake_capacity_candidate(record),
        account_wake_native_review(record)
    )
}

fn account_wake_recovery(record: &str) -> String {
    format!(
        "((COALESCE(json_extract({record},'$.provider'),'codex')='codex' \
           AND json_extract({record},'$.startAttempt.submitted')=1 \
           AND json_extract({record},'$.startAttempt.connectionGeneration') IS NOT NULL \
           AND json_extract({record},'$.status') IN \
               ('starting','running','waiting','paused','interrupted')) \
         OR (json_type({record},'$.disconnectRecovery')='object' AND ( \
           COALESCE(json_extract({record},'$.status'),'') \
               NOT IN ('completed','waiting','paused','parked') \
           OR json_extract({record},'$.disconnectRecovery.stage') IS NOT NULL)) \
         OR (json_type({record},'$.restartRecovery')='object' AND ( \
           COALESCE(json_extract({record},'$.status'),'') \
               NOT IN ('completed','waiting','paused','parked') \
           OR COALESCE(json_extract({record},'$.restartRecovery.stage'),'') \
               NOT IN ('finished','continued','reattached','input_restored'))))"
    )
}

fn account_wake_target_filter(record: &str, held_after: &str) -> String {
    let eligibility = account_wake_index_eligibility(record);
    let pending = account_wake_pending(record, held_after);
    let recovery = account_wake_recovery(record);
    format!(
        "{eligibility} \
         AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts h \
           WHERE h.id=?1 || a.id AND h.signature=?2) \
         AND (({pending}) OR ({recovery}))"
    )
}

fn account_wake_invalid_target_query() -> String {
    let target_filter = account_wake_target_filter("a.record", "?3");
    format!(
        "SELECT a.id FROM runtime_agents AS a INDEXED BY {ACCOUNT_WAKE_INDEX} \
         WHERE {target_filter} \
           AND (COALESCE(json_extract(a.record,'$.accountKey'),'default')='' \
             OR json_type(a.record,'$.accountKey') NOT IN ('null','text')) LIMIT 1"
    )
}

fn validate_account_wake_targets(connection: &Connection, held_after: f64) -> StoreResult<()> {
    if let Some(id) = connection
        .query_row(
            &account_wake_invalid_target_query(),
            rusqlite::params![
                crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                held_after
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Err(crate::StoreError::InvalidStoredObject {
            table: "runtime_agents",
            id,
        });
    }
    Ok(())
}

fn account_wake_accounts_query() -> String {
    let target_filter = account_wake_target_filter("a.record", "?5");
    let account = "COALESCE(json_extract(a.record,'$.accountKey'),'default')";
    format!(
        "SELECT DISTINCT {account} FROM runtime_agents AS a \
         INDEXED BY {ACCOUNT_WAKE_INDEX} WHERE {target_filter} \
         AND {account}>?3 ORDER BY {account} LIMIT ?4"
    )
}

fn account_wake_account_target_query() -> String {
    let target_filter = account_wake_target_filter("a.record", "?4");
    let account = "COALESCE(json_extract(a.record,'$.accountKey'),'default')";
    format!(
        "SELECT 1 FROM runtime_agents AS a INDEXED BY {ACCOUNT_WAKE_INDEX} \
         WHERE {target_filter} AND {account}=?3 LIMIT 1"
    )
}

fn account_wake_pending_agents_query() -> String {
    let pending = account_wake_pending("a.record", "?6");
    let eligibility = account_wake_index_eligibility("a.record");
    let account = "COALESCE(json_extract(a.record,'$.accountKey'),'default')";
    format!(
        "SELECT a.id,json_extract(a.record,'$.epoch'),json_extract(a.record,'$.workspaceMode') \
         FROM runtime_agents AS a INDEXED BY {ACCOUNT_WAKE_INDEX} \
         WHERE {eligibility} AND {account}=?1 AND a.id>?2 \
           AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts h \
             WHERE h.id=?3 || a.id AND h.signature=?4) \
           AND ({pending}) ORDER BY a.id LIMIT ?5"
    )
}

impl SchedulerRepository {
    /// Install the candidate index used by worker reconciliation. This index
    /// deliberately contains only agent-record predicates; pending-event and
    /// invalid-agent-hold receipt checks remain in the consuming queries.
    pub(crate) fn prepare_account_wake_index(transaction: &Transaction<'_>) -> StoreResult<()> {
        for retired in RETIRED_ACCOUNT_WAKE_INDEXES {
            transaction.execute_batch(&format!("DROP INDEX IF EXISTS {retired}"))?;
        }
        transaction.execute_batch(&Self::account_wake_index_definition().create_sql)?;
        Ok(())
    }

    /// Canonical DDL for the repository-owned account wake candidate index.
    #[doc(hidden)]
    pub fn account_wake_index_definition() -> crate::OwnedRuntimeIndexDefinition {
        let eligibility = account_wake_index_eligibility("record");
        crate::OwnedRuntimeIndexDefinition {
            name: ACCOUNT_WAKE_INDEX,
            table: "runtime_agents",
            create_sql: format!(
                "CREATE INDEX IF NOT EXISTS {ACCOUNT_WAKE_INDEX} \
                 ON runtime_agents(COALESCE(json_extract(record,'$.accountKey'),'default'),id) \
                 WHERE {eligibility}"
            ),
        }
    }

    /// Return one bounded account/agent page for worker startup and recovery.
    /// The account cursor wraps when it reaches the final account; per-account
    /// agent cursors are supplied by the caller and wrap at the final agent.
    pub fn account_wake_page(
        connection: &Connection,
        after_account: &str,
        after_agents: &BTreeMap<String, String>,
        account_limit: usize,
        agent_limit: usize,
        now: f64,
    ) -> StoreResult<AccountWakePage> {
        let held_after = child_result_held_after(connection, now)?;
        validate_account_wake_targets(connection, held_after)?;

        let mut account_statement = connection.prepare_cached(&account_wake_accounts_query())?;
        let mut read_accounts = |after: &str| -> rusqlite::Result<Vec<String>> {
            account_statement
                .query_map(
                    rusqlite::params![
                        crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                        crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                        after,
                        account_limit as i64,
                        held_after
                    ],
                    |row| row.get(0),
                )?
                .collect()
        };
        let mut selected_accounts = read_accounts(after_account)?;
        if selected_accounts.is_empty() && !after_account.is_empty() {
            selected_accounts = read_accounts("")?;
        }
        drop(account_statement);

        let mut agent_statement = connection.prepare_cached(&account_wake_pending_agents_query())?;
        let mut result = BTreeMap::new();
        for account in selected_accounts {
            let after = after_agents.get(&account).map(String::as_str).unwrap_or("");
            let mut read_agents =
                |after: &str| -> rusqlite::Result<Vec<(String, i64, Option<String>)>> {
                    agent_statement
                        .query_map(
                            rusqlite::params![
                                account,
                                after,
                                crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                                crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                                agent_limit as i64,
                                held_after
                            ],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )?
                        .collect()
                };
            let mut agents = read_agents(after)?;
            if agents.is_empty() && !after.is_empty() {
                agents = read_agents("")?;
            }
            result.insert(account, agents);
        }
        Ok(result)
    }

    /// Return the wake page of only the named accounts: an account is present when it
    /// still has a pending or recovery target, with its first bounded page of pending
    /// agents. Cost depends on the named accounts, not on the number of accounts.
    pub fn account_wake_for_accounts<'a>(
        connection: &Connection,
        accounts: impl IntoIterator<Item = &'a str>,
        agent_limit: usize,
        now: f64,
    ) -> StoreResult<AccountWakePage> {
        let held_after = child_result_held_after(connection, now)?;
        validate_account_wake_targets(connection, held_after)?;
        let mut target_statement =
            connection.prepare_cached(&account_wake_account_target_query())?;
        let mut agent_statement = connection.prepare_cached(&account_wake_pending_agents_query())?;
        let mut result = BTreeMap::new();
        for account in accounts {
            let has_target = target_statement.exists(rusqlite::params![
                crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                account,
                held_after
            ])?;
            if !has_target {
                continue;
            }
            let agents = agent_statement
                .query_map(
                    rusqlite::params![
                        account,
                        "",
                        crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                        crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                        agent_limit as i64,
                        held_after
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            result.insert(account.to_owned(), agents);
        }
        Ok(result)
    }

    /// Return a bounded account page strictly after the account cursor. Agent
    /// cursors retain their ordinary wrap behavior; only the account cursor is
    /// strict so callers can drain a real wake without cycling over its prefix.
    pub fn account_wake_page_after(
        connection: &Connection,
        after_account: &str,
        after_agents: &BTreeMap<String, String>,
        account_limit: usize,
        agent_limit: usize,
        now: f64,
    ) -> StoreResult<AccountWakePage> {
        let held_after = child_result_held_after(connection, now)?;
        validate_account_wake_targets(connection, held_after)?;

        let mut account_statement = connection.prepare_cached(&account_wake_accounts_query())?;
        let accounts = account_statement
            .query_map(
                rusqlite::params![
                    crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                    crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                    after_account,
                    account_limit as i64,
                    held_after
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(account_statement);

        let mut agent_statement = connection.prepare_cached(&account_wake_pending_agents_query())?;
        let mut result = BTreeMap::new();
        for account in accounts {
            let after = after_agents.get(&account).map(String::as_str).unwrap_or("");
            let mut read_agents =
                |after: &str| -> rusqlite::Result<Vec<(String, i64, Option<String>)>> {
                    agent_statement
                        .query_map(
                            rusqlite::params![
                                account,
                                after,
                                crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                                crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                                agent_limit as i64,
                                held_after
                            ],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )?
                        .collect()
                };
            let mut agents = read_agents(after)?;
            if agents.is_empty() && !after.is_empty() {
                agents = read_agents("")?;
            }
            result.insert(account, agents);
        }
        Ok(result)
    }

    /// Return a bounded page for one account strictly after the supplied agent
    /// cursor. Unlike `account_wake_page`, this continuation never wraps; the
    /// caller uses it only to drain a page already selected by a real wake.
    pub fn account_wake_agents_after(
        connection: &Connection,
        account: &str,
        after_agent: &str,
        agent_limit: usize,
        now: f64,
    ) -> StoreResult<Vec<(String, i64, Option<String>)>> {
        let held_after = child_result_held_after(connection, now)?;
        validate_account_wake_targets(connection, held_after)?;
        let mut statement = connection.prepare_cached(&account_wake_pending_agents_query())?;
        let rows = statement
            .query_map(
                rusqlite::params![
                    account,
                    after_agent,
                    crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                    crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                    agent_limit as i64,
                    held_after
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The Unix time at which the earliest held `child_result` stops waiting
    /// for its siblings, so a scheduler can wake without a new commit.
    pub fn child_result_release_at(connection: &Connection, now: f64) -> StoreResult<Option<f64>> {
        let hold = crate::OrchestrationSettingsRepository::child_result_coalesce(connection)?;
        let earliest: Option<f64> = connection.query_row(
            &child_result_release_query(),
            [now - hold.as_secs_f64()],
            |row| row.get(0),
        )?;
        Ok(earliest.map(|created| created + hold.as_secs_f64()))
    }

    /// Install the indexes that keep the child-result hold off full scans.
    pub(crate) fn prepare_child_result_hold_indexes(
        transaction: &Transaction<'_>,
    ) -> StoreResult<()> {
        for definition in Self::child_result_hold_index_definitions() {
            transaction.execute_batch(&definition.create_sql)?;
        }
        Ok(())
    }

    /// Canonical DDL for the child-result hold indexes.
    #[doc(hidden)]
    pub fn child_result_hold_index_definitions() -> Vec<crate::OwnedRuntimeIndexDefinition> {
        vec![
            crate::OwnedRuntimeIndexDefinition {
                name: CHILD_RESULT_PENDING_INDEX,
                table: "runtime_events",
                create_sql: format!(
                    "CREATE INDEX IF NOT EXISTS {CHILD_RESULT_PENDING_INDEX} \
                     ON runtime_events(created) WHERE kind='child_result' AND status='pending'"
                ),
            },
            crate::OwnedRuntimeIndexDefinition {
                name: AGENT_PARENT_INDEX,
                table: "runtime_agents",
                create_sql: format!(
                    "CREATE INDEX IF NOT EXISTS {AGENT_PARENT_INDEX} ON runtime_agents({})",
                    agent_parent_key("record")
                ),
            },
        ]
    }

    /// The same metadata identity used by claim replay and submit authorization.
    /// Diagnostic timing fields may change while preparation is running.
    pub fn input_metadata_identity(metadata: &Value) -> Value {
        business_metadata(Some(metadata))
    }

    /// The text a claim carries for one stored event and its metadata.
    pub fn claimed_event_text(text: &str, metadata: &Value) -> String {
        model_event_text(text, Some(metadata)).unwrap_or_else(|| text.to_owned())
    }

    /// Return queued local agents with pending events in the current epoch.
    pub fn pending_agents(connection: &Connection) -> StoreResult<Vec<(String, i64)>> {
        let mut statement = connection.prepare_cached(
            "SELECT a.id,json_extract(a.record,'$.epoch') FROM runtime_agents a \
             WHERE json_extract(a.record,'$.status')='queued' \
             AND json_extract(a.record,'$.autoWake')=1 \
             AND COALESCE(json_extract(a.record,'$.inFlight'),0)=0 \
             AND COALESCE(json_extract(a.record,'$.nativeFailureHold'),0)=0 \
             AND COALESCE(json_extract(a.record,'$.remoteWorker'),0)=0 \
             AND COALESCE(json_extract(a.record,'$.remoteAnchor'),0)=0 \
             AND EXISTS(SELECT 1 FROM runtime_events e WHERE e.agent=a.id \
                        AND e.epoch=json_extract(a.record,'$.epoch') AND e.status='pending') \
             ORDER BY a.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Claim the oldest pending events and persist the matching unsubmitted attempt atomically.
    pub fn claim(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        attempt_created: f64,
        limit: usize,
    ) -> StoreResult<Option<ClaimedEvents>> {
        Self::claim_selected(
            transaction,
            agent_id,
            epoch,
            attempt_id,
            attempt_created,
            limit,
            false,
            None,
        )
    }

    /// Claim a batch whose admission is owned by the checked preparation adapter.
    pub fn claim_for_preparation(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        attempt_created: f64,
        limit: usize,
    ) -> StoreResult<Option<ClaimedEvents>> {
        Self::claim_selected(
            transaction,
            agent_id,
            epoch,
            attempt_id,
            attempt_created,
            limit,
            true,
            None,
        )
    }

    /// Claim exactly one radio turn selected by the runtime-services policy in
    /// this transaction. The caller must authorize the event before invoking
    /// this entry point; this method fences its pending identity and journals
    /// the same durable attempt as the ordinary claim path.
    pub fn claim_selected_radio_turn(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        attempt_created: f64,
        event_id: &str,
    ) -> StoreResult<Option<ClaimedEvents>> {
        Self::claim_selected(
            transaction,
            agent_id,
            epoch,
            attempt_id,
            attempt_created,
            1,
            true,
            Some(event_id),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn claim_selected(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
        attempt_created: f64,
        limit: usize,
        preparation: bool,
        radio_event: Option<&str>,
    ) -> StoreResult<Option<ClaimedEvents>> {
        let Some(mut agent) = load_agent(transaction, agent_id)? else {
            return Ok(None);
        };
        if agent
            .extra
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
        {
            return Ok(None);
        }
        if let Some(saved) = crate::ExecutionHistoryRepository::attempt(transaction, attempt_id)? {
            if saved.get("claimResult").is_none() {
                return Err(crate::ExecutionHistoryRepository::invalid(
                    "runtime_execution_attempts",
                    attempt_id,
                ));
            }
            return replay_claim(transaction, agent, epoch, &saved).map(Some);
        }
        if agent.id.0 != agent_id
            || agent.epoch != epoch
            || agent.status != AgentStatus::Queued
            || !continuation_allowed(&agent)
            || agent.extra.get("autoWake").and_then(Value::as_bool) != Some(true)
            || agent.extra.get("inFlight").and_then(Value::as_bool) == Some(true)
            || agent.extra.get("nativeFailureHold").is_some_and(truthy)
            || agent.extra.get("remoteWorker").is_some_and(truthy)
            || agent.extra.get("remoteAnchor").is_some_and(truthy)
        {
            return Ok(None);
        }
        // Python's dispatcher claims a pending native review before queued
        // input (`codex_agent_review.claim`); the review driver owns this turn.
        if agent
            .extra
            .get("nativeReview")
            .and_then(|review| review.get("status"))
            .and_then(Value::as_str)
            == Some("pending")
        {
            return Ok(None);
        }
        // A claim waits for input that another claim still owns. An `uncertain`
        // input is never claimed again, and, as in Python `pending_batch`, it
        // does not stop new input: one old unknown delivery must not block
        // the worker for good (agent 841ebc0e, 18 October 2026).
        let unresolved = transaction
            .query_row_cached(
                "SELECT 1 FROM runtime_events WHERE agent=?1 \
                 AND status IN ('reserved','dispatching') LIMIT 1",
                [agent_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if unresolved {
            return Ok(None);
        }
        if limit == 0 {
            return Ok(None);
        }
        // Held child results wait for their siblings; any deliverable event
        // claims the whole ordered prefix, held results included.
        if radio_event.is_none()
            && !transaction.query_row(
                &format!("SELECT {}", deliverable_pending("?1", "?2", "?3")),
                params![
                    agent_id,
                    epoch,
                    child_result_held_after(transaction, attempt_created)?
                ],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Ok(None);
        }

        // Python gives radio turns priority over the ordinary queue prefix.
        if radio_event.is_none()
            && transaction.query_row_cached(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 \
             AND status='pending' AND kind IN ('radio','radio_turn'))",
                params![agent_id, epoch],
                |row| row.get::<_, bool>(0),
            )?
        {
            return Ok(Some(refused(
                &agent,
                "radio-turn requires radio-policy".to_owned(),
            )));
        }

        let prefix = limit.min(MAX_EVENTS_PER_TURN);
        let has_metadata = table_exists(transaction, "runtime_event_meta")?;
        if has_metadata {
            let radio = transaction
                .query_row_cached(
                    "SELECT e.id,m.record FROM runtime_events e \
                     JOIN runtime_event_meta m ON m.id=e.id \
                     WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
                     AND json_valid(m.record) \
                     AND json_type(m.record,'$.radioAnswerTurnId') IS NOT NULL \
                     AND json_type(m.record,'$.radioAnswerTurnId')!='null' \
                     ORDER BY e.created,e.rowid LIMIT 1",
                    params![agent_id, epoch],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some((id, raw)) = radio {
                let value: Value = serde_json::from_str(&raw)
                    .map_err(|_| invalid_object("runtime_event_meta", &id))?;
                if !value.get("radioAnswerTurnId").is_some_and(Value::is_string) {
                    return Err(invalid_object("runtime_event_meta", &id));
                }
                return Ok(Some(refused(
                    &agent,
                    format!("radio answer {id} requires radio-policy"),
                )));
            }
        }
        let sql = if let Some(_event_id) = radio_event {
            if has_metadata {
                "SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,m.record \
                 FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id \
                 WHERE e.id=?1 AND e.agent=?2 AND e.epoch=?3 AND e.status='pending' AND e.kind='radio_turn'"
            } else {
                "SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,NULL \
                 FROM runtime_events e WHERE e.id=?1 AND e.agent=?2 AND e.epoch=?3 AND e.status='pending' AND e.kind='radio_turn'"
            }
        } else if has_metadata {
            "SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,m.record \
             FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id \
             WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
             ORDER BY e.created,e.rowid LIMIT ?3"
        } else {
            "SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,NULL \
             FROM runtime_events e WHERE e.agent=?1 AND e.epoch=?2 AND e.status='pending' \
             ORDER BY e.created,e.rowid LIMIT ?3"
        };
        let mut statement = transaction.prepare_cached(sql)?;
        let joined = if let Some(event_id) = radio_event {
            statement
                .query_map(params![event_id, agent_id, epoch], |row| {
                    Ok((
                        crate::QueueEventRepository::from_row(row)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            statement
                .query_map(params![agent_id, epoch, prefix as i64], |row| {
                    Ok((
                        crate::QueueEventRepository::from_row(row)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        drop(statement);
        if joined.is_empty() {
            return Ok(None);
        }
        let mut events = Vec::with_capacity(joined.len());
        let mut metadata = BTreeMap::new();
        for (event, raw) in joined {
            if let Some(raw) = raw {
                let value: Value = serde_json::from_str(&raw)
                    .map_err(|_| invalid_object("runtime_event_meta", &event.id.0))?;
                let valid = value.is_object()
                    && value.get("assets").is_none_or(Value::is_array)
                    && value
                        .get("radioAnswerTurnId")
                        .is_none_or(|turn| turn.is_null() || turn.is_string());
                if !valid {
                    return Err(invalid_object("runtime_event_meta", &event.id.0));
                }
                metadata.insert(event.id.0.clone(), value);
            }
            events.push(event);
        }

        if let Some(event) = events.iter().find(|event| {
            metadata
                .get(&event.id.0)
                .and_then(|value| value.get("radioAnswerTurnId"))
                .is_some_and(|turn| !turn.is_null())
        }) {
            return Ok(Some(refused(
                &agent,
                format!("radio answer {} requires radio-policy", event.id.0),
            )));
        }
        let (complaints, _work) = load_policy_records(transaction, &agent, &events)?;
        let obsolete_work = obsolete_work_review_ids(transaction, &agent, &events)?;
        let prepared =
            prepare_pending_batch(&agent, events, metadata, &complaints, &obsolete_work)?;
        if radio_event.is_none()
            && !preparation
            && let Some((kind, dependency)) = prepared.events.iter().find_map(|event| {
                let dependency =
                    event_policy_dependency(&event.kind).or(match event.kind.as_str() {
                        "complaint" => Some("complaint-policy"),
                        "work_review" => Some("work-policy"),
                        _ => None,
                    });
                dependency.map(|dependency| (event.kind.as_str(), dependency))
            })
        {
            return Ok(Some(refused(
                &agent,
                format!("scheduler cannot claim {kind}: requires {dependency}"),
            )));
        }
        let take_single_event = radio_event.is_some()
            || (preparation
                && prepared
                    .events
                    .first()
                    .is_some_and(|e| !shares_turn(&e.kind, true)));
        let mut events = if take_single_event {
            prepared.events.iter().take(1).cloned().collect()
        } else {
            select_turn_batch(&prepared.events, &prepared.metadata, prefix, preparation)
        };

        let unsupported = if radio_event.is_some() {
            None
        } else if agent.extra.get("pendingSettings").is_some_and(truthy) {
            Some("pendingSettings are not supported by the Rust turn driver; apply settings first.")
        } else if agent.extra.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
            && agent
                .extra
                .get("imageWorkspaceReady")
                .and_then(Value::as_bool)
                != Some(true)
        {
            Some("image workspace is not ready; prepare the workspace before submitting input.")
        } else {
            events
                .iter()
                .find(|event| !matches!(event.kind.as_str(), "user" | "complaint"))
                .map(|_| "event kind is not supported by the Rust turn driver.")
        };
        if !preparation && let Some(reason) = unsupported {
            agent.status = AgentStatus::Waiting;
            agent
                .extra
                .insert("inFlight".to_owned(), Value::Bool(false));
            agent.extra.insert("turnId".to_owned(), Value::Null);
            agent
                .extra
                .insert("nativeFailureHold".to_owned(), Value::Bool(true));
            agent
                .extra
                .insert("error".to_owned(), Value::String(reason.to_owned()));
            agent.extra.insert(
                "startOutcomeHold".to_owned(),
                json!({"stage":"held","reason":reason}),
            );
            AgentRepository::put(transaction, &agent)?;
            return Ok(None);
        }
        if events.is_empty() {
            apply_preparation(transaction, &prepared)?;
            set_waiting(transaction, &mut agent, agent_id)?;
            return Ok(None);
        }
        apply_preparation(transaction, &prepared)?;
        let metadata = prepared.metadata;

        for event in &mut events {
            let changed = transaction.execute_cached(
                "UPDATE runtime_events SET status='reserved' WHERE id=?1 AND agent=?2 \
                 AND epoch=?3 AND status='pending'",
                params![event.id.0, agent_id, epoch],
            )?;
            if changed != 1 {
                return Err(StoreError::SchemaMismatch(
                    "scheduler event claim changed during its write transaction".to_owned(),
                ));
            }
            event.status = "reserved".to_owned();
        }

        let event_ids = events
            .iter()
            .map(|event| event.id.0.clone())
            .collect::<Vec<_>>();
        mark_timing(
            transaction,
            &json!(event_ids).to_string(),
            "dispatchPickedAt",
        )?;
        agent.status = AgentStatus::Starting;
        // The start gate's wait record (`startWait`) ends with the claim.
        agent.extra.remove("startWait");
        agent.extra.insert("inFlight".to_owned(), Value::Bool(true));
        agent.extra.insert("turnId".to_owned(), Value::Null);
        agent.extra.insert("turnEpoch".to_owned(), json!(epoch));
        agent.extra.insert(
            "startAttempt".to_owned(),
            json!({
                "id": attempt_id,
                "epoch": epoch,
                "activeAtReservation": false,
                "accountKey":agent.extra.get("accountKey").cloned().unwrap_or(json!("default")),
                "events": event_ids,
                "submitted": false,
                "created": attempt_created
            }),
        );
        publish_agent(transaction, &agent)?;
        let identity = input_identity(&events, &metadata);
        crate::ExecutionHistoryRepository::save_claim(
            transaction,
            attempt_id,
            &identity,
            &json!({"agent":agent,"metadata":metadata}),
        )?;
        Ok(Some(ClaimedEvents {
            agent,
            events,
            metadata,
            refusal_reason: None,
        }))
    }

    /// Reconcile notifications that became obsolete after reservation, before native submission.
    /// A false result means the caller must stop and must not cross the submission boundary.
    pub fn reconcile_obsolete_before_submit(
        transaction: &Transaction<'_>,
        agent_id: &str,
        epoch: i64,
        attempt_id: &str,
    ) -> StoreResult<bool> {
        let Some(mut agent) = AgentRepository::get(transaction, &AgentId::from(agent_id))? else {
            return Err(invalid_agent(agent_id));
        };
        if agent
            .extra
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
        {
            return Err(invalid_agent(agent_id));
        }
        let Some(attempt) = agent.extra.get("startAttempt").and_then(Value::as_object) else {
            return Ok(false);
        };
        if attempt.get("id").and_then(Value::as_str) != Some(attempt_id)
            || attempt.get("epoch").and_then(Value::as_i64) != Some(epoch)
            || attempt.get("submitted").and_then(Value::as_bool) != Some(false)
            || attempt.get("turnId").is_some_and(|value| !value.is_null())
            || attempt
                .get("observedTurnId")
                .is_some_and(|value| !value.is_null())
            || agent.epoch != epoch
        {
            return Ok(false);
        }
        let saved = attempt
            .get("events")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_agent(agent_id))?;
        let mut event_ids = Vec::with_capacity(saved.len());
        for id in saved {
            let Some(id) = id.as_str() else {
                return Err(invalid_agent(agent_id));
            };
            event_ids.push(id.to_owned());
        }
        if event_ids.is_empty() || event_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid_agent(agent_id));
        }
        let mut statement = transaction.prepare_cached(
            "SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events \
             WHERE id IN (SELECT value FROM json_each(?1)) AND agent=?2 AND epoch=?3 ORDER BY created,rowid",
        )?;
        let payload = serde_json::to_string(&event_ids)?;
        let rows = statement.query_map(
            params![payload, agent_id, epoch],
            crate::QueueEventRepository::from_row,
        )?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if rows.len() != event_ids.len()
            || rows
                .iter()
                .any(|event| !matches!(event.status.as_str(), "reserved" | "dispatching"))
            || rows
                .iter()
                .map(|event| event.id.0.as_str())
                .collect::<Vec<_>>()
                != event_ids.iter().map(String::as_str).collect::<Vec<_>>()
        {
            return Ok(false);
        }
        let (complaints, _work) = load_policy_records(transaction, &agent, &rows)?;
        let obsolete_work = obsolete_work_review_ids(transaction, &agent, &rows)?;
        // Submission reconciliation retires resolved business notifications only.
        // Progress coalescing belongs to selection and must not run a second time.
        let obsolete = obsolete_events(&agent, &rows, &complaints, &obsolete_work);
        let mut changes = BTreeMap::new();
        let mut retired = Vec::new();
        for event in &rows {
            let reason = obsolete.get(&event.id.0);
            if let Some(reason) = reason {
                retired.push(event.id.0.clone());
                changes.insert(
                    event.id.0.clone(),
                    json!({"status":"stored_only","error":reason}),
                );
            } else {
                changes.insert(event.id.0.clone(), json!({"status":"pending","error":null}));
            }
        }
        if retired.is_empty() {
            return Ok(true);
        }
        let payload = serde_json::to_string(&changes)?;
        let mut update = transaction.prepare_cached(
            "UPDATE runtime_events SET status=json_extract((SELECT value FROM json_each(?1) WHERE key=runtime_events.id),'$.status'), \
             error=json_extract((SELECT value FROM json_each(?1) WHERE key=runtime_events.id),'$.error') \
             WHERE id IN (SELECT key FROM json_each(?1)) AND agent=?2 AND epoch=?3 \
             AND status IN ('reserved','dispatching') RETURNING id",
        )?;
        let updated = update
            .query_map(params![payload, agent_id, epoch], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<BTreeSet<_>, _>>()?;
        if updated.len() != changes.len() {
            return Err(StoreError::SchemaMismatch(
                "scheduler reservation changed during submission reconciliation".to_owned(),
            ));
        }
        let attempt = agent
            .extra
            .get_mut("startAttempt")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid_agent(agent_id))?;
        attempt.insert(
            "notSubmittedReason".to_owned(),
            json!("Notifications resolved before native submission"),
        );
        attempt.insert("retiredEvents".to_owned(), json!(retired));
        let active = attempt
            .get("activeAtReservation")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !active {
            let pending = transaction.query_row_cached(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND status='pending')",
                params![agent_id, epoch], |row| row.get::<_, bool>(0),
            )?;
            agent.status = if pending {
                AgentStatus::Queued
            } else {
                AgentStatus::Waiting
            };
            agent
                .extra
                .insert("inFlight".to_owned(), Value::Bool(false));
        }
        publish_agent(transaction, &agent)?;
        Ok(false)
    }
}

pub(super) fn business_metadata(value: Option<&Value>) -> Value {
    let mut value = value.cloned().unwrap_or(json!({}));
    if let Some(map) = value.as_object_mut() {
        map.remove("timing");
    }
    value
}
pub(super) fn input_identity(events: &[QueueEvent], metadata: &BTreeMap<String, Value>) -> Value {
    json!(events.iter().map(|e|json!({"id":e.id.0,"agent":e.agent.0,"kind":e.kind,"text":e.text,"created":e.created,"epoch":e.epoch,"error":e.error,"metadata":business_metadata(metadata.get(&e.id.0))})).collect::<Vec<_>>())
}
pub(super) fn replay_claim(
    tx: &Transaction<'_>,
    agent: AgentRecord,
    epoch: i64,
    saved: &Value,
) -> StoreResult<ClaimedEvents> {
    let receipt_error = || {
        crate::ExecutionHistoryRepository::invalid(
            "runtime_execution_attempts",
            saved["id"].as_str().unwrap_or("unknown"),
        )
    };
    let original: AgentRecord = serde_json::from_value(saved["claimResult"]["agent"].clone())
        .map_err(|_| receipt_error())?;
    let preparation = &saved["receipts"]["threadPrepared"];
    let prepared_scope = preparation["result"] == true
        && preparation["input"]["agent"] == agent.id.0
        && preparation["input"]["epoch"] == epoch
        && preparation["input"]["account"]
            == agent
                .extra
                .get("accountKey")
                .cloned()
                .unwrap_or(json!("default"))
        && preparation["input"]["previousThread"]
            == original
                .extra
                .get("threadId")
                .cloned()
                .unwrap_or(Value::Null)
        && preparation["input"]["thread"]
            == agent.extra.get("threadId").cloned().unwrap_or(Value::Null);
    if original.id != agent.id
        || original.epoch != epoch
        || !(same_native_scope(&original, &agent) || prepared_scope)
    {
        return Err(invalid_agent(&agent.id.0));
    }
    let identity = saved["inputIdentity"]
        .as_array()
        .ok_or_else(receipt_error)?;
    let has_metadata: bool = tx.query_row_cached("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_event_meta')", [], |r| r.get(0))?;
    let metadata_column = if has_metadata { "m.record" } else { "NULL" };
    let metadata_join = if has_metadata {
        "LEFT JOIN runtime_event_meta m ON m.id=e.id"
    } else {
        ""
    };
    let mut statement = tx.prepare_cached(&format!("SELECT e.id,e.agent,e.kind,e.text,e.status,e.created,e.epoch,e.turn_id,e.error,{metadata_column} FROM json_each(?1) j JOIN runtime_events e ON e.id=json_extract(j.value,'$.id') {metadata_join} ORDER BY j.key"))?;
    let mut events = Vec::new();
    let mut metadata = BTreeMap::new();
    for row in statement.query_map([saved["inputIdentity"].to_string()], |r| {
        Ok((
            crate::QueueEventRepository::from_row(r)?,
            r.get::<_, Option<String>>(9)?,
        ))
    })? {
        let (mut event, raw) = row?;
        event.status = "reserved".into();
        event.turn_id = None;
        if let Some(raw) = raw {
            metadata.insert(
                event.id.0.clone(),
                serde_json::from_str(&raw).map_err(|_| receipt_error())?,
            );
        }
        if let Some(text) = model_event_text(&event.text, metadata.get(&event.id.0)) {
            event.text = text;
        }
        events.push(event);
    }
    if events.len() != identity.len() {
        return Err(receipt_error());
    }
    if input_identity(&events, &metadata) != saved["inputIdentity"] {
        return Err(invalid_agent(&agent.id.0));
    }
    let metadata = serde_json::from_value(saved["claimResult"]["metadata"].clone())
        .map_err(|_| receipt_error())?;
    Ok(ClaimedEvents {
        agent: original,
        events,
        metadata,
        refusal_reason: None,
    })
}

pub(super) struct PreparedBatch {
    events: Vec<QueueEvent>,
    metadata: BTreeMap<String, Value>,
    metadata_updates: BTreeMap<String, Value>,
    retired: BTreeMap<String, String>,
    cancelled: BTreeMap<String, String>,
}

pub(super) fn refused(agent: &AgentRecord, reason: String) -> ClaimedEvents {
    ClaimedEvents {
        agent: agent.clone(),
        events: Vec::new(),
        metadata: BTreeMap::new(),
        refusal_reason: Some(reason),
    }
}

pub(super) fn invalid_object(table: &str, id: &str) -> StoreError {
    StoreError::InvalidSyncObject {
        scope: table.to_owned(),
        id: id.to_owned(),
    }
}

pub(super) fn table_exists(transaction: &Transaction<'_>, table: &str) -> StoreResult<bool> {
    Ok(transaction.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

pub(super) fn load_policy_records(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    events: &[QueueEvent],
) -> StoreResult<(BTreeMap<String, Value>, BTreeMap<String, Value>)> {
    let mut complaints = BTreeMap::new();
    let mut work = BTreeMap::new();
    if table_exists(transaction, "runtime_complaints")? {
        for event in events.iter().filter(|event| event.kind == "complaint") {
            for id in complaint_ids(&event.text).unwrap_or_default() {
                if complaints.contains_key(&id) {
                    continue;
                }
                if let Some(value) =
                    get_json_record::<Value>(transaction, "runtime_complaints", &id)?
                {
                    let valid = value.get("leadId").and_then(Value::as_str).is_some()
                        && value.get("status").and_then(Value::as_str).is_some()
                        && value.get("recipient").and_then(Value::as_str).is_some()
                        && value
                            .get("responses")
                            .and_then(Value::as_array)
                            .is_some_and(|responses| {
                                responses.iter().all(|response| {
                                    response.get("author").and_then(Value::as_str).is_some()
                                })
                            });
                    if !valid {
                        return Err(invalid_object("runtime_complaints", &id));
                    }
                    if value["leadId"] != agent.id.0 {
                        continue;
                    }
                    complaints.insert(id, value);
                }
            }
        }
    }
    if table_exists(transaction, "runtime_work")? {
        for event in events.iter().filter(|event| event.kind == "work_review") {
            let Some((id, _)) = work_review_ids(event) else {
                continue;
            };
            if work.contains_key(&id) {
                continue;
            }
            if let Some(value) = get_json_record::<Value>(transaction, "runtime_work", &id)? {
                let valid = value.get("rootId").and_then(Value::as_str).is_some()
                    && value
                        .get("results")
                        .and_then(Value::as_array)
                        .is_some_and(|results| {
                            results
                                .iter()
                                .all(|result| result.get("id").and_then(Value::as_str).is_some())
                        })
                    && value
                        .get("decisions")
                        .and_then(Value::as_array)
                        .is_some_and(|decisions| {
                            decisions.iter().all(|decision| {
                                decision.get("resultId").and_then(Value::as_str).is_some()
                            })
                        });
                if !valid {
                    return Err(invalid_object("runtime_work", &id));
                }
                work.insert(id, value);
            }
        }
    }
    Ok((complaints, work))
}

pub(super) fn complaint_ids(text: &str) -> Option<Vec<String>> {
    let value: Value = serde_json::from_str(text).ok()?;
    let entries = value.get("complaints")?.as_array()?;
    if entries.is_empty() {
        return None;
    }
    entries
        .iter()
        .map(|entry| {
            let id = entry.get("complaint_id")?.as_str()?;
            (!id.is_empty()).then(|| id.to_owned())
        })
        .collect()
}

pub(super) fn complaints_resolved(
    agent: &AgentRecord,
    event: &QueueEvent,
    records: &BTreeMap<String, Value>,
) -> bool {
    let Some(ids) = complaint_ids(&event.text) else {
        return false;
    };
    ids.iter().all(|id| {
        records.get(id).is_some_and(|record| {
            let status = record["status"].as_str().unwrap_or_default();
            if matches!(status, "resolved" | "declined") {
                return true;
            }
            if record["sourceType"] == "user_task" && status == "open" {
                return false;
            }
            let responsible = if record["recipient"] == "user" {
                "user"
            } else {
                agent.id.0.as_str()
            };
            record["responses"]
                .as_array()
                .is_some_and(|responses| responses.iter().any(|r| r["author"] == responsible))
        })
    })
}

pub(super) fn progress_update(text: &str) -> Option<((String, String, String), u64)> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("importance")?.as_str()? != "progress" {
        return None;
    }
    let field = |key| {
        value
            .get(key)?
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let version = value.get("progress_version")?.as_u64()?;
    Some((
        (field("sender")?, field("room")?, field("progress_key")?),
        version,
    ))
}

fn obsolete_events(
    agent: &AgentRecord,
    events: &[QueueEvent],
    complaints: &BTreeMap<String, Value>,
    obsolete_work: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let mut retired = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == "complaint") {
        if complaints_resolved(agent, event, complaints) {
            retired.insert(
                event.id.0.clone(),
                "Every referenced complaint already has a response".to_owned(),
            );
        }
    }
    for event in events
        .iter()
        .filter(|event| obsolete_work.contains(&event.id.0))
    {
        retired.insert(
            event.id.0.clone(),
            "The exact result already has a decision".to_owned(),
        );
    }
    retired
}

pub(super) fn prepare_pending_batch(
    agent: &AgentRecord,
    events: Vec<QueueEvent>,
    mut metadata: BTreeMap<String, Value>,
    complaints: &BTreeMap<String, Value>,
    obsolete_work: &BTreeSet<String>,
) -> StoreResult<PreparedBatch> {
    let mut metadata_updates = BTreeMap::new();
    let mut retired = BTreeMap::new();
    let mut cancelled = BTreeMap::new();
    for event in events.iter().filter(|event| event.kind == "chat_review") {
        cancelled.insert(
            event.id.0.clone(),
            "Scheduled chat reviews have been removed".to_owned(),
        );
    }
    retired.extend(obsolete_events(agent, &events, complaints, obsolete_work));
    let mut groups: BTreeMap<(String, String, String), Vec<(usize, u64)>> = BTreeMap::new();
    let mut preserved = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        let saved = metadata
            .get(&event.id.0)
            .and_then(|value| value.get("preserveProgress"))
            .is_some_and(truthy);
        preserved.push(saved);
        if event.kind == "agent_message"
            && let Some((key, version)) = progress_update(&event.text)
        {
            groups.entry(key).or_default().push((index, version));
        }
    }
    for group in groups.values() {
        let versions = group
            .iter()
            .map(|(_, version)| *version)
            .collect::<BTreeSet<_>>();
        if versions.len() != group.len() || group.iter().any(|(index, _)| preserved[*index]) {
            for (index, _) in group {
                let id = events[*index].id.0.clone();
                let value = metadata.entry(id.clone()).or_insert_with(|| json!({}));
                value
                    .as_object_mut()
                    .ok_or_else(|| invalid_object("runtime_event_meta", &id))?
                    .insert("preserveProgress".to_owned(), Value::Bool(true));
                metadata_updates.insert(id, value.clone());
            }
            continue;
        }
        if group.len() < 2 {
            continue;
        }
        let latest = group
            .iter()
            .max_by_key(|(_, version)| *version)
            .map(|(index, _)| *index)
            .unwrap_or_default();
        let latest_id = events[latest].id.0.clone();
        let previous = group.iter().try_fold(0_u64, |sum, (index, _)| {
            let count = metadata
                .get(&events[*index].id.0)
                .and_then(|meta| meta.get("earlierProgressUpdates"));
            match count {
                None | Some(Value::Null) => Some(sum),
                Some(Value::Number(number)) => {
                    number.as_u64().and_then(|count| sum.checked_add(count))
                }
                _ => None,
            }
        });
        let Some(count) = previous.and_then(|count| count.checked_add((group.len() - 1) as u64))
        else {
            continue;
        };
        for (index, _) in group {
            let id = events[*index].id.0.clone();
            if id != latest_id {
                retired.insert(id, format!("Superseded by progress event {latest_id}"));
            }
        }
        let value = metadata
            .entry(latest_id.clone())
            .or_insert_with(|| json!({}));
        value
            .as_object_mut()
            .ok_or_else(|| invalid_object("runtime_event_meta", &latest_id))?
            .insert("earlierProgressUpdates".to_owned(), json!(count));
        metadata_updates.insert(latest_id, value.clone());
    }
    let mut prepared_events = Vec::new();
    for mut event in events {
        if retired.contains_key(&event.id.0) || cancelled.contains_key(&event.id.0) {
            continue;
        }
        if let Some(text) = model_event_text(&event.text, metadata.get(&event.id.0)) {
            event.text = text;
        }
        prepared_events.push(event);
    }
    Ok(PreparedBatch {
        events: prepared_events,
        metadata,
        metadata_updates,
        retired,
        cancelled,
    })
}

/// The claimed text of one event when it differs from the stored row: a
/// progress update that replaced earlier updates names their count. Only this
/// model projection changes; the stored row keeps the original payload
/// (Python `codex_wakeups.pending_batch`). Every check that compares a claim
/// with its stored rows applies this same projection to them.
pub(super) fn model_event_text(text: &str, metadata: Option<&Value>) -> Option<String> {
    let count = metadata?
        .get("earlierProgressUpdates")
        .filter(|count| truthy(count))?;
    let Ok(Value::Object(mut value)) = serde_json::from_str(text) else {
        return None;
    };
    value.insert("earlierProgressUpdates".to_owned(), count.clone());
    value.insert(
        "history".to_owned(),
        json!("Read earlier progress with orchestration_chat_read in this room."),
    );
    Some(Value::Object(value).to_string())
}

pub(super) fn apply_preparation(
    transaction: &Transaction<'_>,
    prepared: &PreparedBatch,
) -> StoreResult<()> {
    let changes = prepared
        .retired
        .iter()
        .map(|(id, reason)| (id, "stored_only", reason))
        .chain(
            prepared
                .cancelled
                .iter()
                .map(|(id, reason)| (id, "cancelled", reason)),
        )
        .map(|(id, status, reason)| (id.clone(), json!({"status":status,"error":reason})))
        .collect::<BTreeMap<_, _>>();
    if !changes.is_empty() {
        let payload = serde_json::to_string(&changes)?;
        let mut statement = transaction.prepare_cached(
            "UPDATE runtime_events SET status=json_extract((SELECT value FROM json_each(?1) WHERE key=runtime_events.id),'$.status'), \
             error=json_extract((SELECT value FROM json_each(?1) WHERE key=runtime_events.id),'$.error') \
             WHERE id IN (SELECT key FROM json_each(?1)) AND status='pending' RETURNING id",
        )?;
        let changed = statement
            .query_map([payload], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        if changed.len() != changes.len() || changes.keys().any(|id| !changed.contains(id)) {
            return Err(StoreError::SchemaMismatch(
                "scheduler event changed during pending preparation".to_owned(),
            ));
        }
    }
    if !prepared.metadata_updates.is_empty() {
        if !table_exists(transaction, "runtime_event_meta")? {
            return Err(StoreError::SchemaMismatch(
                "scheduler progress preparation requires runtime_event_meta".to_owned(),
            ));
        }
        for (id, value) in &prepared.metadata_updates {
            transaction.execute_cached(
                "INSERT OR REPLACE INTO runtime_event_meta(id,record) VALUES(?1,?2)",
                params![id, value.to_string()],
            )?;
        }
    }
    Ok(())
}

pub(super) fn work_review_ids(event: &QueueEvent) -> Option<(String, String)> {
    let value: Value = serde_json::from_str(&event.text).ok()?;
    Some((
        value.get("task")?.as_str()?.to_owned(),
        value.get("result")?.get("id")?.as_str()?.to_owned(),
    ))
}

pub(super) fn obsolete_work_review_ids(
    transaction: &Transaction<'_>,
    agent: &AgentRecord,
    events: &[QueueEvent],
) -> StoreResult<BTreeSet<String>> {
    if !table_exists(transaction, "runtime_work")? {
        return Ok(BTreeSet::new());
    }
    let ids = serde_json::to_string(
        &events
            .iter()
            .map(|event| event.id.0.as_str())
            .collect::<Vec<_>>(),
    )?;
    let mut statement = transaction.prepare_cached(
        "SELECT e.id FROM runtime_events e \
         JOIN json_each(?1) selected ON selected.value=e.id \
         WHERE e.kind='work_review' AND json_valid(e.text) \
         AND EXISTS(SELECT 1 FROM runtime_work w \
             WHERE w.id=json_extract(e.text,'$.task') \
             AND json_extract(w.record,'$.rootId')=?2 \
             AND EXISTS(SELECT 1 FROM json_each(w.record,'$.results') result \
                 WHERE json_extract(result.value,'$.id')=json_extract(e.text,'$.result.id')) \
             AND EXISTS(SELECT 1 FROM json_each(w.record,'$.decisions') decision \
                 WHERE json_extract(decision.value,'$.resultId')=json_extract(e.text,'$.result.id')))"
    )?;
    Ok(statement
        .query_map(params![ids, agent.id.0], |row| row.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?)
}

pub(super) fn event_policy_dependency(kind: &str) -> Option<&'static str> {
    match kind {
        "agent_message" | "complaint_response" | "followup" => Some("team-isolation-policy"),
        "radio" | "radio_turn" => Some("radio-policy"),
        _ => None,
    }
}

pub(super) fn set_waiting(
    transaction: &Transaction<'_>,
    agent: &mut AgentRecord,
    agent_id: &str,
) -> StoreResult<()> {
    agent.status = AgentStatus::Waiting;
    AgentRepository::put(transaction, agent)?;
    EventWriteRepository::set_agent_sync_lifecycle(transaction, agent_id, "waiting", false, None)
}

/// Event kinds that share one turn with neighboring input. Child results
/// batch only on the preparation path, the one that renders them, so a lead
/// receives results that arrive together in a single turn.
fn shares_turn(kind: &str, child_results: bool) -> bool {
    matches!(kind, "user" | "complaint") || (child_results && kind == "child_result")
}

pub(super) fn select_turn_batch(
    events: &[QueueEvent],
    metadata: &BTreeMap<String, Value>,
    limit: usize,
    child_results: bool,
) -> Vec<QueueEvent> {
    let mut selected = Vec::new();
    let mut has_composer_input = false;
    for event in events.iter().take(limit.min(MAX_EVENTS_PER_TURN)) {
        if !shares_turn(&event.kind, child_results) {
            break;
        }
        let meta = metadata.get(&event.id.0);
        let composer = event.kind == "user"
            && (meta
                .and_then(|value| value.get("delivery"))
                .and_then(Value::as_str)
                == Some("after_turn")
                || meta
                    .and_then(|value| value.get("sendNow"))
                    .is_some_and(truthy));
        if composer && has_composer_input {
            break;
        }
        has_composer_input |= composer;
        selected.push(event.clone());
    }
    selected
}

#[cfg(test)]
mod account_wake_index_tests {
    use super::{
        ACCOUNT_WAKE_INDEX, SchedulerRepository, account_wake_accounts_query,
        account_wake_index_eligibility,
    };
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn old_schema() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                 CREATE TABLE runtime_events(
                   id TEXT PRIMARY KEY,agent TEXT NOT NULL,kind TEXT NOT NULL,text TEXT NOT NULL,
                   status TEXT NOT NULL,created REAL NOT NULL,epoch INTEGER NOT NULL,
                   turn_id TEXT,error TEXT);
                 CREATE TABLE runtime_operation_receipts(
                   id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
                 ",
            )
            .expect("pre-index schema creates");
        connection
    }

    fn put_agent(connection: &Connection, id: &str, value: serde_json::Value) {
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                rusqlite::params![id, value.to_string()],
            )
            .expect("agent fixture inserts");
    }

    fn install_index(connection: &mut Connection) {
        let transaction = connection
            .transaction()
            .expect("candidate-index transaction begins");
        SchedulerRepository::prepare_account_wake_index(&transaction)
            .expect("candidate index prepares on existing schema");
        transaction
            .commit()
            .expect("candidate-index transaction commits");
    }

    #[test]
    fn account_wake_candidates_preserve_pending_recovery_and_cursor_semantics_after_upgrade() {
        let mut connection = old_schema();
        put_agent(
            &connection,
            "aaa",
            json!({
                "accountKey":"account-a", "epoch":1, "status":"queued",
                "autoWake":true, "inFlight":false, "nativeFailureHold":false
            }),
        );
        put_agent(
            &connection,
            "zzz",
            json!({
                "accountKey":"account-a", "epoch":2, "status":"queued",
                "autoWake":true, "inFlight":false, "nativeFailureHold":false
            }),
        );
        put_agent(
            &connection,
            "bbb",
            json!({
                "accountKey":"account-b", "epoch":3, "status":"interrupted",
                "provider":"codex", "restartRecovery":{"stage":"held"}
            }),
        );
        put_agent(
            &connection,
            "no-event",
            json!({
                "accountKey":"account-c", "epoch":4, "status":"queued",
                "autoWake":true, "inFlight":false, "nativeFailureHold":false
            }),
        );
        put_agent(
            &connection,
            "remote",
            json!({
                "accountKey":"account-d", "epoch":5, "status":"interrupted",
                "remoteWorker":true, "restartRecovery":{"stage":"pending"}
            }),
        );
        connection
            .execute_batch(
                "INSERT INTO runtime_events VALUES
                   ('event-aaa','aaa','user','input','pending',1,1,NULL,NULL),
                   ('event-zzz','zzz','user','input','pending',2,2,NULL,NULL);",
            )
            .expect("pending events insert");
        connection
            .execute_batch(
                "CREATE INDEX runtime_agent_live_root ON runtime_agents(id)
                   WHERE json_extract(record,'$.deletedAt') IS NULL;",
            )
            .expect("legacy live-agent index creates before upgrade");

        install_index(&mut connection);

        let page = SchedulerRepository::account_wake_page(
            &connection,
            "account-a",
            &BTreeMap::new(),
            1,
            1,
            0.0,
        )
        .expect("account cursor advances to recovery account");
        assert_eq!(page, BTreeMap::from([("account-b".to_owned(), vec![])]));

        let wrapped = SchedulerRepository::account_wake_page(
            &connection,
            "account-z",
            &BTreeMap::new(),
            1,
            1,
            0.0,
        )
        .expect("account cursor wraps");
        assert_eq!(
            wrapped,
            BTreeMap::from([("account-a".to_owned(), vec![("aaa".to_owned(), 1, None)])])
        );

        let mut after_agent = BTreeMap::new();
        after_agent.insert("account-a".to_owned(), "aaa".to_owned());
        let next_agent =
            SchedulerRepository::account_wake_page(&connection, "", &after_agent, 1, 1, 0.0)
                .expect("agent cursor advances");
        assert_eq!(next_agent["account-a"], vec![("zzz".to_owned(), 2, None)]);

        after_agent.insert("account-a".to_owned(), "zzz".to_owned());
        let wrapped_agent =
            SchedulerRepository::account_wake_page(&connection, "", &after_agent, 1, 1, 0.0)
                .expect("agent cursor wraps");
        assert_eq!(
            wrapped_agent["account-a"],
            vec![("aaa".to_owned(), 1, None)]
        );

        let no_wrap =
            SchedulerRepository::account_wake_agents_after(&connection, "account-a", "zzz", 1, 0.0)
                .expect("capacity continuation does not wrap the agent cursor");
        assert!(no_wrap.is_empty());
    }

    /// Python's dispatcher starts a queued native review child without a
    /// queued input event (`codex_agent_review.claim`). Agent 8388eb70 waited
    /// forever because the Rust wake page required an event.
    #[test]
    fn a_queued_native_review_child_wakes_without_an_input_event() {
        let mut connection = old_schema();
        let review = |status: &str| {
            json!({
                "accountKey":"default", "epoch":0, "status":"queued", "autoWake":true,
                "inFlight":false, "nativeFailureHold":false, "provider":"codex",
                "nativeReview":{"status":status,"target":{"type":"uncommittedChanges"}}
            })
        };
        put_agent(&connection, "pending-review", review("pending"));
        put_agent(&connection, "started-review", review("started"));
        install_index(&mut connection);
        let page =
            SchedulerRepository::account_wake_page(&connection, "", &BTreeMap::new(), 8, 8, 0.0)
                .expect("wake page");
        assert_eq!(
            page,
            BTreeMap::from([(
                "default".to_owned(),
                vec![("pending-review".to_owned(), 0, None)]
            )])
        );
    }

    #[test]
    fn strict_account_wake_pages_reach_agent_after_full_page_without_wrapping() {
        let mut connection = old_schema();
        for index in 0..33 {
            let id = format!("agent-{index:03}");
            put_agent(
                &connection,
                &id,
                json!({
                    "accountKey":"account-middle", "epoch":1, "status":"queued",
                    "autoWake":true, "inFlight":false, "nativeFailureHold":false
                }),
            );
            connection
                .execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES(?1,?2,'user','input','pending',?3,1)",
                    rusqlite::params![format!("event-{index:03}"), id, index as f64],
                )
                .expect("pending event fixture inserts");
        }
        install_index(&mut connection);

        let first =
            SchedulerRepository::account_wake_page(&connection, "", &BTreeMap::new(), 1, 32, 0.0)
                .expect("initial bounded account page is selected");
        let first = &first["account-middle"];
        assert_eq!(first.len(), 32);
        let next = SchedulerRepository::account_wake_agents_after(
            &connection,
            "account-middle",
            &first.last().expect("first page has a final row").0,
            32,
            0.0,
        )
        .expect("strict continuation page is selected");
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].0, "agent-032");
        assert!(
            SchedulerRepository::account_wake_agents_after(
                &connection,
                "account-middle",
                "agent-999",
                32,
                0.0,
            )
            .expect("strict continuation never wraps")
            .is_empty()
        );
    }

    #[test]
    fn exact_account_capacity_query_is_independent_of_global_account_cursor() {
        let mut connection = old_schema();
        for index in 0..130 {
            let id = format!("agent-{index:03}");
            put_agent(
                &connection,
                &id,
                json!({
                    "accountKey":format!("account-{index:03}"), "epoch":1,
                    "status":"queued", "autoWake":true,
                    "inFlight":false, "nativeFailureHold":false
                }),
            );
            connection
                .execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES(?1,?2,'user','input','pending',?3,1)",
                    rusqlite::params![format!("event-{index:03}"), id, index as f64],
                )
                .expect("pending event fixture inserts");
        }
        install_index(&mut connection);

        let normal = SchedulerRepository::account_wake_page(
            &connection,
            "account-000",
            &BTreeMap::new(),
            128,
            32,
            0.0,
        )
        .expect("ordinary account selection stays bounded");
        assert_eq!(normal.len(), 128);
        assert!(!normal.contains_key("account-000"));
        assert!(normal.contains_key("account-128"));

        let tail = SchedulerRepository::account_wake_page_after(
            &connection,
            "account-128",
            &BTreeMap::new(),
            128,
            32,
            0.0,
        )
        .expect("strict account continuation reaches the final candidate account");
        assert_eq!(tail.keys().cloned().collect::<Vec<_>>(), ["account-129"]);
        assert!(
            SchedulerRepository::account_wake_page_after(
                &connection,
                "account-129",
                &BTreeMap::new(),
                128,
                32,
                0.0,
            )
            .expect("strict account continuation never wraps")
            .is_empty()
        );

        let released =
            SchedulerRepository::account_wake_agents_after(&connection, "account-000", "", 32, 0.0)
                .expect("released account is queried directly despite global cursor progress");
        assert_eq!(released, vec![("agent-000".to_owned(), 1, None)]);
    }

    #[test]
    fn account_wake_index_query_uses_candidate_index_and_guards_invalid_json() {
        let mut connection = old_schema();
        put_agent(
            &connection,
            "recoverable",
            json!({
                "epoch":1, "status":"interrupted", "restartRecovery":{"stage":"held"}
            }),
        );
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES('malformed','{bad json')",
                [],
            )
            .expect("malformed record fixture inserts");
        connection
            .execute_batch(
                "CREATE INDEX runtime_agent_live_root ON runtime_agents(id)
                   WHERE json_valid(record) AND json_extract(record,'$.deletedAt') IS NULL;",
            )
            .expect("guarded legacy live-agent index creates");
        install_index(&mut connection);

        let index_exists = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)",
                [ACCOUNT_WAKE_INDEX],
                |row| row.get::<_, bool>(0),
            )
            .expect("installed index is visible");
        assert!(
            index_exists,
            "index installation upgrades existing databases"
        );

        let details = connection
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                account_wake_accounts_query()
            ))
            .expect("candidate query prepares against its partial index")
            .query_map(
                rusqlite::params![
                    crate::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                    crate::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                    "",
                    4_i64,
                    0.0
                ],
                |row| row.get::<_, String>(3),
            )
            .expect("query plan loads")
            .collect::<Result<Vec<_>, _>>()
            .expect("query plan rows load")
            .join(" ");
        assert!(details.contains(ACCOUNT_WAKE_INDEX), "{details}");
        assert!(
            !details.contains("SCAN a USING INDEX runtime_agent_live_root"),
            "{details}"
        );

        let page =
            SchedulerRepository::account_wake_page(&connection, "", &BTreeMap::new(), 4, 4, 0.0)
                .expect("guarded candidate selection skips malformed JSON");
        assert_eq!(page.keys().cloned().collect::<Vec<_>>(), vec!["default"]);
    }

    #[test]
    fn child_result_hold_queries_use_their_indexes() {
        let connection = old_schema();
        for definition in SchedulerRepository::child_result_hold_index_definitions() {
            connection
                .execute_batch(&definition.create_sql)
                .expect("hold index creates");
        }
        let plan = |sql: &str| {
            connection
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("query prepares")
                .query_map([0.0], |row| row.get::<_, String>(3))
                .expect("plan loads")
                .collect::<Result<Vec<_>, _>>()
                .expect("plan rows load")
                .join(" ")
        };
        let release = plan(&super::child_result_release_query());
        assert!(
            release.contains(&format!(
                "{} (created>?)",
                super::CHILD_RESULT_PENDING_INDEX
            )),
            "{release}"
        );
        assert!(
            release.contains(&format!("{} (<expr>=?)", super::AGENT_PARENT_INDEX)),
            "{release}"
        );
        assert!(!release.contains("SCAN"), "{release}");
    }

    #[test]
    fn account_wake_index_predicate_keeps_invalid_agent_receipt_exclusion_outside_index() {
        let predicate = account_wake_index_eligibility("record");
        assert!(predicate.starts_with("CASE WHEN json_valid(record) THEN"));
        assert!(!predicate.contains("runtime_operation_receipts"));
        assert!(!predicate.contains("runtime_events"));
    }
}
