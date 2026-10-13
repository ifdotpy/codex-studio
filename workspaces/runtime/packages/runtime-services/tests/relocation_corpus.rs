//! Python-produced snapshots exercised through the transaction consumer interfaces.
use rusqlite::params;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};
use studio_runtime_services::{agent_move, project_locations, relocation::identity};
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
        let python = studio_runtime_services::agents_create::prepared_python();
        let output = Command::new(python)
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/relocation/oracle.py"))
            .arg(&template)
            .output()
            .expect("isolated oracle starts");
        assert!(
            output.status.success(),
            "oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let data = serde_json::from_slice(&output.stdout).expect("complete Python oracle output");
        Self {
            root,
            template,
            data,
        }
    }
    fn store(&self, name: &str) -> (PathBuf, RuntimeStore) {
        let path = self.root.path().join(name);
        fs::create_dir(&path).expect("case directory");
        for filename in ["canvas.sqlite3", "analytics.sqlite3"] {
            fs::copy(self.template.join(filename), path.join(filename))
                .expect("clone prepared template");
        }
        let store = RuntimeStore::open(Some(&path)).expect("open Python-written schema");
        (path, store)
    }
    fn seed(&self, store: &RuntimeStore, case: &Value) {
        store.transaction(|tx|{
            if case["family"]=="location" {
                if !case["project"].is_null() {ProjectsRepository::put_project(tx,&case["project"])?;}
                let outbox=&case["outbox"];
                tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,result) VALUES(?1,?2,?3,?4,?5)",params![text(outbox,"id"),text(outbox,"server"),outbox["body"].to_string(),text(outbox,"state"),outbox.get("result").filter(|v|!v.is_null()).map(Value::to_string)])?;
                ProjectsRepository::save_receipt(tx,"project-registration:/home-project","registration",&json!({"outcome":"unknown","projectId":"/home-project","requestId":outbox["id"]}))?;
            } else if case["family"] == "tool" {
                let record=&case["record"];let outbox=&case["outbox"];
                tx.execute("INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",params![text(record,"id"),record.to_string()])?;
                tx.execute("INSERT INTO runtime_tool_results(id,result) VALUES(?1,?2)",params![text(record,"id"),case["previous"].to_string()])?;
                tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,result) VALUES(?1,?2,?3,?4,?5)",params![text(outbox,"id"),text(outbox,"server"),outbox["body"].to_string(),text(outbox,"state"),outbox["result"].to_string()])?;
            } else {
                AgentMoveRepository::save(tx,&case["operation"])?;
                AgentRepository::put(tx,&AgentRecord::from_value(case["agent"].clone()).expect("Python agent"))?;
            }
            Ok(())
        }).expect("seed Python records without rebuilding snapshots");
    }
}
fn text<'a>(row: &'a Value, key: &str) -> &'a str {
    row[key].as_str().expect("fixture string")
}
fn full() -> bool {
    std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some()
}
fn snapshot(store: &RuntimeStore) -> Value {
    let db = store.read().expect("read state");
    let mut result = serde_json::Map::new();
    for table in [
        "runtime_projects",
        "runtime_operation_receipts",
        "runtime_server_outbox",
        "runtime_agent_moves",
        "runtime_agents",
        "runtime_tool_requests",
        "runtime_tool_results",
        "sync_entities",
        "sync_versions",
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

#[test]
fn python_location_and_move_corpus_through_consumers() {
    let fixture = Fixture::new();
    let now = fixture.data["now"].as_f64().expect("clock");
    let local = text(&fixture.data, "local");
    for case in fixture.data["cases"].as_array().expect("cases") {
        // Full matrix remains opt-in; fast witnesses cover each semantic family.
        if !full()
            && ![
                "add-changed-location",
                "remove-location",
                "release-waiting-not-started",
                "hold-exported-active",
                "expire-importing",
                "cache-first-observation",
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
        let (path, store) = fixture.store(text(case, "name"));
        fixture.seed(&store, case);
        let family = text(case, "family");
        let before = snapshot(&store);
        match family {
            "location" => {
                let outbox = &case["outbox"];
                let key = text(outbox, "id");
                let value = project_locations::complete(
                    &store,
                    key,
                    "peer",
                    &outbox["body"],
                    &case["receipt"],
                    local,
                    now,
                )
                .expect("location applies");
                assert_eq!(
                    value, case["expected"]["receipt"],
                    "{} receipt",
                    case["name"]
                );
                let db = store.read().expect("read");
                assert_eq!(
                    ProjectsRepository::get(&db, "/home-project")
                        .expect("project")
                        .map(|(_, v)| v)
                        .unwrap_or(Value::Null),
                    case["expected"]["project"],
                    "{} project",
                    case["name"]
                );
                assert_eq!(
                    ProjectsRepository::receipt(&db, "project-registration:/home-project")
                        .expect("registration")
                        .expect("exists")
                        .1,
                    case["expected"]["registration"]
                );
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
                let reopened = RuntimeStore::open(Some(&path)).expect("restart");
                assert_eq!(
                    project_locations::complete(
                        &reopened,
                        key,
                        "peer",
                        &outbox["body"],
                        &case["receipt"],
                        local,
                        now + 1.0
                    )
                    .expect("lost reply replay"),
                    value
                );
                assert_eq!(snapshot(&reopened), committed, "replay writes nothing");
                continue;
            }
            "tool" => {
                let mut record = case["record"].clone();
                let changed = store
                    .transaction_with(|tx| {
                        project_locations::refresh_tool_receipt(tx, &mut record, now)
                    })
                    .expect("repair committed location receipt");
                assert_eq!(changed, case["changed"].as_bool().expect("changed"));
                let db = store.read().expect("read");
                let stored = ProjectLocationsRepository::tool(&db, text(&record, "id"))
                    .expect("stored request")
                    .expect("exists");
                let result = ProjectLocationsRepository::tool_result(&db, text(&record, "id"))
                    .expect("stored result")
                    .expect("exists");
                assert_eq!(
                    generated_tool_json(stored),
                    generated_tool_json(case["expected"]["record"].clone())
                );
                assert_eq!(
                    generated_tool_json(result),
                    generated_tool_json(case["expected"]["result"].clone())
                );
                drop(db);
                let committed = snapshot(&store);
                drop(store);
                let store = RuntimeStore::open(Some(&path)).expect("restart");
                assert!(
                    !store
                        .transaction_with(|tx| project_locations::refresh_tool_receipt(
                            tx,
                            &mut record,
                            now + 1.0
                        ))
                        .expect("already repaired")
                );
                assert_eq!(snapshot(&store), committed);
                continue;
            }
            "release" => {
                let operation = &case["operation"];
                let key = text(operation, "id");
                let ticket = identity(&[key, "status", "50"]);
                store.transaction(|tx|{tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,result) VALUES(?1,'peer',?2,'complete',?3)",params![ticket,json!({"requestId":ticket,"action":"move_status","payload":{"move":key,"reconciliationGeneration":agent_move::MoveSnapshot::from_record(operation).expect("checked source").reconciliation_generation()}}).to_string(),json!({"requestId":ticket,"outcome":"applied","value":case["targetReceipt"]}).to_string()])?;Ok(())}).expect("saved status proof");
                assert_eq!(
                    agent_move::release_untransferred(&store, key, &ticket, now)
                        .expect("reconcile"),
                    case["changed"].as_bool().expect("changed")
                );
            }
            "expire" => {
                assert_eq!(
                    agent_move::expire(&store, text(&case["operation"], "id"), now)
                        .expect("expire"),
                    case["changed"].as_bool().expect("changed")
                );
            }
            "cache" => {
                store
                    .transaction_with(|tx| {
                        agent_move::capture_cache(
                            tx,
                            text(&case["agent"], "id"),
                            &case["notification"],
                            now,
                        )
                    })
                    .expect("cache notification");
            }
            "status" => {
                let value = store
                    .transaction_with(|tx| {
                        agent_move::status(
                            tx,
                            text(&fixture.data, "actor"),
                            text(case, "name"),
                            local,
                        )
                    })
                    .expect("status");
                assert_eq!(value, case["status"]);
                assert_eq!(snapshot(&store), before);
                continue;
            }
            _ => panic!("unknown fixture family"),
        }
        let db = store.read().expect("read");
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

#[test]
fn location_commit_failpoints_reopen_and_apply_once() {
    let fixture = Fixture::new();
    let case = &fixture.data["cases"][0];
    let now = fixture.data["now"].as_f64().expect("clock");
    let local = text(&fixture.data, "local");
    let outbox = &case["outbox"];
    let key = text(outbox, "id");
    let boundaries = [
        ("runtime_operation_receipts", "INSERT"),
        ("runtime_operation_receipts", "UPDATE"),
        ("runtime_projects", "UPDATE"),
        ("sync_entities", "UPDATE"),
        ("runtime_server_outbox", "UPDATE"),
    ];
    for (index, (table, verb)) in boundaries.into_iter().enumerate() {
        if !full() && ![0, 4].contains(&index) {
            continue;
        }
        for timing in ["BEFORE", "AFTER"] {
            let (path, store) = fixture.store(&format!("fault-{index}-{timing}"));
            fixture.seed(&store, case);
            store.transaction(|tx|{tx.execute_batch(&format!("CREATE TRIGGER relocation_fault {timing} {verb} ON {table} BEGIN SELECT RAISE(ABORT,'relocation failpoint'); END"))?;Ok(())}).expect("arm failpoint");
            let before = snapshot(&store);
            assert!(
                project_locations::complete(
                    &store,
                    key,
                    "peer",
                    &outbox["body"],
                    &case["receipt"],
                    local,
                    now
                )
                .is_err(),
                "{table} {verb} {timing}"
            );
            assert_eq!(snapshot(&store), before, "rollback {table} {verb} {timing}");
            drop(store);
            let store = RuntimeStore::open(Some(&path)).expect("restart after failed transaction");
            assert_eq!(snapshot(&store), before);
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER relocation_fault")?;
                    Ok(())
                })
                .expect("remove injected failure");
            project_locations::complete(
                &store,
                key,
                "peer",
                &outbox["body"],
                &case["receipt"],
                local,
                now,
            )
            .expect("retry completes");
            let committed = snapshot(&store);
            drop(store);
            let store =
                RuntimeStore::open(Some(&path)).expect("restart after committed transaction");
            project_locations::complete(
                &store,
                key,
                "peer",
                &outbox["body"],
                &case["receipt"],
                local,
                now + 10.0,
            )
            .expect("reply lost replay");
            assert_eq!(snapshot(&store), committed);
        }
    }
}

#[test]
fn malformed_and_foreign_receipts_refuse_without_writes() {
    let fixture = Fixture::new();
    let case = &fixture.data["cases"][0];
    let (_, store) = fixture.store("refusals");
    fixture.seed(&store, case);
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
                project_locations::complete(
                    &store,
                    key,
                    "peer",
                    &outbox["body"],
                    &receipt,
                    local,
                    now
                )
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
            project_locations::complete(&store, key, "peer", &outbox["body"], &receipt, local, now)
                .is_err()
        );
        assert_eq!(snapshot(&store), before);
    }
    assert!(
        project_locations::complete(
            &store,
            key,
            "foreign",
            &outbox["body"],
            &case["receipt"],
            local,
            now
        )
        .is_err()
    );
    let unknown = json!({"requestId":key,"outcome":"unknown"});
    assert_eq!(
        project_locations::complete(&store, key, "peer", &outbox["body"], &unknown, local, now)
            .expect("unknown preserved"),
        unknown
    );
    assert_eq!(snapshot(&store), before);
    assert!(
        store
            .transaction_with(|tx| agent_move::start(
                tx,
                text(&fixture.data, "actor"),
                &json!({"server":"local","cwd":"/destination"})
            ))
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

#[test]
fn tool_reconciliation_binds_identity_aliases_and_rolls_back() {
    let fixture = Fixture::new();
    let case = fixture.data["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == "tool-applied")
        .expect("tool case");
    let (path, store) = fixture.store("tool-binding");
    fixture.seed(&store, case);
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
                .transaction_with(|tx| project_locations::refresh_tool_receipt(tx, &mut stale, now))
                .is_err(),
            "{field}"
        );
        assert_eq!(snapshot(&store), before);
    }
    for (index, table) in ["runtime_tool_results", "runtime_tool_requests"]
        .into_iter()
        .enumerate()
    {
        for timing in ["BEFORE", "AFTER"] {
            if !full() && timing == "BEFORE" {
                continue;
            }
            let mut record = record.clone();
            store.transaction(|tx|{tx.execute_batch(&format!("CREATE TRIGGER tool_fault {timing} UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'tool fault'); END"))?;Ok(())}).expect("arm tool failpoint");
            assert!(
                store
                    .transaction_with(|tx| project_locations::refresh_tool_receipt(
                        tx,
                        &mut record,
                        now
                    ))
                    .is_err(),
                "{index} {timing}"
            );
            assert_eq!(snapshot(&store), before);
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER tool_fault")?;
                    Ok(())
                })
                .expect("disarm tool failpoint");
        }
    }
    drop(store);
    let store = RuntimeStore::open(Some(&path)).expect("reopen failed repairs");
    assert_eq!(snapshot(&store), before);
    let mut repaired = record.clone();
    assert!(
        store
            .transaction_with(|tx| project_locations::refresh_tool_receipt(tx, &mut repaired, now))
            .expect("recover")
    );
    let actor = text(record, "agent");
    let request = text(record, "id");
    let key=store.transaction_with(|tx|{
        tx.execute("INSERT INTO runtime_tool_request_aliases(agent,alias,request) VALUES(?1,'alias',?2)",params![actor,request])?;
        project_locations::tool_receipt_key(tx,actor,"alias","peer")
    }).expect("alias resolves");
    assert_eq!(key, case["outbox"]["id"]);
    store.transaction(|tx|{
        tx.execute("INSERT INTO runtime_tool_request_aliases(agent,alias,request) VALUES(?1,'alias','second')",[actor])?;
        tx.execute("INSERT INTO runtime_server_retired(id,direction,signature,server) VALUES(?1,'out','signature','peer')",[identity(&["project-location",actor,"second"])])?;Ok(())
    }).expect("second durable alias receipt");
    assert!(
        store
            .transaction_with(|tx| project_locations::tool_receipt_key(tx, actor, "alias", "peer"))
            .is_err()
    );
}

#[test]
fn location_process_loss_before_and_after_commit_is_recoverable() {
    let fixture = Fixture::new();
    let case = &fixture.data["cases"][0];
    for committed in [false, true] {
        let (path, store) = fixture.store(&format!("process-loss-{committed}"));
        fixture.seed(&store, case);
        let before = snapshot(&store);
        drop(store);
        let input = json!({"case":case,"local":fixture.data["local"],"now":fixture.data["now"],"committed":committed});
        fs::write(path.join("command.json"), input.to_string()).expect("immutable command fixture");
        let status = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "relocation_process_stop_child", "--nocapture"])
            .env("RELOCATION_CRASH_ROOT", &path)
            .status()
            .expect("crash fixture starts");
        assert!(status.success());
        let store = RuntimeStore::open(Some(&path)).expect("recover process-owned journal");
        if !committed {
            assert_eq!(snapshot(&store), before);
        }
        let outbox = &case["outbox"];
        project_locations::complete(
            &store,
            text(outbox, "id"),
            "peer",
            &outbox["body"],
            &case["receipt"],
            text(&fixture.data, "local"),
            fixture.data["now"].as_f64().expect("clock"),
        )
        .expect("recover same request");
        assert_eq!(
            ProjectsRepository::get(&store.read().expect("read"), "/home-project")
                .expect("read project")
                .expect("exists")
                .1,
            case["expected"]["project"]
        );
    }
}

#[test]
fn relocation_process_stop_child() {
    let Some(path) = std::env::var_os("RELOCATION_CRASH_ROOT") else {
        return;
    };
    let path = PathBuf::from(path);
    let input: Value =
        serde_json::from_slice(&fs::read(path.join("command.json")).expect("command"))
            .expect("input");
    let store = RuntimeStore::open(Some(&path)).expect("child owns state");
    let case = &input["case"];
    let outbox = &case["outbox"];
    store
        .transaction_with(|tx| {
            project_locations::complete_in_transaction(
                tx,
                text(outbox, "id"),
                "peer",
                &outbox["body"],
                &case["receipt"],
                text(&input, "local"),
                input["now"].as_f64().expect("clock"),
            )?;
            if input["committed"] == false {
                std::process::exit(0);
            }
            Ok::<(), studio_runtime_services::project_locations::LocationError>(())
        })
        .expect("commit before process loss");
    std::process::exit(0);
}

#[test]
fn oracle_guard_rejects_content_and_metadata_before_io() {
    let output = Command::new(studio_runtime_services::agents_create::prepared_python())
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/relocation/oracle.py"))
        .arg("--guard-self-test")
        .output()
        .expect("guard self-test starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn move_expiry_and_cache_failpoints_reopen_without_partial_effects() {
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
                let (path, store) = fixture.store(&format!("{name}-{index}-{timing}"));
                fixture.seed(&store, case);
                store.transaction(|tx| {tx.execute_batch(&format!("CREATE TRIGGER move_fault {timing} {verb} ON {table} BEGIN SELECT RAISE(ABORT,'move failpoint'); END"))?; Ok(())}).expect("arm failpoint");
                let before = snapshot(&store);
                let apply = |store: &RuntimeStore| {
                    if name == "expire-importing" {
                        agent_move::expire(store, text(&case["operation"], "id"), now)
                    } else {
                        store.transaction_with(|tx| {
                            agent_move::capture_cache(
                                tx,
                                text(&case["agent"], "id"),
                                &case["notification"],
                                now,
                            )
                        })
                    }
                };
                assert!(apply(&store).is_err(), "{name} {table} {timing}");
                assert_eq!(snapshot(&store), before);
                drop(store);
                let store = RuntimeStore::open(Some(&path)).expect("restart before retry");
                assert_eq!(snapshot(&store), before);
                store
                    .transaction(|tx| {
                        tx.execute_batch("DROP TRIGGER move_fault")?;
                        Ok(())
                    })
                    .expect("remove failpoint");
                assert!(apply(&store).expect("retry"));
                let committed = snapshot(&store);
                drop(store);
                let store = RuntimeStore::open(Some(&path)).expect("restart after commit");
                assert!(!apply(&store).expect("replay"));
                assert_eq!(snapshot(&store), committed);
            }
        }
        if name == "expire-importing" {
            let (_, store) = fixture.store("stale-move-epoch");
            fixture.seed(&store, case);
            store
                .transaction(|tx| {
                    let mut row = case["agent"].clone();
                    row["epoch"] = json!(2);
                    AgentRepository::put(tx, &AgentRecord::from_value(row).expect("agent"))?;
                    Ok(())
                })
                .expect("advance epoch");
            let before = snapshot(&store);
            assert!(agent_move::expire(&store, text(&case["operation"], "id"), now).is_err());
            assert_eq!(snapshot(&store), before);
        }
    }
}

#[test]
fn move_status_ticket_rejects_stale_generation_and_replay() {
    let fixture = Fixture::new();
    let case = fixture.data["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == "release-waiting-not-started")
        .expect("release case");
    let (path, store) = fixture.store("status-generation");
    fixture.seed(&store, case);
    let now = fixture.data["now"].as_f64().expect("clock");
    let key = text(&case["operation"], "id");
    let old_generation = agent_move::MoveSnapshot::from_record(&case["operation"])
        .expect("Python-written source")
        .reconciliation_generation();
    let save_ticket = |store: &RuntimeStore, ticket: &str, generation: Value| {
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,result) VALUES(?1,'peer',?2,'complete',?3)",
                params![ticket, json!({"requestId":ticket,"action":"move_status","payload":{"move":key,"reconciliationGeneration":generation}}).to_string(),
                    json!({"requestId":ticket,"outcome":"applied","value":case["targetReceipt"]}).to_string()])?;
            Ok(())
        }).expect("save durable status ticket");
    };
    save_ticket(&store, "old-ticket", json!(old_generation));
    // A newer source status arrived after this target observation. Both states
    // could release on not_started, so only generation binding refuses the old proof.
    let mut current = case["operation"].clone();
    current["uncertainPhase"] = json!("exported");
    store
        .transaction(|tx| AgentMoveRepository::save(tx, &current))
        .expect("advance source status");
    let before = snapshot(&store);
    assert!(agent_move::release_untransferred(&store, key, "old-ticket", now).is_err());
    assert_eq!(snapshot(&store), before);
    drop(store);
    let store = RuntimeStore::open(Some(&path)).expect("restart preserves source generation");
    assert!(agent_move::release_untransferred(&store, key, "old-ticket", now).is_err());
    assert_eq!(snapshot(&store), before);
    let generation = agent_move::MoveSnapshot::from_record(&current)
        .expect("current snapshot")
        .reconciliation_generation();
    assert_ne!(generation, old_generation);
    for (index, malformed) in [Value::Null, json!(true), json!(0), json!({})]
        .into_iter()
        .enumerate()
    {
        let ticket = format!("malformed-{index}");
        save_ticket(&store, &ticket, malformed);
        let before = snapshot(&store);
        assert!(agent_move::release_untransferred(&store, key, &ticket, now).is_err());
        assert_eq!(snapshot(&store), before);
    }
    save_ticket(&store, "current-ticket", json!(generation));
    assert!(
        agent_move::release_untransferred(&store, key, "current-ticket", now)
            .expect("fresh proof releases")
    );
    let committed = snapshot(&store);
    drop(store);
    let store = RuntimeStore::open(Some(&path)).expect("restart after release");
    assert!(agent_move::release_untransferred(&store, key, "current-ticket", now + 1.0).is_err());
    assert_eq!(
        snapshot(&store),
        committed,
        "replayed proof cannot write twice"
    );
}
