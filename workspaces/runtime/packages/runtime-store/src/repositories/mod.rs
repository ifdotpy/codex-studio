pub mod account_state;
pub(crate) mod agents;
pub(crate) mod coordination;
pub(crate) mod event_write;
pub(crate) mod events;
pub(crate) mod messages;
pub(crate) mod monitors;
pub(crate) mod profiles;
pub(crate) mod questions_reads;
pub(crate) mod queue_write;
pub(crate) mod rules;
pub(crate) mod scheduler;
pub(crate) mod sync_documents;
pub(crate) mod tasks;
pub(crate) mod terminals;
pub(crate) mod transcript;
pub(crate) mod work_items;
pub(crate) mod workspace_files;
pub(crate) mod workspace_views;

use crate::schema::SchemaRequirements;

pub(crate) const REQUIREMENTS: &[&SchemaRequirements] = &[
    &agents::REQUIREMENTS,
    &account_state::REQUIREMENTS,
    &coordination::REQUIREMENTS,
    &tasks::REQUIREMENTS,
    &events::REQUIREMENTS,
    &event_write::REQUIREMENTS,
    &queue_write::REQUIREMENTS,
    &messages::REQUIREMENTS,
    &monitors::REQUIREMENTS,
    &profiles::REQUIREMENTS,
    &rules::REQUIREMENTS,
    &questions_reads::REQUIREMENTS,
    &scheduler::REQUIREMENTS,
    &sync_documents::REQUIREMENTS,
    &terminals::REQUIREMENTS,
    &transcript::REQUIREMENTS,
    &work_items::REQUIREMENTS,
    &workspace_files::REQUIREMENTS,
];

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
            "id":"t", "rootId":"a", "status":"ready", "title":"",
            "description":"", "owner":null, "created":0, "updated":0,
            "createdBy":"a", "version":0, "dependencies":[], "decisions":[],
            "results":[]
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
