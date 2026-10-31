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
//!   copies it; [`complete_turn`] and [`turn_completed`] are the only writer and
//!   reader, and the old table is kept read only.

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
            provider_turn TEXT, attempt TEXT, interrupt_ticket TEXT,
            attempt_submitted INTEGER GENERATED ALWAYS AS (attempt->>'$.submitted') STORED,
            attempt_connection_generation INTEGER GENERATED ALWAYS AS (attempt->>'$.connectionGeneration') STORED,
            attempt_action TEXT GENERATED ALWAYS AS (attempt->>'$.action') STORED,
            attempt_active_at_reservation INTEGER GENERATED ALWAYS AS (attempt->>'$.activeAtReservation') STORED) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS turns_one_attempt ON {TURN_TABLE}(chat) WHERE attempt IS NOT NULL;
         CREATE INDEX IF NOT EXISTS turns_callback_context
            ON {TURN_TABLE}(attempt_connection_generation,chat) WHERE attempt IS NOT NULL;
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
         AND (NEW.interrupt_ticket IS NULL OR NEW.attempt IS NOT NULL)",
        in_list(TurnState::VARIANTS),
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

/// The legacy completed-turn table is kept as data and refuses every write. Turn
/// `Completed` is the only live form.
pub const LEGACY_COMPLETED_TURNS_TRIGGERS: [(&str, &str); 3] = [
    ("runtime_completed_turns_read_only_insert", "INSERT"),
    ("runtime_completed_turns_read_only_update", "UPDATE"),
    ("runtime_completed_turns_read_only_delete", "DELETE"),
];

pub fn legacy_completed_turns_read_only_sql() -> String {
    let mut sql = String::new();
    for (name, event) in LEGACY_COMPLETED_TURNS_TRIGGERS {
        sql.push_str(&format!(
            "DROP TRIGGER IF EXISTS {name};
             CREATE TRIGGER {name} BEFORE {event} ON runtime_completed_turns
             BEGIN SELECT RAISE(ABORT,'runtime_completed_turns is read only; use turns'); END;\n"
        ));
    }
    sql
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

fn words(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Create the trigger of each `DROP ...; CREATE ...;` pair in `sql` when it is
/// missing or differs from the stored one. A trigger that is current is not written.
fn ensure_triggers(transaction: &Transaction<'_>, sql: &str) -> StoreResult<()> {
    for pair in sql.split("DROP TRIGGER IF EXISTS ").skip(1) {
        let name = pair.split(';').next().unwrap_or_default().trim();
        let create = pair
            .find("CREATE TRIGGER")
            .map(|start| pair[start..].trim_end_matches([';', '\n', ' ']))
            .ok_or(StoreError::RecordContract("generated trigger has no body"))?;
        let stored: Option<String> = transaction
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if stored.as_deref().map(words) != Some(words(create)) {
            transaction.execute_batch(&format!("DROP TRIGGER IF EXISTS {name}; {create};"))?;
        }
    }
    Ok(())
}

/// Everything the migration installs. It runs at every start, so it writes only
/// what is missing or changed. Returns whether the tables were new.
pub(crate) fn install(transaction: &Transaction<'_>) -> StoreResult<bool> {
    let fresh = !crate::schema::table_exists(transaction, CHAT_TABLE)?;
    transaction.execute_batch(&tables_sql())?;
    ensure_triggers(transaction, &check_triggers_sql())?;
    ensure_triggers(transaction, &owner_delete_trigger_sql())?;
    if crate::schema::table_exists(transaction, "runtime_completed_turns")? {
        ensure_triggers(transaction, &legacy_completed_turns_read_only_sql())?;
    }
    // Slice 1 mirrored `runtime_completed_turns` into turns with a trigger; the
    // table is read only now and its rows are copied by `backfill`.
    let mirror: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='turns_from_completed_turns')",
        [],
        |row| row.get(0),
    )?;
    if mirror {
        transaction.execute_batch("DROP TRIGGER turns_from_completed_turns;")?;
    }
    Ok(fresh)
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
    project_state(connection, id, &fields)?;
    place_attempt(connection, id, &fields)
}

/// The start attempt object of a chat lives on one of its turn rows: the
/// unfinished turn, else the turn that has the attempt's provider turn, else the
/// latest turn. The generated `attempt_*` columns of that row are the typed form
/// that the scheduler queries read.
fn place_attempt(connection: &Connection, chat: &str, fields: &Fields) -> StoreResult<()> {
    let Some(attempt) = fields
        .start_attempt
        .as_ref()
        .filter(|value| value.is_object())
    else {
        connection
            .prepare_cached(
                "UPDATE turns SET attempt=NULL,interrupt_ticket=NULL WHERE chat=?1 AND attempt IS NOT NULL",
            )?
            .execute([chat])?;
        return Ok(());
    };
    let text = serde_json::to_string(attempt)?;
    let ticket = attempt
        .pointer("/interrupt/requestTicket")
        .and_then(Value::as_str);
    let provider = attempt
        .get("turnId")
        .and_then(Value::as_str)
        .or_else(|| attempt.get("observedTurnId").and_then(Value::as_str));
    let target: Option<String> = connection
        .prepare_cached(
            "SELECT id FROM turns WHERE chat=?1
             ORDER BY (state IN ('starting','running')) DESC,(provider_turn IS ?2) DESC,rowid DESC LIMIT 1",
        )?
        .query_row(params![chat, provider], |row| row.get(0))
        .optional()?;
    let target = match target {
        Some(target) => target,
        None => {
            let turn = uuid::Uuid::new_v4().to_string();
            connection
                .prepare_cached(
                    "INSERT INTO turns(id,chat,state,provider_turn) VALUES(?1,?2,'interrupted',?3)",
                )?
                .execute(params![turn, chat, provider])?;
            turn
        }
    };
    connection
        .prepare_cached(
            "UPDATE turns SET attempt=NULL,interrupt_ticket=NULL WHERE chat=?1 AND id<>?2 AND attempt IS NOT NULL",
        )?
        .execute(params![chat, target])?;
    connection
        .prepare_cached(
            "UPDATE turns SET attempt=?3,interrupt_ticket=?4
             WHERE id=?2 AND (attempt IS NOT ?3 OR interrupt_ticket IS NOT ?4)",
        )?
        .execute(params![chat, target, text, ticket])?;
    Ok(())
}

fn project_state(connection: &Connection, id: &str, fields: &Fields) -> StoreResult<()> {
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
            if fields.in_flight == Some(true) {
                // A turn runs on the thread again: the provider connection is back.
                connection
                    .prepare_cached("UPDATE sessions SET state=?2 WHERE id=?1 AND state=?3")?
                    .execute(params![
                        session,
                        SessionState::Ready.column(),
                        SessionState::Lost.column()
                    ])?;
            }
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
    if let Some(turn) = current {
        let same = turn.provider_turn.is_none() || turn.provider_turn == fields.turn_id;
        if same {
            connection
                .prepare_cached(
                    "UPDATE turns SET state=?2,session=?3,provider_turn=?4
                     WHERE id=?1 AND (state IS NOT ?2 OR session IS NOT ?3 OR provider_turn IS NOT ?4)",
                )?
                .execute(params![
                    turn.id.0,
                    state.kind(),
                    state.session().map(|s| s.0.as_str()),
                    fields.turn_id,
                ])?;
            return Ok(());
        }
        finish_turn(connection, &turn.id, "interrupted")?;
    }
    connection
        .prepare_cached(
            "INSERT INTO turns(id,chat,state,session,provider_turn) VALUES(?1,?2,?3,?4,?5)",
        )?
        .execute(params![
            uuid::Uuid::new_v4().to_string(),
            id,
            state.kind(),
            state.session().map(|s| s.0.as_str()),
            fields.turn_id,
        ])?;
    Ok(())
}

/// End an unfinished turn in `outcome` (`completed`, `failed` or `interrupted`).
fn finish_turn(connection: &Connection, turn: &TurnId, outcome: &str) -> StoreResult<()> {
    connection
        .prepare_cached("UPDATE turns SET state=?2,session=NULL WHERE id=?1")?
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

/// Turn `provider_turn` of `chat` is completed. The one writer of Turn `Completed`:
/// it ends the unfinished turn with that provider turn, or records a completed turn
/// when none exists. Repeating it changes nothing.
pub fn complete_turn(connection: &Connection, chat: &str, provider_turn: &str) -> StoreResult<()> {
    let id = format!("{chat}:{provider_turn}");
    connection
        .prepare_cached(
            "UPDATE turns SET state='completed',session=NULL WHERE chat=?1 AND provider_turn=?2",
        )?
        .execute([chat, provider_turn])?;
    connection
        .prepare_cached(
            "INSERT INTO turns(id,chat,state,provider_turn)
             SELECT ?3,?1,'completed',?2
             WHERE NOT EXISTS(SELECT 1 FROM turns WHERE chat=?1 AND provider_turn=?2)",
        )?
        .execute(params![chat, provider_turn, id])?;
    Ok(())
}

/// Whether turn `provider_turn` of `chat` is completed.
pub fn turn_completed(
    connection: &Connection,
    chat: &str,
    provider_turn: &str,
) -> StoreResult<bool> {
    Ok(connection
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE chat=?1 AND provider_turn=?2 AND state='completed')",
        )?
        .query_row([chat, provider_turn], |row| row.get(0))?)
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
        let mut base = json!({"id":id,"rootId":id,"epoch":1,"status":"idle","provider":"codex","compactions":0,
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
            "INSERT INTO turns(id,chat,state,interrupt_ticket) VALUES('t','c','completed','ticket')",
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
        // A state from before the table was read only.
        for (name, _) in LEGACY_COMPLETED_TURNS_TRIGGERS {
            db.execute_batch(&format!("DROP TRIGGER {name}")).unwrap();
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

    #[test]
    fn complete_turn_is_the_one_writer_of_completed_and_is_idempotent() {
        let db = crate::fresh_state::test_canvas();
        put(
            &db,
            &agent(
                "c",
                json!({"status":"running","inFlight":true,"turnId":"p1","threadId":"th"}),
            ),
        );
        assert!(!turn_completed(&db, "c", "p1").unwrap());
        complete_turn(&db, "c", "p1").unwrap();
        complete_turn(&db, "c", "p1").unwrap();
        assert!(turn_completed(&db, "c", "p1").unwrap());
        assert!(!chat_busy(&db, "c").unwrap());
        assert_eq!(read_turns(&db, "c").unwrap().len(), 1);
        complete_turn(&db, "c", "p2").unwrap();
        assert!(turn_completed(&db, "c", "p2").unwrap());
        let legacy: i64 = db
            .query_row("SELECT count(*) FROM runtime_completed_turns", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(legacy, 0, "the legacy table is read only");
    }

    #[test]
    fn a_lost_session_is_ready_again_when_a_turn_runs_on_its_thread() {
        let db = crate::fresh_state::test_canvas();
        put(&db, &agent("c", json!({"threadId":"th"})));
        mark_sessions_lost(&db, "c").unwrap();
        assert_eq!(
            read_sessions(&db, "c").unwrap()[0].state,
            SessionState::Lost
        );
        put(&db, &agent("c", json!({"threadId":"th"})));
        assert_eq!(
            read_sessions(&db, "c").unwrap()[0].state,
            SessionState::Lost
        );
        put(
            &db,
            &agent(
                "c",
                json!({"status":"running","inFlight":true,"turnId":"p","threadId":"th"}),
            ),
        );
        assert_eq!(
            read_sessions(&db, "c").unwrap()[0].state,
            SessionState::Ready
        );
    }

    fn attempt_columns(
        db: &Connection,
        chat: &str,
    ) -> Vec<(Option<i64>, Option<i64>, Option<String>, Option<i64>)> {
        db.prepare(
            "SELECT attempt_submitted,attempt_connection_generation,attempt_action,attempt_active_at_reservation
             FROM turns WHERE chat=?1 AND attempt IS NOT NULL",
        )
        .unwrap()
        .query_map([chat], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    #[test]
    fn the_start_attempt_is_typed_columns_of_one_turn_row_also_after_it_settles() {
        let db = crate::fresh_state::test_canvas();
        let attempt = json!({"id":"a1","submitted":true,"connectionGeneration":4,"action":"capacity","activeAtReservation":false});
        put(
            &db,
            &agent(
                "c",
                json!({"status":"running","inFlight":true,"turnId":"p1","threadId":"th","startAttempt":attempt}),
            ),
        );
        assert_eq!(
            attempt_columns(&db, "c"),
            [(Some(1), Some(4), Some("capacity".into()), Some(0))]
        );
        // The turn ends; the settled attempt stays on that turn row.
        put(
            &db,
            &agent(
                "c",
                json!({"status":"completed","inFlight":false,"threadId":"th","lastCompletedTurn":"p1","lastCompletedTurnStatus":"completed","startAttempt":attempt}),
            ),
        );
        let turns = read_turns(&db, "c").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].state, TurnState::Completed);
        assert!(turns[0].attempt.is_some());
        // A new turn takes the attempt: a chat has one attempt.
        let next = json!({"id":"a2","submitted":false,"connectionGeneration":5});
        put(
            &db,
            &agent(
                "c",
                json!({"status":"running","inFlight":true,"turnId":"p2","threadId":"th","startAttempt":next}),
            ),
        );
        assert_eq!(attempt_columns(&db, "c"), [(Some(0), Some(5), None, None)]);
        assert_eq!(read_turns(&db, "c").unwrap().len(), 2);
        // Removing the attempt from the record clears it.
        put(
            &db,
            &agent(
                "c",
                json!({"status":"idle","inFlight":false,"threadId":"th"}),
            ),
        );
        assert!(attempt_columns(&db, "c").is_empty());
        let error = fail(
            &db,
            "UPDATE turns SET attempt='{}' WHERE provider_turn IN ('p1','p2')",
        );
        assert!(error.contains("UNIQUE"), "{error}");
    }

    #[test]
    fn an_attempt_without_any_turn_gets_an_interrupted_turn_row() {
        let db = crate::fresh_state::test_canvas();
        put(
            &db,
            &agent(
                "c",
                json!({"status":"queued","startAttempt":{"id":"a","turnId":"p9","activeAtReservation":true}}),
            ),
        );
        let turns = read_turns(&db, "c").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].state, TurnState::Interrupted);
        assert_eq!(turns[0].provider_turn.as_deref(), Some("p9"));
        assert_eq!(attempt_columns(&db, "c"), [(None, None, None, Some(1))]);
    }

    #[test]
    fn the_legacy_completed_turn_table_keeps_its_rows_and_refuses_writes() {
        let db = crate::fresh_state::test_canvas();
        for (name, _) in LEGACY_COMPLETED_TURNS_TRIGGERS {
            db.execute_batch(&format!("DROP TRIGGER {name}")).unwrap();
        }
        db.execute(
            "INSERT INTO runtime_completed_turns(id) VALUES('c:old')",
            [],
        )
        .unwrap();
        let tx = Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate).unwrap();
        assert!(!install(&tx).unwrap(), "the tables exist already");
        tx.commit().unwrap();
        for sql in [
            "INSERT INTO runtime_completed_turns(id) VALUES('c:p9')",
            "UPDATE runtime_completed_turns SET id='c:p9'",
            "DELETE FROM runtime_completed_turns",
        ] {
            assert!(fail(&db, sql).contains("read only"), "{sql}");
        }
        let kept: i64 = db
            .query_row("SELECT count(*) FROM runtime_completed_turns", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kept, 1);
        complete_turn(&db, "c", "p9").unwrap();
        assert!(turn_completed(&db, "c", "p9").unwrap());
    }
}
