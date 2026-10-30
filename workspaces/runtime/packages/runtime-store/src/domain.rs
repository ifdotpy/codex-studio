//! Chat, Turn and Session as typed tables (decision 2026-10-28, "Domain model").
//!
//! Slice 1 (issue 630): the tables, their Rust types, generated check triggers,
//! the one-unfinished-turn index, the migration and a projection. The agent JSON
//! record stays the source of truth; [`project`] copies its chat, run and
//! scheduling fields into these rows in the same transaction as every record
//! write, so rows and record cannot differ. Later slices move the writers to the
//! owner functions of this module and remove the fields from the record.
//!
//! One owner per rule:
//! - The Rust enums own the state vocabularies. The check triggers are generated
//!   from them ([`check_triggers_sql`]), so no value list is written twice.
//! - A chat has at most one unfinished turn (`Starting` or `Running`). The partial
//!   unique index [`ONE_UNFINISHED_TURN_INDEX`] enforces it. A chat is busy exactly
//!   when it has such a turn ([`chat_busy`]).
//! - `runtime_completed_turns` is the legacy form of Turn `Completed`. The migration
//!   copies it and a trigger keeps the copy current until its writers move.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use strum::VariantNames;

use crate::records::{AgentExecutionMode, AgentId, AgentStatus};
use crate::{StoreError, StoreResult};

pub const CHAT_TABLE: &str = "chats";
pub const TURN_TABLE: &str = "turns";
pub const SESSION_TABLE: &str = "sessions";
/// At most one row per chat can be in a state of [`TurnState::UNFINISHED`].
pub const ONE_UNFINISHED_TURN_INDEX: &str = "turns_one_unfinished";

/// A chat is the agent row id.
pub type ChatId = AgentId;

macro_rules! row_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}
row_id!(TurnId);
row_id!(SessionId);

/// What the user sees of a conversation. Archived wins over paused.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, VariantNames)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ChatState {
    Active,
    Paused,
    Archived,
}

/// One provider run inside a chat. `Running` carries its session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, VariantNames)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TurnState {
    Starting,
    Running { session: SessionId },
    Completed,
    Failed,
    Interrupted,
}

impl TurnState {
    /// Wire kinds of the states that block the next turn of the chat.
    pub const UNFINISHED: [&'static str; 2] = ["starting", "running"];

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running { .. } => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn is_unfinished(&self) -> bool {
        Self::UNFINISHED.contains(&self.kind())
    }

    fn session(&self) -> Option<&SessionId> {
        match self {
            Self::Running { session } => Some(session),
            _ => None,
        }
    }

    /// Strict decode of the `state` and `session` columns.
    fn from_columns(state: &str, session: Option<String>) -> StoreResult<Self> {
        let session = session.map(SessionId);
        match (state, session) {
            ("starting", None) => Ok(Self::Starting),
            ("running", Some(session)) => Ok(Self::Running { session }),
            ("completed", None) => Ok(Self::Completed),
            ("failed", None) => Ok(Self::Failed),
            ("interrupted", None) => Ok(Self::Interrupted),
            _ => Err(StoreError::RecordContract("invalid stored turn state")),
        }
    }
}

/// One provider thread that runs turns.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, VariantNames)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SessionState {
    Starting,
    Ready,
    Lost,
}

macro_rules! text_enum {
    ($ty:ty, $what:literal) => {
        impl $ty {
            /// Strict decode of the `state` column.
            fn from_column(value: &str) -> StoreResult<Self> {
                <$ty>::deserialize(
                    serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value),
                )
                .map_err(|_| StoreError::RecordContract($what))
            }
            /// The wire value of the variant, as the check trigger lists it.
            fn column(self) -> &'static str {
                <$ty as VariantNames>::VARIANTS[self as usize]
            }
        }
    };
}
text_enum!(ChatState, "invalid stored chat state");
text_enum!(SessionState, "invalid stored session state");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRow {
    pub id: ChatId,
    pub state: ChatState,
    /// Scheduling status; moves out of the record in slice 3.
    pub status: AgentStatus,
    pub auto_wake: Option<bool>,
    pub usage_resume_enabled: Option<bool>,
    pub execution_mode: Option<AgentExecutionMode>,
    pub turn_epoch: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnRow {
    pub id: TurnId,
    pub chat: ChatId,
    pub state: TurnState,
    /// The provider's turn id, once known.
    pub provider_turn: Option<String>,
    /// The start attempt object, while the turn is unfinished.
    pub attempt: Option<String>,
    /// Request ticket of the interrupt that was sent for this turn.
    pub interrupt_ticket: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRow {
    pub id: SessionId,
    pub chat: ChatId,
    pub thread_id: String,
    pub account_key: String,
    pub state: SessionState,
}

fn in_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Tables and indexes. Column lists are the contract of the row types above.
pub fn tables_sql() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {CHAT_TABLE}(
            id TEXT PRIMARY KEY, state TEXT NOT NULL, status TEXT NOT NULL,
            auto_wake INTEGER, usage_resume_enabled INTEGER, execution_mode TEXT, turn_epoch INTEGER) STRICT;
         CREATE TABLE IF NOT EXISTS {SESSION_TABLE}(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, thread_id TEXT NOT NULL,
            account_key TEXT NOT NULL, state TEXT NOT NULL) STRICT;
         CREATE INDEX IF NOT EXISTS sessions_chat ON {SESSION_TABLE}(chat);
         CREATE TABLE IF NOT EXISTS {TURN_TABLE}(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, state TEXT NOT NULL, session TEXT,
            provider_turn TEXT, attempt TEXT, interrupt_ticket TEXT) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS {ONE_UNFINISHED_TURN_INDEX}
            ON {TURN_TABLE}(chat) WHERE state IN ({unfinished});
         CREATE INDEX IF NOT EXISTS turns_provider_turn ON {TURN_TABLE}(chat,provider_turn);",
        unfinished = in_list(&TurnState::UNFINISHED),
    )
}

/// Names of the generated check triggers, in creation order.
pub const CHECK_TRIGGERS: [(&str, &str); 6] = [
    ("chats_check_insert", CHAT_TABLE),
    ("chats_check_update", CHAT_TABLE),
    ("turns_check_insert", TURN_TABLE),
    ("turns_check_update", TURN_TABLE),
    ("sessions_check_insert", SESSION_TABLE),
    ("sessions_check_update", SESSION_TABLE),
];

/// Generated `BEFORE INSERT` and `BEFORE UPDATE` check triggers. A migration
/// replaces them in milliseconds, with no table rebuild.
pub fn check_triggers_sql() -> String {
    let chat = format!(
        "NEW.state IN ({}) AND NEW.status IN ({}) AND COALESCE(NEW.auto_wake,0) IN (0,1) \
         AND COALESCE(NEW.usage_resume_enabled,0) IN (0,1) \
         AND (NEW.execution_mode IS NULL OR NEW.execution_mode IN ('native','host','vm'))",
        in_list(ChatState::VARIANTS),
        in_list(AgentStatus::VALUES),
    );
    let turn = format!(
        "NEW.state IN ({}) AND (NEW.state='running')=(NEW.session IS NOT NULL) \
         AND (NEW.state IN ({}) OR (NEW.attempt IS NULL AND NEW.interrupt_ticket IS NULL))",
        in_list(TurnState::VARIANTS),
        in_list(&TurnState::UNFINISHED),
    );
    let session = format!("NEW.state IN ({})", in_list(SessionState::VARIANTS));
    let mut sql = String::new();
    for ((name, table), condition) in CHECK_TRIGGERS
        .iter()
        .zip([&chat, &chat, &turn, &turn, &session, &session])
    {
        let event = if name.ends_with("insert") {
            "INSERT"
        } else {
            "UPDATE"
        };
        sql.push_str(&format!(
            "DROP TRIGGER IF EXISTS {name};
             CREATE TRIGGER {name} BEFORE {event} ON {table} WHEN NOT ({condition})
             BEGIN SELECT RAISE(ABORT,'invalid {table} state'); END;\n"
        ));
    }
    sql
}

/// Keeps the legacy completed-turn table and Turn `Completed` the same until the
/// writers of the table move. The legacy id is `<chat>:<provider turn>`.
pub const COMPLETED_TURNS_TRIGGER: &str = "turns_from_completed_turns";

pub fn completed_turns_trigger_sql() -> String {
    format!(
        "DROP TRIGGER IF EXISTS {COMPLETED_TURNS_TRIGGER};
         CREATE TRIGGER {COMPLETED_TURNS_TRIGGER} AFTER INSERT ON runtime_completed_turns BEGIN
           UPDATE {TURN_TABLE} SET state='completed',session=NULL,attempt=NULL,interrupt_ticket=NULL
             WHERE chat=substr(NEW.id,1,instr(NEW.id,':')-1)
               AND provider_turn=substr(NEW.id,instr(NEW.id,':')+1);
           INSERT INTO {TURN_TABLE}(id,chat,state,provider_turn)
             SELECT NEW.id,substr(NEW.id,1,instr(NEW.id,':')-1),'completed',substr(NEW.id,instr(NEW.id,':')+1)
             WHERE NOT EXISTS(SELECT 1 FROM {TURN_TABLE}
               WHERE chat=substr(NEW.id,1,instr(NEW.id,':')-1)
                 AND provider_turn=substr(NEW.id,instr(NEW.id,':')+1));
         END;"
    )
}

/// Deleting an agent deletes its chat and sessions and closes its unfinished turn.
/// Finished turns stay, as `runtime_completed_turns` rows do today.
pub const OWNER_DELETE_TRIGGER: &str = "chats_owner_delete";

pub fn owner_delete_trigger_sql() -> String {
    format!(
        "DROP TRIGGER IF EXISTS {OWNER_DELETE_TRIGGER};
         CREATE TRIGGER {OWNER_DELETE_TRIGGER} AFTER DELETE ON runtime_agents BEGIN
           UPDATE {TURN_TABLE} SET state='interrupted',session=NULL,attempt=NULL,interrupt_ticket=NULL
             WHERE chat=OLD.id AND state IN ({unfinished});
           DELETE FROM {SESSION_TABLE} WHERE chat=OLD.id;
           DELETE FROM {CHAT_TABLE} WHERE id=OLD.id;
         END;",
        unfinished = in_list(&TurnState::UNFINISHED),
    )
}

/// Everything the migration installs. Safe to run again.
pub(crate) fn install(transaction: &Transaction<'_>) -> StoreResult<()> {
    transaction.execute_batch(&tables_sql())?;
    transaction.execute_batch(&check_triggers_sql())?;
    transaction.execute_batch(&owner_delete_trigger_sql())?;
    if crate::schema::table_exists(transaction, "runtime_completed_turns")? {
        transaction.execute_batch(&completed_turns_trigger_sql())?;
    }
    Ok(())
}

/// Copy every completed turn and project every agent, in the caller's transaction.
/// A row that cannot be projected fails the migration; nothing is skipped.
pub(crate) fn backfill(transaction: &Transaction<'_>) -> StoreResult<()> {
    if crate::schema::table_exists(transaction, "runtime_completed_turns")? {
        let malformed: i64 = transaction.query_row(
            "SELECT count(*) FROM runtime_completed_turns WHERE instr(id,':')<2 OR instr(id,':')=length(id)",
            [],
            |row| row.get(0),
        )?;
        if malformed != 0 {
            return Err(StoreError::RecordContract(
                "completed turn id is not <chat>:<turn>",
            ));
        }
        transaction.execute(
            "INSERT OR IGNORE INTO turns(id,chat,state,provider_turn)
             SELECT id,substr(id,1,instr(id,':')-1),'completed',substr(id,instr(id,':')+1)
             FROM runtime_completed_turns",
            [],
        )?;
    }
    let mut statement = transaction.prepare("SELECT id,record FROM runtime_agents")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let record: String = row.get(1)?;
        let value: Value = serde_json::from_str(&record)?;
        project(transaction, &id, &value)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fields {
    status: String,
    in_flight: Option<bool>,
    auto_wake: Option<bool>,
    usage_resume_enabled: Option<bool>,
    execution_mode: Option<AgentExecutionMode>,
    turn_epoch: Option<i64>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    account_key: Option<String>,
    start_attempt: Option<Value>,
    agent_archive: Option<Value>,
    last_completed_turn: Option<String>,
    last_completed_turn_status: Option<String>,
}

/// The truthiness that the agent writers and `agent_archived` use.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

/// Write the chat, session and turn rows that follow from agent record `record`.
/// Called inside every record write; an unchanged row is not written.
pub(crate) fn project(connection: &Connection, id: &str, record: &Value) -> StoreResult<()> {
    let fields = Fields::deserialize(record)
        .map_err(|_| StoreError::RecordContract("agent record fields do not decode"))?;
    if !AgentStatus::VALUES.contains(&fields.status.as_str()) {
        return Err(StoreError::RecordContract("unknown agent status"));
    }
    let state = if truthy(fields.agent_archive.as_ref()) {
        ChatState::Archived
    } else if fields.status == "paused" {
        ChatState::Paused
    } else {
        ChatState::Active
    };
    connection
        .prepare_cached(
            "INSERT INTO chats(id,state,status,auto_wake,usage_resume_enabled,execution_mode,turn_epoch)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(id) DO UPDATE SET state=excluded.state,status=excluded.status,
               auto_wake=excluded.auto_wake,usage_resume_enabled=excluded.usage_resume_enabled,
               execution_mode=excluded.execution_mode,turn_epoch=excluded.turn_epoch
             WHERE state IS NOT excluded.state OR status IS NOT excluded.status
               OR auto_wake IS NOT excluded.auto_wake
               OR usage_resume_enabled IS NOT excluded.usage_resume_enabled
               OR execution_mode IS NOT excluded.execution_mode OR turn_epoch IS NOT excluded.turn_epoch",
        )?
        .execute(params![
            id,
            state.column(),
            fields.status,
            fields.auto_wake,
            fields.usage_resume_enabled,
            fields.execution_mode.map(|mode| match mode {
                AgentExecutionMode::Native => "native",
                AgentExecutionMode::Host => "host",
                AgentExecutionMode::Vm => "vm",
            }),
            fields.turn_epoch,
        ])?;

    let session = match (&fields.thread_id, &fields.account_key) {
        (Some(thread), Some(account)) => {
            let session = format!("{id}:{thread}");
            connection
                .prepare_cached(
                    "UPDATE sessions SET state='lost' WHERE chat=?1 AND id<>?2 AND state<>'lost'",
                )?
                .execute(params![id, session])?;
            connection
                .prepare_cached(
                    "INSERT INTO sessions(id,chat,thread_id,account_key,state) VALUES(?1,?2,?3,?4,?5)
                     ON CONFLICT(id) DO UPDATE SET account_key=excluded.account_key
                     WHERE account_key IS NOT excluded.account_key",
                )?
                .execute(params![session, id, thread, account, SessionState::Ready.column()])?;
            Some(SessionId(session))
        }
        _ => None,
    };

    let current = unfinished_turn(connection, id)?;
    let live = fields.in_flight == Some(true);
    if !live {
        if let Some(turn) = current {
            let outcome = match (&turn.provider_turn, &fields.last_completed_turn) {
                (Some(provider), Some(last)) if provider == last => {
                    match fields.last_completed_turn_status.as_deref() {
                        Some("completed") => "completed",
                        Some("failed") => "failed",
                        _ => "interrupted",
                    }
                }
                _ => "interrupted",
            };
            finish_turn(connection, &turn.id, outcome)?;
        }
        return Ok(());
    }
    let state = match (&fields.turn_id, session) {
        (Some(_), Some(session)) => TurnState::Running { session },
        _ => TurnState::Starting,
    };
    let attempt = fields
        .start_attempt
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let ticket = fields
        .start_attempt
        .as_ref()
        .and_then(|attempt| attempt.pointer("/interrupt/requestTicket"))
        .and_then(Value::as_str);
    if let Some(turn) = current {
        let same = turn.provider_turn.is_none() || turn.provider_turn == fields.turn_id;
        if same {
            connection
                .prepare_cached(
                    "UPDATE turns SET state=?2,session=?3,provider_turn=?4,attempt=?5,interrupt_ticket=?6
                     WHERE id=?1 AND (state IS NOT ?2 OR session IS NOT ?3 OR provider_turn IS NOT ?4
                       OR attempt IS NOT ?5 OR interrupt_ticket IS NOT ?6)",
                )?
                .execute(params![
                    turn.id.0,
                    state.kind(),
                    state.session().map(|s| s.0.as_str()),
                    fields.turn_id,
                    attempt,
                    ticket,
                ])?;
            return Ok(());
        }
        finish_turn(connection, &turn.id, "interrupted")?;
    }
    connection
        .prepare_cached(
            "INSERT INTO turns(id,chat,state,session,provider_turn,attempt,interrupt_ticket)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
        )?
        .execute(params![
            uuid::Uuid::new_v4().to_string(),
            id,
            state.kind(),
            state.session().map(|s| s.0.as_str()),
            fields.turn_id,
            attempt,
            ticket,
        ])?;
    Ok(())
}

/// End an unfinished turn in `outcome` (`completed`, `failed` or `interrupted`).
fn finish_turn(connection: &Connection, turn: &TurnId, outcome: &str) -> StoreResult<()> {
    connection
        .prepare_cached(
            "UPDATE turns SET state=?2,session=NULL,attempt=NULL,interrupt_ticket=NULL WHERE id=?1",
        )?
        .execute(params![turn.0, outcome])?;
    Ok(())
}

type RawTurn = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn turn_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawTurn> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

const TURN_COLUMNS: &str = "id,chat,state,session,provider_turn,attempt,interrupt_ticket";

fn decode_turn(raw: RawTurn) -> StoreResult<TurnRow> {
    let (id, chat, state, session, provider_turn, attempt, interrupt_ticket) = raw;
    Ok(TurnRow {
        id: TurnId(id),
        chat: AgentId(chat),
        state: TurnState::from_columns(&state, session)?,
        provider_turn,
        attempt,
        interrupt_ticket,
    })
}

/// The unfinished turn of a chat, if any.
pub fn unfinished_turn(connection: &Connection, chat: &str) -> StoreResult<Option<TurnRow>> {
    connection
        .prepare_cached(&format!(
            "SELECT {TURN_COLUMNS} FROM turns WHERE chat=?1 AND state IN ({})",
            in_list(&TurnState::UNFINISHED)
        ))?
        .query_row([chat], turn_row)
        .optional()?
        .map(decode_turn)
        .transpose()
}

/// A chat is busy exactly when it has an unfinished turn.
pub fn chat_busy(connection: &Connection, chat: &str) -> StoreResult<bool> {
    Ok(connection
        .prepare_cached(&format!(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE chat=?1 AND state IN ({}))",
            in_list(&TurnState::UNFINISHED)
        ))?
        .query_row([chat], |row| row.get(0))?)
}

pub fn read_turns(connection: &Connection, chat: &str) -> StoreResult<Vec<TurnRow>> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT {TURN_COLUMNS} FROM turns WHERE chat=?1 ORDER BY rowid"
    ))?;
    let rows = statement.query_map([chat], turn_row)?;
    rows.map(|raw| decode_turn(raw?)).collect()
}

pub fn read_chat(connection: &Connection, id: &str) -> StoreResult<Option<ChatRow>> {
    let raw = connection
        .prepare_cached(
            "SELECT id,state,status,auto_wake,usage_resume_enabled,execution_mode,turn_epoch
             FROM chats WHERE id=?1",
        )?
        .query_row([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<bool>>(3)?,
                row.get::<_, Option<bool>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })
        .optional()?;
    raw.map(
        |(id, state, status, auto_wake, usage_resume_enabled, mode, turn_epoch)| {
            let status = match serde_json::from_value::<AgentStatus>(Value::String(status))? {
                AgentStatus::Other(_) => {
                    return Err(StoreError::RecordContract("invalid stored chat status"));
                }
                known => known,
            };
            let execution_mode = mode
                .map(|mode| serde_json::from_value(Value::String(mode)))
                .transpose()?;
            Ok(ChatRow {
                id: AgentId(id),
                state: ChatState::from_column(&state)?,
                status,
                auto_wake,
                usage_resume_enabled,
                execution_mode,
                turn_epoch,
            })
        },
    )
    .transpose()
}

pub fn read_sessions(connection: &Connection, chat: &str) -> StoreResult<Vec<SessionRow>> {
    let mut statement = connection.prepare_cached(
        "SELECT id,chat,thread_id,account_key,state FROM sessions WHERE chat=?1 ORDER BY rowid",
    )?;
    let rows = statement.query_map([chat], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    rows.map(|raw| {
        let (id, chat, thread_id, account_key, state) = raw?;
        Ok(SessionRow {
            id: SessionId(id),
            chat: AgentId(chat),
            thread_id,
            account_key,
            state: SessionState::from_column(&state)?,
        })
    })
    .collect()
}

/// A lost provider connection: every ready session of the chat is `Lost`.
pub fn mark_sessions_lost(connection: &Connection, chat: &str) -> StoreResult<usize> {
    Ok(connection
        .prepare_cached("UPDATE sessions SET state='lost' WHERE chat=?1 AND state<>'lost'")?
        .execute([chat])?)
}

/// Tables of this format as the agents repository's schema check lists them.
pub(crate) const TABLE_REQUIREMENTS: [crate::schema::TableRequirement; 3] = [
    crate::schema::TableRequirement {
        name: CHAT_TABLE,
        columns: &[
            "id",
            "state",
            "status",
            "auto_wake",
            "usage_resume_enabled",
            "execution_mode",
            "turn_epoch",
        ],
    },
    crate::schema::TableRequirement {
        name: TURN_TABLE,
        columns: &[
            "id",
            "chat",
            "state",
            "session",
            "provider_turn",
            "attempt",
            "interrupt_ticket",
        ],
    },
    crate::schema::TableRequirement {
        name: SESSION_TABLE,
        columns: &["id", "chat", "thread_id", "account_key", "state"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(id: &str, extra: Value) -> Value {
        let mut base = json!({"id":id,"rootId":id,"epoch":1,"status":"idle","provider":"codex",
            "accountKey":"default","autoWake":true,"turnEpoch":0,"inFlight":false});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    fn put(db: &Connection, value: &Value) {
        let record = crate::records::AgentRecord::from_value(value.clone()).expect("record");
        crate::AgentRepository::put(db, &record).expect("put");
    }

    fn fail(db: &Connection, sql: &str) -> String {
        db.execute(sql, []).expect_err(sql).to_string()
    }

    #[test]
    fn two_unfinished_turns_of_one_chat_are_rejected() {
        let db = crate::fresh_state::test_canvas();
        db.execute(
            "INSERT INTO turns(id,chat,state) VALUES('t1','c','starting')",
            [],
        )
        .unwrap();
        let error = fail(
            &db,
            "INSERT INTO turns(id,chat,state) VALUES('t2','c','starting')",
        );
        assert!(error.contains("UNIQUE"), "{error}");
        db.execute(
            "INSERT INTO turns(id,chat,state) VALUES('t3','other','starting')",
            [],
        )
        .unwrap();
        db.execute("UPDATE turns SET state='completed' WHERE id='t1'", [])
            .unwrap();
        db.execute(
            "INSERT INTO turns(id,chat,state) VALUES('t4','c','starting')",
            [],
        )
        .unwrap();
        let error = fail(
            &db,
            "INSERT INTO turns(id,chat,state,session) VALUES('t5','c','running','s')",
        );
        assert!(error.contains("UNIQUE"), "{error}");
        assert!(chat_busy(&db, "c").unwrap());
        db.execute("UPDATE turns SET state='failed' WHERE id='t4'", [])
            .unwrap();
        assert!(!chat_busy(&db, "c").unwrap());
    }

    #[test]
    fn invalid_states_are_rejected_by_the_generated_triggers() {
        let db = crate::fresh_state::test_canvas();
        for sql in [
            "INSERT INTO chats(id,state,status) VALUES('c','deleted','idle')",
            "INSERT INTO chats(id,state,status) VALUES('c','active','nonsense')",
            "INSERT INTO chats(id,state,status,execution_mode) VALUES('c','active','idle','cloud')",
            "INSERT INTO turns(id,chat,state) VALUES('t','c','bogus')",
            "INSERT INTO turns(id,chat,state) VALUES('t','c','running')",
            "INSERT INTO turns(id,chat,state,session) VALUES('t','c','starting','s')",
            "INSERT INTO turns(id,chat,state,attempt) VALUES('t','c','completed','{}')",
            "INSERT INTO sessions(id,chat,thread_id,account_key,state) VALUES('s','c','t','a','dead')",
        ] {
            let error = fail(&db, sql);
            assert!(error.contains("invalid"), "{sql}: {error}");
        }
        db.execute(
            "INSERT INTO chats(id,state,status) VALUES('c','active','idle')",
            [],
        )
        .unwrap();
        let error = fail(&db, "UPDATE chats SET state='gone' WHERE id='c'");
        assert!(error.contains("invalid chats state"), "{error}");
        db.execute(
            "INSERT INTO turns(id,chat,state) VALUES('t','c','starting')",
            [],
        )
        .unwrap();
        let error = fail(&db, "UPDATE turns SET state='running' WHERE id='t'");
        assert!(error.contains("invalid turns state"), "{error}");
    }

    #[test]
    fn installed_triggers_are_the_generated_ones() {
        let db = crate::fresh_state::test_canvas();
        let generated = check_triggers_sql();
        for (name, table) in CHECK_TRIGGERS {
            let stored: String = db
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1 AND tbl_name=?2",
                    [name, table],
                    |row| row.get(0),
                )
                .unwrap();
            let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(words(&generated).contains(&words(&stored)), "{name}");
        }
        let kinds: Vec<_> = TurnState::UNFINISHED.to_vec();
        for kind in TurnState::VARIANTS {
            let unfinished = match *kind {
                "running" => TurnState::Running {
                    session: SessionId(String::new()),
                },
                "starting" => TurnState::Starting,
                "completed" => TurnState::Completed,
                "failed" => TurnState::Failed,
                _ => TurnState::Interrupted,
            };
            assert_eq!(kinds.contains(kind), unfinished.is_unfinished(), "{kind}");
            assert_eq!(unfinished.kind(), *kind);
        }
    }

    #[test]
    fn migration_projects_every_agent_and_keeps_completed_turns() {
        let db = crate::fresh_state::test_canvas();
        let rows = [
            agent("idle", json!({})),
            agent("paused", json!({"status":"paused","autoWake":false})),
            agent(
                "archived",
                json!({"status":"paused","agentArchive":{"at":1}}),
            ),
            agent(
                "starting",
                json!({"status":"starting","inFlight":true,"executionMode":"vm"}),
            ),
            agent(
                "running",
                json!({"status":"running","inFlight":true,"turnId":"p1","threadId":"th",
                    "startAttempt":{"id":"a1","interrupt":{"requestTicket":"tk"}}}),
            ),
            agent(
                "settled",
                json!({"status":"completed","startAttempt":{"id":"old"},"usageResumeEnabled":true}),
            ),
        ];
        for row in &rows {
            let id = row["id"].as_str().unwrap();
            db.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                params![id, row.to_string()],
            )
            .unwrap();
        }
        db.execute_batch("INSERT INTO runtime_completed_turns(id) VALUES('idle:x1'),('idle:x2')")
            .unwrap();
        let tx = Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate).unwrap();
        crate::migrations::m2026_10_30_domain_model_for_tests(&tx).unwrap();
        tx.commit().unwrap();

        let states: Vec<_> = rows
            .iter()
            .map(|row| {
                let chat = read_chat(&db, row["id"].as_str().unwrap())
                    .unwrap()
                    .unwrap();
                (chat.id.0, chat.state, chat.status.as_str().to_owned())
            })
            .collect();
        use ChatState::*;
        assert_eq!(
            states,
            [
                ("idle".into(), Active, "idle".into()),
                ("paused".into(), Paused, "paused".into()),
                ("archived".into(), Archived, "paused".into()),
                ("starting".into(), Active, "starting".into()),
                ("running".into(), Active, "running".into()),
                ("settled".into(), Active, "completed".into()),
            ]
        );
        assert_eq!(
            read_chat(&db, "starting").unwrap().unwrap().execution_mode,
            Some(AgentExecutionMode::Vm)
        );
        assert_eq!(
            read_chat(&db, "settled")
                .unwrap()
                .unwrap()
                .usage_resume_enabled,
            Some(true)
        );
        assert_eq!(
            unfinished_turn(&db, "starting").unwrap().unwrap().state,
            TurnState::Starting
        );
        let running = unfinished_turn(&db, "running").unwrap().unwrap();
        assert_eq!(
            running.state,
            TurnState::Running {
                session: SessionId("running:th".into())
            }
        );
        assert_eq!(running.interrupt_ticket.as_deref(), Some("tk"));
        for idle in ["idle", "paused", "archived", "settled"] {
            assert!(!chat_busy(&db, idle).unwrap(), "{idle}");
        }
        let completed = read_turns(&db, "idle").unwrap();
        assert_eq!(completed.len(), 2);
        assert!(
            completed
                .iter()
                .all(|turn| turn.state == TurnState::Completed)
        );
        assert_eq!(
            read_sessions(&db, "running").unwrap()[0].state,
            SessionState::Ready
        );
        // A second run changes nothing.
        let tx = Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate).unwrap();
        crate::migrations::m2026_10_30_domain_model_for_tests(&tx).unwrap();
        tx.commit().unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 4);
    }

    /// A recorded command sequence: each step is the agent record that the command
    /// leaves, with the rows that must follow. Start and interrupt are both covered.
    #[test]
    fn replayed_start_and_interrupt_commands_produce_the_recorded_rows() {
        struct Step {
            command: &'static str,
            record: Value,
            turns: Vec<(&'static str, Option<&'static str>)>,
            busy: bool,
        }
        let steps = [
            Step {
                command: "start",
                record: agent(
                    "c",
                    json!({"status":"starting","inFlight":true,"startAttempt":{"id":"a1"}}),
                ),
                turns: vec![("starting", None)],
                busy: true,
            },
            Step {
                command: "observe_started",
                record: agent(
                    "c",
                    json!({"status":"running","inFlight":true,"turnId":"p1","threadId":"th","startAttempt":{"id":"a1"}}),
                ),
                turns: vec![("running", Some("p1"))],
                busy: true,
            },
            Step {
                command: "interrupt",
                record: agent(
                    "c",
                    json!({"status":"running","inFlight":true,"turnId":"p1","threadId":"th","startAttempt":{"id":"a1","interrupt":{"requestTicket":"i1","outcome":"unsent"}}}),
                ),
                turns: vec![("running", Some("p1"))],
                busy: true,
            },
            Step {
                command: "interrupt_accepted",
                record: agent(
                    "c",
                    json!({"status":"paused","inFlight":false,"turnId":null,"threadId":"th","lastCompletedTurn":"p1","lastCompletedTurnStatus":"interrupted"}),
                ),
                turns: vec![("interrupted", Some("p1"))],
                busy: false,
            },
            Step {
                command: "start",
                record: agent(
                    "c",
                    json!({"status":"running","inFlight":true,"turnId":"p2","threadId":"th"}),
                ),
                turns: vec![("interrupted", Some("p1")), ("running", Some("p2"))],
                busy: true,
            },
            Step {
                command: "complete",
                record: agent(
                    "c",
                    json!({"status":"idle","inFlight":false,"threadId":"th","lastCompletedTurn":"p2","lastCompletedTurnStatus":"completed"}),
                ),
                turns: vec![("interrupted", Some("p1")), ("completed", Some("p2"))],
                busy: false,
            },
        ];
        let db = crate::fresh_state::test_canvas();
        for step in steps {
            put(&db, &step.record);
            let rows: Vec<_> = read_turns(&db, "c")
                .unwrap()
                .into_iter()
                .map(|turn| (turn.state.kind(), turn.provider_turn))
                .collect();
            let expected: Vec<_> = step
                .turns
                .iter()
                .map(|(kind, turn)| (*kind, turn.map(str::to_owned)))
                .collect();
            assert_eq!(rows, expected, "after {}", step.command);
            assert_eq!(
                chat_busy(&db, "c").unwrap(),
                step.busy,
                "after {}",
                step.command
            );
        }
        let turn = &read_turns(&db, "c").unwrap()[0];
        assert!(turn.attempt.is_none() && turn.interrupt_ticket.is_none());
    }

    #[test]
    fn the_interrupt_ticket_and_attempt_live_on_the_unfinished_turn() {
        let db = crate::fresh_state::test_canvas();
        put(
            &db,
            &agent(
                "c",
                json!({"status":"running","inFlight":true,"turnId":"p1","threadId":"th",
                "startAttempt":{"id":"a1","interrupt":{"requestTicket":"i1"}}}),
            ),
        );
        let turn = unfinished_turn(&db, "c").unwrap().unwrap();
        assert_eq!(turn.interrupt_ticket.as_deref(), Some("i1"));
        let attempt: Value = serde_json::from_str(turn.attempt.as_deref().unwrap()).unwrap();
        assert_eq!(attempt["id"], "a1");
    }

    #[test]
    fn the_legacy_completed_turn_table_completes_the_unfinished_turn() {
        let db = crate::fresh_state::test_canvas();
        put(
            &db,
            &agent(
                "c",
                json!({"status":"running","inFlight":true,"turnId":"p1","threadId":"th"}),
            ),
        );
        db.execute("INSERT INTO runtime_completed_turns(id) VALUES('c:p1')", [])
            .unwrap();
        assert!(!chat_busy(&db, "c").unwrap());
        let turns = read_turns(&db, "c").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].state, TurnState::Completed);
        db.execute("INSERT INTO runtime_completed_turns(id) VALUES('c:p9')", [])
            .unwrap();
        assert_eq!(read_turns(&db, "c").unwrap().len(), 2);
    }

    #[test]
    fn a_new_thread_loses_the_old_session_and_deleting_the_agent_closes_the_chat() {
        let db = crate::fresh_state::test_canvas();
        put(&db, &agent("c", json!({"threadId":"one"})));
        put(
            &db,
            &agent(
                "c",
                json!({"threadId":"two","status":"running","inFlight":true,"turnId":"p"}),
            ),
        );
        let states: Vec<_> = read_sessions(&db, "c")
            .unwrap()
            .into_iter()
            .map(|s| (s.thread_id, s.state))
            .collect();
        assert_eq!(
            states,
            [
                ("one".into(), SessionState::Lost),
                ("two".into(), SessionState::Ready)
            ]
        );
        assert_eq!(mark_sessions_lost(&db, "c").unwrap(), 1);
        db.execute("DELETE FROM runtime_agents WHERE id='c'", [])
            .unwrap();
        assert!(read_chat(&db, "c").unwrap().is_none());
        assert!(read_sessions(&db, "c").unwrap().is_empty());
        assert!(!chat_busy(&db, "c").unwrap());
        assert_eq!(
            read_turns(&db, "c").unwrap()[0].state,
            TurnState::Interrupted
        );
    }
}
