//! analytics.sqlite3 as a recoverable projection of Canvas.
//!
//! The journal commit logs each `Analytics`/`AnalyticsStream` effect of a
//! current agent identity in the Canvas table `runtime_analytics_projection`
//! ([`log_effect`]). The projector applies log rows in sequence order and saves
//! the last applied sequence in analytics with the rows it produced, so a
//! replay after any crash starts exactly after the last durable analytics
//! commit. It locks only the analytics connection and reads Canvas through a
//! reader; it never holds the Canvas writer except for a rare log prune.
//!
//! Commits use `synchronous=NORMAL` (`studio_runtime_store::sqlite_durability`),
//! so a commit reaches the disk only with a later flush. A prune therefore
//! first flushes analytics, and `RuntimeStore::open` keeps new log sequence
//! numbers after the analytics cursor when an OS crash rolled Canvas back.
use crate::analytics_capture::{CaptureError, capture_logged_effect, record_capture_errors};
use crate::notification_dispatch::NotificationPostCommit;
use rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;
use studio_runtime_store::analytics_capture::AnalyticsCaptureRepository as Repo;
use studio_runtime_store::{ObservedTable, RuntimeStore, StoreError};

/// Canvas log rows one projector analytics transaction applies. Unit: rows.
const PROJECTION_BATCH_ROWS: usize = 256;
/// Projected log rows after which the projector flushes analytics and deletes
/// them from Canvas in one writer transaction. Unit: rows (Canvas log sequence numbers).
const PROJECTION_PRUNE_ROWS: i64 = 4096;
/// Wait before a failed projection pass is retried. Unit: time.
const PROJECTION_RETRY_DELAY: Duration = Duration::from_secs(1);
/// `captureErrors` operation name of a log row the projector could not apply.
const PROJECTION_OPERATION: &str = "analytics_projection";
/// `captureErrors` code of a log row whose input or analytics rows are refused.
const PROJECTION_REFUSED: &str = "invalidInput";

/// Log an analytics effect in the caller's Canvas transaction and return the
/// effect that remains to be published: `AnalyticsCaptured` or
/// `AnalyticsStreamCaptured` for token rates without another state reset, or `None`
/// when the agent identity is stale (nothing is captured or published, as
/// before). Any other effect passes through unchanged.
pub(crate) fn log_effect(
    canvas: &Connection,
    effect: NotificationPostCommit,
) -> Result<Option<NotificationPostCommit>, StoreError> {
    let observation = match &effect {
        NotificationPostCommit::Analytics { observation, .. }
        | NotificationPostCommit::AnalyticsStream { observation, .. } => observation,
        _ => return Ok(Some(effect)),
    };
    // An agent-less effect is logged anyway: the projector counts its refusal.
    if let Some(agent) = &observation.agent {
        match crate::analytics_capture::check_identity(canvas, agent) {
            Ok(()) => {}
            Err(CaptureError::StaleIdentity) => return Ok(None),
            Err(CaptureError::Store(error)) => return Err(error),
            Err(CaptureError::Sqlite(error)) => return Err(error.into()),
            Err(CaptureError::Invalid(_)) => return Ok(None),
        }
    }
    Repo::append_projection(canvas, &serde_json::to_string(&effect)?)?;
    Ok(Some(match effect {
        NotificationPostCommit::Analytics { observation, .. } => {
            NotificationPostCommit::AnalyticsCaptured(observation)
        }
        NotificationPostCommit::AnalyticsStream { observation, .. } => {
            NotificationPostCommit::AnalyticsStreamCaptured(observation)
        }
        other => other,
    }))
}

/// The outcome of one analytics transaction of a pass.
enum Batch {
    /// Applied through this Canvas log sequence.
    Applied { through: i64, refused: i64 },
    /// The saved cursor differs from the one the page was read after.
    Moved(i64),
}

/// Apply every Canvas log row after the analytics cursor. Returns the cursor,
/// which analytics holds durably on return. A refused row (invalid input or
/// analytics rows) is counted in `captureErrors` and skipped; an SQLite failure
/// rolls back the batch and returns, so a later pass retries it.
pub async fn project_pending(store: &RuntimeStore) -> Result<i64, CaptureError> {
    let mut cursor = store
        .analytics_projection_transaction(|tx| Ok::<_, CaptureError>(Repo::projection_cursor(tx)?))
        .await?;
    loop {
        let after = cursor;
        let page = store
            .read_async("analytics/projection-page", move |canvas| {
                Repo::projection_page(canvas, after, PROJECTION_BATCH_ROWS)
            })
            .await?;
        let Some(&(last, _)) = page.last() else {
            return Ok(cursor);
        };
        let full = page.len() == PROJECTION_BATCH_ROWS;
        let batch = store
            .analytics_projection_transaction(move |tx| apply_page(tx, after, &page))
            .await?;
        match batch {
            Batch::Applied { through, refused } => {
                if refused > 0 {
                    eprintln!(
                        "analytics projection refused {refused} Canvas log rows through {through}"
                    );
                }
                cursor = through;
            }
            Batch::Moved(saved) => cursor = saved,
        }
        if !full && cursor == last {
            return Ok(cursor);
        }
    }
}

fn apply_page(
    tx: &rusqlite::Transaction<'_>,
    after: i64,
    page: &[(i64, String)],
) -> Result<Batch, CaptureError> {
    let saved = Repo::projection_cursor(tx)?;
    if saved != after {
        return Ok(Batch::Moved(saved));
    }
    let mut refused = 0_i64;
    let mut through = after;
    for (seq, record) in page {
        tx.execute_batch("SAVEPOINT projection_row")?;
        let applied = serde_json::from_str::<NotificationPostCommit>(record)
            .map_err(|_| CaptureError::Invalid("/record"))
            .and_then(|effect| capture_logged_effect(tx, &effect));
        match applied {
            Ok(()) => tx.execute_batch("RELEASE projection_row")?,
            Err(error) => {
                tx.execute_batch("ROLLBACK TO projection_row; RELEASE projection_row")?;
                if is_transient(&error) {
                    return Err(error);
                }
                refused += 1;
                record_capture_errors(
                    tx,
                    1,
                    PROJECTION_OPERATION,
                    PROJECTION_REFUSED,
                    &format!("Canvas log row {seq}: {error}"),
                )?;
            }
        }
        through = *seq;
    }
    Repo::set_projection_cursor(tx, through)?;
    Ok(Batch::Applied { through, refused })
}

/// SQLite failures may succeed on retry; every other refusal is deterministic.
fn is_transient(error: &CaptureError) -> bool {
    matches!(
        error,
        CaptureError::Sqlite(_) | CaptureError::Store(StoreError::Sqlite(_))
    )
}

/// Flush analytics, then delete Canvas log rows through `through`, the cursor
/// of an analytics commit made before this call. Returns `None` without a
/// prune when a reader kept the flush incomplete; a later pass retries.
pub async fn prune(store: &RuntimeStore, through: i64) -> Result<Option<usize>, StoreError> {
    if !store.flush_analytics_wal().await? {
        return Ok(None);
    }
    store
        .transaction(move |tx| Repo::prune_projection(tx, through))
        .await
        .map(Some)
}

/// Catch up from the durable cursor, then project after every Canvas commit
/// that touches the log, until `shutdown` turns true.
pub async fn run(store: Arc<RuntimeStore>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let wake = Arc::new(tokio::sync::Notify::new());
    {
        let wake = Arc::clone(&wake);
        store.add_table_change_observer(ObservedTable::AnalyticsProjection, move || {
            wake.notify_one()
        });
    }
    // Zero prunes once after the startup catch-up.
    let mut pruned = None;
    while !*shutdown.borrow() {
        let failed = match project_pending(&store).await {
            Ok(cursor) => {
                if pruned.is_none_or(|pruned| cursor - pruned >= PROJECTION_PRUNE_ROWS) {
                    match prune(&store, cursor).await {
                        Ok(Some(_)) => pruned = Some(cursor),
                        Ok(None) => {}
                        Err(error) => eprintln!("analytics projection prune failed: {error}"),
                    }
                }
                false
            }
            Err(error) => {
                eprintln!("analytics projection pass failed: {error}");
                true
            }
        };
        tokio::select! {
            () = wake.notified(), if !failed => {}
            () = tokio::time::sleep(PROJECTION_RETRY_DELAY), if failed => {}
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification_dispatch::{
        NotificationObservation, NotificationPublicationBatch, ProviderDeliveryIdentity, SharedJson,
    };
    use serde_json::{Value, json};

    /// Analytics tables whose rows the projection produces.
    const PROJECTED_TABLES: [&str; 7] = [
        "analytics_agents",
        "analytics_notifications",
        "analytics_turns",
        "analytics_items",
        "analytics_usage",
        "analytics_usage_roots",
        "analytics_meta",
    ];

    fn agent() -> Value {
        json!({"id":"agent","name":"Agent","rootId":"root","accountKey":"default",
            "accountHistory":[],"provider":"codex","threadId":"thread","turnId":"turn",
            "model":"gpt-5","effort":"high","fastMode":false,"daybreakEnabled":false,
            "cyberAccessProgram":false,"cwd":"/disposable","deletedAt":null,"epoch":0,"status":"running"})
    }

    fn effect(cursor: u64, method: &str, params: Value) -> NotificationPostCommit {
        NotificationPostCommit::Analytics {
            observation: NotificationObservation {
                agent: Some(SharedJson::new(agent())),
                method: method.into(),
                params: SharedJson::new(params),
                delivery: ProviderDeliveryIdentity {
                    provider_handle: "account:default".into(),
                    generation: 7,
                    cursor,
                },
                observed_at: 10.0 + cursor as f64,
                provider_at: 10.0 + cursor as f64,
            },
            budget_capture_value: None,
        }
    }

    /// A representative turn: start, a command and a message item, two usage
    /// updates, completion.
    fn turn_mix() -> Vec<NotificationPostCommit> {
        let usage = |cursor, total: u64| {
            effect(
                cursor,
                "thread/tokenUsage/updated",
                json!({"threadId":"thread","turnId":"turn","tokenUsage":{
                    "total":{"inputTokens":total,"outputTokens":10,"totalTokens":total + 10},
                    "last":{"inputTokens":total,"outputTokens":10,"totalTokens":total + 10}}}),
            )
        };
        vec![
            effect(
                1,
                "turn/started",
                json!({"threadId":"thread","turn":{"id":"turn"}}),
            ),
            effect(
                2,
                "item/started",
                json!({"threadId":"thread","turnId":"turn",
                    "item":{"id":"cmd","type":"commandExecution","command":"ls"}}),
            ),
            effect(
                3,
                "item/completed",
                json!({"threadId":"thread","turnId":"turn",
                    "item":{"id":"cmd","type":"commandExecution","command":"ls","aggregatedOutput":"a\nb"}}),
            ),
            usage(4, 20),
            effect(
                5,
                "item/completed",
                json!({"threadId":"thread","turnId":"turn",
                    "item":{"id":"answer","type":"agentMessage","text":"done"}}),
            ),
            usage(6, 45),
            effect(
                7,
                "turn/completed",
                json!({"threadId":"thread","turn":{"id":"turn","status":"completed"}}),
            ),
        ]
    }

    async fn open(root: &std::path::Path) -> RuntimeStore {
        RuntimeStore::open(Some(root)).await.expect("open store")
    }

    async fn seed_agent(store: &RuntimeStore) {
        let agent = agent();
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES ('agent',?1)",
                    [agent.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("seed agent");
    }

    /// Commit `effects` through the journal's publication path, as one batch.
    async fn journal(store: &RuntimeStore, sequence: u64, effects: Vec<NotificationPostCommit>) {
        let batch = NotificationPublicationBatch {
            sequence,
            effects,
            analytics_overflow: 0,
        };
        store
            .transaction_with(move |tx| {
                crate::account_worker::store::persist_publication_batch(tx, "default", &batch)
            })
            .await
            .expect("journal commit");
    }

    /// Every projected table in rowid order, without the store's own metadata
    /// and row sequence numbers.
    fn dump(root: &std::path::Path) -> Value {
        let db = Connection::open(root.join("analytics.sqlite3")).expect("analytics DB");
        let mut result = json!({});
        for table in PROJECTED_TABLES {
            let mut statement = db
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .expect("prepare dump");
            // SQLite's own row sequence (analytics_usage.seq) is not projected data.
            let columns = (0..statement.column_count())
                .filter(|&index| statement.column_name(index).is_ok_and(|name| name != "seq"))
                .collect::<Vec<_>>();
            let rows = statement
                .query_map([], |row| {
                    columns
                        .iter()
                        .map(|&index| {
                            Ok(match row.get_ref(index)? {
                                rusqlite::types::ValueRef::Null => Value::Null,
                                rusqlite::types::ValueRef::Integer(value) => json!(value),
                                rusqlite::types::ValueRef::Real(value) => json!(value),
                                rusqlite::types::ValueRef::Text(value) => {
                                    json!(String::from_utf8_lossy(value))
                                }
                                rusqlite::types::ValueRef::Blob(value) => json!(value),
                            })
                        })
                        .collect::<Result<Vec<_>, rusqlite::Error>>()
                })
                .expect("query dump")
                .collect::<Result<Vec<_>, _>>()
                .expect("decode dump");
            let rows = rows
                .into_iter()
                .filter(|row| {
                    table != "analytics_meta"
                        || !matches!(row[0].as_str(), Some(key) if key == "trackingSince" || key == CURSOR_KEY)
                })
                .collect::<Vec<_>>();
            result[table] = json!(rows);
        }
        result
    }

    const CURSOR_KEY: &str = studio_runtime_store::analytics_capture::PROJECTION_CURSOR_KEY;

    fn log_rows(store: &RuntimeStore) -> i64 {
        store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT count(*) FROM runtime_analytics_projection",
                [],
                |row| row.get(0),
            )
            .expect("log rows")
    }

    #[tokio::test]
    async fn projection_equals_the_direct_capture_of_a_turn() {
        let projected = tempfile::tempdir().expect("projected state");
        let store = open(projected.path()).await;
        seed_agent(&store).await;
        journal(&store, 1, turn_mix()).await;
        assert_eq!(log_rows(&store), 7);
        assert_eq!(dump(projected.path())["analytics_turns"], json!([]));
        assert_eq!(project_pending(&store).await.expect("project"), 7);

        let direct = tempfile::tempdir().expect("direct state");
        let direct_store = open(direct.path()).await;
        seed_agent(&direct_store).await;
        for effect in turn_mix() {
            let NotificationPostCommit::Analytics { observation, .. } = effect else {
                unreachable!("the mix holds complete analytics effects");
            };
            let prepared = crate::analytics_capture::PreparedCapture::event(
                &agent(),
                &observation.method,
                &observation.params,
                observation.observed_at,
                observation.observed_at,
                crate::analytics_capture::CaptureSource::Live,
            )
            .expect("prepare direct capture");
            crate::analytics_capture::capture(&direct_store, prepared)
                .await
                .expect("direct capture");
        }
        let projected_rows = dump(projected.path());
        assert_ne!(projected_rows["analytics_usage"], json!([]));
        assert_eq!(projected_rows, dump(direct.path()));
    }

    #[tokio::test]
    async fn a_crash_between_canvas_commit_and_projection_recovers_once() {
        let state = tempfile::tempdir().expect("state");
        let store = open(state.path()).await;
        seed_agent(&store).await;
        journal(&store, 1, turn_mix()).await;
        // The process stops after the Canvas commit, before any projection.
        drop(store);

        let store = open(state.path()).await;
        assert_eq!(project_pending(&store).await.expect("catch up"), 7);
        let recovered = dump(state.path());
        assert_ne!(recovered["analytics_turns"], json!([]));
        // A second pass and a second restart apply nothing again.
        assert_eq!(project_pending(&store).await.expect("idle pass"), 7);
        drop(store);
        let store = open(state.path()).await;
        assert_eq!(project_pending(&store).await.expect("restart pass"), 7);
        assert_eq!(dump(state.path()), recovered);
    }

    #[tokio::test]
    async fn rebuilding_from_empty_analytics_reproduces_the_totals() {
        let state = tempfile::tempdir().expect("state");
        let store = open(state.path()).await;
        seed_agent(&store).await;
        journal(&store, 1, turn_mix()).await;
        project_pending(&store).await.expect("project");
        let projected = dump(state.path());

        store
            .analytics_projection_transaction(|tx| {
                for table in PROJECTED_TABLES {
                    let keep = if table == "analytics_meta" {
                        " WHERE key<>'trackingSince'"
                    } else {
                        ""
                    };
                    tx.execute(&format!("DELETE FROM {table}{keep}"), [])?;
                }
                Ok::<_, StoreError>(())
            })
            .await
            .expect("empty analytics");
        assert_eq!(project_pending(&store).await.expect("rebuild"), 7);
        assert_eq!(dump(state.path()), projected);
    }

    #[tokio::test]
    async fn a_refused_row_is_counted_and_later_rows_still_project() {
        let state = tempfile::tempdir().expect("state");
        let store = open(state.path()).await;
        seed_agent(&store).await;
        journal(
            &store,
            1,
            vec![
                effect(1, "item/started", json!(["not an object"])),
                effect(
                    2,
                    "turn/started",
                    json!({"threadId":"thread","turn":{"id":"turn"}}),
                ),
            ],
        )
        .await;
        assert_eq!(project_pending(&store).await.expect("project"), 2);
        let rows = dump(state.path());
        assert_eq!(rows["analytics_turns"].as_array().map(Vec::len), Some(1));
        let errors = rows["analytics_meta"]
            .as_array()
            .and_then(|meta| meta.iter().find(|row| row[0] == "captureErrors"))
            .cloned()
            .expect("captureErrors recorded");
        let errors: Value =
            serde_json::from_str(errors[1].as_str().expect("JSON text")).expect("errors JSON");
        assert_eq!(errors["count"], 1);
        assert_eq!(errors["last"]["operation"], PROJECTION_OPERATION);
        assert_eq!(errors["last"]["code"], PROJECTION_REFUSED);
    }

    #[tokio::test]
    async fn pruned_rows_are_never_reapplied_and_new_rows_continue_the_sequence() {
        let state = tempfile::tempdir().expect("state");
        let store = open(state.path()).await;
        seed_agent(&store).await;
        journal(&store, 1, turn_mix()).await;
        let cursor = project_pending(&store).await.expect("project");
        assert_eq!(prune(&store, cursor).await.expect("prune"), Some(7));
        assert_eq!(log_rows(&store), 0);
        journal(
            &store,
            2,
            vec![effect(
                8,
                "turn/started",
                json!({"threadId":"thread","turn":{"id":"next"}}),
            )],
        )
        .await;
        assert_eq!(project_pending(&store).await.expect("project new row"), 8);
        assert_eq!(
            dump(state.path())["analytics_turns"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    // With synchronous=NORMAL an OS crash can roll Canvas back below the
    // analytics cursor: the last log rows and their AUTOINCREMENT high-water
    // mark are lost while analytics kept their projection. New log rows must
    // still get sequence numbers after the cursor, or the projector skips them.
    #[tokio::test]
    async fn rows_logged_after_a_canvas_rollback_below_the_cursor_still_project() {
        let state = tempfile::tempdir().expect("state");
        let store = open(state.path()).await;
        seed_agent(&store).await;
        journal(&store, 1, turn_mix()).await;
        assert_eq!(project_pending(&store).await.expect("project"), 7);
        drop(store);
        // Simulate the lost Canvas tail: no log rows and a reset sequence.
        let canvas = Connection::open(state.path().join("canvas.sqlite3")).expect("canvas DB");
        canvas
            .execute_batch(
                "DELETE FROM runtime_analytics_projection; \
                 UPDATE sqlite_sequence SET seq=0 WHERE name='runtime_analytics_projection';",
            )
            .expect("simulate Canvas rollback");
        drop(canvas);

        let store = open(state.path()).await;
        journal(
            &store,
            2,
            vec![effect(
                8,
                "turn/started",
                json!({"threadId":"thread","turn":{"id":"after-rollback"}}),
            )],
        )
        .await;
        assert_eq!(project_pending(&store).await.expect("project new row"), 8);
        assert_eq!(
            dump(state.path())["analytics_turns"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    // Analytics commits are not synced on commit, so Canvas may delete
    // projected log rows only after a complete analytics flush; while a reader
    // keeps the flush incomplete the prune waits and deletes nothing.
    #[tokio::test]
    async fn a_prune_waits_for_a_complete_analytics_flush() {
        let state = tempfile::tempdir().expect("state");
        let store = open(state.path()).await;
        seed_agent(&store).await;
        journal(&store, 1, turn_mix()).await;
        project_pending(&store).await.expect("project");
        let reader = Connection::open(state.path().join("analytics.sqlite3")).expect("reader");
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM analytics_meta;")
            .expect("hold an old analytics snapshot");
        journal(
            &store,
            2,
            vec![effect(
                8,
                "turn/started",
                json!({"threadId":"thread","turn":{"id":"next"}}),
            )],
        )
        .await;
        let cursor = project_pending(&store)
            .await
            .expect("project after the snapshot");
        assert_eq!(prune(&store, cursor).await.expect("deferred prune"), None);
        assert_eq!(log_rows(&store), 8);
        reader
            .execute_batch("COMMIT")
            .expect("release the snapshot");
        assert_eq!(prune(&store, cursor).await.expect("prune"), Some(8));
        assert_eq!(log_rows(&store), 0);
    }
}
