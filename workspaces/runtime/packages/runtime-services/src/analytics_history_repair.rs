//! Bounded legacy analytics repairs. Budget receipts are obtained outside the
//! analytics transaction; a compare-and-swap acknowledges their durable page.
use crate::analytics_capture::{CaptureError, check_identity, number, text};
use crate::analytics_history::{HistoryBudgetSink, HistoryError};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository as Repo, CaptureTable};
use studio_runtime_store::{AnalyticsRepository, RuntimeStore, truthy};
const PAGE_SIZE: i64 = 64;
/// Analytics metadata key owning the legacy terminal-error repair checkpoint.
const TERMINAL_ERROR_REPAIR_KEY: &str = "terminalErrorRepairV1";
fn metadata(db: &Connection, key: &str) -> Result<Option<Value>, HistoryError> {
    let raw: Option<String> = db
        .query_row("SELECT value FROM analytics_meta WHERE key=?", [key], |r| {
            r.get(0)
        })
        .optional()
        .map_err(studio_runtime_store::StoreError::from)?;
    raw.map(|v| {
        serde_json::from_str(&v)
            .map_err(studio_runtime_store::StoreError::from)
            .map_err(HistoryError::from)
    })
    .transpose()
}
fn cursor(state: &Value) -> Result<(i64, i64), HistoryError> {
    let value = |k| {
        state[k]
            .as_i64()
            .filter(|n| *n >= 0)
            .ok_or(CaptureError::Invalid("/history/migration/cursor"))
    };
    Ok((value("cursor")?, value("end")?))
}

pub async fn migrate_budget(
    store: &RuntimeStore,
    agent: Value,
    budget: &dyn HistoryBudgetSink,
    now: f64,
) -> Result<bool, HistoryError> {
    let key = format!("budgetUsageMigrationV1:{}", text(&agent["id"]));
    let metadata_key = key.clone();
    let root = store.root().to_owned();
    let agent_id = text(&agent["id"]).to_owned();
    let agent = Arc::new(agent);
    let blocking_agent = Arc::clone(&agent);
    let (previous, mut state, records, captures, end) =
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/analytics_history_budget_repair",
            move || {
                let db = AnalyticsRepository::new(&root)
                    .open_read_only()
                    .map_err(|_| HistoryError::read(crate::analytics_history::ANALYTICS_DATABASE))?;
                let previous = metadata(&db, &metadata_key)?;
                let mut state = match &previous {
                    Some(v) => v.clone(),
                    None => json!({"cursor":0,"end":db.query_row("SELECT COALESCE(MAX(seq),0) FROM analytics_usage",[],|r|r.get::<_,i64>(0)).map_err(studio_runtime_store::StoreError::from)?}),
                };
                let (start, end) = cursor(&state)?;
                if start >= end {
                    return Ok::<_, HistoryError>((previous, state, Vec::new(), Vec::new(), end));
                }
                let records = db.prepare("SELECT seq,record FROM analytics_usage WHERE agent=? AND seq>? AND seq<=? ORDER BY seq LIMIT ?")
                    .map_err(studio_runtime_store::StoreError::from)?
                    .query_map(params![agent_id, start, end, PAGE_SIZE], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))
                    .map_err(studio_runtime_store::StoreError::from)?
                    .collect::<Result<Vec<_>,_>>()
                    .map_err(studio_runtime_store::StoreError::from)?;
                let mut captures = Vec::with_capacity(records.len());
                for (seq, raw) in &records {
                    let record: Value = serde_json::from_str(raw).map_err(studio_runtime_store::StoreError::from)?;
                    CaptureTable::Usage.validate(&record)?;
                    let provider = if blocking_agent["provider"] == "claude" { studio_runtime_schema::usage::UsageProvider::Claude } else { studio_runtime_schema::usage::UsageProvider::Codex };
                    let mut params = if record.get("studioUsage").is_some() {
                        // Typed receipts retain the first capture key; never reconstruct a wire notification.
                        studio_runtime_schema::usage::ledger::UsageLedgerRow::from_analytics(provider, &record).map_err(CaptureError::from)?;
                        record.clone()
                    } else {
                        studio_runtime_schema::usage::persisted::analytics_usage_notification(provider, &record).map_err(CaptureError::from)?
                    };
                    params["_analyticsTimestampSource"] = record["timestampSource"].clone();
                    let at = number(&record["at"]).ok_or(CaptureError::Invalid("/analytics_usage/at"))?;
                    captures.push((Arc::clone(&blocking_agent), params, at));
                    state["cursor"] = json!(seq);
                }
                Ok((previous, state, records, captures, end))
            },
        )
        .await
        .map_err(|_| HistoryError::read(crate::analytics_history::BLOCKING_TASK))??;
    if records.is_empty() {
        return Ok(false);
    }
    budget.capture_page(captures, now).await?;
    if records.len() < PAGE_SIZE as usize {
        state["cursor"] = json!(end)
    }
    store.analytics_transaction(move |tx, canvas| {
        check_identity(canvas, &agent)?;
        if metadata(tx, &key)? != previous {
            return Err(HistoryError::StaleCheckpoint);
        }
        tx.execute(
            "INSERT INTO analytics_meta VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, state.to_string()],
        ).map_err(studio_runtime_store::StoreError::from)?;
        Ok(())
    }).await?;
    Ok(!records.is_empty())
}
/// Repair exactly identified terminal errors; ambiguous old threadless notices
/// advance the repair cursor but never replace a turn's error evidence.
pub async fn repair_terminal_errors(store: &RuntimeStore) -> Result<(), HistoryError> {
    let root = store.root().to_owned();
    let completed = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/analytics_history_terminal_repair_check",
        move || {
            let db = AnalyticsRepository::new(&root)
                .open_read_only()
                .map_err(|_| HistoryError::read(crate::analytics_history::ANALYTICS_DATABASE))?;
            let checkpoint = metadata(&db, TERMINAL_ERROR_REPAIR_KEY)?;
            Ok::<_, HistoryError>(
                checkpoint
                    .as_ref()
                    .map(cursor)
                    .transpose()?
                    .is_some_and(|(start, end)| start >= end),
            )
        },
    )
    .await
    .map_err(|_| HistoryError::read(crate::analytics_history::BLOCKING_TASK))??;
    if completed {
        return Ok(());
    }
    store.analytics_transaction(|tx, canvas| {
        // Another repair may have advanced the cursor before writer admission.
        let key = TERMINAL_ERROR_REPAIR_KEY;
        let mut state = match metadata(tx, key)? {
            Some(v) => v,
            None => {
                let end = tx.query_row("SELECT COALESCE(MAX(rowid),0) FROM analytics_turns", [], |r| r.get::<_, i64>(0))
                    .map_err(studio_runtime_store::StoreError::from)?;
                json!({"cursor":0,"end":end,"repaired":0,"ambiguous":0})
            }
        };
        let (start, end) = cursor(&state)?;
        if start >= end { return Ok(()); }
        let mut statement = tx.prepare(
            "SELECT rowid,agent,record FROM analytics_turns WHERE rowid>? AND rowid<=? ORDER BY rowid LIMIT ?"
        ).map_err(studio_runtime_store::StoreError::from)?;
        let rows = statement.query_map(params![start, end, PAGE_SIZE], |r| {
            Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))
        }).map_err(studio_runtime_store::StoreError::from)?
            .collect::<Result<Vec<_>,_>>().map_err(studio_runtime_store::StoreError::from)?;
        for (rowid, agent, raw) in &rows {
            state["cursor"] = json!(rowid);
            let record: Value = serde_json::from_str(raw).map_err(studio_runtime_store::StoreError::from)?;
            CaptureTable::Turns.validate(&record)?;
            if !truthy(Some(&record["turnId"])) || record["status"] == "failed" && truthy(Some(&record["error"])) { continue; }
            let raw: Option<String> = canvas.query_row(
                "SELECT record FROM runtime_items WHERE id=? AND agent=?",
                params![format!("{agent}:native-notice:error:{}", text(&record["turnId"])), agent], |r| r.get(0)
            ).optional().map_err(studio_runtime_store::StoreError::from)?;
            let Some(raw) = raw else { continue; };
            let notice: Value = serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
            if notice["turnId"] != record["turnId"] || !truthy(Some(&notice["nativeError"])) { continue; }
            let exact = if truthy(Some(&notice["threadId"])) {
                notice["threadId"] == record["threadId"]
            } else {
                tx.query_row(
                    "SELECT COUNT(*) FROM analytics_turns WHERE agent=? AND json_extract(record,'$.turnId')=?",
                    params![agent, record["turnId"].as_str()], |r| r.get::<_,i64>(0)
                ).map_err(studio_runtime_store::StoreError::from)? == 1
            };
            let field = if exact { "repaired" } else { "ambiguous" };
            let count = state[field].as_u64().ok_or(CaptureError::Invalid("/history/repair/count"))?;
            state[field] = json!(count.checked_add(1).ok_or(CaptureError::Invalid("/history/repair/count"))?);
            if exact {
                Repo::write(tx, CaptureTable::Turns, &repaired_turn(record, &notice))?;
            }
        }
        if rows.len() < PAGE_SIZE as usize { state["cursor"] = json!(end); }
        tx.execute(
            "INSERT INTO analytics_meta VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, state.to_string()]
        ).map_err(studio_runtime_store::StoreError::from)?;
        Ok(())
    }).await
}
/// The caller supplies the validated turn and exact persisted native notice.
fn repaired_turn(mut record: Value, notice: &Value) -> Value {
    record["status"] = json!("failed");
    record["error"] = notice["nativeError"].clone();
    record["terminalSource"] = json!("liveNoticeRepair");
    if let Some(at) = number(&notice["at"]) {
        record["finishedAt"] = notice["at"].clone();
        if let Some(start) = number(&record["startedAt"]) {
            record["durationMs"] = json!(((at - start) * 1000.).max(0.));
        }
    }
    record
}

#[cfg(test)]
mod usage_contract_tests {
    use super::*;

    async fn ledger_store(provider: &str) -> (tempfile::TempDir, RuntimeStore, Value) {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        let agent = json!({"id":"agent","rootId":"agent","name":"Agent","status":"running","provider":provider,"threadId":"thread","turnId":"turn","accountKey":"default","accountHistory":[],"model":"claude-test","epoch":0,"created":0,"tokenBudget":1000,"tokensUsed":0});
        let seed = agent.clone();
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES ('agent',?1)",
                    [seed.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("agent");
        (temp, store, agent)
    }

    async fn ledger_capture(
        store: &RuntimeStore,
        agent: &Value,
        params: Value,
        source: crate::budget::CaptureSource,
        at: f64,
    ) {
        crate::budget_service::prepare_legacy_corrections(
            store,
            vec![(agent.clone(), params.clone())],
        )
        .await
        .expect("prepare legacy correction");
        let mut supplied = agent.clone();
        let budget_params = params.clone();
        let acceptance = store
            .transaction_with(move |tx| {
                crate::budget_service::capture_acceptance(
                    tx,
                    &mut supplied,
                    &budget_params,
                    at,
                    source,
                    0.,
                )
            })
            .await
            .expect("budget caller");
        let conflict = match acceptance {
            crate::budget::UsageAcceptance::Accepted => None,
            crate::budget::UsageAcceptance::RejectedReplayConflict(conflict) => Some(conflict),
        };
        if source == crate::budget::CaptureSource::Live {
            use crate::notification_dispatch::{
                NotificationObservation, NotificationPostCommit, ProviderDeliveryIdentity,
                SharedJson,
            };
            let effect = NotificationPostCommit::Analytics {
                observation: NotificationObservation {
                    agent: Some(SharedJson::new(agent.clone())),
                    method: "thread/tokenUsage/updated".into(),
                    params: SharedJson::new(params),
                    delivery: ProviderDeliveryIdentity {
                        provider_handle: "account:default".into(),
                        generation: 1,
                        cursor: at as u64,
                    },
                    observed_at: at,
                    provider_at: at,
                },
                budget_capture_value: None,
                usage_conflict: conflict.map(Box::new),
                ledger_since: None,
            };
            store
                .transaction(move |tx| {
                    let _remaining = crate::analytics_projection::log_effect(tx, effect)?;
                    Ok(())
                })
                .await
                .expect("journal analytics effect");
            let cursor = crate::analytics_projection::project_pending(store)
                .await
                .expect("project usage");
            assert_eq!(
                crate::analytics_projection::project_pending(store)
                    .await
                    .expect("replay projection"),
                cursor
            );
        } else {
            let mut prepared = crate::analytics_capture::PreparedCapture::event(
                agent,
                "thread/tokenUsage/updated",
                &params,
                at,
                at,
                crate::analytics_capture::CaptureSource::Rollout,
            )
            .expect("prepare");
            prepared.apply_budget_conflicts(&conflict.into_iter().collect::<Vec<_>>());
            crate::analytics_capture::capture(store, prepared)
                .await
                .expect("analytics history caller");
        }
    }

    async fn ledger_tokens(store: &RuntimeStore) -> u64 {
        store
            .transaction(|tx| {
                let agent = studio_runtime_store::budget::BudgetRepository::agent(tx, "agent")?;
                Ok(agent["tokensUsed"].as_u64().expect("tokens"))
            })
            .await
            .expect("budget total")
    }

    fn ledger_response(evidence: &str, output: u64) -> Value {
        json!({"threadId":"thread","studioUsage":{"kind":"response","responseId":"response","model":"claude-test","evidence":evidence,"tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":output}}})
    }

    fn counter_params() -> Value {
        json!({"threadId":"thread","turnId":"turn","tokenUsage":{"total":{"inputTokens":10,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0,"totalTokens":10},"last":{"inputTokens":10,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0,"totalTokens":10}}})
    }

    async fn delta_unknown_counter() -> (tempfile::TempDir, RuntimeStore, Value) {
        let (temp, store, agent) = ledger_store("codex").await;
        ledger_capture(
            &store,
            &agent,
            counter_params(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        store
            .transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_budget_usage SET id='erased-legacy-key',details=NULL",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("unknown counter");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let raw: String =
                    tx.query_row("SELECT record FROM analytics_usage", [], |row| row.get(0))?;
                let mut row: Value = serde_json::from_str(&raw)?;
                row.as_object_mut().expect("row").remove("usageIdentity");
                row.as_object_mut().expect("row").remove("receiptKey");
                tx.execute("UPDATE analytics_usage SET record=?1", [row.to_string()])?;
                Ok(())
            })
            .await
            .expect("pre-v5 row");
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent.clone(), &sink, 2.)
            .await
            .expect("restore");
        (temp, store, agent)
    }

    #[tokio::test]
    async fn typed_usage_ledger_delta_unknown_receipt_ignores_context_metadata() {
        let (temp, store, agent) = delta_unknown_counter().await;
        drop(store);
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("restart");
        let mut enriched = counter_params();
        enriched["tokenUsage"]["modelContextWindow"] = json!(200000);
        ledger_capture(
            &store,
            &agent,
            enriched,
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "context metadata cannot bypass accounting uncertainty"
        );
        let original = counter_params();
        let mut observation = studio_runtime_schema::usage::parse_token_usage(
            studio_runtime_schema::usage::UsageProvider::Codex,
            &original,
        )
        .expect("counter");
        if let studio_runtime_schema::usage::UsageObservation::Cumulative {
            models,
            model_context_window,
            ..
        } = &mut observation
        {
            *model_context_window = Some(400000);
            *models = Some(vec![studio_runtime_schema::usage::CumulativeModelUsage {
                model: "informational-model".into(),
                tokens: studio_runtime_schema::usage::UsageTokens::default(),
                provider_cost_usd: Some(1.25),
            }]);
        }
        ledger_capture(
            &store,
            &agent,
            json!({"threadId":"thread","turnId":"turn","studioUsage":observation}),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "model lists and provider cost never identify a debit"
        );
    }

    fn rollout_counter(thread: &str) -> Value {
        let mut context = crate::analytics_rollout::RolloutContext::from_record(
            json!({"threadId":thread,"model":"gpt-test"}),
        )
        .expect("context");
        let native = json!({"input_tokens":10,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":10});
        context.actions(&json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":native,"last_token_usage":native}}}),"counter",1.).expect("real rollout").into_iter().find_map(|action|if let crate::analytics_rollout::RolloutAction::Event{params,..}=action{Some(params)}else{None}).expect("counter event")
    }

    async fn era_state(store: &RuntimeStore) -> Value {
        store
            .transaction(|tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_budget WHERE id='agent'",
                    [],
                    |row| row.get(0),
                )?;
                Ok(serde_json::from_str(&raw)?)
            })
            .await
            .expect("state")
    }

    async fn era_analytics_rows(store: &RuntimeStore) -> i64 {
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                Ok(tx.query_row("SELECT COUNT(*) FROM analytics_usage", [], |row| row.get(0))?)
            })
            .await
            .expect("analytics count")
    }

    async fn era_switch_account(store: &RuntimeStore, agent: &mut Value) {
        agent["accountKey"] = json!("switched");
        let seed = agent.clone();
        store
            .transaction(move |tx| {
                studio_runtime_store::budget::BudgetRepository::save_agent(tx, &seed)
            })
            .await
            .expect("account switch");
    }

    async fn era_old_typed(store: &RuntimeStore, agent: &Value) -> Value {
        let observation = studio_runtime_schema::usage::parse_token_usage(
            studio_runtime_schema::usage::UsageProvider::Codex,
            &counter_params(),
        )
        .expect("typed");
        let params = json!({"threadId":"thread","studioUsage":observation});
        ledger_capture(
            store,
            agent,
            params.clone(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        let old = studio_runtime_schema::usage::ledger::receipt_hash(&json!([
            "agent",
            "default",
            "notice",
            "turn",
            observation
        ]));
        store
            .transaction(move |tx| {
                tx.execute("UPDATE runtime_budget_usage SET id=?1,details=NULL", [old])?;
                let raw: String =
                    tx.query_row("SELECT record FROM runtime_budget", [], |row| row.get(0))?;
                let mut state: Value = serde_json::from_str(&raw)?;
                state.as_object_mut().expect("state").remove("ledgerSince");
                tx.execute("UPDATE runtime_budget SET record=?1", [state.to_string()])?;
                Ok(())
            })
            .await
            .expect("pre-v5 budget");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let raw: String =
                    tx.query_row("SELECT record FROM analytics_usage", [], |row| row.get(0))?;
                let mut row: Value = serde_json::from_str(&raw)?;
                row.as_object_mut().expect("row").remove("usageIdentity");
                row.as_object_mut().expect("row").remove("receiptKey");
                let fingerprint = studio_runtime_store::sha256_hex(
                    studio_runtime_store::python_json_compact(&row["studioUsage"]).as_bytes(),
                );
                let old_id = studio_runtime_store::sha256_hex(
                    studio_runtime_store::python_json_compact(&json!([
                        "agent",
                        "thread",
                        fingerprint
                    ]))
                    .as_bytes(),
                );
                row["id"] = json!(old_id);
                tx.execute(
                    "UPDATE analytics_usage SET id=?1,record=?2",
                    params![old_id, row.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("pre-v5 analytics");
        store
            .transaction_with(|tx| {
                crate::budget_service::status(tx, "agent", &Default::default(), false, 100.)
            })
            .await
            .expect("O(1) epoch migration");
        params
    }

    async fn era_old_change_never_debits_again(metadata: bool) {
        {
            let (_temp, store, mut agent) = ledger_store("codex").await;
            let mut params = era_old_typed(&store, &agent).await;
            if metadata {
                params["studioUsage"]["modelContextWindow"] = json!(200000);
            } else {
                era_switch_account(&store, &mut agent).await;
            }
            ledger_capture(
                &store,
                &agent,
                params,
                crate::budget::CaptureSource::History,
                1.,
            )
            .await;
            assert_eq!(
                ledger_tokens(&store).await,
                10,
                "legacy alias misses never debit"
            );
            assert_eq!(era_state(&store).await["historicalNotices"], 10);
            assert_eq!(
                era_analytics_rows(&store).await,
                1,
                "legacy miss cannot duplicate accounting"
            );
        }
    }

    #[tokio::test]
    async fn typed_usage_ledger_era_projection_keeps_misses_unknown_and_metrics() {
        let (_temp, store, agent) = ledger_store("codex").await;
        let mut params = era_old_typed(&store, &agent).await;
        params["studioUsage"]["modelContextWindow"] = json!(200000);
        for _ in 0..2 {
            ledger_capture(
                &store,
                &agent,
                params.clone(),
                crate::budget::CaptureSource::Live,
                1.,
            )
            .await;
        }
        assert_eq!(ledger_tokens(&store).await, 10);
        assert_eq!(era_analytics_rows(&store).await, 1);
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let unknown: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM analytics_meta WHERE key LIKE 'unknownUsageReceipt:%'",
                    [],
                    |row| row.get(0),
                )?;
                let metrics: i64 = tx.query_row(
                    "SELECT SUM(count) FROM analytics_notifications",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(
                    unknown, 1,
                    "unknown fingerprint survives replay without duplicate diagnostics"
                );
                assert_eq!(
                    metrics, 2,
                    "accounting rejection retains both replay notification metrics"
                );
                Ok(())
            })
            .await
            .expect("projection diagnostics");
    }

    #[tokio::test]
    async fn typed_usage_ledger_era_old_metadata_never_debits_again() {
        era_old_change_never_debits_again(true).await;
    }

    #[tokio::test]
    async fn typed_usage_ledger_era_old_account_never_debits_again() {
        era_old_change_never_debits_again(false).await;
    }

    #[tokio::test]
    async fn typed_usage_ledger_era_scalar_notices_share_era_and_stable_identity() {
        for legacy in [false, true] {
            let (_temp, store, mut agent) = ledger_store("claude").await;
            let params = json!({"threadId":"thread","turnId":"turn","tokenUsage":{"total":{"totalTokens":10},"last":{"totalTokens":10}}});
            ledger_capture(
                &store,
                &agent,
                params.clone(),
                crate::budget::CaptureSource::History,
                1.,
            )
            .await;
            if legacy {
                store
                    .transaction(|tx| {
                        tx.execute(
                            "UPDATE runtime_budget SET record=json_set(record,'$.ledgerSince',100)",
                            [],
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("legacy state");
            }
            era_switch_account(&store, &mut agent).await;
            ledger_capture(
                &store,
                &agent,
                params,
                crate::budget::CaptureSource::History,
                1.,
            )
            .await;
            assert_eq!(ledger_tokens(&store).await, 10);
            assert_eq!(era_analytics_rows(&store).await, 1);
        }
    }

    #[tokio::test]
    async fn typed_usage_ledger_era_post_epoch_replay_has_one_accounting_row() {
        let (_temp, store, mut agent) = ledger_store("codex").await;
        let observation = studio_runtime_schema::usage::parse_token_usage(
            studio_runtime_schema::usage::UsageProvider::Codex,
            &counter_params(),
        )
        .expect("typed");
        let mut params = json!({"threadId":"thread","studioUsage":observation});
        ledger_capture(
            &store,
            &agent,
            params.clone(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        params["studioUsage"]["modelContextWindow"] = json!(200000);
        era_switch_account(&store, &mut agent).await;
        ledger_capture(
            &store,
            &agent,
            params,
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(ledger_tokens(&store).await, 10);
        assert_eq!(era_analytics_rows(&store).await, 1);
        assert!(
            era_state(&store).await["ledgerSince"].is_null(),
            "v5-created state has no legacy era"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_era_estimated_counter_stays_unknown() {
        let (_temp, store, agent) = ledger_store("codex").await;
        let mut params = era_old_typed(&store, &agent).await;
        params["studioUsage"]["total"]["uncachedInput"] = json!(30);
        params["studioUsage"]["last"]["uncachedInput"] = json!(30);
        params["_analyticsTimestampSource"] = json!("fileModifiedEstimate");
        ledger_capture(
            &store,
            &agent,
            params,
            crate::budget::CaptureSource::History,
            200.,
        )
        .await;
        assert_eq!(ledger_tokens(&store).await, 10);
        let state = era_state(&store).await;
        assert!(
            state["faults"]
                .as_array()
                .expect("faults")
                .contains(&json!("unknownUsageReceipt"))
        );
        assert_eq!(era_analytics_rows(&store).await, 1);
    }

    #[tokio::test]
    async fn typed_usage_ledger_era_epoch_survives_restart_restore_and_new_counters_debit_once() {
        let (temp, store, agent) = ledger_store("codex").await;
        let params = era_old_typed(&store, &agent).await;
        assert_eq!(era_state(&store).await["ledgerSince"], 100.);
        drop(store);
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("restart");
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent.clone(), &sink, 300.)
            .await
            .expect("restore");
        assert_eq!(era_state(&store).await["ledgerSince"], 100.);
        for (turn, total, last) in [("turn", 30, 20), ("new-turn", 40, 10)] {
            let mut next = params.clone();
            next["studioUsage"]["turnId"] = json!(turn);
            next["studioUsage"]["total"]["uncachedInput"] = json!(total);
            next["studioUsage"]["last"]["uncachedInput"] = json!(last);
            for _ in 0..2 {
                ledger_capture(
                    &store,
                    &agent,
                    next.clone(),
                    crate::budget::CaptureSource::History,
                    200.,
                )
                .await;
            }
        }
        assert_eq!(ledger_tokens(&store).await, 40);
        assert_eq!(era_analytics_rows(&store).await, 3);
    }

    #[tokio::test]
    async fn typed_usage_ledger_delta_known_counter_metadata_does_not_create_new_debit() {
        let (_temp, store, agent) = ledger_store("codex").await;
        let native = rollout_counter("thread");
        let observation = studio_runtime_schema::usage::parse_token_usage(
            studio_runtime_schema::usage::UsageProvider::Codex,
            &native,
        )
        .expect("complete canonical counter");
        let first = json!({"threadId":"thread","studioUsage":observation});
        ledger_capture(
            &store,
            &agent,
            first.clone(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        let mut enriched = first;
        enriched["studioUsage"]["modelContextWindow"] = json!(200000);
        studio_runtime_schema::usage::parse_token_usage(
            studio_runtime_schema::usage::UsageProvider::Codex,
            &enriched,
        )
        .expect("enriched supported counter");
        ledger_capture(
            &store,
            &agent,
            enriched,
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "a known counter's informational metadata cannot create a debit either"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_delta_proven_scalar_metadata_cannot_recharge() {
        let (_temp, store, agent) = ledger_store("codex").await;
        let original = counter_params();
        ledger_capture(
            &store,
            &agent,
            original.clone(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        let old = studio_runtime_schema::usage::ledger::receipt_hash(&json!([
            "agent",
            "default",
            "notice",
            "turn",
            original["tokenUsage"]["total"],
            original["tokenUsage"]["last"]
        ]));
        store
            .transaction(move |tx| {
                tx.execute("UPDATE runtime_budget_usage SET id=?1,details=NULL", [old])?;
                Ok(())
            })
            .await
            .expect("old scalar");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let raw: String =
                    tx.query_row("SELECT record FROM analytics_usage", [], |row| row.get(0))?;
                let mut row: Value = serde_json::from_str(&raw)?;
                row.as_object_mut().expect("row").remove("usageIdentity");
                row.as_object_mut().expect("row").remove("receiptKey");
                tx.execute("UPDATE analytics_usage SET record=?1", [row.to_string()])?;
                Ok(())
            })
            .await
            .expect("old source");
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent.clone(), &sink, 2.)
            .await
            .expect("prove old scalar ownership");
        let mut enriched = original;
        enriched["tokenUsage"]["modelContextWindow"] = json!(200000);
        ledger_capture(
            &store,
            &agent,
            enriched,
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "proven legacy notices cannot debit metadata again"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_delta_partial_old_gap_inventory_never_proves_complete() {
        let (temp, store, agent) = ledger_store("claude").await;
        for id in ["retained-old", "erased-old"] {
            ledger_capture(&store,&agent,json!({"threadId":"thread","studioUsage":{"kind":"gap","responseId":id,"reason":"queryClosed"}}),crate::budget::CaptureSource::Live,1.).await;
        }
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_budget_usage", [])?;
                let raw: String =
                    tx.query_row("SELECT record FROM runtime_budget", [], |row| row.get(0))?;
                let mut state: Value = serde_json::from_str(&raw)?;
                state
                    .as_object_mut()
                    .expect("state")
                    .remove("unresolvedGaps");
                state.as_object_mut().expect("state").remove("gapCoverage");
                tx.execute("UPDATE runtime_budget SET record=?1", [state.to_string()])?;
                Ok(())
            })
            .await
            .expect("real v4 gap state without usage receipts");
        store.analytics_transaction::<_,studio_runtime_store::StoreError>(|tx,_|{
            tx.execute("DELETE FROM analytics_usage WHERE json_extract(record,'$.studioUsage.responseId')='erased-old'",[])?;
            let raw:String=tx.query_row("SELECT record FROM analytics_usage",[],|row|row.get(0))?;let mut row:Value=serde_json::from_str(&raw)?;row.as_object_mut().expect("row").remove("usageIdentity");row.as_object_mut().expect("row").remove("receiptKey");tx.execute("UPDATE analytics_usage SET record=?1",[row.to_string()])?;Ok(())
        }).await.expect("partial pre-ledger inventory");
        let mut final_retained = ledger_response("live", 1);
        final_retained["studioUsage"]["responseId"] = json!("retained-old");
        ledger_capture(
            &store,
            &agent,
            final_retained,
            crate::budget::CaptureSource::Live,
            2.,
        )
        .await;
        drop(store);
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("restart");
        let accounting=store.transaction(|tx|Ok(studio_runtime_store::budget::BudgetRepository::agent(tx,"agent")?["tokenUsageAccounting"].clone())).await.expect("accounting");
        assert_eq!(
            accounting, "provisional",
            "one retained old gap never proves that erased gaps did not exist"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_delta_scalar_alias_requires_thread_ownership() {
        let (_temp, store, mut agent) = ledger_store("codex").await;
        agent.as_object_mut().expect("agent").remove("turnId");
        let seed = agent.clone();
        store
            .transaction(move |tx| {
                studio_runtime_store::budget::BudgetRepository::save_agent(tx, &seed)
            })
            .await
            .expect("no turn");
        let first = rollout_counter("thread");
        ledger_capture(
            &store,
            &agent,
            first.clone(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        let observation = studio_runtime_schema::usage::parse_token_usage(
            studio_runtime_schema::usage::UsageProvider::Codex,
            &first,
        )
        .expect("typed counter");
        let old = studio_runtime_schema::usage::ledger::legacy_receipt_keys(
            "agent",
            "default",
            None,
            &first,
            &observation,
        )
        .remove(0);
        store
            .transaction(move |tx| {
                tx.execute("UPDATE runtime_budget_usage SET id=?1,details=NULL", [old])?;
                Ok(())
            })
            .await
            .expect("old scalar receipt");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let raw: String =
                    tx.query_row("SELECT record FROM analytics_usage", [], |row| row.get(0))?;
                let mut row: Value = serde_json::from_str(&raw)?;
                row.as_object_mut().expect("row").remove("usageIdentity");
                row.as_object_mut().expect("row").remove("receiptKey");
                tx.execute("UPDATE analytics_usage SET record=?1", [row.to_string()])?;
                Ok(())
            })
            .await
            .expect("old analytics");
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent.clone(), &sink, 2.)
            .await
            .expect("restore first thread");
        agent["threadId"] = json!("second-thread");
        let seed = agent.clone();
        store
            .transaction(move |tx| {
                studio_runtime_store::budget::BudgetRepository::save_agent(tx, &seed)
            })
            .await
            .expect("thread transition");
        ledger_capture(
            &store,
            &agent,
            rollout_counter("second-thread"),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            20,
            "a proven first-thread alias cannot suppress another thread"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_delta_unproven_alias_stays_visible_after_new_rows() {
        let (_temp, store, mut agent) = ledger_store("codex").await;
        agent.as_object_mut().expect("agent").remove("turnId");
        let seed = agent.clone();
        store
            .transaction(move |tx| {
                studio_runtime_store::budget::BudgetRepository::save_agent(tx, &seed)
            })
            .await
            .expect("no turn");
        let first = rollout_counter("thread");
        ledger_capture(
            &store,
            &agent,
            first.clone(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        let observation = studio_runtime_schema::usage::parse_token_usage(
            studio_runtime_schema::usage::UsageProvider::Codex,
            &first,
        )
        .expect("counter");
        let old = studio_runtime_schema::usage::ledger::legacy_receipt_keys(
            "agent",
            "default",
            None,
            &first,
            &observation,
        )
        .remove(0);
        store
            .transaction(move |tx| {
                tx.execute("UPDATE runtime_budget_usage SET id=?1,details=NULL", [old])?;
                tx.execute(
                    "UPDATE runtime_budget SET record=json_set(record,'$.ledgerSince',100)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("scalar alias");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                tx.execute("DELETE FROM analytics_usage", [])?;
                Ok(())
            })
            .await
            .expect("lost ownership evidence");
        agent["threadId"] = json!("second-thread");
        let seed = agent.clone();
        store
            .transaction(move |tx| {
                studio_runtime_store::budget::BudgetRepository::save_agent(tx, &seed)
            })
            .await
            .expect("thread transition");
        for _ in 0..2 {
            ledger_capture(
                &store,
                &agent,
                rollout_counter("second-thread"),
                crate::budget::CaptureSource::History,
                1.,
            )
            .await;
        }
        let (accounting, state) = store
            .transaction(|tx| {
                let agent = studio_runtime_store::budget::BudgetRepository::agent(tx, "agent")?;
                let raw: String =
                    tx.query_row("SELECT record FROM runtime_budget", [], |row| row.get(0))?;
                Ok((
                    agent["tokenUsageAccounting"].clone(),
                    serde_json::from_str::<Value>(&raw)?,
                ))
            })
            .await
            .expect("visible uncertainty");
        assert_eq!(ledger_tokens(&store).await, 10);
        assert_eq!(accounting, "provisional");
        assert!(
            state["faults"]
                .as_array()
                .expect("faults")
                .iter()
                .any(|fault| fault == studio_runtime_schema::usage::ledger::UNKNOWN_USAGE_RECEIPT),
            "new rows cannot manufacture old ownership"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_delta_new_gaps_do_not_prove_missing_old_inventory() {
        let (_temp, store, agent) = ledger_store("claude").await;
        ledger_capture(&store,&agent,json!({"threadId":"thread","studioUsage":{"kind":"gap","responseId":"lost-old","reason":"queryClosed"}}),crate::budget::CaptureSource::Live,1.).await;
        store
            .transaction(|tx| {
                let raw: String =
                    tx.query_row("SELECT record FROM runtime_budget", [], |row| row.get(0))?;
                let mut state: Value = serde_json::from_str(&raw)?;
                state
                    .as_object_mut()
                    .expect("state")
                    .remove("unresolvedGaps");
                state.as_object_mut().expect("state").remove("gapCoverage");
                tx.execute("UPDATE runtime_budget SET record=?1", [state.to_string()])?;
                Ok(())
            })
            .await
            .expect("old coverage");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                tx.execute("DELETE FROM analytics_usage", [])?;
                Ok(())
            })
            .await
            .expect("lost old inventory");
        ledger_capture(&store,&agent,json!({"threadId":"thread","studioUsage":{"kind":"gap","responseId":"response","reason":"queryClosed"}}),crate::budget::CaptureSource::Live,2.).await;
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 1),
            crate::budget::CaptureSource::Live,
            3.,
        )
        .await;
        let accounting=store.transaction(|tx|Ok(studio_runtime_store::budget::BudgetRepository::agent(tx,"agent")?["tokenUsageAccounting"].clone())).await.expect("accounting");
        assert_eq!(
            accounting, "provisional",
            "new gaps never discharge unknown old coverage"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_review_pre_v5_counter_restore_cannot_add_a_debit() {
        let (_temp, store, agent) = ledger_store("codex").await;
        ledger_capture(
            &store,
            &agent,
            counter_params(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        let params = counter_params();
        let old_key = studio_runtime_schema::usage::ledger::receipt_hash(&json!([
            "agent",
            "default",
            "notice",
            "turn",
            params["tokenUsage"]["total"],
            params["tokenUsage"]["last"]
        ]));
        let old = old_key.clone();
        store
            .transaction(move |tx| {
                tx.execute("UPDATE runtime_budget_usage SET id=?1,details=NULL", [old])?;
                tx.execute(
                    "UPDATE runtime_budget SET record=json_set(record,'$.ledgerSince',100)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("old receipt");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let (id, raw): (String, String) =
                    tx.query_row("SELECT id,record FROM analytics_usage", [], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?;
                let mut row: Value = serde_json::from_str(&raw)?;
                row.as_object_mut().expect("row").remove("usageIdentity");
                row.as_object_mut().expect("row").remove("receiptKey");
                tx.execute(
                    "UPDATE analytics_usage SET record=?1 WHERE id=?2",
                    params![row.to_string(), id],
                )?;
                Ok(())
            })
            .await
            .expect("pre-v5 analytics row");
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent.clone(), &sink, 2.)
            .await
            .expect("restore");
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "a missing v5 identity cannot create another debit"
        );
        ledger_capture(
            &store,
            &agent,
            counter_params(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "re-import reuses the recovered receipt"
        );
        store
            .transaction(move |tx| {
                let row = studio_runtime_store::budget::BudgetRepository::usage(tx, &old_key)?
                    .expect("original debit key retained");
                assert_eq!(row.tokens, 10);
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_budget WHERE id='agent'",
                    [],
                    |row| row.get(0),
                )?;
                let state: Value = serde_json::from_str(&raw)?;
                assert!(
                    !state["faults"]
                        .as_array()
                        .expect("faults")
                        .iter()
                        .any(|fault| fault
                            == studio_runtime_schema::usage::ledger::UNKNOWN_USAGE_RECEIPT),
                    "the supported old receipt was recovered, not treated as an unknown debit"
                );
                Ok(())
            })
            .await
            .expect("resolved old receipt");
    }

    #[tokio::test]
    async fn typed_usage_ledger_review_unrecoverable_counter_stays_unknown_after_reimport() {
        let (temp, store, agent) = ledger_store("codex").await;
        ledger_capture(
            &store,
            &agent,
            counter_params(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        store
            .transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_budget_usage SET id='unrecoverable-old-receipt',details=NULL",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("unknown old receipt");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let raw: String =
                    tx.query_row("SELECT record FROM analytics_usage", [], |row| row.get(0))?;
                let mut row: Value = serde_json::from_str(&raw)?;
                row.as_object_mut().expect("row").remove("receiptKey");
                row.as_object_mut().expect("row").remove("usageIdentity");
                tx.execute("UPDATE analytics_usage SET record=?1", [row.to_string()])?;
                Ok(())
            })
            .await
            .expect("pre-v5 row");
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent.clone(), &sink, 2.)
            .await
            .expect("unknown restore");
        assert_eq!(ledger_tokens(&store).await, 10);
        drop(store);
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("restart with unknown receipt");
        ledger_capture(
            &store,
            &agent,
            counter_params(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "re-import cannot bypass an unresolved old counter"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_review_restore_cannot_bypass_ambiguous_provenance() {
        let (_temp, store, agent) = ledger_store("claude").await;
        ledger_capture(
            &store,
            &agent,
            ledger_response("legacy", 1),
            crate::budget::CaptureSource::Live,
            1.,
        )
        .await;
        store
            .transaction(|tx| {
                tx.execute("UPDATE runtime_budget_usage SET details=NULL", [])?;
                Ok(())
            })
            .await
            .expect("old receipt");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let raw: String =
                    tx.query_row("SELECT record FROM analytics_usage", [], |row| row.get(0))?;
                let mut legacy: Value = serde_json::from_str(&raw)?;
                legacy.as_object_mut().expect("row").remove("usageIdentity");
                legacy.as_object_mut().expect("row").remove("receiptKey");
                legacy["id"] = json!("legacy");
                tx.execute("DELETE FROM analytics_usage", [])?;
                Repo::write(tx, CaptureTable::Usage, &legacy)?;
                let mut final_row = legacy;
                final_row["id"] = json!("final");
                final_row["at"] = json!(2.);
                final_row["studioUsage"]["evidence"] = json!("live");
                final_row["studioUsage"]["tokens"]["output"] = json!(100);
                Repo::write(tx, CaptureTable::Usage, &final_row)?;
                Ok(())
            })
            .await
            .expect("ambiguous old evidence");
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent, &sink, 3.)
            .await
            .expect("restore");
        assert_eq!(
            ledger_tokens(&store).await,
            11,
            "restore must use the same uniqueness check as capture"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_review_unrelated_final_does_not_resolve_unknown_old_gaps() {
        let (temp, store, agent) = ledger_store("claude").await;
        ledger_capture(&store,&agent,json!({"threadId":"thread","studioUsage":{"kind":"gap","responseId":"missing","reason":"queryClosed"}}),crate::budget::CaptureSource::Live,1.).await;
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                let raw: String =
                    tx.query_row("SELECT record FROM analytics_usage", [], |row| row.get(0))?;
                let mut row: Value = serde_json::from_str(&raw)?;
                row.as_object_mut()
                    .expect("old gap")
                    .remove("usageIdentity");
                row.as_object_mut().expect("old gap").remove("receiptKey");
                tx.execute("UPDATE analytics_usage SET record=?1", [row.to_string()])?;
                Ok(())
            })
            .await
            .expect("pre-ledger gap inventory");
        store
            .transaction(|tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_budget WHERE id='agent'",
                    [],
                    |row| row.get(0),
                )?;
                let mut state: Value = serde_json::from_str(&raw)?;
                state
                    .as_object_mut()
                    .expect("state")
                    .remove("unresolvedGaps");
                state.as_object_mut().expect("state").remove("gapCoverage");
                tx.execute(
                    "UPDATE runtime_budget SET record=?1 WHERE id='agent'",
                    [state.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("old gap state");
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 100),
            crate::budget::CaptureSource::Live,
            2.,
        )
        .await;
        let accounting=store.transaction(|tx|Ok(studio_runtime_store::budget::BudgetRepository::agent(tx,"agent")?["tokenUsageAccounting"].clone())).await.expect("accounting");
        assert_eq!(
            accounting, "provisional",
            "unrelated finals cannot prove old gap coverage"
        );
        drop(store);
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("restart");
        let mut final_missing = ledger_response("live", 1);
        final_missing["studioUsage"]["responseId"] = json!("missing");
        ledger_capture(
            &store,
            &agent,
            final_missing,
            crate::budget::CaptureSource::Live,
            3.,
        )
        .await;
        let accounting=store.transaction(|tx|Ok(studio_runtime_store::budget::BudgetRepository::agent(tx,"agent")?["tokenUsageAccounting"].clone())).await.expect("accounting");
        assert_eq!(
            accounting, "provisional",
            "a known resolved subset cannot prove that no other old gap was erased"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_review_missing_gap_inventory_remains_unknown_after_restart() {
        let (temp, store, agent) = ledger_store("claude").await;
        ledger_capture(&store,&agent,json!({"threadId":"thread","studioUsage":{"kind":"gap","responseId":"old-gap","reason":"queryClosed"}}),crate::budget::CaptureSource::Live,1.).await;
        store
            .transaction(|tx| {
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_budget WHERE id='agent'",
                    [],
                    |row| row.get(0),
                )?;
                let mut state: Value = serde_json::from_str(&raw)?;
                state
                    .as_object_mut()
                    .expect("state")
                    .remove("unresolvedGaps");
                state.as_object_mut().expect("state").remove("gapCoverage");
                tx.execute(
                    "UPDATE runtime_budget SET record=?1 WHERE id='agent'",
                    [state.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("old coverage");
        store
            .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                tx.execute("DELETE FROM analytics_usage", [])?;
                Ok(())
            })
            .await
            .expect("missing old gap inventory");
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 1),
            crate::budget::CaptureSource::Live,
            2.,
        )
        .await;
        drop(store);
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("restart");
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 1),
            crate::budget::CaptureSource::Live,
            3.,
        )
        .await;
        let (accounting, state) = store
            .transaction(|tx| {
                let agent = studio_runtime_store::budget::BudgetRepository::agent(tx, "agent")?;
                let raw: String = tx.query_row(
                    "SELECT record FROM runtime_budget WHERE id='agent'",
                    [],
                    |row| row.get(0),
                )?;
                Ok((
                    agent["tokenUsageAccounting"].clone(),
                    serde_json::from_str::<Value>(&raw)?,
                ))
            })
            .await
            .expect("unknown coverage");
        assert_eq!(accounting, "provisional");
        assert_eq!(state["gapCoverage"], "legacyUnknown");
    }

    #[tokio::test]
    async fn typed_usage_ledger_review_rollout_counters_include_their_thread_scope() {
        let (_temp, store, mut agent) = ledger_store("codex").await;
        for thread in ["thread", "second-thread"] {
            agent["threadId"] = json!(thread);
            agent.as_object_mut().expect("agent").remove("turnId");
            let updated = agent.clone();
            store
                .transaction(move |tx| {
                    studio_runtime_store::budget::BudgetRepository::save_agent(tx, &updated)
                })
                .await
                .expect("thread transition");
            let mut context = crate::analytics_rollout::RolloutContext::from_record(
                json!({"threadId":thread,"model":"gpt-test"}),
            )
            .expect("rollout");
            let native = json!({"input_tokens":10,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":10});
            for action in context.actions(&json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":native,"last_token_usage":native}}}),"counter",1.).expect("real rollout converter") {
                if let crate::analytics_rollout::RolloutAction::Event{params,..}=action {
                    ledger_capture(&store,&agent,params,crate::budget::CaptureSource::History,1.).await;
                }
            }
        }
        assert_eq!(
            ledger_tokens(&store).await,
            20,
            "equal counters from distinct thread scopes are distinct observations"
        );
    }

    #[tokio::test]
    async fn typed_usage_ledger_legacy_correction_replay_and_restart_match_costs() {
        let (temp, store, agent) = ledger_store("claude").await;
        ledger_capture(
            &store,
            &agent,
            ledger_response("legacy", 1),
            crate::budget::CaptureSource::Live,
            1.,
        )
        .await;
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 100),
            crate::budget::CaptureSource::Live,
            2.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            110,
            "legacy may be corrected by final evidence"
        );
        ledger_capture(
            &store,
            &agent,
            ledger_response("recovered", 100),
            crate::budget::CaptureSource::History,
            3.,
        )
        .await;
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 200),
            crate::budget::CaptureSource::Live,
            4.,
        )
        .await;
        assert_eq!(ledger_tokens(&store).await, 110, "first final wins");
        let db = AnalyticsRepository::new(temp.path())
            .open_read_only()
            .expect("analytics");
        let groups =
            studio_runtime_store::cost_aggregate::cost_groups(&db, "agent", &Default::default())
                .expect("groups");
        assert_eq!(
            groups
                .iter()
                .map(|g| (g.tokens.input_tokens.unwrap_or(0.)
                    + g.tokens.output_tokens.unwrap_or(0.))
                    * g.count as f64)
                .sum::<f64>(),
            110.
        );
        drop(db);
        std::fs::create_dir_all(temp.path().join("model-pricing")).expect("pricing directory");
        std::fs::write(temp.path().join("model-pricing/models-dev-v1.json"),json!({"version":1,"catalog":{"providers":{"openai":{"models":{"gpt-test":{"cost":{"input":1,"output":1}}}},"anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":1,"cache_read":1,"cache_write":1}}}}}}}).to_string()).expect("catalog");
        let cost =
            crate::costs::session_cost_with_account_profiles(&store, "agent", &Default::default())
                .expect("session cost");
        assert_eq!(cost["pricedSamples"], 1);
        assert!((cost["totalUSD"].as_f64().expect("cost") - 110. / 1_000_000.).abs() < 1e-12);
        drop(store);
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("restart");
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 100),
            crate::budget::CaptureSource::Live,
            5.,
        )
        .await;
        assert_eq!(ledger_tokens(&store).await, 110);
        let after =
            crate::costs::session_cost_with_account_profiles(&store, "agent", &Default::default())
                .expect("restarted session cost");
        assert_eq!(after["totalUSD"], cost["totalUSD"]);
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent, &sink, 6.)
            .await
            .expect("restore final receipt");
        assert_eq!(ledger_tokens(&store).await, 110);
    }

    #[tokio::test]
    async fn typed_usage_ledger_old_receipts_recover_provenance_before_correction() {
        for evidence in [Some("legacy"), Some("live"), None] {
            let (temp, store, agent) = ledger_store("claude").await;
            let db = AnalyticsRepository::new(temp.path())
                .open_read_only()
                .expect("analytics");
            let plan: String = db
                .query_row(
                    &format!(
                        "EXPLAIN QUERY PLAN {}",
                        studio_runtime_store::analytics_capture::USAGE_PROVENANCE_SQL
                    ),
                    params!["agent", "thread", "response"],
                    |row| row.get(3),
                )
                .expect("indexed provenance plan");
            assert!(
                plan.contains("SEARCH")
                    && plan.contains("analytics_usage_response")
                    && !plan.contains("SCAN"),
                "{plan}"
            );
            drop(db);
            ledger_capture(
                &store,
                &agent,
                ledger_response(evidence.unwrap_or("live"), 1),
                crate::budget::CaptureSource::Live,
                1.,
            )
            .await;
            store
                .transaction(|tx| {
                    tx.execute("UPDATE runtime_budget_usage SET details=NULL", [])?;
                    Ok(())
                })
                .await
                .expect("pre-v5 receipt");
            if evidence.is_none() {
                store
                    .transaction(|tx| {
                        tx.execute("UPDATE runtime_budget_usage SET breakdown=NULL", [])?;
                        Ok(())
                    })
                    .await
                    .expect("old total-only receipt");
                store
                    .analytics_transaction::<_, studio_runtime_store::StoreError>(|tx, _| {
                        tx.execute("DELETE FROM analytics_usage", [])?;
                        Ok(())
                    })
                    .await
                    .expect("missing analytics evidence");
            }
            ledger_capture(
                &store,
                &agent,
                ledger_response("recovered", 100),
                crate::budget::CaptureSource::History,
                2.,
            )
            .await;
            let expected = if evidence == Some("legacy") { 110 } else { 11 };
            assert_eq!(
                ledger_tokens(&store).await,
                expected,
                "old receipt provenance: {evidence:?}"
            );
            drop(store);
            let store = RuntimeStore::open(Some(temp.path()))
                .await
                .expect("restart");
            ledger_capture(
                &store,
                &agent,
                ledger_response("live", 100),
                crate::budget::CaptureSource::Live,
                3.,
            )
            .await;
            assert_eq!(ledger_tokens(&store).await, expected);
            if evidence.is_none() {
                let db = AnalyticsRepository::new(temp.path())
                    .open_read_only()
                    .expect("analytics");
                assert_eq!(
                    db.query_row("SELECT count(*) FROM analytics_usage", [], |row| row
                        .get::<_, i64>(0))
                        .expect("usage count"),
                    0,
                    "a rejected final cannot fabricate an analytics row"
                );
                let result =
                    crate::costs::usage_reconciliation(&store, "agent", &Default::default())
                        .expect("diagnostic conflicts");
                assert_eq!(result["replayConflicts"][0]["retainedTotalTokens"], 11);
                assert_eq!(result["replayConflicts"][0]["incomingTotalTokens"], 110);
                assert_eq!(
                    result["replayConflicts"]
                        .as_array()
                        .expect("conflicts")
                        .len(),
                    1,
                    "replay deduplicates the conflict"
                );
            }
        }
    }

    #[tokio::test]
    async fn typed_usage_ledger_final_can_correct_legacy_downward() {
        let (_temp, store, agent) = ledger_store("claude").await;
        ledger_capture(
            &store,
            &agent,
            ledger_response("legacy", 100),
            crate::budget::CaptureSource::Live,
            1.,
        )
        .await;
        ledger_capture(
            &store,
            &agent,
            ledger_response("recovered", 1),
            crate::budget::CaptureSource::History,
            2.,
        )
        .await;
        assert_eq!(ledger_tokens(&store).await, 11);
    }

    #[tokio::test]
    async fn typed_usage_ledger_gap_after_legacy_stays_unresolved_until_final() {
        let (_temp, store, agent) = ledger_store("claude").await;
        ledger_capture(
            &store,
            &agent,
            ledger_response("legacy", 1),
            crate::budget::CaptureSource::Live,
            1.,
        )
        .await;
        ledger_capture(&store,&agent,json!({"threadId":"thread","studioUsage":{"kind":"gap","responseId":"response","reason":"queryClosed"}}),crate::budget::CaptureSource::Live,2.).await;
        let accounting=store.transaction(|tx|Ok(studio_runtime_store::budget::BudgetRepository::agent(tx,"agent")?["tokenUsageAccounting"].clone())).await.expect("accounting");
        assert_eq!(accounting, "provisional");
        assert_eq!(
            ledger_tokens(&store).await,
            11,
            "the gap preserves provisional known usage"
        );
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 100),
            crate::budget::CaptureSource::Live,
            3.,
        )
        .await;
        assert_eq!(ledger_tokens(&store).await, 110);
    }

    #[tokio::test]
    async fn typed_usage_ledger_final_resolves_only_its_gap() {
        let (_temp, store, agent) = ledger_store("claude").await;
        for response in ["response", "other"] {
            ledger_capture(&store,&agent,json!({"threadId":"thread","studioUsage":{"kind":"gap","responseId":response,"reason":"queryClosed"}}),crate::budget::CaptureSource::Live,1.).await;
        }
        ledger_capture(
            &store,
            &agent,
            ledger_response("live", 100),
            crate::budget::CaptureSource::Live,
            2.,
        )
        .await;
        let accounting = store.transaction(|tx| Ok(studio_runtime_store::budget::BudgetRepository::agent(tx,"agent")?["tokenUsageAccounting"].clone())).await.expect("one gap remains");
        assert_eq!(accounting, "provisional");
        let mut final_other = ledger_response("live", 100);
        final_other["studioUsage"]["responseId"] = json!("other");
        ledger_capture(
            &store,
            &agent,
            final_other,
            crate::budget::CaptureSource::Live,
            3.,
        )
        .await;
        store
            .transaction(|tx| {
                let agent = studio_runtime_store::budget::BudgetRepository::agent(tx, "agent")?;
                assert_eq!(
                    agent["tokenUsageAccounting"], "responseRecords",
                    "all gaps are resolved"
                );
                Ok(())
            })
            .await
            .expect("resolved gaps");
    }

    #[tokio::test]
    async fn typed_usage_ledger_restore_reimport_keeps_first_counter_receipt() {
        let (_temp, store, agent) = ledger_store("codex").await;
        let params = json!({"threadId":"thread","turnId":"turn","tokenUsage":{"total":{"inputTokens":10,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0,"totalTokens":10},"last":{"inputTokens":10,"cachedInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0,"totalTokens":10}}});
        ledger_capture(
            &store,
            &agent,
            params.clone(),
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(ledger_tokens(&store).await, 10);
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        migrate_budget(&store, agent.clone(), &sink, 2.)
            .await
            .expect("restore persisted row");
        ledger_capture(
            &store,
            &agent,
            params,
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "restore and re-import reuse the first receipt"
        );
        let mut response = ledger_response("recovered", 0);
        response["studioUsage"]["model"] = json!("gpt-test");
        ledger_capture(
            &store,
            &agent,
            response,
            crate::budget::CaptureSource::History,
            1.,
        )
        .await;
        assert_eq!(
            ledger_tokens(&store).await,
            10,
            "an exact record settles the historical counter"
        );
    }

    #[tokio::test]
    async fn legacy_total_only_claude_migration_preserves_budget_total() {
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        let agent = json!({"id":"agent","rootId":"agent","name":"Agent","status":"running","provider":"claude","threadId":"thread","turnId":"turn","accountKey":"default","accountHistory":[],"model":"claude-sonnet-4-5","epoch":0,"tokenBudget":1000,"tokensUsed":0});
        let seed = agent.clone();
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                    params!["agent", seed.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("seed agent");
        store.analytics_transaction::<_,studio_runtime_store::StoreError>(|tx,_| {
            Repo::write(tx,CaptureTable::Usage,&json!({"id":"legacy","agentId":"agent","rootId":"agent","threadId":"thread","turnId":"turn","at":1,"last":{"totalTokens":23},"total":{"totalTokens":23}}))?;
            Ok(())
        }).await.expect("seed sparse legacy usage");
        let sink = crate::analytics_history::StoreHistoryBudgetSink::new(&store);
        assert!(
            migrate_budget(&store, agent.clone(), &sink, 2.0)
                .await
                .expect("migrate legacy budget")
        );
        assert!(
            !migrate_budget(&store, agent, &sink, 2.0)
                .await
                .expect("idempotent migration")
        );
        let tokens = store
            .transaction(|tx| {
                Ok(tx.query_row(
                    "SELECT sum(tokens) FROM runtime_budget_usage WHERE agent='agent'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?)
            })
            .await
            .expect("budget ledger");
        assert_eq!(tokens, 23);
    }
}
