pub(crate) mod agents;
pub(crate) mod events;
pub(crate) mod messages;
pub(crate) mod rules;
pub(crate) mod sync_documents;
pub(crate) mod tasks;

use crate::schema::SchemaRequirements;

pub(crate) const REQUIREMENTS: &[&SchemaRequirements] = &[
    &agents::REQUIREMENTS,
    &tasks::REQUIREMENTS,
    &events::REQUIREMENTS,
    &messages::REQUIREMENTS,
    &rules::REQUIREMENTS,
    &sync_documents::REQUIREMENTS,
];

#[cfg(test)]
mod tests {
    use super::{agents, events, messages, sync_documents, tasks};
    use crate::records::{
        AgentId, AgentRecord, AgentRepository, AgentStatus, ChatMessage, ChatMessageId,
        MessageRepository, QueueEvent, QueueEventId, QueueEventRepository, TaskId, TaskRecord,
        TaskRepository, WorkStatus,
    };
    use rusqlite::Connection;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use sync_documents::SyncDocumentsRepository;

    fn database_with(requirements: &crate::schema::SchemaRequirements) -> Connection {
        let connection = Connection::open_in_memory().expect("memory SQLite opens");
        for table in requirements.tables {
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
        let agent = AgentRecord {
            id: AgentId::from("a"),
            root_id: AgentId::from("a"),
            epoch: 0,
            status: AgentStatus::Idle,
            extra: BTreeMap::new(),
        };
        AgentRepository::put(&db, &agent).expect("agent write uses declared objects");
        AgentRepository::get(&db, &agent.id).expect("agent read uses declared objects");

        let db = database_with(&tasks::REQUIREMENTS);
        let task = TaskRecord {
            id: TaskId::from("t"),
            root_id: AgentId::from("a"),
            status: WorkStatus::Ready,
            title: String::new(),
            description: String::new(),
            owner: None,
            created: 0.0,
            updated: 0.0,
            created_by: AgentId::from("a"),
            version: 0,
            dependencies: vec![],
            decisions: vec![],
            results: vec![],
            extra: BTreeMap::<String, Value>::new(),
        };
        TaskRepository::put(&db, &task).expect("task write uses declared objects");
        TaskRepository::get(&db, &task.id).expect("task read uses declared objects");

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
