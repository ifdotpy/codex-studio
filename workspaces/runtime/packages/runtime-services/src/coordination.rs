//! Stored room membership, team checks and idempotent Canvas chat writes.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use studio_runtime_store::{CoordinationRepository, StoreError, Transaction};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoordinationError {
    #[error("{0}")]
    InvalidRequest(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("Not implemented: requires {0}")]
    RequiresDependency(&'static str),
    #[error("invalid stored object {object}/{id}")]
    InvalidStored { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn messages(
    connection: &studio_runtime_store::Connection,
    room: &str,
) -> Result<Value, CoordinationError> {
    let rows = CoordinationRepository::messages(connection, room)?;
    Ok(json!(rows.into_iter().map(|row| json!({"id":row.id,"room":row.room,"author":row.author,"text":row.text,"at":row.at,"deliveries":row.deliveries})).collect::<Vec<_>>()))
}

pub fn create_chat(
    transaction: &Transaction<'_>,
    id: &str,
    name: &str,
    members: &[String],
) -> Result<Value, CoordinationError> {
    if !is_uuid(id) {
        return Err(CoordinationError::InvalidRequest(
            "Invalid request identity",
        ));
    }
    let name = name.trim();
    if !(1..=100).contains(&name.chars().count()) {
        return Err(CoordinationError::InvalidRequest(
            "Use a chat name with 1 to 100 characters.",
        ));
    }
    if members.len() > 100 {
        return Err(CoordinationError::InvalidRequest(
            "Select at most 100 agents.",
        ));
    }
    let mut members = members.to_vec();
    members.sort();
    members.dedup();
    let teams = teams(transaction)?;
    let selected = members
        .iter()
        .map(|member| {
            teams.get(member).ok_or(CoordinationError::InvalidRequest(
                "The selected agents are no longer available.",
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    require_single_team(selected.iter().map(|team| team.as_str()))?;
    let by_agent = teams.contains_key(id);
    if by_agent {
        return Err(CoordinationError::InvalidRequest(
            "This identity belongs to an agent.",
        ));
    }

    if let Some(existing) = CoordinationRepository::group(transaction, id)? {
        let original_members = validate_group(&existing)?;
        if existing.name != name || original_members != members {
            return Err(CoordinationError::Conflict(
                "The request identity already has different content.",
            ));
        }
        check_historical_team(
            transaction,
            id,
            original_members
                .iter()
                .map(String::as_str)
                .chain(members.iter().map(String::as_str)),
        )?;
        return Ok(json!({"id":id}));
    }

    let group = studio_runtime_store::ChatGroup {
        id: id.to_owned(),
        name: name.to_owned(),
        members: members.clone(),
        original_members: json!(members).to_string(),
    };
    CoordinationRepository::put_group(transaction, &group)?;
    for member in members {
        let edge_id = CoordinationRepository::edge_id(&member, id);
        CoordinationRepository::put_edge(transaction, &edge_id, &member, id)?;
        CoordinationRepository::edge_projection(transaction, &edge_id, &member, id, false)?;
    }
    CoordinationRepository::chat_projection(transaction, id, false)?;
    Ok(json!({"id":id}))
}

pub fn connect_chat(
    transaction: &Transaction<'_>,
    source: &str,
    target: &str,
    connected: bool,
) -> Result<Value, CoordinationError> {
    let group = CoordinationRepository::group(transaction, target)?.ok_or(
        CoordinationError::InvalidRequest("The target must be a chat."),
    )?;
    let original_members = validate_group(&group)?;
    let known = teams(transaction)?;
    if connected && !known.contains_key(source) {
        return Err(CoordinationError::InvalidRequest(
            "The source must be a current agent.",
        ));
    }
    if connected {
        let mut members = group.members.clone();
        members.push(source.to_owned());
        check_historical_team(
            transaction,
            target,
            original_members
                .iter()
                .map(String::as_str)
                .chain(members.iter().map(String::as_str))
                .chain(std::iter::once(source)),
        )?;
    }
    let edge_id = CoordinationRepository::edge_id(source, target);
    if connected {
        CoordinationRepository::put_edge(transaction, &edge_id, source, target)?;
    } else {
        CoordinationRepository::remove_edge(transaction, &edge_id)?;
    }
    CoordinationRepository::edge_projection(transaction, &edge_id, source, target, !connected)?;
    CoordinationRepository::chat_projection(transaction, target, false)?;
    Ok(json!({"id":edge_id,"connected":connected}))
}

pub struct PostMessage<'a> {
    pub room: &'a str,
    pub id: &'a str,
    pub text: &'a str,
}

pub fn post_message(
    transaction: &Transaction<'_>,
    input: PostMessage<'_>,
) -> Result<Value, CoordinationError> {
    if !is_uuid(input.id) {
        return Err(CoordinationError::InvalidRequest(
            "Invalid request identity",
        ));
    }
    let text = input.text.trim();
    if !(1..=12_000).contains(&text.chars().count()) {
        return Err(CoordinationError::InvalidRequest(
            "Use a message with 1 to 12000 characters.",
        ));
    }
    let teams = teams(transaction)?;
    let group = CoordinationRepository::group(transaction, input.room)?;
    let (members, original_members) = if let Some(group) = &group {
        (group.members.clone(), validate_group(group)?)
    } else if teams.contains_key(input.room) {
        return Err(CoordinationError::RequiresDependency(
            "event-driver/message-send",
        ));
    } else {
        return Err(CoordinationError::InvalidRequest(
            "This chat is no longer available.",
        ));
    };
    check_historical_team(
        transaction,
        input.room,
        original_members
            .iter()
            .chain(members.iter())
            .map(String::as_str),
    )?;

    if let Some(previous) =
        CoordinationRepository::message(transaction, input.id).map_err(|_| {
            CoordinationError::InvalidStored {
                object: "messages",
                id: input.id.to_owned(),
            }
        })?
    {
        validate_message(&previous)?;
        if previous.room != input.room || previous.author != "user" || previous.text != text {
            return Err(CoordinationError::Conflict(
                "The request identity already has different content.",
            ));
        }
        return Ok(message_receipt(previous));
    }
    let deliveries = members
        .into_iter()
        .filter(|member| member != input.room)
        .map(|member| (member, Value::String("pending".to_owned())))
        .collect::<BTreeMap<_, _>>();
    let message = studio_runtime_store::CanvasMessage {
        id: input.id.to_owned(),
        room: input.room.to_owned(),
        author: "user".to_owned(),
        text: text.to_owned(),
        at: unix_time()?,
        deliveries: Value::Object(deliveries.into_iter().collect()),
    };
    CoordinationRepository::put_message(transaction, &message)?;
    if group.is_some() {
        CoordinationRepository::chat_projection(transaction, input.room, false)?;
    }
    Ok(message_receipt(message))
}

fn teams(transaction: &Transaction<'_>) -> Result<BTreeMap<String, String>, CoordinationError> {
    let mut values = BTreeMap::<String, Value>::new();
    for (id, raw) in CoordinationRepository::agents(transaction)? {
        let value: Value = serde_json::from_str(&raw).map_err(|_| invalid_agent(&id))?;
        if value.get("id").and_then(Value::as_str) != Some(id.as_str()) || !value.is_object() {
            return Err(invalid_agent(&id));
        }
        values.insert(id, value);
    }
    let mut result = BTreeMap::new();
    for (id, value) in &values {
        let deleted = value
            .get("deletedAt")
            .is_some_and(|v| !v.is_null() && v != false && v != 0 && v != "");
        if deleted {
            continue;
        }
        let team = if let Some(root) = value.get("rootId").and_then(Value::as_str) {
            if !values.contains_key(root) {
                return Err(invalid_agent(id));
            }
            format!("managed:{root}")
        } else {
            let mut current = id.as_str();
            let mut seen = BTreeSet::new();
            loop {
                if !seen.insert(current.to_owned()) {
                    return Err(invalid_agent(id));
                }
                let node = values.get(current).ok_or_else(|| invalid_agent(id))?;
                match node.get("parentId").and_then(Value::as_str) {
                    Some(parent) => current = parent,
                    None => break format!("registered:{current}"),
                }
            }
        };
        result.insert(id.clone(), team);
    }
    Ok(result)
}

fn check_historical_team<'a>(
    transaction: &Transaction<'_>,
    room: &str,
    members: impl Iterator<Item = &'a str>,
) -> Result<(), CoordinationError> {
    let known = teams(transaction)?;
    let mut participants = members.map(str::to_owned).collect::<BTreeSet<_>>();
    for (author, deliveries) in
        CoordinationRepository::participants(transaction, room).map_err(|_| {
            CoordinationError::InvalidStored {
                object: "messages",
                id: room.to_owned(),
            }
        })?
    {
        if author != "user" {
            participants.insert(author);
        }
        let deliveries =
            deliveries
                .as_object()
                .ok_or_else(|| CoordinationError::InvalidStored {
                    object: "messages",
                    id: room.to_owned(),
                })?;
        if deliveries.values().any(|value| !value.is_string()) {
            return Err(CoordinationError::InvalidStored {
                object: "messages",
                id: room.to_owned(),
            });
        }
        participants.extend(deliveries.keys().cloned());
    }
    let mut found = BTreeSet::new();
    for member in participants {
        found.insert(
            known
                .get(&member)
                .ok_or_else(|| {
                    CoordinationError::InvalidRequest(
                        "The agent team cannot be verified. Use the current team's runtime tools.",
                    )
                })?
                .as_str(),
        );
    }
    require_single_team(found.into_iter())
}

fn require_single_team<'a>(
    mut teams: impl Iterator<Item = &'a str>,
) -> Result<(), CoordinationError> {
    let Some(first) = teams.next() else {
        return Ok(());
    };
    if teams.any(|team| team != first) {
        return Err(CoordinationError::InvalidRequest(
            "Communication is limited to one team. This chat contains another team.",
        ));
    }
    Ok(())
}

fn validate_group(
    group: &studio_runtime_store::ChatGroup,
) -> Result<Vec<String>, CoordinationError> {
    let members: Vec<String> = serde_json::from_str(&group.original_members).map_err(|_| {
        CoordinationError::InvalidStored {
            object: "groups",
            id: group.id.clone(),
        }
    })?;
    if group.id.is_empty()
        || group.name.trim().is_empty()
        || group.name.chars().count() > 100
        || members.len() > 100
        || members.iter().any(String::is_empty)
        || members.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(CoordinationError::InvalidStored {
            object: "groups",
            id: group.id.clone(),
        });
    }
    Ok(members)
}

fn validate_message(
    message: &studio_runtime_store::CanvasMessage,
) -> Result<(), CoordinationError> {
    if message.id.is_empty()
        || message.room.is_empty()
        || message.author.is_empty()
        || !(1..=12_000).contains(&message.text.chars().count())
        || !message.at.is_finite()
        || !message
            .deliveries
            .as_object()
            .is_some_and(|items| items.values().all(Value::is_string))
    {
        return Err(CoordinationError::InvalidStored {
            object: "messages",
            id: message.id.clone(),
        });
    }
    Ok(())
}

fn message_receipt(message: studio_runtime_store::CanvasMessage) -> Value {
    let deliveries = message.deliveries.as_object();
    let status = if deliveries.is_none_or(|items| items.is_empty()) {
        "delivered"
    } else if deliveries.is_some_and(|items| {
        items
            .values()
            .all(|value| value == "pending" || value == "queued")
    }) {
        "queued"
    } else {
        "uncertain"
    };
    json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries,"status":status})
}

fn invalid_agent(id: &str) -> CoordinationError {
    CoordinationError::InvalidStored {
        object: "agents",
        id: id.to_owned(),
    }
}
fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
fn unix_time() -> Result<f64, CoordinationError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .map_err(|_| CoordinationError::InvalidRequest("system clock is before the Unix epoch"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use studio_runtime_store::Connection;

    use super::{
        CoordinationError, PostMessage, connect_chat, create_chat, is_uuid, post_message,
        require_single_team,
    };

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        connection.execute_batch(
            "CREATE TABLE groups(id TEXT PRIMARY KEY,name TEXT NOT NULL,members TEXT NOT NULL);
             CREATE TABLE graph_edges(id TEXT PRIMARY KEY,source TEXT NOT NULL,target TEXT NOT NULL,kind TEXT NOT NULL,UNIQUE(source,target,kind));
             CREATE TABLE messages(id TEXT PRIMARY KEY,room TEXT NOT NULL,author TEXT NOT NULL,text TEXT NOT NULL,at REAL NOT NULL,deliveries TEXT NOT NULL);
             CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE graph_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             CREATE TABLE sync_entities(collection TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,hash TEXT NOT NULL,payload TEXT,deleted INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(collection,id));
             CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY,scope TEXT NOT NULL,id TEXT NOT NULL,payload TEXT NOT NULL,deleted INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE sync_versions(seq INTEGER PRIMARY KEY,scope TEXT NOT NULL,hash TEXT NOT NULL,deleted INTEGER NOT NULL,updated REAL NOT NULL);"
        ).expect("coordination schema creates");
        for (id, root) in [("lead", "lead"), ("worker", "lead"), ("other", "other")] {
            let record = json!({"id":id,"rootId":root,"deletedAt":null}).to_string();
            connection
                .execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    [id, record.as_str()],
                )
                .expect("agent inserts");
        }
        connection
    }

    fn count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count reads")
    }

    #[test]
    fn stable_identifiers_are_uuid_shaped() {
        assert!(is_uuid("12345678-1234-1234-1234-123456789abc"));
        assert!(!is_uuid("not-a-request-id"));
    }

    #[test]
    fn mixed_teams_are_refused() {
        assert!(require_single_team(["team-a", "team-b"].into_iter()).is_err());
        assert!(require_single_team(["team-a", "team-a"].into_iter()).is_ok());
    }

    #[test]
    fn chat_write_is_idempotent_and_conflicts_leave_no_rows() {
        let mut connection = database();
        let id = "12345678-1234-1234-1234-123456789abc";
        let create = |connection: &mut Connection, name: &str| {
            let transaction = connection.transaction().expect("transaction starts");
            let result = create_chat(
                &transaction,
                id,
                name,
                &["lead".to_owned(), "worker".to_owned()],
            );
            if result.is_ok() {
                transaction.commit().expect("chat commits");
            }
            result
        };
        assert_eq!(
            create(&mut connection, "Team").expect("chat creates"),
            json!({"id":id})
        );
        assert_eq!(
            create(&mut connection, "Team").expect("chat replays"),
            json!({"id":id})
        );
        assert!(matches!(
            create(&mut connection, "Changed"),
            Err(CoordinationError::Conflict(_))
        ));
        assert_eq!(count(&connection, "groups"), 1);
        assert_eq!(count(&connection, "graph_edges"), 2);
        assert_eq!(count(&connection, "sync_entities"), 3);
    }

    #[test]
    fn cross_team_connection_refusal_writes_nothing() {
        let mut connection = database();
        let id = "12345678-1234-1234-1234-123456789abc";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(&transaction, id, "Team", &["lead".to_owned()])
            .expect("same team room creates");
        transaction.commit().expect("chat commits");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            connect_chat(&transaction, "other", id, true),
            Err(CoordinationError::InvalidRequest(_))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "graph_edges"), 1);
        assert_eq!(count(&connection, "sync_entities"), 2);
    }

    #[test]
    fn message_replay_is_idempotent_and_changed_content_conflicts() {
        let mut connection = database();
        let chat_id = "12345678-1234-1234-1234-123456789abc";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(
            &transaction,
            chat_id,
            "Team",
            &["lead".to_owned(), "worker".to_owned()],
        )
        .expect("chat creates");
        transaction.commit().expect("chat commits");
        let message_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let post = |connection: &mut Connection, text: &str| {
            let transaction = connection.transaction().expect("transaction starts");
            let result = post_message(
                &transaction,
                PostMessage {
                    room: chat_id,
                    id: message_id,
                    text,
                },
            );
            if result.is_ok() {
                transaction.commit().expect("message commits");
            }
            result
        };
        let first = post(&mut connection, "Hello").expect("message writes");
        assert_eq!(first["status"], "queued");
        assert_eq!(first["deliveries"]["worker"], "pending");
        assert_eq!(
            post(&mut connection, "Hello").expect("exact replay returns receipt"),
            first
        );
        assert!(matches!(
            post(&mut connection, "Changed"),
            Err(CoordinationError::Conflict(_))
        ));
        assert_eq!(count(&connection, "messages"), 1);
        assert_eq!(count(&connection, "sync_entities"), 3);
    }

    #[test]
    fn direct_agent_send_requires_the_event_consumer() {
        let connection = database();
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        let result = post_message(
            &transaction,
            PostMessage {
                room: "worker",
                id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                text: "Hello",
            },
        );
        assert!(matches!(
            result,
            Err(CoordinationError::RequiresDependency(
                "event-driver/message-send"
            ))
        ));
        assert_eq!(count(&connection, "messages"), 0);
    }
}
