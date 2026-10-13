//! Both sides start from a committed Python-written database, never a Rust snapshot.
#[path = "support/prepared_python.rs"]
mod prepared_python;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_store::RuntimeStore;

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn new(scratch: &Path) -> Self {
        let python = prepared_python::prepared_python(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let mut child = Command::new(python)
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle/oracle.py"))
            .arg(scratch)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Python oracle starts");
        let input = child.stdin.take().expect("oracle stdin");
        let output = BufReader::new(child.stdout.take().expect("oracle stdout"));
        Self {
            child,
            input,
            output,
        }
    }
    fn call(&mut self, value: Value) -> Value {
        writeln!(self.input, "{value}").expect("oracle command");
        self.input.flush().expect("oracle flush");
        let mut line = String::new();
        self.output.read_line(&mut line).expect("oracle response");
        let response: Value = serde_json::from_str(&line).expect("oracle JSON response");
        assert!(response.get("error").is_none(), "{}", response["error"]);
        response["result"].clone()
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        if std::thread::panicking() {
            drop(self.child.kill());
        } else {
            writeln!(self.input, "{{\"command\":\"close\"}}").expect("close oracle");
            self.input.flush().expect("close flush");
        }
        self.child.wait().expect("reap oracle");
    }
}
struct State(PathBuf);
impl Drop for State {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove disposable states");
    }
}
// The budget owner's decision is independently captured from the real Python
// call. This corpus owns notification rows; budget's corpus owns its ledger.
struct CapturedEffects<'a>(&'a Value);
impl studio_runtime_services::notification_dispatch::NotificationEffects for CapturedEffects<'_> {
    fn capture_usage(
        &self,
        _tx: &studio_runtime_store::Transaction<'_>,
        agent: &mut Value,
        params: &Value,
        _at: f64,
    ) -> Result<(), studio_runtime_services::notification_dispatch::NotificationDispatchError> {
        let capture = self
            .0
            .as_array()
            .expect("captured budget calls")
            .iter()
            .find(|c| c["params"] == *params)
            .expect("real Python capture");
        agent["tokensUsed"] = capture["tokensUsed"].clone();
        agent["tokenUsageAccounting"] = capture["tokenUsageAccounting"].clone();
        Ok(())
    }
}
fn apply(store: &RuntimeStore, case: &Value, prepared: &Value) -> Result<Value, String> {
    use studio_runtime_providers::LineDecoder;
    use studio_runtime_services::notification_dispatch::{
        NotificationStreamBuffer, ProviderDeliveryIdentity,
        dispatch_notification_with_effects as dispatch_notification,
    };
    if case["submission"] == true {
        use studio_runtime_services::notification_dispatch::{
            PreparedSubmission, SubmittedInput, record_user_submission,
        };
        let submission = PreparedSubmission {
            first_event_id: prepared["firstEventId"].as_str().expect("first ID").into(),
            text: prepared["text"].as_str().expect("text").into(),
            submitted_at: 1234.5,
            created_at: 1234.5,
            assets: prepared["assets"].clone(),
            inputs: prepared["inputs"]
                .as_array()
                .expect("inputs")
                .iter()
                .map(|i| SubmittedInput {
                    id: i["id"].as_str().expect("ID").into(),
                    at: i["at"].as_f64().expect("time"),
                    kind: i["kind"].as_str().expect("kind").into(),
                    text: i["text"].as_str().expect("text").into(),
                    assets: i["assets"].clone(),
                })
                .collect(),
        };
        let mut rolled = None;
        let rollback: Result<(), studio_runtime_services::notification_dispatch::NotificationDispatchError> = store.transaction_with(|tx| {
            rolled = Some(record_user_submission(tx, "agent", &submission)?);
            Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification)
        });
        assert!(rollback.is_err());
        let result = store
            .transaction_with(|tx| record_user_submission(tx, "agent", &submission))
            .map_err(|e| e.to_string())?;
        assert_eq!(rolled, Some(result.clone()), "submission outer rollback");
        store.transaction_with(|tx| {
            let before = tx.total_changes();
            assert_eq!(record_user_submission(tx,"agent",&submission)?,result);
            assert_eq!(before, tx.total_changes(), "submission replay wrote");
            let mut changed = submission.clone();
            changed.text.push_str("CONFLICT-SUFFIX");
            assert!(matches!(record_user_submission(tx,"agent",&changed),Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::ConflictingInput)));
            changed = submission.clone();
            changed.inputs[0].text.push_str("CONFLICT-INPUT-SUFFIX");
            assert!(matches!(record_user_submission(tx,"agent",&changed),Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::ConflictingInput)));
            assert_eq!(before, tx.total_changes(), "conflicting submission wrote");
            Ok::<_,studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
        }).map_err(|e|e.to_string())?;
        let deleted_replay: Result<(), studio_runtime_services::notification_dispatch::NotificationDispatchError> = store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1234.5) WHERE id='agent'", []).map_err(studio_runtime_store::StoreError::from)?;
            let before = tx.total_changes();
            assert_eq!(record_user_submission(tx,"agent",&submission)?,result);
            assert_eq!(before, tx.total_changes(), "saved replay depends on mutable agent");
            Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification)
        });
        assert!(deleted_replay.is_err());
        return Ok(json!([result]));
    }
    let captured_effects = CapturedEffects(prepared);
    let runtime_effects =
        studio_runtime_services::notification_runtime_effects::RuntimeNotificationEffects;
    let effects: &dyn studio_runtime_services::notification_dispatch::NotificationEffects =
        if case["runtime_effects"] == true {
            &runtime_effects
        } else {
            &captured_effects
        };
    let mut handoff = Vec::new();
    let mut analytics_overflow = 0u64;
    let mut results = Vec::new();
    let mut streams = NotificationStreamBuffer::default();
    for (index, step) in case["steps"].as_array().expect("steps").iter().enumerate() {
        let agent_id = step["agent"].as_str().unwrap_or("agent");
        if step["recover_task"] == true {
            use studio_runtime_services::notification_dispatch::{
                NotificationDispatchError, recover_task_in_transaction,
            };
            use studio_runtime_store::NotificationDispatchRepository;
            let recover = |tx:&studio_runtime_store::Transaction<'_>| -> Result<(),NotificationDispatchError> {
                let agent=NotificationDispatchRepository::agent_record(tx,"agent")?.expect("agent");
                let raw:String=tx.query_row("SELECT record FROM runtime_tasks WHERE id='agent:tool'",[],|r|r.get(0)).map_err(studio_runtime_store::StoreError::from)?;
                let previous:Value=serde_json::from_str(&raw)?;
                let before=raw_rows(tx);
                let mut stale=previous.clone(); stale["name"]=json!("stale snapshot");
                assert!(recover_task_in_transaction(tx,&agent,&stale,&step["frame"]["params"]["item"],1234.5).is_err());
                assert_eq!(before,raw_rows(tx),"stale recovery snapshot wrote rows");
                recover_task_in_transaction(tx,&agent,&previous,&step["frame"]["params"]["item"],1234.5)
            };
            let before = store
                .transaction(|tx| Ok(raw_rows(tx)))
                .map_err(|e| e.to_string())?;
            let rollback: Result<(), NotificationDispatchError> = store.transaction_with(|tx| {
                recover(tx)?;
                Err(NotificationDispatchError::InvalidNotification)
            });
            assert!(rollback.is_err());
            assert_eq!(
                before,
                store
                    .transaction(|tx| Ok(raw_rows(tx)))
                    .map_err(|e| e.to_string())?,
                "recovery rollback leaked task/transcript"
            );
            write_failpoints(store, |tx| recover(tx))?;
            store.transaction_with(recover).map_err(|e| e.to_string())?;
            results.push(json!({"status":"written"}));
            continue;
        }

        let mut bytes = serde_json::to_vec(&step["frame"]).expect("frame JSON");
        bytes.push(b'\n');
        let frame = LineDecoder::new().push(&bytes).expect("frame").remove(0);
        let identity = ProviderDeliveryIdentity {
            provider_handle: step["provider_handle"]
                .as_str()
                .unwrap_or("account:default")
                .into(),
            generation: step["generation"].as_u64().unwrap_or(7),
            cursor: step["cursor"].as_u64().unwrap_or(index as u64 + 1),
        };
        if let Some(service) = step["required_service"].as_str() {
            assert_eq!(case["requires_service"], service);
            assert_eq!(index + 1, case["steps"].as_array().expect("steps").len());
            return store.transaction_with(|tx| {
                let before = raw_rows(tx);
                let error = dispatch_notification(
                    tx, agent_id, store.root().to_str().expect("path"), &streams, &identity, &frame, 1234.5, effects,
                ).expect_err("missing service must refuse before acknowledgement");
                assert!(matches!(error, studio_runtime_services::notification_dispatch::NotificationDispatchError::Dependency(name) if name == service));
                assert_eq!(raw_rows(tx), before, "caught service refusal leaked rows or receipt");
                Ok::<_,studio_runtime_store::StoreError>(error.to_string())
            }).map_err(|e|e.to_string()).and_then(Err);
        }
        if case["late_failure"] == true {
            let original_streams = streams.clone();
            let refusal = store.transaction_with(|tx| {
                tx.execute_batch("CREATE TABLE corpus_caller(value TEXT); CREATE TEMP TRIGGER corpus_late_failure BEFORE INSERT ON runtime_operation_receipts BEGIN SELECT RAISE(ABORT, 'injected late failure'); END;").expect("late service failure trigger");
                let before = raw_rows(tx);
                let refusal = dispatch_notification(tx, agent_id, store.root().to_str().expect("path"), &streams, &identity, &frame, 1234.5, effects).expect_err("late failure must reach the service caller");
                assert_eq!(before, raw_rows(tx), "service leaked rows after caller caught its error");
                assert_eq!(streams, original_streams, "service leaked stream state");
                tx.execute_batch("DROP TRIGGER corpus_late_failure; INSERT INTO corpus_caller VALUES ('caller committed');")?;
                Ok::<_, studio_runtime_store::StoreError>(refusal.to_string())
            }).map_err(|e|e.to_string())?;
            store
                .transaction(|tx| {
                    assert_eq!(
                        tx.query_row("SELECT value FROM corpus_caller", [], |r| r
                            .get::<_, String>(0))?,
                        "caller committed"
                    );
                    Ok(())
                })
                .expect("unrelated caller work committed");
            return Err(refusal);
        }
        if case["effect_matrix"] == true && std::env::var_os("NOTIFICATION_REPLAY_CHILD").is_none()
        {
            effect_failpoints(store, &identity, &frame)?;
        }
        if full_failpoints(case) {
            write_failpoints(store, |tx| {
                dispatch_notification(
                    tx,
                    agent_id,
                    store.root().to_str().expect("path"),
                    &streams,
                    &identity,
                    &frame,
                    1234.5,
                    effects,
                )
                .map(|_| ())
            })?;
        }
        let mut rolled = None;
        let rollback: Result<(), studio_runtime_services::notification_dispatch::NotificationDispatchError> = store.transaction_with(|tx| {
            rolled = Some(dispatch_notification(tx, agent_id, store.root().to_str().expect("state path"), &streams, &identity, &frame, 1234.5, effects)?);
            Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::InvalidNotification)
        });
        assert!(rollback.is_err());
        if step["rollback"] == true {
            continue;
        }
        let result = store
            .transaction_with(|tx| {
                dispatch_notification(
                    tx,
                    agent_id,
                    store.root().to_str().expect("state path"),
                    &streams,
                    &identity,
                    &frame,
                    1234.5,
                    effects,
                )
            })
            .map_err(|e| e.to_string())?;
        assert_eq!(
            rolled.as_ref(),
            Some(&result),
            "rollback must leave buffer unchanged"
        );
        let replay = store
            .transaction_with(|tx| {
                let before = tx.total_changes();
                let replay = dispatch_notification(
                    tx,
                    agent_id,
                    store.root().to_str().expect("path"),
                    &streams,
                    &identity,
                    &frame,
                    9876.0,
                    effects,
                )?;
                assert_eq!(before, tx.total_changes(), "replay wrote SQLite");
                assert!(replay.stream_update.is_none(), "replay changed buffer");
                assert!(
                    replay.post_commit.is_empty(),
                    "replay repeated external effect"
                );
                Ok::<_, studio_runtime_services::notification_dispatch::NotificationDispatchError>(
                    replay,
                )
            })
            .map_err(|e| e.to_string())?;
        assert_eq!(
            replay.outcome, result.outcome,
            "replay must return saved result"
        );
        let mut conflicting = step["frame"].clone();
        conflicting["conflict"] = json!("different full payload");
        let mut bytes = serde_json::to_vec(&conflicting).expect("JSON");
        bytes.push(b'\n');
        let conflict = LineDecoder::new()
            .push(&bytes)
            .expect("conflict frame")
            .remove(0);
        store.transaction_with(|tx| {
            let before = tx.total_changes();
            assert!(matches!(dispatch_notification(tx,agent_id,store.root().to_str().expect("path"),&streams,&identity,&conflict,1234.5,effects), Err(studio_runtime_services::notification_dispatch::NotificationDispatchError::ConflictingDelivery)));
            assert_eq!(before,tx.total_changes());
            Ok::<_,studio_runtime_store::StoreError>(())
        }).map_err(|e|e.to_string())?;
        analytics_overflow += result.post_commit.analytics_overflow;
        for effect in &result.post_commit.effects {
            use studio_runtime_services::notification_runtime_effects::NotificationPostCommitSink;
            let sink = CommittedSink { store };
            sink.publish(effect).map_err(|e| e.to_string())?;
            handoff.push(post_commit_value(effect));
        }
        results.push(json!(result.outcome));
        if let Some(update) = result.stream_update {
            // The worker persists this buffer with the receipt. Exercise the
            // restart representation, including pending analytic identities.
            streams =
                serde_json::from_slice(&serde_json::to_vec(&update).expect("durable stream JSON"))
                    .expect("restore stream buffer");
            assert_eq!(streams, update);
        }
        if let Some(bytes) = step["expected_pending_bytes"].as_u64() {
            let buffered = serde_json::to_value(&streams).expect("stream state");
            let actual: usize = buffered["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .map(|entry| {
                    assert!(
                        entry["params"].get("delta").is_none(),
                        "do not retain delta twice"
                    );
                    entry["pending"].as_str().expect("pending text").len()
                })
                .sum();
            assert_eq!(
                actual as u64, bytes,
                "bounded pending bytes before timer flush"
            );
        }
        if step["tick"] == true {
            flush_agents(store, case, &mut streams)?;
            let batch = streams.take_post_commit().map_err(|e| e.to_string())?;
            analytics_overflow += batch.analytics_overflow;
            for effect in batch.effects {
                use studio_runtime_services::notification_runtime_effects::NotificationPostCommitSink;
                CommittedSink { store }
                    .publish(&effect)
                    .map_err(|error| error.to_string())?;
                handoff.push(post_commit_value(&effect));
            }
        }
    }
    if case["advance_before_flush"] == true {
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.turnId','replay-turn-2') WHERE id='agent'",[])?;
            // Keep the canonical projection in step with Python.put.
            let raw:String=tx.query_row("SELECT record FROM runtime_agents WHERE id='agent'",[],|r|r.get(0))?;
            let agent:Value=serde_json::from_str(&raw)?;
            studio_runtime_store::SyncDocumentsRepository::patch_entity(tx,"agent","agent",json!({"turnId":agent["turnId"]}))?;
            Ok(())
        }).map_err(|e|e.to_string())?;
    }
    if full_failpoints(case) {
        write_failpoints(store, |tx| {
            studio_runtime_services::notification_dispatch::flush_notification_streams(
                tx,
                "agent",
                store.root().to_str().expect("path"),
                &streams,
                1234.5,
            )
            .map(|_| ())
        })?;
    }
    flush_agents(store, case, &mut streams)?;
    let batch = streams.take_post_commit().map_err(|e| e.to_string())?;
    analytics_overflow += batch.analytics_overflow;
    for effect in batch.effects {
        use studio_runtime_services::notification_runtime_effects::NotificationPostCommitSink;
        CommittedSink { store }
            .publish(&effect)
            .map_err(|error| error.to_string())?;
        handoff.push(post_commit_value(&effect));
    }
    if case["runtime_effects"] == true && std::env::var_os("NOTIFICATION_REPLAY_CHILD").is_none() {
        let expected = prepared["post_commit"]
            .as_array()
            .expect("Python post-commit captures");
        let selected: Vec<_> = handoff
            .iter()
            .filter(|effect| case["analytics_semantics"] != true || effect["kind"] != "analytics")
            .cloned()
            .collect();
        let mut diff = vec![];
        differences(
            Some(&json!(expected)),
            Some(&json!(selected)),
            "/handoff",
            &mut diff,
        );
        if !diff.is_empty() {
            return Err(format!(
                "{}: handoff differences: {}",
                case["id"],
                json!(diff)
            ));
        }
    }
    if case["runtime_effects"] == true && std::env::var_os("NOTIFICATION_REPLAY_CHILD").is_none() {
        assert_eq!(
            json!(analytics_overflow),
            prepared["analytics_overflow"],
            "analytics overflow parity"
        );
    }
    if case["analytics_semantics"] == true {
        Ok(json!({"outcomes":results,"handoff":handoff,"analytics_overflow":analytics_overflow}))
    } else {
        Ok(json!(results))
    }
}

struct CommittedSink<'a> {
    store: &'a RuntimeStore,
}
impl studio_runtime_services::notification_runtime_effects::NotificationPostCommitSink
    for CommittedSink<'_>
{
    fn publish(
        &self,
        effect: &studio_runtime_services::notification_dispatch::NotificationPostCommit,
    ) -> Result<(), studio_runtime_services::notification_dispatch::NotificationDispatchError> {
        use studio_runtime_services::notification_dispatch::NotificationPostCommit;
        let observation = match effect {
            NotificationPostCommit::Analytics { observation, .. }
            | NotificationPostCommit::AnalyticsStream { observation, .. }
            | NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation) => observation,
        };
        let identity = &observation.delivery;
        let signature = serde_json::to_vec(&json!({"providerHandle":identity.provider_handle,"generation":identity.generation,"cursor":identity.cursor})).expect("identity");
        let receipt = format!(
            "provider-notification:{}",
            studio_runtime_store::sha256_hex(&signature)
        );
        let read = self.store.read()?;
        assert!(read.is_autocommit());
        assert_eq!(
            read.query_row(
                "SELECT count(*) FROM runtime_operation_receipts WHERE id=?1",
                [receipt],
                |r| r.get::<_, i64>(0)
            )
            .expect("receipt"),
            1,
            "publish preceded receipt commit"
        );
        if let NotificationPostCommit::Analytics {
            budget_capture_value: Some(tokens),
            ..
        } = effect
        {
            let agent = observation.agent.as_ref().expect("budget owner")["id"]
                .as_str()
                .expect("agent");
            assert_eq!(
                read.query_row(
                    "SELECT json_extract(record,'$.tokensUsed') FROM runtime_agents WHERE id=?1",
                    [agent],
                    |r| r.get::<_, i64>(0)
                )
                .expect("committed budget"),
                i64::try_from(*tokens).expect("bounded token count")
            );
        }
        Ok(())
    }
}

fn post_commit_value(
    effect: &studio_runtime_services::notification_dispatch::NotificationPostCommit,
) -> Value {
    use studio_runtime_services::notification_dispatch::NotificationPostCommit;
    let (kind, observation, captured) = match effect {
        NotificationPostCommit::Analytics {
            observation,
            budget_capture_value,
        } => ("analytics", observation, *budget_capture_value),
        NotificationPostCommit::AnalyticsStream { observation, .. } => {
            ("analytics", observation, None)
        }
        NotificationPostCommit::TokenRateStream(observation) => ("stream", observation, None),
        NotificationPostCommit::TokenRateAssociation(observation) => {
            ("association", observation, None)
        }
    };
    let mut value = json!({"kind":kind, "agent":observation.agent, "method":observation.method, "params":observation.params,
        "account":observation.delivery.provider_handle.strip_prefix("account:"),
        "connection":format!("connection-{}", observation.delivery.generation),
        "at":if kind=="analytics" { observation.observed_at } else { observation.provider_at },
        "budget_capture_value":captured});
    if let NotificationPostCommit::AnalyticsStream { metrics, .. } = effect {
        value["stream_metrics"] = serde_json::to_value(metrics).expect("metrics");
    }
    value
}

/// Fail immediately before and after each transactional effect, then let the
/// ordinary corpus call commit and replay the same provider delivery.
fn effect_failpoints(
    store: &RuntimeStore,
    identity: &studio_runtime_services::notification_dispatch::ProviderDeliveryIdentity,
    frame: &studio_runtime_providers::Frame,
) -> Result<(), String> {
    use studio_runtime_services::notification_dispatch::{
        NotificationDispatchError as Error, NotificationEffects, NotificationStreamBuffer,
        dispatch_notification_with_effects,
    };
    use studio_runtime_services::notification_runtime_effects::RuntimeNotificationEffects;
    use studio_runtime_store::{StoreError, Transaction};
    struct Failure(u8);
    impl NotificationEffects for Failure {
        fn capture_usage(
            &self,
            tx: &Transaction<'_>,
            agent: &mut Value,
            params: &Value,
            at: f64,
        ) -> Result<(), Error> {
            if self.0 == 0 {
                return Err(Error::InvalidNotification);
            }
            RuntimeNotificationEffects.capture_usage(tx, agent, params, at)?;
            if self.0 == 1 {
                return Err(Error::InvalidNotification);
            }
            Ok(())
        }
        fn capture_move_cache(
            &self,
            tx: &Transaction<'_>,
            agent: &Value,
            params: &Value,
            at: f64,
        ) -> Result<(), Error> {
            if self.0 == 2 {
                return Err(Error::InvalidNotification);
            }
            RuntimeNotificationEffects.capture_move_cache(tx, agent, params, at)?;
            Err(Error::InvalidNotification)
        }
    }
    for phase in 0..=3 {
        let result: Result<(), Error> = store.transaction_with(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.movedFrom',json('{\"server\":\"remote\"}')) WHERE id='agent'", []).map_err(StoreError::from)?;
            let before = raw_rows(tx);
            let failure = Failure(phase);
            let effects: &dyn NotificationEffects = &failure;
            let result = dispatch_notification_with_effects(tx, "agent", store.root().to_str().expect("path"), &NotificationStreamBuffer::default(), identity, frame, 1234.5, effects);
            assert!(result.is_err(), "effect boundary {phase} must fail");
            assert_eq!(before, raw_rows(tx), "effect boundary {phase} leaked rows/receipt");
            Err(Error::InvalidNotification)
        });
        assert!(result.is_err());
    }
    Ok(())
}

static WRITE_FAILURE_POINTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn full_failpoints(case: &Value) -> bool {
    case["fault_matrix"] == true
        && std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some()
        && std::env::var_os("NOTIFICATION_REPLAY_CHILD").is_none()
}

fn write_failpoints(
    store: &RuntimeStore,
    operation: impl Fn(
        &studio_runtime_store::Transaction<'_>,
    ) -> Result<
        (),
        studio_runtime_services::notification_dispatch::NotificationDispatchError,
    >,
) -> Result<(), String> {
    use studio_runtime_services::notification_dispatch::NotificationDispatchError;
    for phase in ["BEFORE", "AFTER"] {
        let mut writes = 0;
        // The successful probe counts every ordinary-table write, including
        // repeated writes and writes performed by existing projection triggers.
        // Virtual-table shadow storage is verified by the complete raw snapshot.
        for ordinal in 0.. {
            let mut baseline_error = None;
            let rolled_back: Result<(), NotificationDispatchError> = store.transaction_with(|tx| {
                tx.execute_batch("CREATE TEMP TABLE notification_write_trace(value INTEGER); CREATE TEMP TABLE notification_failure(ordinal INTEGER); INSERT INTO notification_failure VALUES(0);").expect("trace fixture");
                tx.execute("UPDATE notification_failure SET ordinal=?1", [ordinal]).expect("failure ordinal");
                let tables = tx.prepare("SELECT name FROM pragma_table_list WHERE schema='main' AND type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").expect("ordinary tables").query_map([], |r|r.get::<_,String>(0)).expect("table names").collect::<Result<Vec<_>,_>>().expect("table names");
                for (index, table) in tables.iter().enumerate() {
                    for action in ["INSERT", "UPDATE", "DELETE"] {
                        tx.execute_batch(&format!("CREATE TEMP TRIGGER notification_trace_{index}_{action} {phase} {action} ON main.\"{}\" BEGIN INSERT INTO notification_write_trace VALUES(1); SELECT CASE WHEN (SELECT count(*) FROM notification_write_trace)=(SELECT ordinal FROM notification_failure) THEN RAISE(ABORT,'notification failpoint') END; END;", table.replace('"', "\"\""))).expect("write trace trigger");
                    }
                }
                let before = raw_rows(tx);
                let result = operation(tx);
                if ordinal == 0 {
                    baseline_error = result.err().map(|error|error.to_string());
                    writes = tx.query_row("SELECT count(*) FROM notification_write_trace", [], |r|r.get::<_,i64>(0)).expect("write count");
                } else {
                    assert!(result.is_err(), "{phase} write {ordinal}/{writes} was not exercised");
                    assert_eq!(before, raw_rows(tx), "{phase} write {ordinal}/{writes}: caught failure leaked rows");
                    WRITE_FAILURE_POINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(NotificationDispatchError::InvalidNotification)
            });
            assert!(rolled_back.is_err());
            if let Some(error) = baseline_error {
                return Err(error);
            }
            if ordinal == writes {
                break;
            }
        }
    }
    Ok(())
}

fn raw_rows(
    tx: &studio_runtime_store::Transaction<'_>,
) -> std::collections::BTreeMap<String, Vec<Vec<String>>> {
    let tables = tx.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name != 'corpus_caller' ORDER BY name").expect("tables").query_map([], |r| r.get::<_,String>(0)).expect("names").collect::<Result<Vec<_>,_>>().expect("names");
    tables
        .into_iter()
        .map(|table| {
            let mut statement = tx
                .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
                .expect("raw table");
            let columns = statement.column_count();
            let rows = statement
                .query_map([], |r| {
                    (0..columns)
                        .map(|i| r.get_ref(i).map(|v| format!("{v:?}")))
                        .collect::<Result<Vec<_>, _>>()
                })
                .expect("raw rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("raw content");
            (table, rows)
        })
        .collect()
}
fn flush_agents(
    store: &RuntimeStore,
    case: &Value,
    streams: &mut studio_runtime_services::notification_dispatch::NotificationStreamBuffer,
) -> Result<(), String> {
    if case["account_only"] == true {
        return Ok(());
    }
    if let Some(agents) = case["flush_agents"].as_array() {
        for agent in agents {
            flush(store, agent.as_str().expect("agent"), streams)?;
        }
    } else {
        flush(store, "agent", streams)?;
    }
    Ok(())
}

fn flush(
    store: &RuntimeStore,
    agent_id: &str,
    streams: &mut studio_runtime_services::notification_dispatch::NotificationStreamBuffer,
) -> Result<(), String> {
    use studio_runtime_services::notification_dispatch::{
        NotificationDispatchError, flush_notification_streams,
    };
    let mut rolled = None;
    let rollback: Result<(), NotificationDispatchError> = store.transaction_with(|tx| {
        rolled = Some(flush_notification_streams(
            tx,
            agent_id,
            store.root().to_str().expect("path"),
            streams,
            1234.5,
        )?);
        Err(NotificationDispatchError::InvalidNotification)
    });
    assert!(rollback.is_err());
    let result = store
        .transaction_with(|tx| {
            flush_notification_streams(
                tx,
                agent_id,
                store.root().to_str().expect("path"),
                streams,
                1234.5,
            )
        })
        .map_err(|e| e.to_string())?;
    assert_eq!(
        rolled,
        Some(result.clone()),
        "timer rollback must preserve pending batches"
    );
    if let Some(update) = result {
        *streams = update;
    }
    Ok(())
}

fn differences(python: Option<&Value>, rust: Option<&Value>, path: &str, out: &mut Vec<Value>) {
    match (python,rust) {
        (Some(Value::Object(a)),Some(Value::Object(b))) => {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys { differences(a.get(key),b.get(key),&format!("{path}/{key}"),out); }
        },
        (Some(Value::Array(a)),Some(Value::Array(b))) => {
            for i in 0..a.len().max(b.len()) { differences(a.get(i),b.get(i),&format!("{path}/{i}"),out); }
        },
        _ if python==rust=>{},
        // JSON has one numeric domain. Treat 0 and 0.0 equally, without
        // rounding integers outside the exactly representable f64 range.
        (Some(Value::Number(a)), Some(Value::Number(b))) if (a.is_f64() || b.is_f64()) && a.as_f64().zip(b.as_f64()).is_some_and(|(a,b)| a == b && a.abs() <= 9_007_199_254_740_992.0) => {},
        _=>out.push(json!({"path":path,"python":python,"rust":rust,"pythonPresent":python.is_some(),"rustPresent":rust.is_some()})),
    }
}
fn expected_with_deviations(mut expected: Value, case: &Value) -> Value {
    let registry: Value = serde_json::from_str(include_str!("turn_lifecycle/deviations.json"))
        .expect("central deviation registry");
    if let Some(class) = case["deviation"].as_str() {
        assert!(
            registry.get(class).is_some(),
            "deviation must be centrally declared: {class}"
        );
    }
    if let Some(patches) = case["expected_differences"].as_array() {
        assert!(
            case["deviation"].is_string(),
            "differences require a named reason"
        );
        for patch in patches {
            let path = patch["path"].as_str().expect("difference path");
            let class = patch["deviation"]
                .as_str()
                .expect("field patch must name its deviation class");
            assert_eq!(patch["deviation"], case["deviation"]);
            let declaration = registry.get(class).expect("declared deviation");
            let field = path
                .split('/')
                .map(|part| {
                    if part.parse::<usize>().is_ok() {
                        "*"
                    } else {
                        part
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            assert!(
                declaration["fields"]
                    .as_array()
                    .expect("declared fields")
                    .contains(&json!(field)),
                "undeclared field: {class} {field}"
            );
            assert!(
                patch["pythonPresent"] != patch["rustPresent"] || patch["python"] != patch["rust"],
                "declared difference does not occur: {path}"
            );
            let actual = expected.pointer(path);
            if patch["pythonPresent"] == true {
                assert_eq!(actual, Some(&patch["python"]), "Python changed at {path}");
            } else {
                assert!(actual.is_none(), "Python unexpectedly has {path}");
            }
            let (parent, key) = path.rsplit_once('/').expect("parent");
            let container = expected.pointer_mut(parent).expect("parent exists");
            if let Some(object) = container.as_object_mut() {
                if patch["rustPresent"] == true {
                    object.insert(
                        key.replace("~1", "/").replace("~0", "~"),
                        patch["rust"].clone(),
                    );
                } else {
                    object.remove(key);
                }
            } else {
                let array = container.as_array_mut().expect("array");
                let index = key.parse::<usize>().expect("index");
                if patch["rustPresent"] == true {
                    array[index] = patch["rust"].clone();
                } else {
                    array.remove(index);
                }
            }
        }
        for row in expected["sync_entities"].as_array_mut().expect("sync rows") {
            row["hash"] = json!(studio_runtime_store::sha256_hex(
                studio_runtime_services::event_write::python_json(&row["payload"]).as_bytes()
            ));
        }
    }
    expected
}
#[test]
fn python_written_notification_corpus() {
    if let Some(path) = std::env::var_os("NOTIFICATION_REPLAY_CHILD") {
        let fixture: Value =
            serde_json::from_slice(&fs::read(path).expect("replay fixture")).expect("replay JSON");
        let store = RuntimeStore::open(Some(Path::new(fixture["root"].as_str().expect("root"))))
            .expect("reopen committed database");
        apply(&store, &fixture["case"], &fixture["prepared"])
            .expect("replay after lost acknowledgement");
        return;
    }
    corpus(
        std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some(),
        false,
    );
}

#[test]
fn wave7_p06_runtime_notification_corpus() {
    corpus(
        std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some(),
        true,
    );
}

fn corpus(full: bool, runtime_effects: bool) {
    let corpus_started = std::time::Instant::now();
    let mut cases: Vec<Value> = serde_json::from_str(if runtime_effects {
        include_str!("wave7_p06_notification_cases.json")
    } else {
        include_str!("turn_lifecycle/notification_cases.json")
    })
    .expect("corpus data");
    for case in &mut cases {
        if let Some(count) = case["empty_deltas"].as_u64() {
            case["steps"] = json!(vec![case["steps"][0].clone(); count as usize]);
            case["return"] = json!(vec![json!({"status":"written"}); count as usize]);
        }
        if let Some(count) = case["many_streams"].as_u64() {
            case["steps"] = json!((0..count).map(|i|json!({"frame":{"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"replay-turn-1","itemId":format!("stream-{i}"),"delta":"x"}}})).collect::<Vec<_>>());
            case["return"] = json!(vec![json!({"status":"written"}); count as usize]);
        }
    }
    if !runtime_effects {
        let coverage: Value =
            serde_json::from_str(include_str!("turn_lifecycle/notification_coverage.json"))
                .expect("coverage manifest");
        assert_eq!(
            coverage["counts"]["total"].as_u64(),
            Some(cases.len() as u64)
        );
        for finding in coverage["review_findings"].as_array().expect("findings") {
            for id in finding["cases"].as_array().expect("witnesses") {
                assert!(
                    cases
                        .iter()
                        .any(|case| case["id"] == *id && case["tier"] == "fast"),
                    "review witness missing from fast tier: {id}"
                );
            }
        }
        let kinds = |fast: bool| -> std::collections::BTreeSet<String> {
            cases
                .iter()
                .filter(|case| !fast || case["tier"] == "fast")
                .flat_map(|case| case["steps"].as_array().into_iter().flatten())
                .map(|step| step["frame"]["method"].as_str().expect("method").to_owned())
                .collect()
        };
        assert_eq!(
            kinds(true),
            kinds(false),
            "default must exercise every method"
        );
    }
    let scratch = std::env::var_os("NOTIFICATION_CORPUS_STATE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let root = State(scratch.join(format!(
        "notification-corpus-{}-{}",
        std::process::id(),
        runtime_effects
    )));
    fs::create_dir(&root.0).expect("unique corpus state");
    let mut oracle = Oracle::new(&root.0);
    let mut report = vec![];
    let mut green = 0;
    let mut refusals = 0;
    for case in cases
        .iter()
        .filter(|case| case["unit_test"].is_null() && (full || case["tier"] == "fast"))
    {
        if let Ok(filter) = std::env::var("NOTIFICATION_CASE")
            && !case["id"].as_str().expect("ID").contains(&filter)
        {
            continue;
        }
        let id = case["id"].as_str().expect("case ID");
        let rust = root.0.join(id).join("rust");
        let started = std::time::Instant::now();
        let expected = oracle.call(
            json!({"command":"seed","root":root.0.join(id).join("python"),"rust":rust,"case":case}),
        );
        let seeded = started.elapsed().as_secs_f64();
        let store = match RuntimeStore::open(Some(&rust)) {
            Ok(store) => store,
            Err(error) => {
                let location = case["expect_refusal"]
                    .as_str()
                    .and_then(|value| {
                        value.strip_prefix("invalid stored object ").or_else(|| {
                            value.strip_prefix(
                                "database schema or migration marker is incompatible: Invalid stored object ",
                            )
                        })
                    })
                    .unwrap_or_else(|| panic!("{id}: open Python-written state: {error}"))
                    .replacen(':', "/", 1)
                    .replace(
                        "$project",
                        expected["pythonRoot"].as_str().expect("Python root"),
                    );
                let message = error.to_string();
                assert!(
                    message.contains(&location),
                    "{id}: startup refusal must locate {location}: {message}"
                );
                assert!(
                    !message.contains("SECRET_STORED_SENTINEL")
                        && !message.contains("CORPUS_PRIVATE_PAYLOAD")
                );
                let applied = started.elapsed().as_secs_f64();
                let actual = oracle.call(
                    json!({"command":"snapshot","module":"notification_dispatch","root":rust}),
                );
                assert_eq!(
                    actual, expected["before"],
                    "{id}: startup refusal must not write"
                );
                report.push(json!({"id":id,"differences":[],"error":null,"firstResult":null,"seconds":{"seed":seeded,"apply":applied-seeded,"snapshot":started.elapsed().as_secs_f64()-applied}}));
                green += 1;
                continue;
            }
        };
        let prepared = if runtime_effects {
            &expected
        } else if case["submission"] == true {
            &expected["before"]["prepared"]
        } else {
            &expected["captures"]
        };
        let first = apply(&store, case, prepared);
        let error = first.as_ref().err().cloned();
        if let Ok(value) = &first {
            assert_eq!(
                value.get("outcomes").unwrap_or(value),
                &case["return"],
                "{id}: entry-point result"
            );
        }
        drop(store);
        if case["crash_replay"] == true {
            let fixture = rust.parent().expect("parent").join("replay.json");
            fs::write(
                &fixture,
                serde_json::to_vec(&json!({"root":rust,"case":case,"prepared":prepared}))
                    .expect("fixture"),
            )
            .expect("fixture file");
            let child = Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "python_written_notification_corpus",
                    "--nocapture",
                ])
                .env("NOTIFICATION_REPLAY_CHILD", fixture)
                .output()
                .expect("second process");
            assert!(
                child.status.success(),
                "replay child: {}",
                String::from_utf8_lossy(&child.stderr)
            );
        }
        let applied = started.elapsed().as_secs_f64();
        let actual =
            oracle.call(json!({"command":"snapshot","module":"notification_dispatch","root":rust}));
        if let Some(service) = case["requires_service"].as_str() {
            assert_eq!(
                error.as_deref(),
                Some(format!("Not implemented: requires {service}").as_str())
            );
            assert_eq!(
                actual, expected["beforeRefusal"],
                "named refusal changed stored state"
            );
            if let Some(table) = case["python_writes"].as_str() {
                assert_ne!(
                    expected["after"][table], expected["beforeRefusal"][table],
                    "Python service witness must store a real effect"
                );
            }
            refusals += 1;
            report.push(json!({"id":id,"contract":"named-service-refusal","dependency":service,
                "pythonExpectedStoredState":expected["after"],"rustStoredState":actual,"error":error,
                "seconds":{"seed":seeded,"apply":applied-seeded,"snapshot":started.elapsed().as_secs_f64()-applied}}));
            continue;
        }
        let mut diff = vec![];
        if case["analytics_semantics"] == true
            && let Ok(result) = &first
        {
            let analytics = oracle.call(json!({"command":"post_commit","module":"notification_dispatch","root":rust,"effects":result["handoff"]}));
            differences(
                Some(&expected["analytics"]),
                Some(&analytics),
                "/analytics",
                &mut diff,
            );
        }
        let expected_after = expected_with_deviations(expected["after"].clone(), case);
        differences(Some(&expected_after), Some(&actual), "", &mut diff);
        let expected_error = case["expect_refusal"].as_str().map(|e| {
            e.replace(
                "$project",
                expected["pythonRoot"].as_str().expect("Python root"),
            )
        });
        let error_matches = match expected_error {
            Some(message) => error.as_deref().is_some_and(|error| {
                error == message.as_str()
                    && !error.contains("SECRET_STORED_SENTINEL")
                    && !error.contains("CORPUS_PRIVATE_PAYLOAD")
            }),
            None => error.is_none(),
        };
        if diff.is_empty() && error_matches {
            green += 1;
        } else {
            eprintln!("{id}: {} differences; error={error:?}", diff.len());
        }
        report.push(json!({"id":id,"differences":diff,"error":error,"firstResult":first.ok(),"seconds":{"seed":seeded,"apply":applied-seeded,"snapshot":started.elapsed().as_secs_f64()-applied}}));
    }
    assert!(!report.is_empty(), "case filter matched nothing");
    let report_path = std::env::var_os("NOTIFICATION_CORPUS_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("notification-corpus-report.json"));
    let report_path = if runtime_effects {
        report_path.with_extension("runtime-effects.json")
    } else {
        report_path
    };
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).expect("report JSON"),
    )
    .expect("report written");
    let oracle_cpu = oracle.call(json!({"command":"metrics","module":"notification_dispatch"}));
    // Linux exposes this process's CPU separately from its oracle/replay children.
    // Other platforms still report oracle CPU and per-phase wall durations.
    let rust_cpu = fs::read_to_string("/proc/self/stat").ok().and_then(|stat| {
        let (_, fields) = stat.rsplit_once(')')?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        let hz = oracle_cpu["clockTicks"].as_f64()?;
        Some(
            json!({"userSeconds":fields.get(11)?.parse::<f64>().ok()?/hz,
                    "systemSeconds":fields.get(12)?.parse::<f64>().ok()?/hz}),
        )
    });
    let phase = |key: &str| {
        report
            .iter()
            .map(|row| row["seconds"][key].as_f64().expect("phase duration"))
            .sum::<f64>()
    };
    let timing = json!({"cases":report.len(),"writeFailurePoints":WRITE_FAILURE_POINTS.load(std::sync::atomic::Ordering::Relaxed),"wallSeconds":corpus_started.elapsed().as_secs_f64(),"rustCpu":rust_cpu,"oracleCpu":oracle_cpu,"phases":{"seed":phase("seed"),"apply":phase("apply"),"snapshot":phase("snapshot")}});
    fs::write(
        report_path.with_extension("timing.json"),
        serde_json::to_vec_pretty(&timing).expect("timing JSON"),
    )
    .expect("timing report");
    eprintln!("corpus timing: {timing}");
    assert_eq!(
        green + refusals,
        report.len(),
        "{green} exact / {refusals} named refusals / {} red; field differences: {}",
        report.len() - green - refusals,
        report_path.display()
    );
}

#[path = "wave7_p06/publication.rs"]
mod publication;
