use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, MessageRepository, QueueEventId, QueueEventRepository, RuntimeLock,
    RuntimeStore, StoreError, Table, TaskId, TaskRepository, WorkItemRecord,
};

const CREATE_PYTHON_STATE: &str = r#"
import json, pathlib, sqlite3, sys
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agent = runtime.create({"name":"Rust store fixture", "cwd":str(root), "prompt":"temporary"}, defer=True)
agent["fixtureFlag"] = True
agent["fixtureInteger"] = 7
agent["fixtureUnicode"] = "Zażółć 東京\n\u0000"
agent["fixtureNested"] = {"items":[0.1,5.0,9007199254740993,True,{"text":"🦀\t"}]}
with runtime.lock, runtime.db() as db:
    runtime.put(db, "agents", agent)
    task = {"id":"cross-task", "agent":agent["id"], "kind":"command", "status":"completed",
            "created":1e-7, "interactive":True, "exitCode":7}
    runtime.put(db, "tasks", task)
    work = {"id":"cross-work", "rootId":agent["id"], "status":"ready", "title":"fixture",
            "description":"", "created":1e-7, "updated":1e21, "version":1,
            "description":"", "created":1e-7, "updated":1e21, "version":1,
            "futureField":{"fixtureFlag":True}, "fixtureFlag":True, "fixtureInteger":7}
    runtime.put(db, "work", work)
    runtime.enqueue(db, agent, "work_review", json.dumps({"fixture": True}), key="cross-event")
    db.execute("UPDATE runtime_events SET created=1720000000.5 WHERE id='cross-event'")
    db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",
               ("cross-message", "room-fixture", agent["id"], "Python message", 1720000000.5,
                json.dumps({"delivery": True, "attempt": 1})))
    numeric_events = [
        ("numeric-created-precision", 1791788985.5209818),
        ("numeric-created-tenth", 0.1),
        ("numeric-created-large-exponent", 1e21),
        ("numeric-created-integer", 7),
        ("numeric-created-integer-real", 5.0),
        ("numeric-created-large-integer-real", 1791788985.0),
        ("numeric-created-17-digits", 1.2345678901234567),
        ("numeric-created-over-2pow53", 9007199254740993),
        ("numeric-created-negative", -0.1),
        ("numeric-created-negative-exponent", -1e-7),
    ]
    for event_id, created in numeric_events:
        db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) "
                   "VALUES(?,?,?,?,'pending',?,0,NULL,?)",
                   (event_id, agent["id"], "work_review", "{}", created,
                    "Zażółć 東京\n\u0000" if event_id == "numeric-created-precision" else None))
agent_id = agent["id"]
runtime.close()
print(json.dumps({"agent_id": agent_id}))
"#;

const CREATE_FEATURE_STATE: &str = r#"
import json, pathlib, sys
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agent = runtime.create({"name":"feature fixture", "cwd":str(root), "prompt":"temporary"}, defer=True)
agent_id = agent["id"]
with runtime.lock, runtime.db() as db:
    runtime.put(db, "agents", agent)
    runtime.put(db, "tasks", {"id":"feature-task", "agent":agent_id, "kind":"command",
        "status":"completed", "created":1.0, "interactive":False, "exitCode":0})
    runtime.put(db, "work", {"id":"feature-work", "rootId":agent_id, "status":"ready",
        "title":"feature", "created":1.0, "updated":2.0, "version":1,
        "futureFeatureField":{"enabled":True}})
from codex_voice import VoiceStore
voice = VoiceStore(runtime)
from codex_canvas import Canvas
canvas = Canvas(root)
from codex_sync import SyncStore
sync = SyncStore(lambda: runtime.db(), lambda _agent: [], runtime=runtime, canvas=canvas)
sync.generation()
from codex_terminals import TerminalManager
terminals = TerminalManager(root)
from codex_multi_server import MultiServerService
with runtime.db() as db:
    MultiServerService.ensure_tables(db)
from codex_server_discovery import ServerDiscovery
from types import SimpleNamespace
discovery = ServerDiscovery(SimpleNamespace(runtime=runtime))
# These tables and indexes are created on first use in production. Install
# them through Python in this fixture so the catalog comparison covers them.
with runtime.db() as db:
    db.execute('CREATE TABLE IF NOT EXISTS runtime_model_status (agent TEXT, revision TEXT, at REAL, record TEXT, PRIMARY KEY(agent,revision))')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_model_modes (agent TEXT, request TEXT, version TEXT, record TEXT, PRIMARY KEY(agent,request,version))')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_native_notices (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_native_catalog_updates (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_native_limit_refreshes (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    db.execute("CREATE TABLE IF NOT EXISTS runtime_payload_migrations (name TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL DEFAULT 'pending',updated REAL,error TEXT)")
    from codex_native_sweep import _account_busy
    _account_busy(runtime, db, 'default')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_native_action_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, receipt TEXT NOT NULL, outcome TEXT NOT NULL)')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_safety_retries (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_context_terminal_receipts (id TEXT PRIMARY KEY, agent TEXT NOT NULL, operation TEXT NOT NULL, record TEXT NOT NULL)')
    db.execute('CREATE TABLE IF NOT EXISTS runtime_account_transfers (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
    db.execute("CREATE INDEX IF NOT EXISTS runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'))")
    db.executescript('''
        CREATE TABLE IF NOT EXISTS runtime_limit_reset_attempts (id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_limit_reset_requests (id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_server_outbox (
            id TEXT PRIMARY KEY, server TEXT NOT NULL, body TEXT NOT NULL,
            state TEXT NOT NULL, result TEXT, attempts INTEGER NOT NULL DEFAULT 0,
            next_at REAL NOT NULL DEFAULT 0, error TEXT);
        CREATE INDEX IF NOT EXISTS runtime_server_outbox_due
            ON runtime_server_outbox(state,next_at);
        CREATE TABLE IF NOT EXISTS runtime_server_inbox (
            id TEXT PRIMARY KEY, signature TEXT NOT NULL, state TEXT NOT NULL,
            result TEXT);
        CREATE TABLE IF NOT EXISTS runtime_server_links (
            id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS runtime_server_links_home_parent
            ON runtime_server_links(json_extract(record,'$.parent'))
            WHERE json_extract(record,'$.side')='home';
        CREATE TABLE IF NOT EXISTS runtime_server_fetch (
            id TEXT PRIMARY KEY, server TEXT NOT NULL, signature TEXT NOT NULL,
            state TEXT NOT NULL, result TEXT);
        CREATE TABLE IF NOT EXISTS runtime_server_retired (
            id TEXT NOT NULL, direction TEXT NOT NULL, signature TEXT NOT NULL,
            server TEXT, PRIMARY KEY(id,direction));
        CREATE TABLE IF NOT EXISTS runtime_server_output_tools (
            id TEXT PRIMARY KEY, expires REAL NOT NULL);
        CREATE INDEX IF NOT EXISTS runtime_server_output_tools_expiry
            ON runtime_server_output_tools(expires);
        CREATE TABLE IF NOT EXISTS runtime_server_sequence (
            id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
        INSERT OR IGNORE INTO runtime_server_sequence VALUES(1,0);
    ''')
    for table in ('outbox', 'inbox'):
        columns = {row[1] for row in db.execute('PRAGMA table_info(runtime_server_' + table + ')')}
        for column in ('created_at', 'completed_at'):
            if column not in columns:
                db.execute('ALTER TABLE runtime_server_' + table + ' ADD COLUMN ' + column + ' REAL NOT NULL DEFAULT 0')
    for table, column, declaration in (
        ('outbox', 'fingerprint', 'TEXT'), ('retired', 'actor', 'TEXT'),
        ('inbox', 'principal', 'TEXT'), ('inbox', 'actor', 'TEXT'),
    ):
        columns = {row[1] for row in db.execute('PRAGMA table_info(runtime_server_' + table + ')')}
        if column not in columns:
            db.execute('ALTER TABLE runtime_server_' + table + ' ADD COLUMN ' + column + ' ' + declaration)
    db.executescript('''
        CREATE TABLE IF NOT EXISTS runtime_server_exec (
            id TEXT PRIMARY KEY, owner TEXT NOT NULL, actor TEXT NOT NULL,
            record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_server_exec_audit (
            id TEXT PRIMARY KEY, actor TEXT NOT NULL, source_server TEXT NOT NULL,
            server TEXT NOT NULL, cwd TEXT NOT NULL, argv_hash TEXT NOT NULL,
            start REAL NOT NULL, end REAL, exit_code INTEGER, signal INTEGER);
        CREATE TABLE IF NOT EXISTS runtime_server_exec_input (
            id TEXT PRIMARY KEY, handle TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS runtime_server_exec_input_handle
            ON runtime_server_exec_input(handle);
    ''')
# Exercise the real migration routine without making the test depend on the
# runner's free-space report. This empty disposable state has no payload to copy.
import codex_work
codex_work.shutil.disk_usage = lambda _path: SimpleNamespace(free=32 * 1024**3)
runtime._search_migration_run()
with runtime.db() as db:
    phase = db.execute("SELECT phase FROM runtime_search_rollout WHERE id=1").fetchone()[0]
    if phase != "complete":
        raise AssertionError(f"search migration phase is {phase}")
terminals.close()
discovery.close()
canvas = None
runtime.close()
print(json.dumps({"agent_id":agent_id,"search_phase":phase}))
"#;

const COPY_AND_REMOVE_BOOTSTRAP_TABLES: &str = r#"
import pathlib, sqlite3, sys
source = pathlib.Path(sys.argv[1])
target = pathlib.Path(sys.argv[3])
target.mkdir(parents=True, exist_ok=True)
with sqlite3.connect(source / "canvas.sqlite3") as original, sqlite3.connect(target / "canvas.sqlite3") as copy:
    original.backup(copy)
with sqlite3.connect(target / "analytics.sqlite3") as copy:
    with sqlite3.connect(source / "analytics.sqlite3") as original:
        original.backup(copy)
with sqlite3.connect(target / "canvas.sqlite3") as db:
    for table in (
        "runtime_work", "runtime_plans", "runtime_annotations", "runtime_operation_receipts",
        "runtime_event_meta", "runtime_item_fulltext", "runtime_search_next_meta", "runtime_search_rollout",
        "runtime_search_next", "runtime_payload_migrations", "sync_versions", "runtime_transcript_revisions",
        "runtime_assets", "runtime_checkpoints", "runtime_profiles", "runtime_projects",
        "runtime_sidebar_order", "runtime_workspace_operations", "runtime_payload_refs",
        "runtime_tool_requests", "runtime_tool_request_aliases", "runtime_tool_results",
        "runtime_budget", "runtime_budget_usage", "runtime_execution_runs",
        "runtime_execution_attempts", "runtime_execution_inputs", "runtime_execution_effects",
        "runtime_execution_nodes", "runtime_federation_settings", "runtime_federation_identity",
        "runtime_federation_peers", "runtime_federation_invites", "runtime_federation_rooms",
        "runtime_federation_outbox", "runtime_federation_inbox", "runtime_federation_nonces",
        "groups", "messages", "graph_agents", "graph_edges",
        "voice_sessions", "voice_records", "voice_approvals", "voice_deliveries",
        "voice_speech_requests", "user_terminals", "user_terminal_receipts",
        "user_terminal_output", "user_terminal_event_cursor", "user_terminal_decoder_state",
        "runtime_access_settings", "runtime_access_discovered", "runtime_access_auto_attempts",
        "runtime_access_unrevokes", "runtime_access_clients", "runtime_access_invites",
        "runtime_access_nonces", "runtime_access_receipts", "runtime_access_outbound",
        "runtime_access_audit", "runtime_access_local_alias", "runtime_access_name_targets",
        "runtime_access_name_requests", "runtime_access_alias_requests",
        "runtime_model_status", "runtime_model_modes", "runtime_native_notices",
        "runtime_native_catalog_updates", "runtime_native_limit_refreshes",
        "runtime_native_sweeps", "runtime_supervisor_cursor",
        "runtime_native_action_receipts", "runtime_safety_retries",
        "runtime_context_terminal_receipts", "runtime_account_transfers",
        "runtime_limit_reset_attempts", "runtime_limit_reset_requests",
        "runtime_agent_moves", "runtime_server_outbox", "runtime_server_inbox",
        "runtime_server_links", "runtime_server_fetch", "runtime_server_retired",
        "runtime_server_output_tools", "runtime_server_sequence", "runtime_server_exec",
        "runtime_server_exec_audit", "runtime_server_exec_input",
    ):
        db.execute(f'DROP TABLE "{table}"')
    for index in (
        "runtime_workspace_operation_agent_phase", "runtime_request_agent_status",
        "runtime_rule_agent_inflight", "runtime_conversion_rules_rootId",
        "runtime_conversion_plans_rootId", "runtime_conversion_annotations_rootId",
        "runtime_conversion_complaints_leadId", "runtime_conversion_event_meta_rootId",
        "runtime_conversion_event_meta_leadId", "runtime_room_root", "runtime_room_member_first",
        "runtime_room_member_second", "runtime_room_many_members",
        "messages_room", "voice_records_agent", "user_terminal_output_end",
        "runtime_access_nonce_age", "runtime_access_receipt_age", "runtime_access_outbound_age",
        "runtime_agent_sweep_busy", "runtime_agent_sweep_cleanup",
        "runtime_account_transfer_status", "runtime_server_outbox_due",
        "runtime_server_links_home_parent", "runtime_server_output_tools_expiry",
        "runtime_server_exec_input_handle",
        "sync_transcript_revision_runtime_agents_insert", "sync_transcript_revision_runtime_agents_update",
        "sync_transcript_revision_runtime_agents_delete", "sync_transcript_revision_runtime_items_insert",
        "sync_transcript_revision_runtime_items_update", "sync_transcript_revision_runtime_items_delete",
    ):
        db.execute(f'DROP INDEX IF EXISTS "{index}"')
(target / "runtime.lock").touch()
"#;

#[test]
fn python_flock_excludes_rust_and_rust_flock_excludes_python() {
    let root = temporary_state("locks");
    let python_lock = start_python_lock(&root);
    assert!(matches!(
        RuntimeLock::acquire(&root),
        Err(StoreError::AlreadyOwned)
    ));
    stop_python_lock(python_lock);

    let rust_lock = RuntimeLock::acquire(&root).expect("cross-runtime test operation succeeds");
    let output = python(
        &root,
        &format!(
            "import sys, pathlib; sys.path.insert(0, {:?}); from codex_file_lock import flock,LOCK_EX,LOCK_NB; f=open(pathlib.Path(sys.argv[1]) / 'runtime.lock', 'a+');\ntry: flock(f,LOCK_EX|LOCK_NB)\nexcept BlockingIOError: print('busy')\nelse: print('acquired')",
            server_source().display().to_string()
        ),
        &[],
    );
    drop(rust_lock);
    assert_eq!(output.trim(), "busy");
    root.cleanup().expect("temporary lock state is removed");
}

#[test]
fn python_fixture_records_are_readable_and_writable_in_rust() {
    let root = temporary_state("cross-runtime");
    let created = python(&root, CREATE_PYTHON_STATE, &[]);
    let agent_id: String = serde_json::from_str::<Value>(&created)
        .expect("cross-runtime test operation succeeds")["agent_id"]
        .as_str()
        .expect("cross-runtime test operation succeeds")
        .to_owned();

    let store = RuntimeStore::open(Some(&root)).expect("cross-runtime test operation succeeds");
    let reader = store.read().expect("cross-runtime test operation succeeds");
    let agent = AgentRepository::get(&reader, &AgentId::from(agent_id.as_str()))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(agent.value()["fixtureFlag"], json!(true));
    assert_eq!(agent.value()["fixtureInteger"].as_i64(), Some(7));
    assert_eq!(
        agent.value()["fixtureUnicode"],
        json!("Zażółć 東京\n\u{0000}")
    );
    assert_eq!(
        agent.value()["fixtureNested"],
        json!({"items":[0.1,5.0,9007199254740993_i64,true,{"text":"🦀\t"}]})
    );
    let task = TaskRepository::get(&reader, &TaskId::from("cross-task"))
        .expect("cross-runtime test operation succeeds")
        .expect("Python fixture contains the task");
    assert_eq!(task.value()["kind"], json!("command"));
    assert_eq!(task.value()["interactive"], json!(true));
    assert_eq!(task.value()["exitCode"].as_i64(), Some(7));
    assert_eq!(task.value()["created"].as_f64(), Some(1e-7));
    let work = Table::<WorkItemRecord>::new(&reader)
        .get(&TaskId::from("cross-work"))
        .expect("work-board record reads")
        .expect("Python fixture contains work");
    assert_eq!(work.value()["fixtureFlag"], json!(true));
    assert_eq!(work.value()["fixtureInteger"].as_i64(), Some(7));
    assert_eq!(work.value()["created"].as_f64(), Some(1e-7));
    assert_eq!(work.value()["updated"].as_f64(), Some(1e21));
    assert_eq!(work.value()["futureField"]["fixtureFlag"], json!(true));

    let event = QueueEventRepository::get(&reader, &QueueEventId::from("cross-event"))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(
        serde_json::from_str::<Value>(&event.text).expect("cross-runtime test operation succeeds")
            ["fixture"],
        true
    );
    let message = MessageRepository::get(
        &reader,
        &studio_runtime_store::ChatMessageId::from("cross-message"),
    )
    .expect("cross-runtime test operation succeeds")
    .expect("cross-runtime test operation succeeds");
    assert_eq!(message.deliveries["delivery"], true);
    drop(reader);

    let failure: Result<(), StoreError> = store.transaction(|_| {
        Err(StoreError::SchemaMismatch(
            "original transaction failure".to_owned(),
        ))
    });
    assert!(matches!(
        failure,
        Err(StoreError::SchemaMismatch(message)) if message == "original transaction failure"
    ));

    store
        .transaction(|transaction| {
            let mut event =
                QueueEventRepository::get(transaction, &QueueEventId::from("cross-event"))?
                    .expect("Python fixture contains the event");
            event.text = json!({"rust": true}).to_string();
            event.status = "running".to_owned();
            QueueEventRepository::put(transaction, &event)?;
            for event_id in [
                "numeric-created-precision",
                "numeric-created-tenth",
                "numeric-created-large-exponent",
                "numeric-created-integer",
                "numeric-created-integer-real",
                "numeric-created-large-integer-real",
                "numeric-created-17-digits",
                "numeric-created-over-2pow53",
                "numeric-created-negative",
                "numeric-created-negative-exponent",
            ] {
                let mut numeric_event =
                    QueueEventRepository::get(transaction, &QueueEventId::from(event_id))?
                        .expect("Python fixture contains each adversarial numeric event");
                numeric_event.status = "running".to_owned();
                QueueEventRepository::put(transaction, &numeric_event)?;
            }
            let mut agent = AgentRepository::get(transaction, &AgentId::from(agent_id.as_str()))?
                .expect("Python fixture contains the agent");
            agent.set("rustAgentFlag", json!(true));
            AgentRepository::put(transaction, &agent)?;
            let mut task = TaskRepository::get(transaction, &TaskId::from("cross-task"))?
                .expect("Python fixture contains the task");
            task.set("status", json!("running"));
            TaskRepository::put(transaction, &task)?;
            let work_table = Table::<WorkItemRecord>::new(transaction);
            let mut work = work_table
                .get(&TaskId::from("cross-work"))?
                .expect("Python fixture contains work");
            work.set("rustFlag", json!(true));
            work_table.put(&work)?;
            let mut message = MessageRepository::get(
                transaction,
                &studio_runtime_store::ChatMessageId::from("cross-message"),
            )?
            .expect("Python fixture contains the chat message");
            message.deliveries["rustReceipt"] = json!(true);
            MessageRepository::put(transaction, &message)?;
            Ok(())
        })
        .expect("cross-runtime test operation succeeds");
    store
        .verify_schema()
        .expect("cross-runtime test operation succeeds");
    let reader = store.read().expect("cross-runtime test operation succeeds");
    let rust_numeric_rows = {
        let mut statement = reader
            .prepare("SELECT seq,payload,hash,deleted FROM sync_entities WHERE collection='event' AND id LIKE 'numeric-created-%' ORDER BY id")
            .expect("cross-runtime test operation succeeds");
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .expect("cross-runtime test operation succeeds")
            .collect::<Result<Vec<_>, _>>()
            .expect("cross-runtime test operation succeeds")
    };
    let agent = AgentRepository::get(&reader, &AgentId::from(agent_id.as_str()))
        .expect("cross-runtime test operation succeeds")
        .expect("Rust-written agent remains readable in Rust");
    assert_eq!(agent.value()["fixtureFlag"], json!(true));
    assert_eq!(agent.value()["rustAgentFlag"], json!(true));

    let task = TaskRepository::get(&reader, &TaskId::from("cross-task"))
        .expect("cross-runtime test operation succeeds")
        .expect("Rust-written task remains readable in Rust");
    assert_eq!(task.value()["exitCode"], json!(7));
    assert_eq!(task.value()["status"], json!("running"));
    let work = Table::<WorkItemRecord>::new(&reader)
        .get(&TaskId::from("cross-work"))
        .expect("work-board read succeeds")
        .expect("Rust-written work remains readable in Rust");
    assert_eq!(work.value()["fixtureFlag"], json!(true));
    assert_eq!(work.value()["rustFlag"], json!(true));

    let event = QueueEventRepository::get(&reader, &QueueEventId::from("cross-event"))
        .expect("cross-runtime test operation succeeds")
        .expect("Rust-written event remains readable in Rust");
    assert_eq!(event.status, "running");
    assert_eq!(
        serde_json::from_str::<Value>(&event.text).expect("event text is JSON")["rust"],
        true
    );

    let message = MessageRepository::get(
        &reader,
        &studio_runtime_store::ChatMessageId::from("cross-message"),
    )
    .expect("cross-runtime test operation succeeds")
    .expect("Rust-written message remains readable in Rust");
    assert_eq!(message.deliveries["delivery"], true);
    assert_eq!(message.deliveries["rustReceipt"], true);

    assert_eq!(rust_numeric_rows.len(), 10);
    let mut numeric_created = std::collections::BTreeMap::new();
    for (_, encoded, _, _) in &rust_numeric_rows {
        let payload: Value = serde_json::from_str(encoded).expect("Rust sync payload is JSON");
        let id = payload["id"].as_str().expect("sync payload has an id");
        let created = payload["value"]["created"]
            .as_f64()
            .expect("sync payload has a numeric created value");
        numeric_created.insert(id.to_owned(), created);
    }
    for (id, expected) in [
        ("numeric-created-precision", 1_791_788_985.520_981_8),
        ("numeric-created-tenth", 0.1),
        ("numeric-created-large-exponent", 1e21),
        ("numeric-created-integer", 7.0),
        ("numeric-created-integer-real", 5.0),
        ("numeric-created-large-integer-real", 1_791_788_985.0),
        ("numeric-created-17-digits", 1.234_567_890_123_456_7),
        ("numeric-created-over-2pow53", 9_007_199_254_740_992.0),
        ("numeric-created-negative", -0.1),
        ("numeric-created-negative-exponent", -1e-7),
    ] {
        assert_eq!(numeric_created.get(id), Some(&expected), "{id}");
    }

    drop(reader);
    drop(store);

    let reopened = RuntimeStore::open(Some(&root)).expect("cross-runtime test operation succeeds");
    let read_back = reopened
        .read()
        .expect("cross-runtime test operation succeeds");
    let work = Table::<WorkItemRecord>::new(&read_back)
        .get(&TaskId::from("cross-work"))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(work.value()["rustFlag"], true);
    reopened
        .verify_schema()
        .expect("cross-runtime test operation succeeds");
    drop(read_back);
    drop(reopened);
    root.cleanup().expect("temporary Python state is removed");
}

#[test]
fn python_feature_initialized_state_round_trips_through_rust() {
    let root = temporary_state("features");
    let created = python(&root, CREATE_FEATURE_STATE, &[]);
    let output: Value = serde_json::from_str(&created).expect("feature setup returns JSON");
    let agent_id = output["agent_id"].as_str().expect("agent id exists");
    assert_eq!(output["search_phase"], "complete");

    let store = RuntimeStore::open(Some(&root)).expect("feature-initialized state opens");
    store
        .transaction(|tx| {
            let mut agent =
                AgentRepository::get(tx, &AgentId::from(agent_id))?.expect("agent exists");
            agent.set("rustWrite", json!(true));
            AgentRepository::put(tx, &agent)?;
            let mut task =
                TaskRepository::get(tx, &TaskId::from("feature-task"))?.expect("task exists");
            task.set("status", json!("running"));
            TaskRepository::put(tx, &task)
        })
        .expect("Rust writes supported records");
    let reader = store.read().expect("Rust reads its updated records");
    let agent = AgentRepository::get(&reader, &AgentId::from(agent_id))
        .expect("updated agent read succeeds")
        .expect("updated agent remains present");
    assert_eq!(agent.value()["rustWrite"], true);
    let task = TaskRepository::get(&reader, &TaskId::from("feature-task"))
        .expect("updated task read succeeds")
        .expect("updated task remains present");
    assert_eq!(task.value()["status"], "running");
    drop(reader);
    drop(store);
    root.cleanup().expect("feature state is removed");
}

#[test]
fn rust_bootstrap_matches_python_feature_table_catalog() {
    let reference = temporary_state("bootstrap-python");
    let rust_state = temporary_state("bootstrap-rust");
    python(&reference, CREATE_FEATURE_STATE, &[]);
    python(
        &reference,
        COPY_AND_REMOVE_BOOTSTRAP_TABLES,
        &[rust_state.to_str().expect("temporary path is UTF-8")],
    );

    let store =
        RuntimeStore::open(Some(&rust_state)).expect("Rust prepares missing feature tables");
    let auto_pair: i64 = store
        .read()
        .expect("Rust feature state opens")
        .query_row(
            "SELECT auto_pair FROM runtime_access_settings WHERE id=1",
            [],
            |row| row.get(0),
        )
        .expect("discovery bootstrap preserves Python's default setting");
    assert_eq!(auto_pair, 1);
    let python_catalog = feature_catalog(&reference.join("canvas.sqlite3"));
    let rust_catalog = feature_catalog(&rust_state.join("canvas.sqlite3"));
    let differing: Vec<_> = python_catalog
        .iter()
        .filter(|python_object| !rust_catalog.contains(python_object))
        .map(|python_object| {
            let rust_object = rust_catalog.iter().find(|rust_object| {
                rust_object.0 == python_object.0 && rust_object.1 == python_object.1
            });
            (
                python_object.1.as_str(),
                python_object.3.as_str(),
                rust_object.map(|object| object.3.as_str()),
            )
        })
        .collect();
    let extra: Vec<_> = rust_catalog
        .iter()
        .filter(|rust_object| !python_catalog.contains(rust_object))
        .map(|object| object.1.as_str())
        .collect();
    assert!(
        differing.is_empty() && extra.is_empty(),
        "sqlite_master differs for Python and Rust bootstrap; differing={differing:?}, extra={extra:?}"
    );
    drop(store);
    reference
        .cleanup()
        .expect("Python reference state is removed");
    rust_state.cleanup().expect("Rust feature state is removed");
}

fn feature_catalog(path: &Path) -> Vec<(String, String, String, String)> {
    const OBJECTS: &[&str] = &[
        "runtime_work",
        "runtime_work_root_status",
        "runtime_work_owner_status",
        "runtime_work_archive_due",
        "runtime_plans",
        "runtime_annotations",
        "runtime_operation_receipts",
        "runtime_event_meta",
        "runtime_item_fulltext",
        "runtime_search_next_meta",
        "runtime_search_rollout",
        "runtime_search_next",
        "runtime_search_next_config",
        "runtime_search_next_data",
        "runtime_search_next_docsize",
        "runtime_search_next_idx",
        "runtime_payload_migrations",
        "sync_versions",
        "runtime_transcript_revisions",
        "sync_transcript_revision_runtime_agents_insert",
        "sync_transcript_revision_runtime_agents_update",
        "sync_transcript_revision_runtime_agents_delete",
        "sync_transcript_revision_runtime_items_insert",
        "sync_transcript_revision_runtime_items_update",
        "sync_transcript_revision_runtime_items_delete",
        "runtime_assets",
        "runtime_checkpoints",
        "runtime_profiles",
        "runtime_projects",
        "runtime_sidebar_order",
        "runtime_workspace_operations",
        "runtime_workspace_operation_phase",
        "runtime_payload_refs",
        "runtime_payload_refs_hash",
        "runtime_payload_checkpoints_insert",
        "runtime_payload_checkpoints_update",
        "runtime_payload_checkpoints_delete",
        "runtime_tool_requests",
        "runtime_tool_request_actor",
        "runtime_tool_request_aliases",
        "runtime_payload_requests_insert",
        "runtime_payload_requests_update",
        "runtime_payload_requests_delete",
        "runtime_tool_results",
        "runtime_payload_results_insert",
        "runtime_payload_results_update",
        "runtime_payload_results_delete",
        "runtime_rules",
        "runtime_rule_ready_tick",
        "runtime_budget",
        "runtime_budget_usage",
        "runtime_budget_usage_agent",
        "runtime_execution_runs",
        "runtime_execution_run_agent",
        "runtime_execution_run_created",
        "runtime_execution_run_message",
        "runtime_execution_run_native",
        "runtime_execution_attempts",
        "runtime_execution_attempt_run",
        "runtime_execution_inputs",
        "runtime_execution_input_event",
        "runtime_execution_effects",
        "runtime_execution_effect_run",
        "runtime_execution_nodes",
        "runtime_execution_node_run",
        "runtime_federation_settings",
        "runtime_federation_identity",
        "runtime_federation_peers",
        "runtime_federation_invites",
        "runtime_federation_rooms",
        "runtime_federation_outbox",
        "runtime_federation_outbox_peer",
        "runtime_federation_inbox",
        "runtime_federation_nonces",
        "runtime_federation_nonce_age",
        "runtime_workspace_operation_agent_phase",
        "runtime_request_agent_status",
        "runtime_rule_agent_inflight",
        "runtime_conversion_rules_rootId",
        "runtime_conversion_plans_rootId",
        "runtime_conversion_annotations_rootId",
        "runtime_conversion_complaints_leadId",
        "runtime_conversion_event_meta_rootId",
        "runtime_conversion_event_meta_leadId",
        "runtime_room_root",
        "runtime_room_member_first",
        "runtime_room_member_second",
        "runtime_room_many_members",
        "groups",
        "messages",
        "messages_room",
        "graph_agents",
        "graph_edges",
        "voice_sessions",
        "voice_records",
        "voice_records_agent",
        "voice_approvals",
        "voice_deliveries",
        "voice_speech_requests",
        "user_terminals",
        "user_terminal_receipts",
        "user_terminal_output",
        "user_terminal_output_end",
        "user_terminal_event_cursor",
        "user_terminal_decoder_state",
        "runtime_access_settings",
        "runtime_access_discovered",
        "runtime_access_auto_attempts",
        "runtime_access_unrevokes",
        "runtime_access_clients",
        "runtime_access_invites",
        "runtime_access_nonces",
        "runtime_access_nonce_age",
        "runtime_access_receipts",
        "runtime_access_receipt_age",
        "runtime_access_outbound",
        "runtime_access_outbound_age",
        "runtime_access_audit",
        "runtime_access_local_alias",
        "runtime_access_name_targets",
        "runtime_access_name_requests",
        "runtime_access_alias_requests",
        "runtime_model_status",
        "runtime_model_modes",
        "runtime_native_notices",
        "runtime_native_catalog_updates",
        "runtime_native_limit_refreshes",
        "runtime_native_sweeps",
        "runtime_supervisor_cursor",
        "runtime_agent_sweep_busy",
        "runtime_agent_sweep_cleanup",
        "runtime_native_action_receipts",
        "runtime_safety_retries",
        "runtime_context_terminal_receipts",
        "runtime_account_transfers",
        "runtime_account_transfer_status",
        "runtime_limit_reset_attempts",
        "runtime_limit_reset_requests",
        "runtime_agent_moves",
        "runtime_server_outbox",
        "runtime_server_outbox_due",
        "runtime_server_inbox",
        "runtime_server_links",
        "runtime_server_links_home_parent",
        "runtime_server_fetch",
        "runtime_server_retired",
        "runtime_server_output_tools",
        "runtime_server_output_tools_expiry",
        "runtime_server_sequence",
        "runtime_server_exec",
        "runtime_server_exec_audit",
        "runtime_server_exec_input",
        "runtime_server_exec_input_handle",
    ];
    let connection = rusqlite::Connection::open(path).expect("feature catalog database opens");
    let mut statement = connection
        .prepare("SELECT type,name,tbl_name,COALESCE(sql,'') FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")
        .expect("feature catalog query prepares");
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .expect("feature catalog reads")
        .filter_map(Result::ok)
        .filter(|(_, name, _, _)| OBJECTS.contains(&name.as_str()))
        .collect()
}

fn python(root: &Path, code: &str, args: &[&str]) -> String {
    let output = Command::new(python_executable())
        .arg("-c")
        .arg(code)
        .arg(root)
        .arg(server_source())
        .args(args)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("cross-runtime test operation succeeds");
    assert!(
        output.status.success(),
        "Python helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("cross-runtime test operation succeeds")
        .lines()
        .last()
        .unwrap_or_default()
        .to_owned()
}

fn start_python_lock(root: &Path) -> (Child, ChildStdin) {
    let code = format!(
        "import sys; sys.path.insert(0, {:?}); from codex_file_lock import flock,LOCK_EX,LOCK_NB; f=open(sys.argv[1], 'a+'); flock(f,LOCK_EX|LOCK_NB); print('held',flush=True); sys.stdin.readline()",
        server_source().display().to_string()
    );
    let mut child = Command::new(python_executable())
        .arg("-c")
        .arg(code)
        .arg(root.join("runtime.lock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .spawn()
        .expect("cross-runtime test operation succeeds");
    let stdout = child
        .stdout
        .take()
        .expect("cross-runtime test operation succeeds");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("cross-runtime test operation succeeds");
    assert_eq!(line.trim(), "held");
    let stdin = child
        .stdin
        .take()
        .expect("cross-runtime test operation succeeds");
    (child, stdin)
}

fn stop_python_lock((mut child, mut stdin): (Child, ChildStdin)) {
    stdin
        .write_all(b"\n")
        .expect("cross-runtime test operation succeeds");
    drop(stdin);
    assert!(
        child
            .wait()
            .expect("cross-runtime test operation succeeds")
            .success()
    );
}

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}

fn python_executable() -> &'static Path {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON.get_or_init(resolve_python).as_path()
}

fn resolve_python() -> PathBuf {
    let selected = std::env::var_os("CODEX_AGENTS_PYTHON")
        .or_else(|| std::env::var_os("PYTHON"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let output = Command::new("python3")
                .arg(server_source().join("codex_python.py"))
                .output()
                .expect("cross-runtime tests need python3 to resolve the managed server runtime");
            assert!(
                output.status.success(),
                "cross-runtime tests require the prepared server Python runtime; run `python3 workspaces/runtime/apps/server/src/install-cli.py --dev --bin-dir <test-bin>` first. Resolver error: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            PathBuf::from(
                String::from_utf8(output.stdout)
                    .expect("Python path is UTF-8")
                    .trim(),
            )
        });

    let probe = Command::new(&selected)
        .arg("-c")
        .arg("import sys; sys.path.insert(0, sys.argv[1]); import codex_runtime; import studio_operations_native")
        .arg(server_source())
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "cross-runtime tests require the prepared server Python interpreter at {}: {error}",
                selected.display()
            )
        });
    assert!(
        probe.status.success(),
        "cross-runtime tests require server requirements and the built studio_operations_native module; prepare them with `cargo build --release -p studio-operations-python` and `python3 workspaces/runtime/apps/server/src/install-cli.py --dev --bin-dir <test-bin>`. Interpreter {} failed its import probe: {}",
        selected.display(),
        String::from_utf8_lossy(&probe.stderr)
    );
    selected
}

struct TemporaryState(PathBuf);

impl TemporaryState {
    fn cleanup(self) -> std::io::Result<()> {
        fs::remove_dir_all(&self.0)
    }
}

impl std::ops::Deref for TemporaryState {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TemporaryState {
    fn drop(&mut self) {
        let _cleanup_result = fs::remove_dir_all(&self.0);
    }
}

fn temporary_state(label: &str) -> TemporaryState {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("cross-runtime test operation succeeds")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "codex-runtime-store-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("cross-runtime test operation succeeds");
    TemporaryState(root)
}
