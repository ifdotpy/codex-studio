//! Python-produced snapshots exercised through the transaction consumer interfaces.
use studio_runtime_schema::provider_kind::ProviderKind;

use rusqlite::params;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command, sync::Arc};
use studio_runtime_services::{agent_move, project_locations};
use studio_runtime_store::{
    AgentId, AgentMoveRepository, AgentRecord, AgentRepository, ProjectLocationsRepository,
    ProjectsRepository, RuntimeStore,
};

struct Fixture {
    root: tempfile::TempDir,
    template: PathBuf,
    data: Value,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary oracle root");
        let template = root.path().join("template");
        crate::recovery_fixtures::state(
            &template,
            include_str!("relocation/canvas.sql"),
            include_str!("relocation/analytics.sql"),
        );
        // Frozen inputs retain the producer's failed/unknown pair for unresolved tools.
        let corpus = if full() {
            include_str!("relocation/oracle-full.json")
        } else {
            include_str!("relocation/oracle.json")
        };
        let mut data: Value = serde_json::from_str(corpus).expect("frozen relocation corpus");
        // Relocation consumes canonical records after startup migration. These
        // historical cases belong to Codex; keep every other oracle field intact.
        for case in data["cases"].as_array_mut().expect("cases") {
            for pointer in ["/agent", "/expected/agent"] {
                if let Some(agent) = case.pointer_mut(pointer).and_then(Value::as_object_mut) {
                    agent.insert("provider".into(), json!("codex"));
                    agent.entry("compactions").or_insert(json!(0));
                }
            }
        }
        for case in data["cases"].as_array_mut().expect("cases") {
            if case["family"] == "location" {
                for pointer in ["/project", "/expected/project"] {
                    if let Some(project) = case.pointer_mut(pointer).and_then(Value::as_object_mut)
                    {
                        project.entry("peerTeams").or_insert(json!([]));
                    }
                }
                let root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
                let identity = studio_runtime_schema::persistence::RequestIdentity::new(
                    "project-location",
                    &["local-fixture", "ui"],
                    &root,
                    &["location"],
                )
                .expect("fixture identity");
                case["resultId"] = json!(identity.id());
                case["requestRoot"] = json!(root);
            }
        }
        for case in data["cases"].as_array_mut().expect("cases") {
            if case["family"] == "tool" {
                if case["name"] == "tool-old-applied-wrapper" {
                    // Completed replies remain exact even if an old wrapper disagrees.
                    case["expected"]["record"] = case["record"].clone();
                    case["expected"]["result"] = case["previous"].clone();
                }
                let admitted = json!({"id":case["record"]["id"],"signature":"fixture-tool","created":chrono::Utc::now().timestamp_millis()});
                let mut arguments = case["outbox"]["body"]["payload"]["request"].clone();
                if arguments["action"] == "remove_location" {
                    arguments
                        .as_object_mut()
                        .expect("location arguments")
                        .remove("path");
                }
                for field in ["record", "expected/record"] {
                    let target = if field == "record" {
                        &mut case["record"]
                    } else {
                        &mut case["expected"]["record"]
                    };
                    target["admitted"] = admitted.clone();
                    target["kind"] = json!("managed_tool");
                    target["arguments"] = arguments.clone();
                    target["aliases"] = json!([]);
                }
            }
        }
        Self {
            root,
            template,
            data,
        }
    }
    async fn store(&self, name: &str) -> (PathBuf, RuntimeStore) {
        let path = self.root.path().join(name);
        fs::create_dir(&path).expect("case directory");
        for filename in ["canvas.sqlite3", "analytics.sqlite3"] {
            fs::copy(self.template.join(filename), path.join(filename))
                .expect("clone prepared template");
        }
        let store = RuntimeStore::open(Some(&path))
            .await
            .expect("open Python-written schema");
        (path, store)
    }
    async fn seed(&self, store: &RuntimeStore, case: &Value) {
        let case = case.clone();
        store.transaction(move |tx|{
            if case["family"]=="location" {
                let outbox=&case["outbox"];
                if !case["project"].is_null() {
                    let mut project = case["project"].clone();
                    project["registration"] = json!({"requestId":outbox["id"],"status":studio_runtime_schema::persistence::WorkEffectStatus::Unknown});
                    ProjectsRepository::put_project(tx,&project)?;
                    let identity = studio_runtime_schema::persistence::RequestIdentity::new("project-location", &["local-fixture","ui"], text(&case,"requestRoot"), &["location"]).expect("fixture identity");
                    let signature = studio_runtime_store::sha256_hex(studio_runtime_schema::json_compat::python_json_compact(&outbox["body"]["payload"]["request"]).as_bytes());
                    // Recreate the historical owner recorded before the expiry watermark.
                    tx.execute("UPDATE runtime_projects SET record=json_patch(record,json_object('locationIntents',json_object(?2,json_object('request',json_object('id',?3,'signature',?4,'created',?5))))) WHERE id=?1",
                        params!["/home-project",text(outbox,"id"),identity.id(),signature,identity.issued_ms()])?;
                    ProjectsRepository::location_request(tx,"/home-project",text(outbox,"id"))?.expect("saved fixture admission");
                }
                studio_runtime_store::remote_outbox::RemoteOutbox::put(tx,studio_runtime_store::remote_outbox::RemoteEffect::ServerRequest,text(outbox,"server"),text(outbox,"id"),"fingerprint",if text(outbox,"state")=="queued"{studio_runtime_schema::persistence::OutboxStatus::Pending}else{studio_runtime_schema::persistence::OutboxStatus::Submitted},Some(0),json!({"body":outbox["body"].to_string(),"admission":{"id":"fixture","signature":"fingerprint","created":1},"attempts":0,"error":null}),1)?;
            } else if case["family"] == "tool" {
                let record=&case["record"];let outbox=&case["outbox"];
                AgentRepository::put(tx,&AgentRecord::from_value(json!({"compactions":0,
                    "id":record["agent"],"rootId":record["agent"],"epoch":record["epoch"],"status":"idle","accountKey":"default","provider":"codex"
                })).expect("tool actor record"))?;
                let mut owner = record.clone();
                if record["outcome"]!="unknown" && record["outcome"]!="pending" {
                    owner.as_object_mut().expect("tool owner").remove("result");
                }
                tx.execute("INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",params![text(record,"id"),owner.to_string()])?;
                #[derive(serde::Deserialize)]
                struct ToolOwner { admitted: studio_runtime_store::request_results::AdmittedRequest }
                let raw: String = tx.query_row("SELECT record FROM runtime_tool_requests WHERE id=?1", [text(record,"id")], |row| row.get(0))?;
                let parent = serde_json::from_str::<ToolOwner>(&raw).expect("owner token").admitted;
                let location=parent.derive_result("project-location", &["location"], &studio_runtime_store::sha256_hex(studio_runtime_schema::json_compat::python_json_compact(&outbox["body"]["payload"]["request"]).as_bytes()));
                let result = &outbox["result"];
                let outcome = match text(result, "outcome") {
                    "applied" => project_locations::LocationOutcome::Applied,
                    "not_applied" => project_locations::LocationOutcome::NotApplied,
                    "unknown" => project_locations::LocationOutcome::Unknown,
                    other => panic!("unexpected location outcome {other}"),
                };
                assert_eq!(result, &json!({"requestId":text(result,"requestId"),"outcome":text(result,"outcome"),"value":{"removed":true,"serverId":"peer"}}));
                let receipt = project_locations::LocationResult {
                    request_id: text(result,"requestId").into(), outcome, project_id: None, error: None,
                    value: Some(project_locations::LocationValue { server_id: "peer".into(), removed: Some(true), path: None, canonical_path: None, project_id: None, git_origin: None, home_project_id: None, request_id: None }),
                };
                if receipt.outcome!=project_locations::LocationOutcome::Unknown {
                    studio_runtime_services::event_write::settle_request(tx,&location,Some(text(record,"agent")),&receipt).expect("location result");
                }
                if record["outcome"]!="unknown" && record["outcome"]!="pending" {
                    let result=studio_runtime_store::ToolResultRecord::from_value(case["previous"].clone()).expect("tool result");
                    studio_runtime_services::event_write::settle_request(tx,&parent,Some(text(record,"agent")),&result).expect("tool result");
                }
            } else {
                AgentMoveRepository::save(tx,&case["operation"])?;
                let mut agent = case["agent"].clone();
                agent["compactions"] = json!(0);
                AgentRepository::put(tx,&AgentRecord::from_value(agent).expect("Python agent"))?;
            }
            Ok(())
        }).await.expect("seed Python records without rebuilding snapshots");
    }
}
fn project_domain(mut project: Value) -> Value {
    if let Some(fields) = project.as_object_mut() {
        fields.remove("registration");
        fields.remove("locationIntents");
    }
    project
}
fn text<'a>(row: &'a Value, key: &str) -> &'a str {
    row[key].as_str().expect("fixture string")
}
fn full() -> bool {
    std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some()
}

fn snapshot(store: &RuntimeStore) -> Value {
    let db = store.test_reader().expect("read state");
    let mut result = serde_json::Map::new();
    for table in [
        "runtime_projects",
        "request_results",
        "runtime_outbox",
        "runtime_agent_moves",
        "runtime_agents",
        "runtime_tool_requests",
        "sync_entities",
    ] {
        let mut statement = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .expect("snapshot statement");
        let count = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                let mut values = Vec::new();
                for index in 0..count {
                    values.push(format!("{:?}", row.get_ref(index)?));
                }
                Ok(values)
            })
            .expect("snapshot query")
            .collect::<Result<Vec<_>, _>>()
            .expect("snapshot rows");
        result.insert(table.to_owned(), json!(rows));
    }
    Value::Object(result)
}

async fn apply_move_case(
    store: &RuntimeStore,
    name: &str,
    case: &Value,
    now: f64,
) -> Result<bool, agent_move::MoveError> {
    if name == "expire-importing" {
        agent_move::expire(store, text(&case["operation"], "id"), now).await
    } else {
        let agent = text(&case["agent"], "id").to_owned();
        let notification = case["notification"].clone();
        use studio_runtime_schema::usage::parse_token_usage;
        let provider = ProviderKind::of_record(&case["agent"]).expect("fixture provider");
        let usage = parse_token_usage(provider, &notification).map_err(|_| {
            agent_move::MoveError::InvalidStored {
                object: "tokenUsage",
                pointer: "/usage".into(),
            }
        })?;
        store
            .transaction_with(move |tx| {
                agent_move::capture_cache(tx, &agent, &notification, &usage, now)
            })
            .await
    }
}

async fn complete_location(
    store: &RuntimeStore,
    key: &str,
    server: &str,
    envelope: &Value,
    receipt: &Value,
    local_server: &str,
    now: f64,
) -> Result<Value, project_locations::LocationError> {
    let key = key.to_owned();
    let server = server.to_owned();
    let envelope = envelope.clone();
    let receipt = receipt.clone();
    let local_server = local_server.to_owned();
    store
        .transaction_with(move |tx| {
            complete_location_in_fixture(tx, &key, &server, &envelope, &receipt, &local_server, now)
        })
        .await
}
fn complete_location_in_fixture(
    tx: &studio_runtime_store::Transaction<'_>,
    key: &str,
    server: &str,
    envelope: &Value,
    receipt: &Value,
    local_server: &str,
    now: f64,
) -> Result<Value, project_locations::LocationError> {
    let raw = ProjectLocationsRepository::outbox(tx, key)?.ok_or(
        project_locations::LocationError::Refused("Unknown location request"),
    )?;
    if raw["server"] != server || raw["body"] != *envelope {
        return Err(project_locations::LocationError::Refused(
            "Different location envelope",
        ));
    }
    let typed = serde_json::from_str(&receipt.to_string())
        .map_err(studio_runtime_store::StoreError::from)?;
    project_locations::complete_snapshot_in_transaction(
        tx,
        key,
        server,
        &envelope.to_string(),
        &typed,
        local_server,
        now,
    )?;
    if receipt["outcome"] != "unknown" {
        tx.execute(
            "DELETE FROM runtime_outbox WHERE kind='remote_delivery' AND request_id=?1",
            [key],
        )?;
    }
    Ok(receipt.clone())
}
fn saved_location(
    store: &RuntimeStore,
    case: &Value,
    now: f64,
) -> project_locations::LocationResult {
    let db = store.test_reader().expect("reader");
    let signature = studio_runtime_store::sha256_hex(
        studio_runtime_schema::json_compat::python_json_compact(
            &case["outbox"]["body"]["payload"]["request"],
        )
        .as_bytes(),
    );
    studio_runtime_services::event_write::replay_request::<project_locations::LocationResult>(
        &db,
        text(case, "resultId"),
        &signature,
        (now * 1000.0) as i64,
    )
    .expect("replay")
    .expect("result")
    .reply
}

#[tokio::test]
async fn python_location_and_move_corpus_through_consumers() {
    let fixture = Fixture::new();
    let now = fixture.data["now"].as_f64().expect("clock");
    let local = text(&fixture.data, "local");
    for case in fixture.data["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .filter(|case| text(case, "family") != "release")
    {
        // Full matrix remains opt-in; fast witnesses cover each semantic family.
        if !full()
            && ![
                "add-changed-location",
                "remove-location",
                "expire-importing",
                "cache-first-observation",
                "cache-boolean-not-integer",
                "cache-wrong-turn",
                "status-waiting",
                "tool-applied",
                "tool-not-applied",
                "tool-old-applied-wrapper",
                "hold-importing-unknown",
                "cache-existing-observation",
            ]
            .contains(&text(case, "name"))
        {
            continue;
        }
        let (path, store) = fixture.store(text(case, "name")).await;
        fixture.seed(&store, case).await;
        let store = Arc::new(store);
        let family = text(case, "family");
        let before = snapshot(&store);
        match family {
            "location" => {
                let outbox = &case["outbox"];
                let key = text(outbox, "id");
                let value = complete_location(
                    &store,
                    key,
                    "peer",
                    &outbox["body"],
                    &case["receipt"],
                    local,
                    now,
                )
                .await
                .expect("location applies");
                assert_eq!(
                    value, case["expected"]["receipt"],
                    "{} receipt",
                    case["name"]
                );
                let db = store.test_reader().expect("read");
                assert_eq!(
                    ProjectsRepository::get(&db, "/home-project")
                        .expect("project")
                        .map(|(_, v)| project_domain(v))
                        .unwrap_or(Value::Null),
                    case["expected"]["project"],
                    "{} project",
                    case["name"]
                );
                let result = studio_runtime_services::event_write::replay_request::<
                    project_locations::LocationResult,
                >(
                    &db,
                    text(case, "resultId"),
                    &studio_runtime_store::sha256_hex(
                        studio_runtime_schema::json_compat::python_json_compact(
                            &outbox["body"]["payload"]["request"],
                        )
                        .as_bytes(),
                    ),
                    (now * 1000.0) as i64,
                )
                .expect("registration")
                .expect("result");
                assert_eq!(json!(result.reply), case["expected"]["registration"]);
                if !case["project"].is_null() {
                    let payload:String=db.query_row("SELECT payload FROM sync_entities WHERE collection='project' AND id='/home-project'",[],|r|r.get(0)).expect("project sync published");
                    let payload: Value = serde_json::from_str(&payload).expect("payload");
                    assert_eq!(
                        payload["value"]["locations"],
                        case["expected"]["project"]["locations"]
                    );
                }
                drop(db);
                let committed = snapshot(&store);
                drop(store);
                let reopened = RuntimeStore::open(Some(&path)).await.expect("restart");
                assert_eq!(
                    json!(saved_location(&reopened, case, now + 1.0)),
                    case["expected"]["registration"]
                );
                assert_eq!(snapshot(&reopened), committed, "replay writes nothing");
                continue;
            }
            "tool" => {
                let record = case["record"].clone();
                let actor = text(&record, "agent").to_owned();
                let tool_id = text(&record, "id").to_owned();
                let actor_for_replay = actor.clone();
                let tool_id_for_replay = tool_id.clone();
                let actual = store
                    .transaction_with(move |tx| {
                        studio_runtime_services::tool_requests::execute(
                            tx,
                            &actor_for_replay,
                            studio_runtime_services::tool_requests::Action::Get(
                                &tool_id_for_replay,
                            ),
                            now,
                        )
                    })
                    .await
                    .expect("repair committed location receipt");
                assert_eq!(actual["outcome"], case["expected"]["record"]["outcome"]);
                assert_eq!(actual["result"], case["expected"]["record"]["result"]);
                let db = store.test_reader().expect("read");
                let stored = ProjectLocationsRepository::tool(&db, text(&record, "id"))
                    .expect("stored request")
                    .expect("exists");
                let result =
                    studio_runtime_services::tool_requests::saved_result(&db, text(&record, "id"))
                        .expect("stored result")
                        .expect("exists");
                let mut expected = case["expected"]["record"].clone();
                if let Some(fields) = expected.as_object_mut() {
                    fields.remove("result");
                }
                assert_eq!(generated_tool_json(stored), generated_tool_json(expected));
                assert_eq!(
                    generated_tool_json(result),
                    generated_tool_json(case["expected"]["result"].clone())
                );
                drop(db);
                let committed = snapshot(&store);
                drop(store);
                let store = RuntimeStore::open(Some(&path)).await.expect("restart");
                let replay = store
                    .transaction_with(move |tx| {
                        studio_runtime_services::tool_requests::execute(
                            tx,
                            &actor,
                            studio_runtime_services::tool_requests::Action::Get(&tool_id),
                            now + 1.0,
                        )
                    })
                    .await
                    .expect("replay repaired location receipt");
                assert_eq!(replay["outcome"], case["expected"]["record"]["outcome"]);
                assert_eq!(snapshot(&store), committed);
                continue;
            }
            "expire" => {
                assert_eq!(
                    agent_move::expire(&store, text(&case["operation"], "id"), now)
                        .await
                        .expect("expire"),
                    case["changed"].as_bool().expect("changed")
                );
            }
            "cache" => {
                let result = apply_move_case(&store, text(case, "name"), case, now).await;
                if case["name"] == "cache-boolean-not-integer" {
                    assert!(
                        result.is_err(),
                        "malformed native token category is rejected"
                    );
                    assert_eq!(
                        snapshot(&store),
                        before,
                        "invalid usage leaves state unchanged"
                    );
                } else {
                    result.expect("cache notification");
                }
            }
            "status" => {
                let actor = text(&fixture.data, "actor").to_owned();
                let case_name = text(case, "name").to_owned();
                let local = local.to_owned();
                let value = store
                    .transaction_with(move |tx| agent_move::status(tx, &actor, &case_name, &local))
                    .await
                    .expect("status");
                assert_eq!(value, case["status"]);
                assert_eq!(snapshot(&store), before);
                continue;
            }
            _ => panic!("unknown fixture family"),
        }
        let db = store.test_reader().expect("read");
        assert_eq!(
            AgentMoveRepository::get(&db, text(&case["operation"], "id"))
                .expect("move")
                .expect("exists"),
            case["expected"]["operation"],
            "{} move",
            case["name"]
        );
        assert_eq!(
            AgentRepository::get(&db, &AgentId::from(text(&case["agent"], "id")))
                .expect("agent")
                .expect("exists")
                .value(),
            case["expected"]["agent"],
            "{} agent",
            case["name"]
        );
    }
}

#[tokio::test]
async fn location_consumer_checks_shared_outbox_storage_before_decoding() {
    let fixture = Fixture::new();
    let case = &fixture.data["cases"][0];
    let (_path, store) = fixture.store("invalid-delivery-metadata").await;
    fixture.seed(&store, case).await;
    let key = text(&case["outbox"], "id");
    let key_for_write = key.to_owned();
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_outbox SET payload=json_set(payload,'$.record.attempts',-1) WHERE kind='remote_delivery' AND request_id=?1",
                [key_for_write],
            )?;
            Ok(())
        })
        .await
        .expect("invalid storage metadata fixture");
    let before = snapshot(&store);
    assert!(ProjectLocationsRepository::outbox(&store.test_reader().expect("read"), key).is_err());
    assert!(
        complete_location(
            &store,
            key,
            "peer",
            &case["outbox"]["body"],
            &case["receipt"],
            text(&fixture.data, "local"),
            fixture.data["now"].as_f64().expect("clock"),
        )
        .await
        .is_err()
    );
    assert_eq!(
        snapshot(&store),
        before,
        "invalid delivery metadata never writes"
    );
}

#[tokio::test]
async fn location_commit_failpoints_reopen_and_apply_once() {
    let fixture = Fixture::new();
    let case = &fixture.data["cases"][0];
    let now = fixture.data["now"].as_f64().expect("clock");
    let local = text(&fixture.data, "local");
    let outbox = &case["outbox"];
    let key = text(outbox, "id");
    let boundaries = [
        ("request_results", "INSERT"),
        ("runtime_projects", "UPDATE"),
        ("sync_entities", "UPDATE"),
        ("runtime_outbox", "DELETE"),
    ];
    for (index, (table, verb)) in boundaries.into_iter().enumerate() {
        if !full() && ![0, 3].contains(&index) {
            continue;
        }
        for timing in ["BEFORE", "AFTER"] {
            let (path, store) = fixture.store(&format!("fault-{index}-{timing}")).await;
            fixture.seed(&store, case).await;
            let trigger = format!(
                "CREATE TRIGGER relocation_fault {timing} {verb} ON {table} BEGIN SELECT RAISE(ABORT,'relocation failpoint'); END"
            );
            store
                .transaction(move |tx| {
                    tx.execute_batch(&trigger)?;
                    Ok(())
                })
                .await
                .expect("arm failpoint");
            let before = snapshot(&store);
            assert!(
                complete_location(
                    &store,
                    key,
                    "peer",
                    &outbox["body"],
                    &case["receipt"],
                    local,
                    now
                )
                .await
                .is_err(),
                "{table} {verb} {timing}"
            );
            assert_eq!(snapshot(&store), before, "rollback {table} {verb} {timing}");
            drop(store);
            let store = RuntimeStore::open(Some(&path))
                .await
                .expect("restart after failed transaction");
            assert_eq!(snapshot(&store), before);
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER relocation_fault")?;
                    Ok(())
                })
                .await
                .expect("remove injected failure");
            complete_location(
                &store,
                key,
                "peer",
                &outbox["body"],
                &case["receipt"],
                local,
                now,
            )
            .await
            .expect("retry completes");
            let committed = snapshot(&store);
            drop(store);
            let store = RuntimeStore::open(Some(&path))
                .await
                .expect("restart after committed transaction");
            assert_eq!(
                json!(saved_location(&store, case, now + 10.0)),
                case["expected"]["registration"]
            );
            assert_eq!(snapshot(&store), committed);
        }
    }
}

#[tokio::test]
async fn malformed_and_foreign_receipts_refuse_without_writes() {
    let fixture = Fixture::new();
    let case = &fixture.data["cases"][0];
    let (_, store) = fixture.store("refusals").await;
    fixture.seed(&store, case).await;
    let outbox = &case["outbox"];
    let now = fixture.data["now"].as_f64().expect("clock");
    let key = text(outbox, "id");
    let local = text(&fixture.data, "local");
    let before = snapshot(&store);
    for pointer in [
        "/requestId",
        "/value/serverId",
        "/value/homeProjectId",
        "/value/path",
        "/value/requestId",
        "/value/canonicalPath",
        "/value/projectId",
    ] {
        for wrong in [
            Value::Null,
            json!(true),
            json!([]),
            json!({}),
            json!("foreign"),
        ] {
            let mut receipt = case["receipt"].clone();
            *receipt.pointer_mut(pointer).expect("receipt field") = wrong;
            assert!(
                complete_location(&store, key, "peer", &outbox["body"], &receipt, local, now)
                    .await
                    .is_err(),
                "{pointer}"
            );
            assert_eq!(snapshot(&store), before);
        }
    }
    for invalid in [
        "relative",
        "/a/../b",
        "/a/./b",
        "/a//b",
        "/a/",
        "///a",
        "/nul\0path",
    ] {
        let mut receipt = case["receipt"].clone();
        receipt["value"]["canonicalPath"] = json!(invalid);
        receipt["value"]["projectId"] = json!(invalid);
        assert!(
            complete_location(&store, key, "peer", &outbox["body"], &receipt, local, now)
                .await
                .is_err()
        );
        assert_eq!(snapshot(&store), before);
    }
    assert!(
        complete_location(
            &store,
            key,
            "foreign",
            &outbox["body"],
            &case["receipt"],
            local,
            now
        )
        .await
        .is_err()
    );
    let unknown = json!({"requestId":key,"outcome":"unknown"});
    assert_eq!(
        complete_location(&store, key, "peer", &outbox["body"], &unknown, local, now)
            .await
            .expect("unknown preserved"),
        unknown
    );
    assert_eq!(snapshot(&store), before);
    let actor = text(&fixture.data, "actor").to_owned();
    assert!(
        store
            .transaction_with(move |tx| agent_move::start(
                tx,
                &actor,
                &json!({"server":"local","cwd":"/destination"})
            ))
            .await
            .is_err()
    );
    assert_eq!(snapshot(&store), before);
    for field in [
        "id",
        "side",
        "phase",
        "created",
        "agent",
        "epoch",
        "server",
        "result",
        "fingerprint",
    ] {
        let source = fixture.data["cases"]
            .as_array()
            .expect("cases")
            .iter()
            .find(|case| case["family"] == "expire")
            .expect("expiry case");
        let mut raw = source["operation"].clone();
        raw.as_object_mut().expect("object").remove(field);
        assert!(
            agent_move::MoveSnapshot::from_record(&raw).is_err(),
            "missing {field}"
        );
        for wrong in [Value::Null, json!(true), json!([])] {
            let mut raw = source["operation"].clone();
            raw[field] = wrong;
            assert!(
                agent_move::MoveSnapshot::from_record(&raw).is_err(),
                "wrong {field}"
            );
        }
    }
}

// Only machine-generated JSON receipt text is parsed; all other text remains exact.
fn generated_tool_json(mut value: Value) -> Value {
    for pointer in ["/contentItems/0/text", "/result/contentItems/0/text"] {
        if let Some(text) = value.pointer(pointer).and_then(Value::as_str) {
            let decoded = serde_json::from_str(text).expect("generated receipt text");
            *value.pointer_mut(pointer).expect("receipt text exists") = decoded;
        }
    }
    value
}

#[tokio::test]
async fn tool_reconciliation_binds_identity_aliases_and_rolls_back() {
    let fixture = Fixture::new();
    let case = fixture.data["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == "tool-applied")
        .expect("tool case");
    let (path, store) = fixture.store("tool-binding").await;
    fixture.seed(&store, case).await;
    let now = fixture.data["now"].as_f64().expect("clock");
    let record = &case["record"];
    let before = snapshot(&store);
    for field in ["agent", "epoch", "accountKey", "connectionId"] {
        let mut stale = record.clone();
        stale[field] = if field == "epoch" {
            json!(2)
        } else {
            json!("foreign")
        };
        assert!(
            store
                .transaction_with(move |tx| project_locations::refresh_tool_receipt(
                    tx, &mut stale, now
                ))
                .await
                .is_err(),
            "{field}"
        );
        assert_eq!(snapshot(&store), before);
    }
    for (index, (table, verb)) in [
        ("request_results", "INSERT"),
        ("runtime_tool_requests", "UPDATE"),
    ]
    .into_iter()
    .enumerate()
    {
        for timing in ["BEFORE", "AFTER"] {
            if !full() && timing == "BEFORE" {
                continue;
            }
            let mut record = record.clone();
            let trigger = format!(
                "CREATE TRIGGER tool_fault {timing} {verb} ON {table} BEGIN SELECT RAISE(ABORT,'tool fault'); END"
            );
            store
                .transaction(move |tx| {
                    tx.execute_batch(&trigger)?;
                    Ok(())
                })
                .await
                .expect("arm tool failpoint");
            assert!(
                store
                    .transaction_with(move |tx| project_locations::refresh_tool_receipt(
                        tx,
                        &mut record,
                        now
                    ))
                    .await
                    .is_err(),
                "{index} {timing}"
            );
            assert_eq!(snapshot(&store), before);
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER tool_fault")?;
                    Ok(())
                })
                .await
                .expect("disarm tool failpoint");
        }
    }
    drop(store);
    let store = RuntimeStore::open(Some(&path))
        .await
        .expect("reopen failed repairs");
    assert_eq!(snapshot(&store), before);
    let mut repaired = record.clone();
    assert!(
        store
            .transaction_with(move |tx| project_locations::refresh_tool_receipt(
                tx,
                &mut repaired,
                now
            ))
            .await
            .expect("recover")
    );
    let actor = text(record, "agent").to_owned();
    let request = text(record, "id").to_owned();
    let actor_for_write = actor.clone();
    let request_for_write = request.clone();
    let receipt=store.transaction_with(move |tx|{
        tx.execute("UPDATE runtime_tool_requests SET record=json_set(record,'$.aliases',json('[\"alias\"]')) WHERE id=?1",[request_for_write])?;
        project_locations::tool_location_receipt(tx,&actor_for_write,"alias","peer",now)
    }).await.expect("alias resolves").expect("receipt");
    assert_eq!(json!(receipt.request_id), case["outbox"]["id"]);
    let mut second = record.clone();
    second["id"] = json!("second");
    second["aliases"] = json!(["alias"]);
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_tool_requests(id,record) VALUES('second',?1)",
                [second.to_string()],
            )?;
            Ok(())
        })
        .await
        .expect("second alias owner");
    assert!(
        store
            .transaction_with(move |tx| project_locations::tool_location_receipt(
                tx, &actor, "alias", "peer", now
            ))
            .await
            .is_err()
    );
}

const BEFORE_COMMIT_EXIT_CODE: i32 = 81;
const AFTER_COMMIT_EXIT_CODE: i32 = 82;

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn location_process_loss_before_and_after_commit_is_recoverable() {
    let fixture = Fixture::new();
    let case = &fixture.data["cases"][0];
    for committed in [false, true] {
        let (path, store) = fixture.store(&format!("process-loss-{committed}")).await;
        fixture.seed(&store, case).await;
        let before = snapshot(&store);
        drop(store);
        let input = json!({"case":case,"local":fixture.data["local"],"now":fixture.data["now"],"committed":committed});
        fs::write(path.join("command.json"), input.to_string()).expect("immutable command fixture");
        let status = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "relocation_corpus::relocation_process_stop_child",
                "--ignored",
                "--nocapture",
            ])
            .env("RELOCATION_CRASH_ROOT", &path)
            .status()
            .expect("crash fixture starts");
        assert_eq!(
            status.code(),
            Some(if committed {
                AFTER_COMMIT_EXIT_CODE
            } else {
                BEFORE_COMMIT_EXIT_CODE
            }),
            "child reaches the selected crash cut"
        );
        let store = RuntimeStore::open(Some(&path))
            .await
            .expect("recover process-owned journal");
        if !committed {
            assert_eq!(snapshot(&store), before);
        }
        let committed_state = snapshot(&store);
        if committed {
            assert_ne!(committed_state, before);
            assert_eq!(
                project_domain(
                    ProjectsRepository::get(&store.test_reader().expect("read"), "/home-project")
                        .expect("project")
                        .expect("exists")
                        .1
                ),
                case["expected"]["project"]
            );
            assert!(
                !committed_state["request_results"]
                    .as_array()
                    .expect("receipts")
                    .is_empty()
            );
        }
        let outbox = &case["outbox"];
        if committed {
            assert_eq!(
                json!(saved_location(
                    &store,
                    case,
                    fixture.data["now"].as_f64().expect("clock")
                )),
                case["expected"]["registration"]
            );
        } else {
            complete_location(
                &store,
                text(outbox, "id"),
                "peer",
                &outbox["body"],
                &case["receipt"],
                text(&fixture.data, "local"),
                fixture.data["now"].as_f64().expect("clock"),
            )
            .await
            .expect("recover same request");
        }
        if committed {
            assert_eq!(
                snapshot(&store),
                committed_state,
                "replay preserves the committed snapshot"
            );
        }
        assert_eq!(
            project_domain(
                ProjectsRepository::get(&store.test_reader().expect("read"), "/home-project")
                    .expect("read project")
                    .expect("exists")
                    .1
            ),
            case["expected"]["project"]
        );
    }
}

#[tokio::test]
#[ignore = "explicit subprocess crash entry"]
async fn relocation_process_stop_child() {
    let Some(path) = std::env::var_os("RELOCATION_CRASH_ROOT") else {
        return;
    };
    let path = PathBuf::from(path);
    let input: Value =
        serde_json::from_slice(&fs::read(path.join("command.json")).expect("command"))
            .expect("input");
    let store = RuntimeStore::open(Some(&path))
        .await
        .expect("child owns state");
    let child_input = input.clone();
    store
        .transaction_with(move |tx| {
            let case = &child_input["case"];
            let outbox = &case["outbox"];
            complete_location_in_fixture(
                tx,
                text(outbox, "id"),
                "peer",
                &outbox["body"],
                &case["receipt"],
                text(&child_input, "local"),
                child_input["now"].as_f64().expect("clock"),
            )?;
            if child_input["committed"] == false {
                std::process::exit(BEFORE_COMMIT_EXIT_CODE);
            }
            Ok::<(), studio_runtime_services::project_locations::LocationError>(())
        })
        .await
        .expect("commit before process loss");
    std::process::exit(AFTER_COMMIT_EXIT_CODE);
}

#[tokio::test]
async fn move_expiry_and_cache_failpoints_reopen_without_partial_effects() {
    let fixture = Fixture::new();
    let now = fixture.data["now"].as_f64().expect("clock");
    for name in ["expire-importing", "cache-first-observation"] {
        let case = fixture.data["cases"]
            .as_array()
            .expect("cases")
            .iter()
            .find(|c| c["name"] == name)
            .expect("case");
        let boundaries = if name == "expire-importing" {
            vec![
                ("runtime_agent_moves", "UPDATE"),
                ("runtime_agents", "UPDATE"),
                ("sync_entities", "INSERT"),
            ]
        } else {
            vec![("runtime_agent_moves", "UPDATE")]
        };
        for (index, (table, verb)) in boundaries.into_iter().enumerate() {
            if !full() && index != 0 {
                continue;
            }
            for timing in ["BEFORE", "AFTER"] {
                if !full() && timing == "BEFORE" {
                    continue;
                }
                let (path, store) = fixture.store(&format!("{name}-{index}-{timing}")).await;
                fixture.seed(&store, case).await;
                let trigger = format!(
                    "CREATE TRIGGER move_fault {timing} {verb} ON {table} BEGIN SELECT RAISE(ABORT,'move failpoint'); END"
                );
                store
                    .transaction(move |tx| {
                        tx.execute_batch(&trigger)?;
                        Ok(())
                    })
                    .await
                    .expect("arm failpoint");
                let before = snapshot(&store);
                assert!(
                    apply_move_case(&store, name, case, now).await.is_err(),
                    "{name} {table} {timing}"
                );
                assert_eq!(snapshot(&store), before);
                drop(store);
                let store = RuntimeStore::open(Some(&path))
                    .await
                    .expect("restart before retry");
                assert_eq!(snapshot(&store), before);
                store
                    .transaction(|tx| {
                        tx.execute_batch("DROP TRIGGER move_fault")?;
                        Ok(())
                    })
                    .await
                    .expect("remove failpoint");
                assert!(
                    apply_move_case(&store, name, case, now)
                        .await
                        .expect("retry")
                );
                let committed = snapshot(&store);
                drop(store);
                let store = RuntimeStore::open(Some(&path))
                    .await
                    .expect("restart after commit");
                assert!(
                    !apply_move_case(&store, name, case, now)
                        .await
                        .expect("replay")
                );
                assert_eq!(snapshot(&store), committed);
            }
        }
        if name == "expire-importing" {
            let (_, store) = fixture.store("stale-move-epoch").await;
            fixture.seed(&store, case).await;
            let mut changed_agent = case["agent"].clone();
            changed_agent["compactions"] = json!(0);
            changed_agent["epoch"] = json!(2);
            store
                .transaction(move |tx| {
                    AgentRepository::put(
                        tx,
                        &AgentRecord::from_value(changed_agent).expect("agent"),
                    )?;
                    Ok(())
                })
                .await
                .expect("advance epoch");
            let before = snapshot(&store);
            assert!(
                agent_move::expire(&store, text(&case["operation"], "id"), now)
                    .await
                    .is_err()
            );
            assert_eq!(snapshot(&store), before);
        }
    }
}
