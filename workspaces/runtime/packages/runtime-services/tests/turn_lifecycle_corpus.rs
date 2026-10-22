//! Lifecycle transitions over captured legacy database rows and expectations.
//! Submission uses the worker pipeline and production store::submitted, comparing
//! the complete native method/params plus the extra Rust transport receipt.
//! The fixture starts with a loaded thread: resume transport is covered by worker
//! matrix scenario 0 (normal_turn_commits_transcript_before_ack_and_pending_event_waits_for_settlement).
//! Fresh thread transport and authenticated manifest promotion are covered by
//! scenario 16 (new_thread_freezes_settings_and_accepts_context); independent artifact
//! creation is covered by scenario 17 (child_result_materializes_before_submission).
//! SessionControl's actual wire/ACK and restart behavior remain worker tests.
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use studio_runtime_services::scheduler::{
    claim_intent, lifecycle_transaction, recover_on_startup, settle_terminal_attempt,
};
use studio_runtime_store::{RuntimeStore, SchedulerRepository, StoreError, TurnAttemptIdentity};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleFixture {
    schemas: std::collections::BTreeMap<String, Vec<String>>,
    cases: Vec<LifecycleCase>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleCase {
    id: String,
    expected: LifecycleExpectation,
    databases: std::collections::BTreeMap<String, LifecycleDatabase>,
    files: std::collections::BTreeMap<String, String>,
}
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LifecycleExpectation {
    before: Value,
    after: Value,
    python_root: String,
    python_error: Option<String>,
    entry_result: Value,
    native_request: Value,
    tokens: std::collections::BTreeMap<String, String>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleDatabase {
    schema_key: String,
    data: Vec<String>,
}
struct CapturedExpectations {
    fixture: LifecycleFixture,
    tokens: std::collections::BTreeMap<String, String>,
}
impl CapturedExpectations {
    fn new(root: &Path) -> Self {
        // Captured SQL and expected paths share this one disposable root.
        let text = std::str::from_utf8(include_bytes!("turn_lifecycle/expected.json"))
            .expect("fixture UTF8");
        let root = serde_json::to_string(&root.to_string_lossy()).expect("JSON root");
        let text = text.replace("${ROOT}", &root[1..root.len() - 1]);
        Self {
            fixture: crate::turn_fixtures::load(text.as_bytes()),
            tokens: Default::default(),
        }
    }
    fn call(&mut self, value: Value) -> Value {
        #[derive(serde::Deserialize)]
        #[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
        enum Request {
            Seed { rust: PathBuf, case: CaseIdentity },
            Snapshot { root: PathBuf },
            Normalize { value: SnapshotValue },
        }
        #[derive(serde::Deserialize)]
        struct CaseIdentity {
            id: String,
        }
        match serde_json::from_value(value).expect("typed lifecycle fixture request") {
            Request::Seed { rust, case } => {
                let captured = self
                    .fixture
                    .cases
                    .iter()
                    .find(|row| row.id == case.id)
                    .expect("captured lifecycle case");
                fs::create_dir_all(&rust).expect("case directory");
                fs::create_dir_all(&captured.expected.python_root)
                    .expect("captured workspace directory");
                fs::create_dir_all(Path::new(&captured.expected.python_root).join("workspace"))
                    .expect("captured shared workspace");
                for (name, database) in &captured.databases {
                    crate::turn_fixtures::write_database(
                        &rust.join(name),
                        &self.fixture.schemas[&database.schema_key],
                        &database.data,
                    );
                }
                for (name, text) in &captured.files {
                    let path = rust.join(name);
                    fs::create_dir_all(path.parent().expect("fixture parent"))
                        .expect("fixture directory");
                    fs::write(&path, text).expect("captured file");
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        const FIXTURE_CREDENTIAL_DIRECTORY_MODE: u32 = 0o700;
                        const FIXTURE_CREDENTIAL_FILE_MODE: u32 = 0o600;
                        fs::set_permissions(
                            path.parent().expect("credential directory"),
                            fs::Permissions::from_mode(FIXTURE_CREDENTIAL_DIRECTORY_MODE),
                        )
                        .expect("private credential directory");
                        fs::set_permissions(
                            &path,
                            fs::Permissions::from_mode(FIXTURE_CREDENTIAL_FILE_MODE),
                        )
                        .expect("private credential file");
                    }
                }
                self.tokens = captured.expected.tokens.clone();
                let mut expected = json!(captured.expected);
                expected["before"] = rebind_expected_hashes(captured.expected.before.clone());
                expected["after"] = rebind_expected_hashes(captured.expected.after.clone());
                expected
            }
            Request::Snapshot { root } => snapshot(&root, &self.tokens),
            Request::Normalize { value } => value.normalize("", &self.tokens),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum SnapshotValue {
    Object(std::collections::BTreeMap<String, SnapshotValue>),
    Array(Vec<SnapshotValue>),
    Text(String),
    Number(serde_json::Number),
    Boolean(bool),
    Null(()),
}
impl SnapshotValue {
    fn normalize(&self, key: &str, tokens: &std::collections::BTreeMap<String, String>) -> Value {
        const CLOCK_FIELDS: &[&str] = &[
            "created",
            "createdAt",
            "created_at",
            "finished",
            "at",
            "lastEvent",
            "reconciledAt",
            "updated",
            "updatedAt",
            "acceptedAt",
            "enqueuedAt",
            "dispatchPickedAt",
            "observedAt",
        ];
        if CLOCK_FIELDS.contains(&key) {
            let valid = match self {
                Self::Number(n) => n.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0),
                Self::Text(t) => chrono::DateTime::parse_from_rfc3339(t).is_ok(),
                _ => false,
            };
            if valid {
                return json!("<time>");
            }
        }
        if ["seq", "revision", "transcriptRevision"].contains(&key)
            && matches!(self,Self::Number(n) if n.as_u64().is_some())
        {
            return json!("<revision>");
        }
        match self {
            Self::Object(values) => Value::Object(
                values
                    .iter()
                    .map(|(k, v)| {
                        let value = if key == "versions" {
                            match v {
                                Self::Text(t) => json!(tokens.get(t).unwrap_or(t)),
                                _ => v.normalize(k, tokens),
                            }
                        } else {
                            v.normalize(k, tokens)
                        };
                        (k.clone(), value)
                    })
                    .collect(),
            ),
            Self::Array(values) => {
                Value::Array(values.iter().map(|v| v.normalize("", tokens)).collect())
            }
            Self::Text(v) => json!(v),
            Self::Number(v) => json!(v),
            Self::Boolean(v) => json!(v),
            Self::Null(()) => Value::Null,
        }
    }
}
fn normalize_snapshot(value: Value, tokens: &std::collections::BTreeMap<String, String>) -> Value {
    serde_json::from_value::<SnapshotValue>(value)
        .expect("snapshot value")
        .normalize("", tokens)
}
fn rebind_expected_hashes(value: Value) -> Value {
    #[derive(serde::Deserialize, serde::Serialize)]
    struct Snapshot {
        sync_entities: Vec<Entity>,
        #[serde(flatten)]
        other: std::collections::BTreeMap<String, Value>,
    }
    #[derive(serde::Deserialize, serde::Serialize)]
    struct Entity {
        payload: Value,
        hash: String,
        #[serde(flatten)]
        other: std::collections::BTreeMap<String, Value>,
    }
    let mut snapshot: Snapshot = serde_json::from_value(value).expect("expected snapshot");
    for row in &mut snapshot.sync_entities {
        row.hash = studio_runtime_store::sha256_hex(
            studio_runtime_store::python_json(&row.payload).as_bytes(),
        );
    }
    json!(snapshot)
}
fn snapshot(root: &Path, tokens: &std::collections::BTreeMap<String, String>) -> Value {
    use rusqlite::types::ValueRef;
    let db = rusqlite::Connection::open(root.join("canvas.sqlite3")).expect("snapshot database");
    let mut result = serde_json::Map::new();
    for table in [
        "runtime_agents",
        "runtime_events",
        "runtime_execution_runs",
        "runtime_execution_attempts",
        "runtime_execution_inputs",
        "runtime_execution_effects",
        "runtime_execution_nodes",
        "runtime_completed_turns",
        "runtime_event_meta",
        "runtime_items",
        "runtime_item_fulltext",
        "runtime_context_manifests",
        "runtime_context_reminders",
        "runtime_budget",
        "runtime_budget_usage",
        "runtime_requests",
        "runtime_tasks",
        "runtime_complaints",
        "runtime_operation_receipts",
        "sync_entities",
        "sync_documents",
        "sync_versions",
        "runtime_search",
        "runtime_search_rows",
        "runtime_search_indexed",
        "runtime_search_next_meta",
        "runtime_search_rollout",
    ] {
        let mut info = db
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))
            .expect("columns");
        let columns: Vec<(String, i64)> = info
            .query_map([], |row| Ok((row.get(1)?, row.get(5)?)))
            .expect("column rows")
            .collect::<Result<_, _>>()
            .expect("column values");
        if columns.is_empty() {
            result.insert(table.into(), json!([]));
            continue;
        }
        let mut keys: Vec<_> = columns.iter().filter(|(_, pk)| *pk > 0).collect();
        keys.sort_by_key(|(_, pk)| *pk);
        let selected = columns
            .iter()
            .map(|(name, _)| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(",");
        let order = if table == "runtime_items" {
            "created,rowid"
        } else {
            "rowid"
        };
        let mut stmt = db
            .prepare(&format!(
                "SELECT {selected} FROM \"{table}\" ORDER BY {order}"
            ))
            .expect("snapshot query");
        let mut rows: Vec<Value> = stmt
            .query_map([], |row| {
                let mut record = serde_json::Map::new();
                for (index, (name, _)) in columns.iter().enumerate() {
                    let value = match row.get_ref(index)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(v) => json!(v),
                        ValueRef::Real(v) => json!(v),
                        ValueRef::Text(v) => {
                            let text = std::str::from_utf8(v).expect("snapshot UTF8");
                            if ["record", "payload", "result"].contains(&name.as_str()) {
                                serde_json::from_str(text).expect("snapshot JSON")
                            } else {
                                json!(text)
                            }
                        }
                        ValueRef::Blob(_) => panic!("unexpected snapshot blob"),
                    };
                    record.insert(name.clone(), value);
                }
                if table == "runtime_agents" {
                    let id = record["id"].as_str().expect("agent id").to_owned();
                    studio_runtime_store::agent_volatile::merge_if_agent(
                        &db,
                        table,
                        &id,
                        record.get_mut("record").expect("agent record"),
                    )
                    .expect("volatile fields");
                }
                Ok(Value::Object(record))
            })
            .expect("snapshot rows")
            .collect::<Result<_, _>>()
            .expect("snapshot values");
        for row in &mut rows {
            if table == "runtime_operation_receipts" {
                let id = row["id"].as_str().expect("receipt id");
                if id == "account-worker:account:default:attempt:turn/start:submitted" {
                    assert_eq!(row["signature"], "submitted");
                    assert_eq!(
                        row["result"]["request"]["operation"],
                        row["result"]["context"]["operation"]
                    );
                    assert_eq!(row["result"]["request"]["method"], "turn/start");
                    row["result"] = json!("<validated-account-worker-submission>");
                } else if id.starts_with("provider-submission:") {
                    // Complete receipt signatures are asserted by the submission corpus.
                    row["signature"] = json!("<validated-signature>");
                }
            }
            if table == "sync_entities" {
                let raw: String = db
                    .query_row(
                        "SELECT payload FROM sync_entities WHERE collection=?1 AND id=?2",
                        rusqlite::params![
                            row["collection"].as_str().expect("collection"),
                            row["id"].as_str().expect("entity id")
                        ],
                        |r| r.get(0),
                    )
                    .expect("raw sync payload");
                assert_eq!(
                    row["hash"],
                    studio_runtime_store::sha256_hex(raw.as_bytes())
                );
                let payload = normalize_snapshot(row["payload"].clone(), tokens);
                row["hash"] = json!(studio_runtime_store::sha256_hex(
                    studio_runtime_store::python_json(&payload).as_bytes()
                ));
            }
            if table == "sync_versions" {
                row["hash"] = json!("<revision>");
            }
            *row = normalize_snapshot(row.clone(), tokens);
        }
        if table != "runtime_items" {
            rows.sort_by_key(|row| {
                json!(
                    keys.iter()
                        .map(|(name, _)| row[name].clone())
                        .collect::<Vec<_>>()
                )
                .to_string()
            });
        }
        result.insert(table.into(), json!(rows));
    }
    let mut postings = db
        .prepare("SELECT 1 FROM sqlite_master WHERE name='runtime_search_next'")
        .expect("fts presence");
    let exists = postings.exists([]).expect("fts existence");
    let rows: Vec<Value> = if exists {
        db.execute_batch("CREATE VIRTUAL TABLE temp.corpus_vocab USING fts5vocab(main,'runtime_search_next','instance')").expect("fts vocabulary");
        let mut stmt = db
            .prepare("SELECT term,doc,col,offset FROM corpus_vocab ORDER BY 1,2,3,4")
            .expect("postings");
        stmt.query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?
            ]))
        })
        .expect("postings rows")
        .collect::<Result<_, _>>()
        .expect("postings values")
    } else {
        vec![]
    };
    result.insert("runtime_search_next_postings".into(), json!(rows));
    Value::Object(result)
}
struct State(PathBuf);
impl Drop for State {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "could not remove disposable state {}: {error}",
                self.0.display()
            );
        }
    }
}
async fn apply(store: &Arc<RuntimeStore>, case: &Value) -> Result<Value, String> {
    let identity = TurnAttemptIdentity {
        agent_id: "agent".into(),
        epoch: 0,
        attempt_id: "attempt".into(),
    };
    let transition = case["transition"].as_str().expect("transition");
    match transition {
        "reconcile" => lifecycle_transaction(store, |tx| SchedulerRepository::reconcile_obsolete_before_submit(tx,"agent",0,"attempt").map(|proceed| json!(!proceed))).await.map_err(|e|e.to_string()),
        "claim" => claim_intent(store, "agent", 0, "attempt", 10.0).await
            .map(|value| value.map(|v| json!({"agent_id":v.agent_id,"epoch":v.epoch,"attempt_id":v.attempt_id,"account_key":v.account_key,"provider":v.provider,"thread_id":v.thread_id,"cwd":v.cwd,"model":v.model,"events":v.events.iter().map(|e|json!({"id":e.id,"kind":e.kind,"text":e.text,"created":e.created,"epoch":e.epoch,"error":e.error,"metadata":e.metadata})).collect::<Vec<_>>()})).unwrap_or(Value::Null))
            .map_err(|e| e.to_string()),
        "submission_identity" => {
            let ticket = case["ticket"].as_str().unwrap_or("ticket").to_owned();
            lifecycle_transaction(store, move |tx| SchedulerRepository::mark_submitted_identity(tx,"agent",0,"attempt",&ticket,"default",1,"test-connection","thread").map(|v|json!(v))).await.map_err(|e|e.to_string())
        },
        "submission" | "reconcile_submission" => {
            use studio_runtime_services::{account_worker::preparation::{prepare_turn_with_connection_id,PreparationResources},scheduler::{TurnIntent,TurnEvent}};
            use studio_runtime_store::{AgentRepository,AgentId,QueueEventRepository,QueueEventId};
            let (intent,submitted)=lifecycle_transaction(store, |tx| {
                let agent=AgentRepository::get(tx,&AgentId::from("agent"))?.expect("seed agent").value();
                let event=QueueEventRepository::get(tx,&QueueEventId::from("event"))?.expect("seed input");
                let metadata=studio_runtime_store::NotificationDispatchRepository::event_metadata(tx,"event")?.unwrap_or_else(||json!({}));
                Ok((TurnIntent {agent_id:"agent".into(),epoch:0,attempt_id:"attempt".into(),account_key:"default".into(),provider:studio_runtime_providers::ProviderKind::Codex,thread_id:agent["threadId"].as_str().map(str::to_owned),cwd:agent["cwd"].as_str().map(str::to_owned),model:agent["model"].as_str().map(str::to_owned),radio_authorized:false,events:vec![TurnEvent {id:event.id.0,kind:event.kind,text:event.text,created:event.created,epoch:event.epoch,error:event.error,metadata}]},agent["startAttempt"]["submitted"]==true))
            }).await.map_err(|e|e.to_string())?;
            // Replays test the durable identity boundary without recompiling delivered input.
            if submitted {
                let ticket = case["ticket"].as_str().unwrap_or("ticket").to_owned();
                return lifecycle_transaction(store, move |tx| {
                    let agent = AgentRepository::get(tx, &AgentId::from("agent"))?
                        .expect("seed agent");
                    let attempt = &agent.extra["startAttempt"];
                    SchedulerRepository::mark_submitted_identity(
                        tx,
                        "agent",
                        0,
                        "attempt",
                        &ticket,
                        attempt["accountKey"].as_str().unwrap_or("default"),
                        attempt["connectionGeneration"].as_u64().unwrap_or(0),
                        attempt["connectionId"].as_str().unwrap_or(""),
                        attempt["threadId"].as_str().unwrap_or("thread"),
                    )
                    .map(|value| json!(value))
                })
                .await
                .map_err(|e| e.to_string());
            }
            let guidance_root = tempfile::tempdir().map_err(|error| error.to_string())?;
            for (relative, content) in [
                ("codex-orchestrator/SKILL.md", "fixture orchestrator skill"),
                ("codex-subagent/SKILL.md", "fixture subagent skill"),
                ("codex-orchestrator/references/native-role.md", "fixture native role"),
                ("codex-orchestrator/references/legacy-move-role.md", "fixture legacy role"),
                ("codex-subagent/references/native-role.md", "fixture subagent role"),
            ] {
                let path = guidance_root.path().join(".agents/skills").join(relative);
                std::fs::create_dir_all(path.parent().expect("guidance parent"))
                    .map_err(|error| error.to_string())?;
                std::fs::write(path, content).map_err(|error| error.to_string())?;
            }
            let resources = PreparationResources::bundled_at(
                String::new(),
                studio_runtime_services::model_catalog::ModelCatalogService::new(),
                guidance_root.path().to_path_buf(),
                guidance_root.path().to_path_buf(),
            )
            .map_err(|e| e.to_string())?;
            assert!(resources
                .role_guidance
                .values()
                .all(|guidance| guidance.starts_with("fixture ")));
            assert!(resources.skills_root.starts_with(guidance_root.path()));
            let prepared=prepare_turn_with_connection_id(Arc::clone(store),&resources,&intent,1,"fixture-connection").await.map_err(|e|e.to_string())?;
            let expected_provider_inputs = prepared.native_request()["params"]["input"].clone();
            if case["transition"] == "submission" {
                let actual = prepared.native_request();
                let expected = &case["nativeRequest"];
                let actual_text = actual["params"]["input"][0]["text"]
                    .as_str()
                    .expect("prepared input text");
                let expected_text = expected["params"]["input"][0]["text"]
                    .as_str()
                    .expect("oracle input text");
                assert_eq!(actual_text, expected_text, "{}: no live or fixture guidance is appended", case["id"]);
                assert_eq!(actual, *expected,"{}: native method and params",case["id"]);
            }
            let submitted=Box::pin(prepared.commit(store,case["ticket"].as_str().unwrap_or("ticket"),1)).await.map_err(|e|e.to_string())?;
            if submitted {
                let db=store.test_reader().map_err(|e|e.to_string())?;
                let receipt=studio_runtime_store::EventWriteRepository::receipt(&db,"account-worker:account:default:attempt:turn/start:submitted").map_err(|e|e.to_string())?.expect("production account-worker submission receipt");
                let receipt:Value=serde_json::from_str(&receipt.result).expect("receipt JSON");
                assert_eq!(receipt["request"],json!({"id":"ticket","generation":1,"method":"turn/start","operation":"account:default:attempt:turn/start","thread":"thread","lightweight":false,"deadline":60000,"unknown":false,"remoteId":null}));
                assert_eq!(receipt["context"]["compiled"]["provider_inputs"],expected_provider_inputs);
                assert_eq!(receipt["context"]["intent"]["events"][0]["id"],"event");
                assert_eq!(receipt["context"]["compiled"]["manifest"],receipt["context"]["compiled"]["metadata_patches"]["event"]["contextManifest"]);
            }
            Ok(json!(submitted))
        },
        "accepted" => {
            let turn_id = case["turnId"].as_str().unwrap_or("turn").to_owned();
            match lifecycle_transaction(store, move |tx| {
            SchedulerRepository::settle_started(
                tx,
                "agent",
                0,
                "attempt",
                &turn_id,
            )
        }).await {
            Ok(value) => Ok(json!(value)),
            Err(error) => {
                if let Some(refusal) = case.get("expectRefusal") {
                    let table = refusal["table"].as_str().expect("refusal table");
                    let stored_id = refusal["id"].as_str().expect("refusal ID");
                    let pointer = refusal["pointer"].as_str().expect("refusal pointer");
                    assert!(
                        matches!(
                            &error,
                            StoreError::InvalidSyncObjectField { scope, id, pointer: actual_pointer, reason }
                                if scope == table
                                    && id == stored_id
                                    && actual_pointer == pointer
                                    && !reason.contains("CORPUS_PRIVATE_PAYLOAD")
                                    && !reason.contains("SECRET_STORED_SENTINEL")
                        ),
                        "{}: acceptance must return the expected typed stored-field refusal: {error:?}",
                        case["id"]
                    );
                }
                Err(error.to_string())
            }
            }
        },
        "unknown" | "not_sent" => {
            let pending = transition == "not_sent";
            lifecycle_transaction(store, move |tx| {
            SchedulerRepository::settle_start_outcome(
                tx,
                &identity,
                if pending {
                    "pending"
                } else {
                    "uncertain"
                },
                None,
            )
        }).await
        .map(|value| json!(value))
        .map_err(|e| e.to_string())
        },
        "rejected" => {
            let message = case["message"].as_str().unwrap_or("rejected").to_owned();
            lifecycle_transaction(store, move |tx| {
            SchedulerRepository::settle_start_rejected_provider(
                tx,
                "agent",
                0,
                "attempt",
                "ticket",
                "default",
                1,
                "thread",
                json!({"message":message}),
            )
            .map(|value| json!(value))
        }).await
        .map(|value| json!(value))
        .map_err(|e| e.to_string())
        },
        "completed" | "failed" | "interrupted" => {
            let mut turn = json!({"id":"turn","status":transition});
            if let Some(fields) = case["turn"].as_object() {
                for (k, v) in fields {
                    turn[k] = v.clone();
                }
            }
            settle_terminal_attempt(store, &identity, &json!({"threadId":"thread","turn":turn})).await
                .map(|value| json!(value))
                .map_err(|e| e.to_string())
        }
        "interrupt_accepted" | "interrupt_rejected" | "interrupt_unknown" => {
            let transition = transition.to_owned();
            lifecycle_transaction(store, move |tx| {
                let prepared = SchedulerRepository::prepare_interrupt(
                    tx,
                    "agent",
                    "thread",
                    "turn",
                    1,
                    "interrupt-ticket",
                )?;
                let settled = SchedulerRepository::settle_interrupt_result(
                    tx,
                    "interrupt-ticket",
                    1,
                    transition.trim_start_matches("interrupt_"),
                    match transition.as_str() {
                        "interrupt_rejected" => Some("rejected"),
                        "interrupt_unknown" => Some("interrupt response unknown"),
                        _ => None,
                    },
                ).map(|value| json!(value))?;
                Ok(json!({"prepared": prepared, "settled": settled}))
            }).await
            .map(|value| json!(value))
            .map_err(|e| e.to_string())
        }
        "stop" | "pause" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::stop_agent(tx, "agent", "Stopped by user").map(|value| json!(value))
        }).await
        .map(|value| json!(value))
        .map_err(|e| e.to_string()),
        "started" => {
            let identity = identity.clone();
            lifecycle_transaction(store, move |tx| {
                SchedulerRepository::observe_started(tx, &identity, "turn").map(|value| json!(value))
            }).await
        .map(|value| json!(value))
        .map_err(|e| e.to_string())
        },
        "restart" => recover_on_startup(store).await
            .map(|value| json!(value))
            .map_err(|e| e.to_string()),
        "disconnect" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::disconnect_account(tx, "default", 1, 20.0, false)
        }).await
        .map(|_interrupted| json!(()))
        .map_err(|e| e.to_string()),
        "reattach" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::reconcile_generation(tx, "default", 1, 20.0, true)
        }).await
        .map(|_interrupted| json!(()))
        .map_err(|e| e.to_string()),
        "generation_change" => lifecycle_transaction(store, |tx| {
            SchedulerRepository::reconcile_generation(tx, "default", 2, 20.0, true)
        }).await
        .map(|_interrupted| json!(()))
        .map_err(|e| e.to_string()),
        _ => panic!("unimplemented Rust transition {transition}"),
    }
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
        _=>out.push(json!({"path":path,"python":python,"rust":rust,"pythonPresent":python.is_some(),"rustPresent":rust.is_some()})),
    }
}
// Named, field-level contracts: a deviation may disappear as its owner is wired,
// but a different value or an additional field never becomes an allowed difference.
fn expected_difference(diff: &Value, case: &Value, python_root: &Path) -> bool {
    case["expectedDifferences"].as_array().is_some_and(|rules| {
        rules.iter().any(|rule| {
            if rule["path"] != diff["path"] {
                return false;
            }
            if rule["pythonPresent"] != diff["pythonPresent"]
                || rule["rustPresent"] != diff["rustPresent"]
            {
                return false;
            }
            if rule["expectedAbsent"] == true {
                return diff["rustPresent"] == false
                    && diff["pythonPresent"] == true
                    && (rule.get("pythonId").is_none()
                        || rule["pythonId"] == diff["python"]["id"]);
            }
            let expanded = serde_json::to_string(rule)
                .expect("expectation JSON")
                .replace("${PYTHON_ROOT}", &python_root.to_string_lossy());
            let rule: Value = serde_json::from_str(&expanded).expect("expanded expectation");
            rule["python"] == diff["python"] && rule["rust"] == diff["rust"]
        })
    })
}
fn unexpected_differences(diff: &[Value], case: &Value, python_root: &Path) -> Vec<Value> {
    diff.iter()
        .filter(|d| {
            if expected_difference(d, case, python_root) {
                return false;
            }
            let path = d["path"].as_str().expect("difference path");
            if path.starts_with("/sync_entities/") && path.ends_with("/hash") {
                // snapshot validates each raw payload hash and derives this normalized
                // hash. Its only permitted cause is an explicitly declared payload delta.
                let payload = format!("{}/payload/", path.trim_end_matches("/hash"));
                let fields: Vec<_> = diff
                    .iter()
                    .filter(|d| d["path"].as_str().is_some_and(|p| p.starts_with(&payload)))
                    .collect();
                return fields.is_empty()
                    || !fields
                        .iter()
                        .all(|d| expected_difference(d, case, python_root));
            }
            true
        })
        .cloned()
        .collect()
}
#[test]
fn fast_witnesses_cover_transitions_states_and_review_findings() {
    // Tuple arrays must remain arrays rather than matching a text-repeat descriptor.
    let expanded: Value = crate::turn_fixtures::load(r#"{"values":[{"text":"base","epoch":["thread",0]}],"root":[["thread",0],{"$repeat":"é","count":3},{"$base":0,"set":{"text":{"$concat":["a",{"$repeat":"b","count":2}]}}}]}"#.as_bytes());
    assert_eq!(
        expanded,
        json!([["thread",0],"ééé",{"text":"abb","epoch":["thread",0]}])
    );
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_lifecycle/cases.json")).expect("corpus");
    let notifications: Vec<Value> =
        serde_json::from_str(include_str!("turn_lifecycle/notification_cases.json"))
            .expect("notification cases");
    let registry: Value = serde_json::from_str(include_str!("turn_lifecycle/deviations.json"))
        .expect("central registry");
    let registry = registry.as_object().expect("class declarations");
    assert_eq!(
        registry.len(),
        3,
        "the remaining lifecycle and notification classes are closed"
    );
    let owners: std::collections::BTreeSet<_> = cases
        .iter()
        .flat_map(|c| c["expectedDifferences"].as_array().into_iter().flatten())
        .map(|rule| rule["deviation"].as_str().expect("class"))
        .chain(notifications.iter().filter_map(|c| c["deviation"].as_str()))
        .collect();
    assert_eq!(
        owners,
        registry.keys().map(String::as_str).collect(),
        "every case deviation is declared and every declaration has an owner, including unit witnesses"
    );
    let mutations: Vec<Value> = serde_json::from_str(include_str!("turn_lifecycle/mutations.json"))
        .expect("mutation inventory");
    for mutation in mutations {
        for witness in mutation["workerWitnesses"].as_array().into_iter().flatten() {
            let name = witness.as_str().expect("worker witness name");
            assert!(
                include_str!("../src/account_worker/tests.rs")
                    .contains(&format!("async fn {name}("))
                    || include_str!("../src/account_worker/tests/closure.rs")
                        .contains(&format!("async fn {name}("))
                    || include_str!("../src/model_catalog.rs")
                        .contains(&format!("async fn {name}("))
                    || include_str!("../src/account_worker/tests/crash_matrix.rs")
                        .contains(&format!("async fn {name}(")),
                "missing worker witness {name}"
            );
        }
        for id in mutation["witnesses"]
            .as_array()
            .expect("mutation witnesses")
        {
            assert!(
                cases
                    .iter()
                    .chain(&notifications)
                    .any(|c| c["id"] == *id && c["tier"] == "fast"),
                "{}: missing fast mutation witness {id}",
                mutation["id"]
            );
        }
        for combination in mutation["lifecycleCombinations"]
            .as_array()
            .expect("transition/state combinations")
        {
            assert!(
                cases.iter().any(|c| c["tier"] == "fast"
                    && combination
                        .as_object()
                        .expect("combination")
                        .iter()
                        .all(|(key, value)| c[key] == *value)),
                "missing mutation transition/state combination: {combination}"
            );
        }
    }
    for key in ["transition", "state"] {
        let all: std::collections::BTreeSet<_> = cases
            .iter()
            .map(|c| c[key].as_str().expect("coverage key"))
            .collect();
        let fast: std::collections::BTreeSet<_> = cases
            .iter()
            .filter(|c| c["tier"] == "fast")
            .map(|c| c[key].as_str().expect("coverage key"))
            .collect();
        assert_eq!(all, fast, "fast {key} witnesses");
    }
    for finding in (1..=13)
        .map(|n| format!("A{n}"))
        .chain((1..=6).map(|n| format!("B{n}")))
    {
        assert!(
            cases.iter().any(|c| c["tier"] == "fast"
                && c["findings"]
                    .as_array()
                    .is_some_and(|f| f.contains(&json!(finding)))),
            "missing {finding}"
        );
    }
    let coverage: Value =
        serde_json::from_str(include_str!("turn_lifecycle/coverage.json")).expect("coverage data");
    for case in coverage["sameStateComparisons"]
        .as_object()
        .expect("comparison map")
        .values()
    {
        assert!(
            cases.iter().any(|c| c["id"] == *case),
            "missing original comparison {case}"
        );
    }
    for entry in coverage["matrix"].as_array().expect("matrix") {
        if let Some(ids) = entry["cases"].as_array() {
            assert!(!ids.is_empty());
            for id in ids {
                assert!(cases.iter().any(|c| c["id"] == *id
                    && c["transition"] == entry["transition"]
                    && c["state"] == entry["state"]));
            }
        } else {
            assert!(
                entry["reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty())
            );
        }
    }
}

#[tokio::test]
async fn python_written_turn_lifecycle_corpus() {
    let mut cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_lifecycle/cases.json")).expect("corpus data");
    if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none()
        && std::env::var_os("LIFECYCLE_CASE_FILTER").is_none()
    {
        cases.retain(|c| c["tier"] == "fast");
    }
    if let Ok(filter) = std::env::var("LIFECYCLE_CASE_FILTER") {
        cases.retain(|c| c["id"].as_str().is_some_and(|id| id.contains(&filter)));
    }
    assert!(!cases.is_empty(), "case filter matched nothing");
    // Rust and Python resolve some stored paths and keep others as written; a
    // resolved root (the macOS temporary folder is under `/var`) gives both one form.
    let scratch = studio_runtime_store::path_identity::resolve(&std::env::temp_dir())
        .expect("resolved temporary folder");
    let root = State(scratch.join(format!("turn-corpus-{}", std::process::id())));
    fs::create_dir(&root.0).expect("unique corpus state");
    let mut oracle = CapturedExpectations::new(&root.0);
    let registry: Value = serde_json::from_str(include_str!("turn_lifecycle/deviations.json"))
        .expect("central deviations");
    let mut report = vec![];
    let mut green = 0;
    for case in &cases {
        #[derive(serde::Deserialize)]
        struct SnapshotExpectation {
            #[serde(default)]
            unchanged: bool,
        }
        let expectation: SnapshotExpectation =
            serde_json::from_value(case.clone()).expect("snapshot expectation");
        let id = case["id"].as_str().expect("case ID");
        for rule in case["expectedDifferences"].as_array().into_iter().flatten() {
            let class = rule["deviation"].as_str().expect("named deviation");
            let declaration = registry
                .get(class)
                .expect("deviation must be centrally declared");
            let path = rule["path"]
                .as_str()
                .expect("deviation field")
                .split('/')
                .map(|p| if p.parse::<usize>().is_ok() { "*" } else { p })
                .collect::<Vec<_>>()
                .join("/");
            assert!(
                declaration["fields"]
                    .as_array()
                    .expect("fixed fields")
                    .contains(&json!(path)),
                "{id}: undeclared deviation field {path}"
            );
            if declaration["approvedClass"] == "a" {
                assert!(
                    declaration["entrypoints"]
                        .as_array()
                        .expect("deviation entrypoints")
                        .contains(&rule["entrypoint"])
                );
            }
        }
        let rust = root.0.join(id).join("rust");
        let expected = oracle.call(json!({"command":"seed","rust":rust,"case":case}));
        let store = match RuntimeStore::open(Some(&rust)).await {
            Ok(store) => Arc::new(store),
            Err(error) => {
                let refusal = case
                    .get("expectRefusal")
                    .unwrap_or_else(|| panic!("{id}: open Python-written state: {error}"));
                let table = refusal["table"].as_str().expect("table");
                let stored_id = refusal["id"].as_str().expect("id");
                let message = error.to_string();
                assert!(
                    matches!(
                        &error,
                        StoreError::InvalidSyncObjectField { scope, id, pointer, reason }
                            if scope == table
                                && id == stored_id
                                && pointer == refusal["pointer"].as_str().expect("pointer")
                                && !reason.contains("CORPUS_PRIVATE_PAYLOAD")
                                && !reason.contains("SECRET_STORED_SENTINEL")
                    ),
                    "{id}: startup refusal must preserve its structured stored-field cause: {message} ({error:?})"
                );
                assert!(
                    !message.contains("CORPUS_PRIVATE_PAYLOAD")
                        && !message.contains("SECRET_STORED_SENTINEL")
                );
                let actual = oracle.call(json!({"command":"snapshot","root":rust}));
                assert_eq!(
                    actual, expected["before"],
                    "{id}: startup refusal must not write"
                );
                report.push(json!({"id":id,"differences":[],"unexpected":[],"error":null,"firstResult":null,"pythonResult":expected["entryResult"]}));
                green += 1;
                continue;
            }
        };
        if case["roundtripHold"] == true {
            lifecycle_transaction(&store, |tx| {
                SchedulerRepository::hold_account(tx, "default", Some(7), "first account hold")?;
                SchedulerRepository::hold_account(tx, "default", Some(8), "second account hold")?;
                SchedulerRepository::clear_account_hold(tx, "default")
            })
            .await
            .expect("nested hold restoration");
        }
        let mut prepared_case = case.clone();
        prepared_case["nativeRequest"] = expected["nativeRequest"].clone();
        let case = &prepared_case;
        let first = Box::pin(apply(&store, case)).await;
        let mut error = first.as_ref().err().cloned();
        if let Ok(value) = &first {
            let normalized = oracle.call(json!({"command":"normalize","value":value}));
            assert_eq!(normalized, case["return"], "{id}: entry-point return value");
        }
        if let Some(text) = case["afterEventText"].as_str() {
            let text = text.to_owned();
            store
                .transaction(move |tx| {
                    tx.execute(
                        "UPDATE runtime_events SET text=?1 WHERE id='event'",
                        [&text],
                    )?;
                    Ok(())
                })
                .await
                .expect("Python writer mutation equivalent");
        }
        if let Some(probe) = case["probe"].as_object() {
            let mut changed = case.clone();
            for (key, value) in probe {
                changed[key] = value.clone();
            }
            let before = store
                .transaction(|tx| Ok(tx.total_changes()))
                .await
                .expect("probe write counter");
            let refusal = Box::pin(apply(&store, &changed))
                .await
                .expect_err("conflicting evidence must be refused");
            assert_eq!(
                refusal,
                case["probeRefusal"].as_str().expect("exact conflict error"),
                "{id}: exact conflict location"
            );
            let after = store
                .transaction(|tx| Ok(tx.total_changes()))
                .await
                .expect("probe write counter");
            assert_eq!(before, after, "conflicting replay wrote SQLite");
        }
        if let Some(outcome) = case["transition"]
            .as_str()
            .and_then(|v| v.strip_prefix("interrupt_"))
            && error.is_none()
        {
            let response_error = match outcome {
                "rejected" => Some("rejected"),
                "unknown" => Some("interrupt response unknown"),
                _ => None,
            };
            let outcome = outcome.to_owned();
            let replay_outcome = outcome.clone();
            let response_error = response_error.map(str::to_owned);
            let before = store
                .transaction(|tx| Ok(tx.total_changes()))
                .await
                .expect("interrupt counter");
            let replay = lifecycle_transaction(&store, move |tx| {
                SchedulerRepository::settle_interrupt_result(
                    tx,
                    "interrupt-ticket",
                    1,
                    &replay_outcome,
                    response_error.as_deref(),
                )
                .map(|value| json!(value))
            })
            .await
            .expect("interrupt settlement replay");
            assert_eq!(
                json!(replay),
                first.as_ref().expect("first result")["settled"],
                "{id}: saved interrupt result"
            );
            let refusal = lifecycle_transaction(&store, move |tx| {
                SchedulerRepository::settle_interrupt_result(
                    tx,
                    "interrupt-ticket",
                    1,
                    &outcome,
                    Some("CORPUS_PRIVATE_PAYLOAD"),
                )
            })
            .await
            .expect_err("changed interrupt result must refuse");
            assert_eq!(
                refusal.to_string(),
                "invalid stored sync object runtime_agents/agent"
            );
            let after = store
                .transaction(|tx| Ok(tx.total_changes()))
                .await
                .expect("interrupt counter");
            assert_eq!(
                before, after,
                "{id}: interrupt replay or refusal wrote SQLite"
            );
        }
        if case["replay"] == true && error.is_none() {
            let before = store
                .transaction(|tx| Ok(tx.total_changes()))
                .await
                .expect("write counter");
            let replay = Box::pin(apply(&store, case)).await;
            assert_eq!(
                replay, first,
                "{id}: replay must return the saved first result"
            );
            error = replay.err();
            let after = store
                .transaction(|tx| Ok(tx.total_changes()))
                .await
                .expect("write counter");
            if after != before {
                error = Some(format!("replay made {} writes", after - before));
            }
        }
        drop(store);
        let actual = oracle.call(json!({"command":"snapshot","root":rust}));
        let mut diff = vec![];
        let comparison = if let Some(refusal) = case.get("expectRefusal") {
            let message = error
                .take()
                .expect("malformed or conflicting stored evidence must be refused");
            if let Some(pointer) = refusal["pointer"].as_str() {
                let prefix = format!(
                    "invalid stored sync object {}/{} at {pointer}: ",
                    refusal["table"].as_str().expect("refusal table"),
                    refusal["id"].as_str().expect("refusal ID")
                );
                assert!(
                    message.starts_with(&prefix),
                    "{id}: wrong typed refusal body: {message}"
                );
                assert!(
                    !message.contains("CORPUS_PRIVATE_PAYLOAD")
                        && !message.contains("SECRET_STORED_SENTINEL"),
                    "{id}: refusal body exposed planted data: {message}"
                );
            } else {
                assert!(
                    message
                        == refusal["message"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!(
                                "invalid stored sync object {}/{}",
                                refusal["table"].as_str().expect("refusal table"),
                                refusal["id"].as_str().expect("refusal ID")
                            )),
                    "{id}: error must locate table and stored id only: {message}"
                );
            }
            &expected["before"]
        } else if expectation.unchanged {
            &expected["before"]
        } else {
            &expected["after"]
        };
        differences(Some(comparison), Some(&actual), "", &mut diff);
        for rule in case["expectedDifferences"].as_array().into_iter().flatten() {
            if !diff.iter().any(|d| d["path"] == rule["path"]) {
                error = Some(format!(
                    "stale deviation: {} {}",
                    rule["deviation"], rule["path"]
                ));
            }
        }
        let unexpected = unexpected_differences(
            &diff,
            case,
            Path::new(expected["pythonRoot"].as_str().expect("oracle Python root")),
        );
        if unexpected.is_empty() && error.is_none() {
            green += 1;
        } else {
            eprintln!(
                "{id}: {} unexpected differences; error={error:?}",
                unexpected.len()
            );
        }
        report.push(json!({"id":id,"differences":diff,"unexpected":unexpected,"error":error,"firstResult":first.ok(),"pythonResult":expected["entryResult"]}));
    }
    let report_path = std::env::var_os("LIFECYCLE_CORPUS_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.0.join("turn-lifecycle-corpus-report.json"));
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).expect("report JSON"),
    )
    .expect("report written");
    assert_eq!(
        green,
        cases.len(),
        "{green} green / {} red; field differences: {}",
        cases.len() - green,
        report_path.display()
    );
}
