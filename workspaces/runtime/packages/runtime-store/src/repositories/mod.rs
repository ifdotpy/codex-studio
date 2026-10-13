pub mod account_state;
pub(crate) mod account_transfer;
pub mod agent_management;
pub(crate) mod agent_move;
pub(crate) mod agents;
pub mod analytics;
pub(crate) mod checkpoints;
pub(crate) mod context_repair;
pub(crate) mod coordination;
pub(crate) mod cost_usage;
pub(crate) mod event_write;
pub(crate) mod events;
pub(crate) mod federation;
pub(crate) mod federation_credentials;
pub mod federation_messaging;
pub(crate) mod messages;
pub mod monitor_runtime;
pub(crate) mod monitors;
pub(crate) mod multi_server;
pub(crate) mod profiles;
pub(crate) mod project_locations;
pub(crate) mod projects;
pub(crate) mod questions_reads;
pub(crate) mod questions_write;
pub(crate) mod queue_write;
pub(crate) mod remote_transport;
pub(crate) mod rules;
pub(crate) mod scheduler;
pub(crate) mod sync_documents;
pub mod system_reads;
pub(crate) mod tasks;
pub(crate) mod terminals;
pub(crate) mod transcript;
pub(crate) mod usage;
pub(crate) mod voice;
pub(crate) mod work_items;
pub(crate) mod workspace_files;
pub mod workspace_operations;
pub mod workspace_ready;
pub(crate) mod workspace_views;

use crate::schema::SchemaRequirements;

pub(crate) struct SchemaRegistration {
    pub(crate) repository: &'static str,
    pub(crate) requirements: &'static SchemaRequirements,
}

inventory::collect!(SchemaRegistration);

pub(crate) fn schema_registrations() -> Vec<&'static SchemaRegistration> {
    let mut registrations = inventory::iter::<SchemaRegistration>
        .into_iter()
        .collect::<Vec<_>>();
    registrations.sort_unstable_by_key(|registration| registration.repository);
    registrations
}

#[cfg(test)]
mod tests {
    use super::{account_state, agents, events, messages, sync_documents, tasks};
    use crate::records::{
        AgentId, AgentRecord, AgentRepository, ChatMessage, ChatMessageId, MessageRepository,
        QueueEvent, QueueEventId, QueueEventRepository, TaskRecord, TaskRepository,
    };
    use rusqlite::Connection;
    use serde_json::Value;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use sync_documents::SyncDocumentsRepository;

    fn database_with(requirements: &crate::schema::SchemaRequirements) -> Connection {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        for table in requirements
            .tables
            .iter()
            .chain(requirements.bootstrap_tables)
        {
            let columns = table
                .columns
                .iter()
                .map(|column| {
                    if *column == "id" {
                        format!("\"{column}\" TEXT PRIMARY KEY")
                    } else if *column == "seq" {
                        format!("\"{column}\" INTEGER NOT NULL DEFAULT 1")
                    } else {
                        format!("\"{column}\"")
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            connection
                .execute_batch(&format!("CREATE TABLE \"{}\" ({columns})", table.name))
                .expect("declared repository table creates");
        }
        connection
    }

    fn declared_modules(source: &str) -> Vec<String> {
        source
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                ["pub(crate) mod ", "pub mod ", "mod "]
                    .into_iter()
                    .find_map(|prefix| line.strip_prefix(prefix))
                    .and_then(|declaration| declaration.strip_suffix(';'))
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn repository_modules_match_the_source_directory_in_alphabetical_order() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/repositories");
        let mut source_modules = fs::read_dir(&directory)
            .expect("repositories directory is readable")
            .map(|entry| {
                entry
                    .expect("repositories directory entry is readable")
                    .path()
            })
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .filter_map(|path| {
                let stem = path.file_stem()?.to_str()?;
                (stem != "mod").then(|| stem.to_owned())
            })
            .collect::<Vec<_>>();
        source_modules.sort_unstable();

        assert_eq!(declared_modules(include_str!("mod.rs")), source_modules);

        let mut requirement_modules = fs::read_dir(&directory)
            .expect("repositories directory is readable")
            .map(|entry| {
                entry
                    .expect("repositories directory entry is readable")
                    .path()
            })
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
            .filter_map(|path| {
                if path.file_stem().is_some_and(|stem| stem == "mod") {
                    return None;
                }
                let source = fs::read_to_string(&path).expect("repository source is readable");
                source
                    .contains(
                        "pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements",
                    )
                    .then(|| path.file_stem()?.to_str().map(str::to_owned))
                    .flatten()
            })
            .collect::<Vec<_>>();
        requirement_modules.sort_unstable();

        let registrations = super::schema_registrations();
        let registered_modules = registrations
            .iter()
            .map(|registration| registration.repository.to_owned())
            .collect::<Vec<_>>();
        assert_eq!(registered_modules, requirement_modules);
        assert_eq!(registrations.len(), requirement_modules.len());
        assert!(registered_modules.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn repository_sql_uses_only_declared_tables() {
        let db = database_with(&agents::REQUIREMENTS);
        let agent = AgentRecord::from_value(json!({
            "id":"a", "rootId":"a", "epoch":0, "status":"idle"
        }))
        .expect("agent JSON has a string id");
        AgentRepository::put(&db, &agent).expect("agent write uses declared objects");
        AgentRepository::get(&db, agent.id()).expect("agent read uses declared objects");

        let db = database_with(&tasks::REQUIREMENTS);
        let task = TaskRecord::from_value(json!({
            "id":"t", "agent":"a", "kind":"command", "status":"completed",
            "created":0, "interactive":false, "exitCode":0
        }))
        .expect("task JSON has a string id");
        TaskRepository::put(&db, &task).expect("task write uses declared objects");
        TaskRepository::get(&db, task.id()).expect("task read uses declared objects");

        let db = database_with(&events::REQUIREMENTS);
        let event = QueueEvent {
            id: QueueEventId::from("e"),
            agent: AgentId::from("a"),
            kind: String::new(),
            text: String::new(),
            status: String::new(),
            created: 0.0,
            epoch: 0,
            turn_id: None,
            error: None,
        };
        QueueEventRepository::put(&db, &event).expect("event write uses declared objects");
        QueueEventRepository::get(&db, &event.id).expect("event read uses declared objects");

        let db = database_with(&messages::REQUIREMENTS);
        let message = ChatMessage {
            sequence: 0,
            id: ChatMessageId::from("m"),
            room: String::new(),
            sender: String::new(),
            text: String::new(),
            created: 0.0,
            deliveries: Value::Null,
        };
        MessageRepository::put(&db, &message).expect("message write uses declared objects");
        MessageRepository::get(&db, &message.id).expect("message read uses declared objects");

        let db = database_with(&account_state::REQUIREMENTS);
        db.execute(
            "INSERT INTO sync_entities(collection, id, payload, deleted) VALUES ('workspace', 'current', ?1, 0)",
            [json!({"value": {"rateLimitsByAccount": {
                "alpha": {"accountKey": "alpha", "data": {"status": "ready"}}
            }}}).to_string()],
        )
        .expect("fixture row uses declared columns");
        account_state::limits_snapshots(&db).expect("account limits read uses only declared table");
        assert!(
            account_state::authentication_recovery_account_keys(&db, 2_000.0)
                .expect("declared resume table query succeeds")
                .is_empty()
        );
    }

    #[test]
    fn sync_repository_sql_uses_only_declared_tables() {
        let mut db = Connection::open_in_memory().expect("memory SQLite opens");
        db.execute_batch(
            "CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
             CREATE TABLE sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE sync_documents (
                 seq INTEGER PRIMARY KEY, scope TEXT NOT NULL, id TEXT NOT NULL,
                 payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
                 UNIQUE(scope,id));
             CREATE TABLE sync_entities (
                 collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
                 hash TEXT NOT NULL, payload TEXT, deleted INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY(collection,id));
             CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE sync_versions (
                 seq INTEGER PRIMARY KEY, scope TEXT NOT NULL UNIQUE,
                 hash TEXT NOT NULL, deleted INTEGER NOT NULL, updated REAL NOT NULL);
             INSERT INTO sync_identity(id) VALUES ('workspace-a');
             INSERT INTO sync_entity_meta(key,value) VALUES ('entity_tombstone_floor','0');",
        )
        .expect("only declared sync tables create");
        assert_eq!(
            SyncDocumentsRepository::workspace_id(&db).expect("identity reads"),
            "workspace-a"
        );

        db.execute(
            "INSERT INTO sync_documents(seq,scope,id,payload,deleted) VALUES (1,'drafts','phone:chat',?1,0)",
            ["{\"device\":\"phone\",\"session\":\"chat\"}"],
        )
        .expect("draft fixture inserts");
        let drafts = SyncDocumentsRepository::pull_drafts(&db, 0, 100).expect("drafts read");
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].id, "phone:chat");
        let transaction = db.transaction().expect("preference transaction begins");
        assert!(
            SyncDocumentsRepository::put_preferences(
                &transaction,
                "user",
                "{\"collection\":\"uiPreferences\",\"id\":\"user\",\"value\":{\"fields\":{}}}",
            )
            .expect("preferences write")
        );
        transaction.commit().expect("preference commits");
        assert!(
            SyncDocumentsRepository::preference_payload(&db, "user")
                .expect("preferences read")
                .is_some()
        );
    }
}
