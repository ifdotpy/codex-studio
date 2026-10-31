//! The one command path (core). Tracking: https://github.com/ifdotpy/layr-studio/issues/630
//!
//! Every write action is a `CommandRequest`. A caller (HTTP edge, agent tools, peer link)
//! runs the same steps:
//!
//! 1. `begin`: check the principal against the permission table, the id, the replay
//!    window, and the replay record. A repeated command returns its first reference.
//! 2. Run the command. `withdraw` runs here, inside the caller's transaction. `send` and
//!    `stop` need provider I/O or the lifecycle runner, so the caller runs them and then
//!    calls `finish`.
//! 3. `finish`: record the reference in `request_results` (not for a create command, whose
//!    row is the receipt) and raise the client's watermark, in the same transaction.
//! 4. `response`: read the referenced rows at response time. They are never stored.

use rusqlite::{Connection, Transaction};
use studio_runtime_schema::command::{
    COMMAND_CLIENT_ID_MAX_CHARS, COMMAND_ROW_MAX_BYTES, Command, CommandRequest, CommandResponse,
    CommandRow, CommandStatus,
};
use studio_runtime_schema::persistence::{
    RequestIdentity, RequestIdentityError, RequestReferenceKind,
};
use studio_runtime_store::command_watermarks::CommandWatermarks;
use studio_runtime_store::request_results::{
    AdmittedRequest, RequestReference, RequestResultError, RequestResults,
};
use studio_runtime_store::{
    AgentId, AgentRepository, QueueWriteRepository, StoreError, sha256_hex, truthy,
};

use crate::principal::{Access, CommandClass, Principal, Scope, access};

/// The text of a conflict between two commands that use one message id.
pub const MESSAGE_ID_CONFLICT: &str = "This message id has different content";

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("This caller may not run this command")]
    Forbidden,
    /// The id is older than the replay window and its number is at or below the watermark:
    /// the command was applied once and must never run again as a new command.
    #[error("This command is older than the replay window and was already applied")]
    TooOld,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("This chat does not exist")]
    NotFound,
    #[error(transparent)]
    RequestResult(#[from] RequestResultError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<rusqlite::Error> for CommandError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}

/// Stable text key of a principal. A client key is `principal_key/client`.
pub fn principal_key(principal: &Principal) -> String {
    match principal {
        Principal::Owner => "owner".into(),
        Principal::Device { id, .. } => format!("device:{id}"),
        Principal::Peer { server } => format!("peer:{server}"),
        Principal::Agent { id, .. } => format!("agent:{id}"),
        Principal::Federated { peer } => format!("federated:{peer}"),
        Principal::Policy => "policy".into(),
    }
}

/// Permission class of a command. Every command of this version controls a chat.
pub fn class_of(_command: &Command) -> CommandClass {
    CommandClass::ChatControl
}

/// A create command makes a row with the request id. That row is its receipt, so it has no
/// `request_results` row.
fn is_create(command: &Command) -> bool {
    matches!(command, Command::Send { .. })
}

/// Check the principal against the permission table and the rows that a scoped answer names.
pub fn authorize(
    tx: &Transaction<'_>,
    principal: &Principal,
    command: &Command,
) -> Result<(), CommandError> {
    match access(principal, class_of(command)) {
        Access::Allow => Ok(()),
        Access::Deny => Err(CommandError::Forbidden),
        Access::Scoped(Scope::Descendant) => {
            let Principal::Agent { id, .. } = principal else {
                return Err(CommandError::Forbidden);
            };
            let below = crate::managed_host_tools::is_descendant(tx, id, command.target())
                .map_err(|_| CommandError::Invalid("Invalid stored agent ancestry"))?;
            below.then_some(()).ok_or(CommandError::Forbidden)
        }
        Access::Scoped(Scope::LinkWorker) => {
            let Principal::Peer { server } = principal else {
                return Err(CommandError::Forbidden);
            };
            let worker = AgentRepository::get(tx, &AgentId::from(command.target()))?
                .ok_or(CommandError::Forbidden)?;
            let linked = worker
                .extra
                .get("remoteWorker")
                .and_then(|remote| remote.get("server"))
                .and_then(serde_json::Value::as_str)
                == Some(server.as_str());
            linked.then_some(()).ok_or(CommandError::Forbidden)
        }
        Access::Scoped(_) => Err(CommandError::Forbidden),
    }
}

/// A command that passed `begin` and has not run yet.
#[derive(Debug)]
pub struct Begun {
    identity: RequestIdentity,
    signature: String,
    client: String,
    number: i64,
    records: bool,
}

#[derive(Debug)]
pub enum Start {
    /// The command ran before. Do not run it again.
    Replay(RequestReference),
    Fresh(Begun),
}

/// Step 1. Call inside the writer transaction, before any effect.
pub fn begin(
    tx: &Transaction<'_>,
    principal: &Principal,
    request: &CommandRequest,
    now_ms: i64,
) -> Result<Start, CommandError> {
    authorize(tx, principal, &request.command)?;
    let number = i64::try_from(request.number)
        .ok()
        .filter(|number| *number >= 1)
        .ok_or(CommandError::Invalid("Command numbers start at 1"))?;
    if request.client.is_empty() || request.client.chars().count() > COMMAND_CLIENT_ID_MAX_CHARS {
        return Err(CommandError::Invalid("Invalid command client id"));
    }
    let who = principal_key(principal);
    let client = format!("{who}/{}", request.client);
    let identity = RequestIdentity::new(
        "command",
        &[who.as_str()],
        &request.id,
        &[request.command.name()],
    )
    .map_err(RequestResultError::from)?;
    let signature = sha256_hex(
        &serde_json::to_vec(&(&who, &request.command)).map_err(RequestResultError::from)?,
    );
    if let Err(error) = identity.validate(now_ms, RequestResults::expired_before_ms(tx)?) {
        let watermark = CommandWatermarks::applied(tx, &client)?;
        return Err(match error {
            RequestIdentityError::Expired { .. } if number <= watermark => CommandError::TooOld,
            error => RequestResultError::from(error).into(),
        });
    }
    let records = !is_create(&request.command);
    if records
        && let Some(reference) =
            RequestResults::replay_reference(tx, identity.id(), &signature, now_ms)?
    {
        return Ok(Start::Replay(reference));
    }
    Ok(Start::Fresh(Begun {
        identity,
        signature,
        client,
        number,
        records,
    }))
}

/// Step 3. Call in the transaction that applied the command.
pub fn finish(
    tx: &Transaction<'_>,
    begun: &Begun,
    reference: &RequestReference,
) -> Result<RequestReference, CommandError> {
    let reference = if begun.records {
        let admitted = AdmittedRequest::from_persisted_fields(
            begun.identity.id().into(),
            begun.signature.clone(),
            begun.identity.issued_ms(),
        );
        RequestResults::settle_reference(tx, &admitted, reference)?
    } else {
        reference.clone()
    };
    CommandWatermarks::advance(tx, &begun.client, begun.number)?;
    Ok(reference)
}

/// Reference to a message (a queue event). The revision counts its changes.
pub fn message_reference(conn: &Connection, id: &str) -> Result<RequestReference, CommandError> {
    Ok(RequestReference {
        kind: RequestReferenceKind::Message,
        id: id.into(),
        revision: CommandWatermarks::event_revision(conn, id)?,
    })
}

/// Reference to a chat. The revision is its epoch.
pub fn agent_reference(tx: &Transaction<'_>, id: &str) -> Result<RequestReference, CommandError> {
    let agent = AgentRepository::get(tx, &AgentId::from(id))?.ok_or(CommandError::NotFound)?;
    Ok(RequestReference {
        kind: RequestReferenceKind::Agent,
        id: id.into(),
        revision: agent.epoch,
    })
}

/// Step 2 for `withdraw`: delete a queued message and return its text.
///
/// TODO(#630): a message that already left the queue is held by a turn. The command must
/// then interrupt that turn and return the text as a server draft. Both need the Turn table
/// of the domain model; until it is on main, such a message is a conflict.
pub fn withdraw(
    tx: &Transaction<'_>,
    chat: &str,
    message: &str,
) -> Result<(RequestReference, String), CommandError> {
    let agent = AgentRepository::get(tx, &AgentId::from(chat))?.ok_or(CommandError::NotFound)?;
    if truthy(agent.extra.get("deletedAt")) {
        return Err(CommandError::Invalid("This agent was deleted"));
    }
    if truthy(agent.extra.get("remoteWorker")) || truthy(agent.extra.get("remoteAnchor")) {
        return Err(CommandError::Invalid(
            "Not implemented: requires cross-server/remote-events",
        ));
    }
    let queued = QueueWriteRepository::pending_events(tx, chat, agent.epoch)?
        .into_iter()
        .find(|event| event.id == message)
        .ok_or(CommandError::Conflict(
            "This message already left the queue",
        ))?;
    if !queued.valid || queued.kind != "user" {
        return Err(CommandError::Invalid(
            "Only a user message can be withdrawn",
        ));
    }
    let text: String = tx.query_row(
        "SELECT text FROM runtime_events WHERE id=?1",
        [message],
        |row| row.get(0),
    )?;
    QueueWriteRepository::update_status(tx, message, "cancelled")?;
    Ok((message_reference(tx, message)?, text))
}

/// Step 4. Read the referenced rows now. `ids` are the entities that the command changed:
/// the reference and its chat. A row above `COMMAND_ROW_MAX_BYTES` is left to the stream.
pub fn response(
    conn: &Connection,
    reference: &RequestReference,
    chat: &str,
) -> Result<CommandResponse, CommandError> {
    let mut statement = conn.prepare_cached(
        "SELECT collection,id,seq,payload FROM sync_entities WHERE id IN (?1,?2) AND deleted=0 ORDER BY collection,id",
    )?;
    let found = statement
        .query_map([reference.id.as_str(), chat], |row| {
            let payload = match row.get_ref(3)? {
                rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
                    bytes.to_vec()
                }
                _ => Vec::new(),
            };
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                payload,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let seq = found.iter().map(|(_, _, seq, _)| *seq).max().unwrap_or(0);
    let rows = found
        .into_iter()
        .filter(|(_, _, _, payload)| payload.len() <= COMMAND_ROW_MAX_BYTES)
        .filter_map(|(collection, id, seq, payload)| {
            Some(CommandRow {
                collection,
                id,
                seq,
                payload: serde_json::from_slice(&payload).ok()?,
            })
        })
        .collect();
    Ok(CommandResponse {
        status: CommandStatus::Applied,
        ref_kind: reference.kind.as_str().into(),
        ref_id: reference.id.clone(),
        rev: reference.revision,
        seq,
        rows,
    })
}
