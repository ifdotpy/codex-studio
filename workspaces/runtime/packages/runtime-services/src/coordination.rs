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

pub fn history(
    connection: &studio_runtime_store::Connection,
    room: &str,
) -> Result<Value, CoordinationError> {
    let rows = CoordinationRepository::messages(connection, room)?;
    rows.into_iter()
        .map(|row| validated_message(row).map(message_json))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

pub fn create_chat(
    transaction: &Transaction<'_>,
    id: &str,
    name: &str,
    members: &[String],
) -> Result<Value, CoordinationError> {
    let name = name.trim();
    if !(1..=100).contains(&name.chars().count()) {
        return Err(CoordinationError::InvalidRequest(
            "Use a chat name with 1 to 100 characters.",
        ));
    }
    let mut members = members.to_vec();
    members.sort();
    members.dedup();
    let directory = teams(transaction)?;
    let selected = members
        .iter()
        .map(|member| {
            required_team(
                &directory,
                member,
                "The selected agents are no longer available.",
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    require_single_team(selected.iter().copied())?;
    check_historical_team(transaction, id, &members, &directory)?;
    let by_agent = directory.team_by_id.contains_key(id);
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
    validate_group(&group)?;
    let directory = teams(transaction)?;
    required_team(&directory, source, "The source must be a current agent.")?;
    if connected {
        let mut members = group.members.clone();
        members.push(source.to_owned());
        check_historical_team(transaction, target, &members, &directory)?;
    }
    let edge_id = CoordinationRepository::edge_id(source, target);
    if connected {
        CoordinationRepository::put_edge(transaction, &edge_id, source, target)?;
    } else {
        CoordinationRepository::remove_edge(transaction, &edge_id)?;
    }
    CoordinationRepository::edge_projection(transaction, &edge_id, source, target, !connected)?;
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
    if CoordinationRepository::federated_room_exists(transaction, input.room)? {
        return Err(CoordinationError::RequiresDependency(
            "federation-driver/rooms",
        ));
    }
    if CoordinationRepository::agent_exists(transaction, input.room)? {
        return Err(CoordinationError::RequiresDependency(
            "event-driver/message-send",
        ));
    }
    let group = CoordinationRepository::group(transaction, input.room)?;
    let Some(group) = group else {
        let directory = teams(transaction)?;
        if directory.suppressed_aliases.contains(input.room) {
            return Err(CoordinationError::InvalidRequest(
                "This chat is no longer available.",
            ));
        }
        if directory.unresolved_thread_agents.contains(input.room) {
            return Err(CoordinationError::RequiresDependency(
                "agent-discovery/launcher-status",
            ));
        }
        if directory.team_by_id.contains_key(input.room) {
            return Err(CoordinationError::RequiresDependency(
                "event-driver/message-send",
            ));
        }
        return Err(CoordinationError::RequiresDependency(
            "agent-discovery/launcher-status",
        ));
    };
    let text = input.text.trim();
    if !(1..=12_000).contains(&text.chars().count()) {
        return Err(CoordinationError::InvalidRequest(
            "Use a message with 1 to 12000 characters.",
        ));
    }
    let members = group.members.clone();
    validate_group(&group)?;
    let directory = teams(transaction)?;
    check_historical_team(transaction, input.room, &members, &directory)?;

    if let Some(previous) =
        CoordinationRepository::message(transaction, input.id).map_err(|_| {
            CoordinationError::InvalidStored {
                object: "messages",
                id: input.id.to_owned(),
            }
        })?
    {
        let previous = validated_message(previous)?;
        if previous.room != input.room || previous.author != "user" || previous.text != text {
            return Err(CoordinationError::Conflict(
                "The request identity already has different content.",
            ));
        }
        return Ok(message_receipt(previous, false));
    }
    let deliveries = members
        .into_iter()
        .filter(|member| member != "user")
        .map(|member| (member, Value::String("pending".to_owned())))
        .collect::<BTreeMap<_, _>>();
    let message = studio_runtime_store::CanvasMessage {
        id: input.id.to_owned(),
        room: input.room.to_owned(),
        author: "user".to_owned(),
        text: text.to_owned(),
        at: unix_time()?,
        deliveries: serde_json::to_string(&Value::Object(deliveries.into_iter().collect()))
            .map_err(|_| CoordinationError::InvalidRequest("Invalid message delivery state"))?,
    };
    CoordinationRepository::put_message(transaction, &message)?;
    CoordinationRepository::chat_projection(transaction, input.room, false)?;
    Ok(message_receipt(validated_message(message)?, true))
}

struct AgentDirectory {
    team_by_id: BTreeMap<String, String>,
    suppressed_aliases: BTreeSet<String>,
    unresolved_thread_agents: BTreeSet<String>,
    invalid_agents: BTreeSet<String>,
}

fn teams(transaction: &Transaction<'_>) -> Result<AgentDirectory, CoordinationError> {
    let mut values = BTreeMap::<String, Value>::new();
    let mut graph_agents = BTreeSet::new();
    let mut managed_threads = BTreeSet::new();
    let mut invalid_agents = BTreeSet::new();
    for (id, mut value, is_graph) in CoordinationRepository::agents(transaction)? {
        if is_graph {
            graph_agents.insert(id.clone());
            value["source"] = Value::String("registered".to_owned());
        } else if let Some(thread_id) = value.get("threadId").and_then(Value::as_str) {
            managed_threads.insert(thread_id.to_owned());
        }
        values.insert(id, value);
    }
    let mut suppressed_aliases = BTreeSet::new();
    let mut unresolved_thread_agents = BTreeSet::new();
    for id in &graph_agents {
        let Some(record) = values.get_mut(id) else {
            continue;
        };
        record["source"] = Value::String("registered".to_owned());
        if let Some(thread_id) = record.get("threadId").and_then(Value::as_str) {
            if managed_threads.contains(thread_id) {
                suppressed_aliases.insert(id.clone());
            } else {
                unresolved_thread_agents.insert(id.clone());
            }
        }
    }
    let mut result = BTreeMap::new();
    'agents: for (id, value) in &values {
        if suppressed_aliases.contains(id) || unresolved_thread_agents.contains(id) {
            continue;
        }
        let deleted = value
            .get("deletedAt")
            .is_some_and(|v| !v.is_null() && v != false && v != 0 && v != "");
        if deleted {
            continue;
        }
        let team = if let Some(root) = value
            .get("rootId")
            .and_then(Value::as_str)
            .filter(|root| !root.is_empty())
        {
            format!("managed:{root}")
        } else if value.get("source").and_then(Value::as_str) == Some("app-server")
            && let (Some(wave), Some(run_id)) = (
                value.get("wave").and_then(Value::as_str),
                value.get("runId").and_then(Value::as_str),
            )
        {
            format!("wave:{wave}:{run_id}")
        } else if value.get("source").and_then(Value::as_str) == Some("registered") {
            let mut current = id.as_str();
            let mut seen = BTreeSet::new();
            loop {
                if !seen.insert(current.to_owned()) {
                    invalid_agents.insert(id.clone());
                    continue 'agents;
                }
                let Some(node) = values.get(current) else {
                    unresolved_thread_agents.insert(id.clone());
                    continue 'agents;
                };
                if let Some(root) = node
                    .get("rootId")
                    .and_then(Value::as_str)
                    .filter(|root| !root.is_empty())
                {
                    break format!("managed:{root}");
                }
                if node.get("source").and_then(Value::as_str) == Some("app-server")
                    && let (Some(wave), Some(run_id)) = (
                        node.get("wave").and_then(Value::as_str),
                        node.get("runId").and_then(Value::as_str),
                    )
                {
                    break format!("wave:{wave}:{run_id}");
                }
                if node.get("source").and_then(Value::as_str) == Some("registered") {
                    match node
                        .get("parentId")
                        .and_then(Value::as_str)
                        .filter(|parent| !parent.is_empty())
                    {
                        Some(parent) => current = parent,
                        None => break format!("registered:{current}"),
                    }
                } else {
                    unresolved_thread_agents.insert(id.clone());
                    continue 'agents;
                }
            }
        } else {
            unresolved_thread_agents.insert(id.clone());
            continue 'agents;
        };
        if !suppressed_aliases.contains(id) && !unresolved_thread_agents.contains(id) {
            result.insert(id.clone(), team);
        }
    }
    Ok(AgentDirectory {
        team_by_id: result,
        suppressed_aliases,
        unresolved_thread_agents,
        invalid_agents,
    })
}

fn check_historical_team(
    transaction: &Transaction<'_>,
    room: &str,
    members: &[String],
    directory: &AgentDirectory,
) -> Result<(), CoordinationError> {
    let mut participants = members.iter().cloned().collect::<BTreeSet<_>>();
    if let Some(group) = CoordinationRepository::group(transaction, room)? {
        validate_group(&group)?;
        participants.extend(group.members);
    }
    for row in CoordinationRepository::participants(transaction, room)? {
        let deliveries = parse_deliveries(&row.deliveries, &row.id)?;
        if row.author != "user" {
            participants.insert(row.author);
        }
        participants.extend(
            deliveries
                .as_object()
                .expect("validated deliveries")
                .keys()
                .cloned(),
        );
    }
    let mut found = BTreeSet::new();
    for member in participants {
        found.insert(required_team(
            directory,
            &member,
            "The agent team cannot be verified. Use the current team's runtime tools.",
        )?);
    }
    require_single_team(found.into_iter())
}

fn required_team<'a>(
    directory: &'a AgentDirectory,
    id: &str,
    unavailable: &'static str,
) -> Result<&'a str, CoordinationError> {
    if directory.invalid_agents.contains(id) {
        return Err(invalid_agent(id));
    }
    if directory.suppressed_aliases.contains(id) {
        return Err(CoordinationError::InvalidRequest(unavailable));
    }
    if directory.unresolved_thread_agents.contains(id) {
        return Err(CoordinationError::RequiresDependency(
            "agent-discovery/launcher-status",
        ));
    }
    directory
        .team_by_id
        .get(id)
        .map(String::as_str)
        .ok_or(CoordinationError::RequiresDependency(
            "agent-discovery/launcher-status",
        ))
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

struct ValidatedMessage {
    id: String,
    room: String,
    author: String,
    text: String,
    at: f64,
    deliveries: Value,
}

fn validated_message(
    message: studio_runtime_store::CanvasMessage,
) -> Result<ValidatedMessage, CoordinationError> {
    let deliveries = parse_deliveries(&message.deliveries, &message.id)?;
    if message.id.is_empty()
        || message.room.is_empty()
        || message.author.is_empty()
        || message.text.chars().count() > 32_000
        || !message.at.is_finite()
    {
        return Err(CoordinationError::InvalidStored {
            object: "messages",
            id: message.id.clone(),
        });
    }
    Ok(ValidatedMessage {
        id: message.id,
        room: message.room,
        author: message.author,
        text: message.text,
        at: message.at,
        deliveries,
    })
}

fn parse_deliveries(raw: &str, id: &str) -> Result<Value, CoordinationError> {
    let deliveries: Value =
        serde_json::from_str(raw).map_err(|_| CoordinationError::InvalidStored {
            object: "messages",
            id: id.to_owned(),
        })?;
    if !deliveries
        .as_object()
        .is_some_and(|items| items.values().all(Value::is_string))
    {
        return Err(CoordinationError::InvalidStored {
            object: "messages",
            id: id.to_owned(),
        });
    }
    Ok(deliveries)
}

fn message_json(message: ValidatedMessage) -> Value {
    json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries})
}

fn message_receipt(message: ValidatedMessage, newly_queued: bool) -> Value {
    let deliveries = message.deliveries.as_object();
    let status = if deliveries.is_none_or(|items| items.is_empty()) {
        "delivered"
    } else {
        let values = deliveries
            .expect("deliveries was checked")
            .values()
            .collect::<Vec<_>>();
        let accepted = if newly_queued {
            &["queued", "pending", "delivered", "accepted", "sent"][..]
        } else {
            &["queued", "delivered", "accepted", "sent"][..]
        };
        if values.iter().all(|value| {
            value
                .as_str()
                .is_some_and(|value| accepted.contains(&value))
        }) {
            if values
                .iter()
                .any(|value| *value == "queued" || (newly_queued && *value == "pending"))
            {
                "queued"
            } else {
                "delivered"
            }
        } else {
            let failed = values
                .iter()
                .filter_map(|value| value.as_str())
                .filter(|value| value.starts_with("failed:") || value.starts_with("cancelled:"))
                .collect::<Vec<_>>();
            if failed.len() == values.len() {
                let mut unique = Vec::new();
                for value in &failed {
                    if !unique.contains(value) {
                        unique.push(*value);
                    }
                }
                return json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries,"status":"failed","error":unique.join("; ")});
            }
            return json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries,"status":"uncertain","error":"Delivery is unconfirmed for one or more recipients. Check the saved deliveries before sending again."});
        }
    };
    json!({"id":message.id,"room":message.room,"author":message.author,"text":message.text,"at":message.at,"deliveries":message.deliveries,"status":status})
}

fn invalid_agent(id: &str) -> CoordinationError {
    CoordinationError::InvalidStored {
        object: "agents",
        id: id.to_owned(),
    }
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
        CoordinationError, PostMessage, connect_chat, create_chat, post_message,
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
            let record = json!({"id":id,"rootId":root,"epoch":0,"status":"idle"}).to_string();
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
        let replay = post(&mut connection, "Hello").expect("exact replay returns receipt");
        assert_eq!(replay["deliveries"], first["deliveries"]);
        assert_eq!(replay["status"], "uncertain");
        assert!(replay.get("error").is_some());
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

    #[test]
    fn direct_dispatch_and_federation_refuse_before_identity_or_text_rules() {
        let connection = database();
        connection
            .execute_batch(
                "CREATE TABLE runtime_federation_rooms(id TEXT PRIMARY KEY,record TEXT NOT NULL);
             INSERT INTO runtime_federation_rooms VALUES('federated:peer:room','{}');",
            )
            .expect("federation fixture creates");
        let transaction = connection
            .unchecked_transaction()
            .expect("transaction starts");
        assert!(matches!(
            post_message(
                &transaction,
                PostMessage {
                    room: "worker",
                    id: "invalid",
                    text: ""
                }
            ),
            Err(CoordinationError::RequiresDependency(
                "event-driver/message-send"
            ))
        ));
        assert!(matches!(
            post_message(
                &transaction,
                PostMessage {
                    room: "federated:peer:room",
                    id: "invalid",
                    text: ""
                }
            ),
            Err(CoordinationError::RequiresDependency(
                "federation-driver/rooms"
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "messages"), 0);
    }

    #[test]
    fn historical_room_participants_prevent_cross_team_takeover() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO messages VALUES(?1,?2,'other','old',1.0,'{}')",
                [
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                ],
            )
            .expect("historical message inserts");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            create_chat(
                &transaction,
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                "Team",
                &["lead".to_owned()]
            ),
            Err(CoordinationError::InvalidRequest(
                "Communication is limited to one team. This chat contains another team."
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "groups"), 0);
        assert_eq!(count(&connection, "graph_edges"), 0);
        assert_eq!(count(&connection, "sync_entities"), 0);
    }

    #[test]
    fn registered_descendants_inherit_managed_team_and_managed_aliases_are_suppressed() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE runtime_agents SET record=?1 WHERE id='lead'",
                [json!({"id":"lead","rootId":"lead","epoch":0,"status":"idle","threadId":"managed-thread"}).to_string()],
            )
            .expect("managed thread fixture updates");
        connection
            .execute(
                "INSERT INTO graph_agents VALUES('child',?1),('alias',?2)",
                [
                    json!({"id":"child","parentId":"lead"}).to_string(),
                    json!({"id":"alias","threadId":"managed-thread"}).to_string(),
                ],
            )
            .expect("registered fixtures insert");
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(
            &transaction,
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "Team",
            &["lead".to_owned(), "child".to_owned()],
        )
        .expect("child inherits parent team");
        transaction.commit().expect("chat commits");
        let before = count(&connection, "groups");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            create_chat(
                &transaction,
                "cccccccc-cccc-cccc-cccc-cccccccccccc",
                "Alias",
                &["alias".to_owned()]
            ),
            Err(CoordinationError::InvalidRequest(
                "The selected agents are no longer available."
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "groups"), before);
    }

    #[test]
    fn status_file_agents_and_aliases_require_launcher_discovery_without_writes() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO graph_agents VALUES('launcher-alias',?1)",
                [
                    json!({"id":"launcher-alias","threadId":"thread-only-in-status-file"})
                        .to_string(),
                ],
            )
            .expect("alias fixture inserts");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            create_chat(
                &transaction,
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                "Launcher",
                &["launcher-alias".to_owned()]
            ),
            Err(CoordinationError::RequiresDependency(
                "agent-discovery/launcher-status"
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "groups"), 0);
        assert_eq!(count(&connection, "graph_edges"), 0);
        assert_eq!(count(&connection, "sync_entities"), 0);
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(
            &transaction,
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "Known",
            &[],
        )
        .expect("chat creates");
        transaction.commit().expect("chat commits");
        let before = count(&connection, "sync_entities");
        let transaction = connection.transaction().expect("transaction starts");
        assert!(matches!(
            connect_chat(
                &transaction,
                "status-only-agent",
                "cccccccc-cccc-cccc-cccc-cccccccccccc",
                false
            ),
            Err(CoordinationError::RequiresDependency(
                "agent-discovery/launcher-status"
            ))
        ));
        drop(transaction);
        assert_eq!(count(&connection, "sync_entities"), before);
    }

    #[test]
    fn group_post_excludes_author_from_pending_recipients_and_bounds_chat_tail() {
        let mut connection = database();
        connection
            .execute(
                "INSERT INTO runtime_agents VALUES('user',?1)",
                [json!({"id":"user","rootId":"lead","epoch":0,"status":"idle"}).to_string()],
            )
            .expect("user agent fixture inserts");
        let room = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(
            &transaction,
            room,
            "Team",
            &["lead".to_owned(), "user".to_owned()],
        )
        .expect("chat creates");
        transaction.commit().expect("chat commits");
        let transaction = connection.transaction().expect("transaction starts");
        let receipt = post_message(
            &transaction,
            PostMessage {
                room,
                id: "cccccccc-cccc-cccc-cccc-cccccccccccc",
                text: &"x".repeat(2_500),
            },
        )
        .expect("message writes");
        assert_eq!(receipt["status"], "queued");
        assert_eq!(receipt["deliveries"]["lead"], "pending");
        assert!(receipt["deliveries"].get("user").is_none());
        transaction.commit().expect("message commits");
        let payload: String = connection
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='chat' AND id=?1",
                [room],
                |row| row.get(0),
            )
            .expect("chat projection reads");
        let payload: serde_json::Value = serde_json::from_str(&payload).expect("projection parses");
        assert_eq!(
            payload["value"]["tail"]
                .as_str()
                .expect("tail is text")
                .chars()
                .count(),
            2_000
        );
    }

    #[test]
    fn replay_preserves_python_delivery_receipts() {
        let mut connection = database();
        let room = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(&transaction, room, "Team", &[]).expect("chat creates");
        transaction.commit().expect("chat commits");
        for (id, delivery, status, error) in [
            (
                "10101010-1010-1010-1010-101010101010",
                "delivered",
                "delivered",
                None,
            ),
            (
                "11111111-1111-1111-1111-111111111111",
                "accepted",
                "delivered",
                None,
            ),
            (
                "22222222-2222-2222-2222-222222222222",
                "sent",
                "delivered",
                None,
            ),
            (
                "33333333-3333-3333-3333-333333333333",
                "failed: mailbox closed",
                "failed",
                Some("failed: mailbox closed"),
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO messages VALUES(?1,?2,'user','saved',1.0,?3)",
                    [id, room, &json!({"worker":delivery}).to_string()],
                )
                .expect("Python message inserts");
            let transaction = connection
                .unchecked_transaction()
                .expect("transaction starts");
            let receipt = post_message(
                &transaction,
                PostMessage {
                    room,
                    id,
                    text: "saved",
                },
            )
            .expect("replay returns saved receipt");
            assert_eq!(receipt["status"], status);
            assert_eq!(
                receipt.get("error").and_then(serde_json::Value::as_str),
                error
            );
            assert_eq!(receipt["deliveries"]["worker"], delivery);
        }
    }

    #[test]
    fn late_chat_projection_failure_rolls_back_group_edges_and_sync_rows() {
        let mut connection = database();
        connection.execute_batch(
            "CREATE TRIGGER fail_chat_projection BEFORE INSERT ON sync_entities
             WHEN NEW.collection='chat' BEGIN SELECT RAISE(ABORT,'injected chat projection failure'); END;",
        ).expect("failure trigger creates");
        let transaction = connection.transaction().expect("transaction starts");
        let result = create_chat(
            &transaction,
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "Team",
            &["lead".to_owned()],
        );
        assert!(matches!(result, Err(CoordinationError::Store(_))));
        drop(transaction);
        assert_eq!(count(&connection, "groups"), 0);
        assert_eq!(count(&connection, "graph_edges"), 0);
        assert_eq!(count(&connection, "sync_entities"), 0);
        assert_eq!(count(&connection, "sync_documents"), 0);
        assert_eq!(count(&connection, "sync_versions"), 0);
    }

    #[test]
    fn connection_changes_publish_only_edge_projection() {
        let mut connection = database();
        let room = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let transaction = connection.transaction().expect("transaction starts");
        create_chat(&transaction, room, "Team", &["lead".to_owned()]).expect("chat creates");
        transaction.commit().expect("chat commits");
        let chat_sequence: i64 = connection
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='chat' AND id=?1",
                [room],
                |row| row.get(0),
            )
            .expect("chat sequence reads");
        let transaction = connection.transaction().expect("transaction starts");
        connect_chat(&transaction, "worker", room, true).expect("edge connects");
        transaction.commit().expect("edge commits");
        let after_connect: i64 = connection
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='chat' AND id=?1",
                [room],
                |row| row.get(0),
            )
            .expect("chat sequence reads");
        assert_eq!(chat_sequence, after_connect);
        let transaction = connection.transaction().expect("transaction starts");
        connect_chat(&transaction, "worker", room, false).expect("edge disconnects");
        transaction.commit().expect("edge removal commits");
        let after_disconnect: i64 = connection
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='chat' AND id=?1",
                [room],
                |row| row.get(0),
            )
            .expect("chat sequence reads");
        assert_eq!(chat_sequence, after_disconnect);
    }
}
