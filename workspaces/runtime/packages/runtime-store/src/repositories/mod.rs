pub(crate) mod agents;
pub(crate) mod events;
pub(crate) mod messages;
pub(crate) mod tasks;

use crate::schema::SchemaRequirements;

pub(crate) const REQUIREMENTS: &[&SchemaRequirements] = &[
    &agents::REQUIREMENTS,
    &tasks::REQUIREMENTS,
    &events::REQUIREMENTS,
    &messages::REQUIREMENTS,
];

#[cfg(test)]
mod tests {
    use super::{agents, events, messages, tasks};
    use crate::records::{
        AgentId, AgentRecord, AgentRepository, AgentStatus, ChatMessage, ChatMessageId,
        MessageRepository, QueueEvent, QueueEventId, QueueEventRepository, TaskId, TaskRecord,
        TaskRepository, WorkStatus,
    };
    use rusqlite::Connection;
    use serde_json::Value;
    use std::collections::BTreeMap;

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
}
