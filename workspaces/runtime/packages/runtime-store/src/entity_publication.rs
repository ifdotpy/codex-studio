//! Default client publication of agent and request rows.
//!
//! The renderer reads chats and cards only from their `sync_entities` rows, and
//! a pull does not rebuild a complete projection. So every writer transaction
//! publishes the `runtime_agents` and `runtime_requests` rows it changed, in the
//! same transaction and before it commits, unless the transaction already
//! published that entity after its last change. The projection itself belongs
//! to runtime-services, which installs it with
//! [`crate::RuntimeStore::install_entity_publisher`].
//!
//! The writer's update hook journals each changed row. Entity writers journal
//! each publication. A partial projection write that cannot recompute derived
//! view fields journals a stale mark instead, which asks for a full
//! publication. Volatile agent fields (`agent_volatile`) are outside this
//! rule: their writers leave `runtime_agents` alone on purpose.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{Connection, OptionalExtension, Transaction, functions::FunctionFlags};

use crate::StoreResult;

/// The projection that publishes the rows a writer transaction changed.
pub trait EntityPublisher: Send + Sync {
    /// Publish the client views of `pending` inside `transaction`, before it commits.
    fn publish(&self, transaction: &Transaction<'_>, pending: &PendingEntities) -> StoreResult<()>;
}

/// Agent and request ids whose client views are older than their stored rows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PendingEntities {
    pub agents: BTreeSet<String>,
    pub requests: BTreeSet<String>,
    pub receipt_events: BTreeSet<String>,
    pub receipt_items: BTreeSet<String>,
    pub plans: BTreeSet<String>,
    pub annotations: BTreeSet<String>,
    pub tasks: BTreeSet<String>,
    pub work: BTreeSet<String>,
    pub work_changes: BTreeMap<String, WorkSourceChanges>,
    pub work_refusal_owners: BTreeSet<String>,
    /// Source rows of the derived Studio error list that changed.
    pub studio_errors: StudioErrorSources,
    pub queues: BTreeSet<String>,
    pub terminals: BTreeSet<String>,
}

impl PendingEntities {
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
            && self.requests.is_empty()
            && self.receipt_events.is_empty()
            && self.receipt_items.is_empty()
            && self.plans.is_empty()
            && self.annotations.is_empty()
            && self.tasks.is_empty()
            && self.work.is_empty()
            && self.work_refusal_owners.is_empty()
            && self.studio_errors.is_empty()
            && self.queues.is_empty()
            && self.terminals.is_empty()
    }
}

/// Canonical work history fields and their scoped entity collections.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum WorkHistoryKind {
    Result,
    Decision,
    Release,
}
impl WorkHistoryKind {
    pub const ALL: [Self; 3] = [Self::Result, Self::Decision, Self::Release];
    pub const fn field(self) -> &'static str {
        match self {
            Self::Result => "results",
            Self::Decision => "decisions",
            Self::Release => "releases",
        }
    }
    pub const fn collection(self) -> &'static str {
        match self {
            Self::Result => "workResult",
            Self::Decision => "workDecision",
            Self::Release => "workRelease",
        }
    }
}

/// An authoritative OLD/NEW source delta recorded by SQLite, never supplied by callers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkSourceChanges {
    histories: BTreeMap<WorkHistoryKind, BTreeSet<usize>>,
    refusal: bool,
}
impl WorkSourceChanges {
    pub fn histories(&self) -> &BTreeMap<WorkHistoryKind, BTreeSet<usize>> {
        &self.histories
    }
    pub fn refusal_changed(&self) -> bool {
        self.refusal
    }
}

/// Connection-local function identities used only by the source journal triggers.
pub(crate) const WORK_HISTORY_FUNCTION: &str = "sync_work_history_change";
pub(crate) const WORK_IDENTITY_FUNCTION: &str = "sync_work_identity_change";

pub(crate) fn register_work_change_functions(connection: &Connection) -> crate::StoreResult<()> {
    connection.create_scalar_function(
        WORK_HISTORY_FUNCTION,
        3,
        FunctionFlags::empty(),
        |context| {
            let id: String = context.get(0)?;
            let field: String = context.get(1)?;
            let index: i64 = context.get(2)?;
            let kind = WorkHistoryKind::ALL
                .into_iter()
                .find(|kind| kind.field() == field)
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let index = usize::try_from(index).map_err(|_| rusqlite::Error::InvalidQuery)?;
            push(Event::WorkHistory(id, kind, index));
            Ok(0)
        },
    )?;
    connection.create_scalar_function(
        WORK_IDENTITY_FUNCTION,
        2,
        FunctionFlags::empty(),
        |context| {
            match (
                context.get::<Option<String>>(0)?,
                context.get::<Option<String>>(1)?,
            ) {
                (Some(owner), None) => push(Event::WorkRefusalOwner(owner)),
                (None, Some(task)) => push(Event::WorkRefusalTask(task)),
                _ => return Err(rusqlite::Error::InvalidQuery),
            }
            Ok(0)
        },
    )?;
    Ok(())
}

/// Classify changes while OLD and NEW are already owned by the SQLite write.
/// Metadata writes compare source fields but never enumerate history entries.
pub(crate) fn prepare_work_change_journal(transaction: &Transaction<'_>) -> crate::StoreResult<()> {
    for kind in WorkHistoryKind::ALL {
        let field = kind.field();
        transaction.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS work_history_{field}_insert AFTER INSERT ON runtime_work BEGIN
             SELECT {WORK_HISTORY_FUNCTION}(NEW.id,'{field}',j.key) FROM json_each(NEW.record,'$.{field}') j; END;
             CREATE TRIGGER IF NOT EXISTS work_history_{field}_update AFTER UPDATE OF record ON runtime_work
             WHEN json_extract(OLD.record,'$.{field}') IS NOT json_extract(NEW.record,'$.{field}') OR json_extract(OLD.record,'$.rootId') IS NOT json_extract(NEW.record,'$.rootId') BEGIN
             SELECT {WORK_HISTORY_FUNCTION}(NEW.id,'{field}',changed.key) FROM (
               WITH old_values AS MATERIALIZED (SELECT key,value,type FROM json_each(OLD.record,'$.{field}')),
                    new_values AS MATERIALIZED (SELECT key,value,type FROM json_each(NEW.record,'$.{field}'))
               SELECT n.key FROM new_values n LEFT JOIN old_values o USING(key)
               WHERE o.key IS NULL OR o.type IS NOT n.type OR o.value IS NOT n.value OR json_extract(OLD.record,'$.rootId') IS NOT json_extract(NEW.record,'$.rootId')
               UNION SELECT o.key FROM old_values o LEFT JOIN new_values n USING(key) WHERE n.key IS NULL
             ) changed; END;"
        ))?;
    }
    transaction.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS work_identity_insert AFTER INSERT ON runtime_work BEGIN SELECT {WORK_IDENTITY_FUNCTION}(NULL,NEW.id); END;
         CREATE TRIGGER IF NOT EXISTS work_identity_update AFTER UPDATE OF record ON runtime_work
         WHEN json_extract(OLD.record,'$.owner') IS NOT json_extract(NEW.record,'$.owner') OR json_extract(OLD.record,'$.version') IS NOT json_extract(NEW.record,'$.version') BEGIN SELECT {WORK_IDENTITY_FUNCTION}(NULL,NEW.id); END;
         CREATE TRIGGER IF NOT EXISTS work_owner_identity_update AFTER UPDATE OF record ON runtime_agents
         WHEN json_extract(OLD.record,'$.epoch') IS NOT json_extract(NEW.record,'$.epoch')
           OR json_extract(OLD.record,'$.accountKey') IS NOT json_extract(NEW.record,'$.accountKey')
           OR json_extract(OLD.record,'$.threadId') IS NOT json_extract(NEW.record,'$.threadId')
           OR json_extract(OLD.record,'$.turnId') IS NOT json_extract(NEW.record,'$.turnId')
         BEGIN SELECT {WORK_IDENTITY_FUNCTION}(NEW.id,NULL); END;
         CREATE TRIGGER IF NOT EXISTS work_attempt_identity_insert AFTER INSERT ON runtime_execution_runs BEGIN SELECT {WORK_IDENTITY_FUNCTION}(NEW.agent,NULL); END;
         CREATE TRIGGER IF NOT EXISTS work_attempt_identity_delete AFTER DELETE ON runtime_execution_runs BEGIN SELECT {WORK_IDENTITY_FUNCTION}(OLD.agent,NULL); END;
         CREATE TRIGGER IF NOT EXISTS work_attempt_identity_update AFTER UPDATE ON runtime_execution_runs
         WHEN OLD.agent IS NOT NEW.agent OR OLD.account IS NOT NEW.account OR OLD.epoch IS NOT NEW.epoch OR OLD.thread IS NOT NEW.thread OR OLD.turn IS NOT NEW.turn OR OLD.created IS NOT NEW.created
           OR json_extract(OLD.record,'$.latestAttemptId') IS NOT json_extract(NEW.record,'$.latestAttemptId')
           OR json_extract(OLD.record,'$.status') IS NOT json_extract(NEW.record,'$.status')
         BEGIN SELECT {WORK_IDENTITY_FUNCTION}(OLD.agent,NULL); SELECT {WORK_IDENTITY_FUNCTION}(NEW.agent,NULL); END;"
    ))?;
    Ok(())
}

/// Ids of the changed rows that the Studio error list reads. The list owner
/// (`studio_errors` in runtime-services) decides which of them can change it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StudioErrorSources {
    pub agents: BTreeSet<String>,
    pub monitors: BTreeSet<String>,
    pub events: BTreeSet<String>,
    /// `runtime_operation_receipts` ids, which include dismissals.
    pub receipts: BTreeSet<String>,
    /// An agent, monitor or event row was deleted; its id is gone.
    pub rows_removed: bool,
    /// A receipt row was deleted; its id is gone.
    pub receipts_removed: bool,
}

impl StudioErrorSources {
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
            && self.monitors.is_empty()
            && self.events.is_empty()
            && self.receipts.is_empty()
            && !self.rows_removed
            && !self.receipts_removed
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Source {
    Agent,
    Request,
    Event,
    EventMeta,
    Item,
    Plan,
    Annotation,
    Task,
    Work,
    QueueAsset,
    Terminal,
    /// Read only by the Studio error list.
    Monitor,
    /// Read only by the Studio error list (dismissals).
    Receipt,
}

impl Source {
    fn of_table(table: &str) -> Option<Self> {
        match table {
            "runtime_agents" => Some(Self::Agent),
            "runtime_requests" => Some(Self::Request),
            "runtime_events" => Some(Self::Event),
            "runtime_event_meta" => Some(Self::EventMeta),
            "runtime_items" => Some(Self::Item),
            "runtime_plans" => Some(Self::Plan),
            "runtime_annotations" => Some(Self::Annotation),
            "runtime_tasks" => Some(Self::Task),
            "runtime_work" => Some(Self::Work),
            "runtime_assets" => Some(Self::QueueAsset),
            "user_terminals" => Some(Self::Terminal),
            "runtime_monitors" => Some(Self::Monitor),
            "runtime_operation_receipts" => Some(Self::Receipt),
            _ => None,
        }
    }

    fn of_collection(collection: &str) -> Option<Self> {
        match collection {
            "agent" => Some(Self::Agent),
            "request" => Some(Self::Request),
            "plan" => Some(Self::Plan),
            "annotation" => Some(Self::Annotation),
            "task" => Some(Self::Task),
            "work" | "workDetail" => Some(Self::Work),
            _ => None,
        }
    }

    fn table(self) -> &'static str {
        match self {
            Self::Agent => "runtime_agents",
            Self::Request => "runtime_requests",
            Self::Event => "runtime_events",
            Self::EventMeta => "runtime_event_meta",
            Self::Item => "runtime_items",
            Self::Plan => "runtime_plans",
            Self::Annotation => "runtime_annotations",
            Self::Task => "runtime_tasks",
            Self::Work => "runtime_work",
            Self::QueueAsset => "runtime_assets",
            Self::Terminal => "user_terminals",
            Self::Monitor => "runtime_monitors",
            Self::Receipt => "runtime_operation_receipts",
        }
    }
}

#[derive(Debug)]
enum Event {
    /// The update hook saw a write of this row.
    Row(Source, i64),
    /// An entity writer published this entity.
    Published(Source, String),
    /// A partial projection write needs a full publication.
    Stale(Source, String),
    WorkHistory(String, WorkHistoryKind, usize),
    WorkRefusalOwner(String),
    WorkRefusalTask(String),
}

thread_local! {
    /// The journal of the writer transaction that runs on this thread, if any.
    /// The writer connection runs all its transactions on one thread.
    static JOURNAL: RefCell<Option<Vec<Event>>> = const { RefCell::new(None) };
}

fn push(event: Event) {
    JOURNAL.with(|journal| {
        if let Some(events) = journal.borrow_mut().as_mut() {
            events.push(event);
        }
    });
}

/// Start the journal of a writer transaction.
pub(crate) fn begin() {
    JOURNAL.with(|journal| *journal.borrow_mut() = Some(Vec::new()));
}

/// The events of one writer transaction, in write order.
pub(crate) struct Journal(Vec<Event>);

/// End the journal of the current writer transaction and return its events.
pub(crate) fn finish() -> Journal {
    Journal(JOURNAL.with(|journal| journal.borrow_mut().take().unwrap_or_default()))
}

/// Whether the current writer transaction publishes its changed rows before commit.
pub(crate) fn active() -> bool {
    JOURNAL.with(|journal| journal.borrow().is_some())
}

/// Journal a row write that the writer update hook reports.
pub(crate) fn observe_row(table: &str, row_id: i64) {
    if let Some(source) = Source::of_table(table) {
        push(Event::Row(source, row_id));
    }
}

/// Journal that an entity writer published `collection/id`.
pub(crate) fn mark_published(collection: &str, id: &str) {
    if let Some(source) = Source::of_collection(collection) {
        push(Event::Published(source, id.to_owned()));
    }
}

/// Journal that `collection/id` needs a full publication before commit.
pub fn mark_stale(collection: &str, id: &str) {
    if collection == "work" {
        push(Event::WorkRefusalTask(id.to_owned()));
    }
    if let Some(source) = Source::of_collection(collection) {
        push(Event::Stale(source, id.to_owned()));
    }
}

/// The rows of `events` whose last change comes after their last publication.
pub(crate) fn pending(
    transaction: &Transaction<'_>,
    Journal(events): Journal,
) -> StoreResult<PendingEntities> {
    let mut ids: HashMap<(Source, i64), Option<String>> = HashMap::new();
    for event in &events {
        if let Event::Row(source, row_id) = event
            && !ids.contains_key(&(*source, *row_id))
        {
            // A deleted row has no id left; its writer owns the entity removal.
            let id = transaction
                .query_row(
                    &match source {
                        Source::QueueAsset => "SELECT json_extract(record,'$.agent') FROM runtime_assets WHERE rowid=?1".to_owned(),
                        _ => format!("SELECT id FROM {} WHERE rowid=?1", source.table()),
                    },
                    [row_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            ids.insert((*source, *row_id), id);
        }
    }
    let mut pending = PendingEntities::default();
    for ((source, _), id) in &ids {
        if *source != Source::Work {
            continue;
        }
        if id.is_none() {
            let mut statement = transaction.prepare(
                "SELECT DISTINCT id FROM sync_entities WHERE collection IN ('work','workDetail') AND deleted=0 AND NOT EXISTS(SELECT 1 FROM runtime_work WHERE runtime_work.id=sync_entities.id)",
            )?;
            for row in statement.query_map([], |row| row.get::<_, String>(0))? {
                pending.work.insert(row?);
            }
        }
    }
    for ((source, _), id) in &ids {
        if matches!(
            source,
            Source::Agent | Source::Event | Source::EventMeta | Source::QueueAsset
        ) {
            let owner = match (source, id) {
                (Source::Event | Source::EventMeta, Some(id)) => transaction
                    .query_row(
                        "SELECT agent FROM runtime_events WHERE id=?1",
                        [id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?,
                (_, id) => id.clone(),
            };
            if let Some(id) = owner {
                pending.queues.insert(id);
            } else {
                // Deleted source rows no longer carry their owner. Recheck only
                // existing queue views; this path never scans event history.
                let mut statement = transaction.prepare(
                    "SELECT id FROM sync_entities WHERE collection='queue' AND deleted=0",
                )?;
                for id in statement.query_map([], |row| row.get::<_, String>(0))? {
                    pending.queues.insert(id?);
                }
            }
        }
    }
    for ((source, _), id) in &ids {
        if *source == Source::Terminal {
            if let Some(id) = id {
                pending.terminals.insert(id.clone());
            } else {
                let mut statement = transaction.prepare(
                    "SELECT id FROM sync_entities WHERE collection='terminal' AND deleted=0",
                )?;
                for id in statement.query_map([], |row| row.get::<_, String>(0))? {
                    pending.terminals.insert(id?);
                }
            }
        }
    }
    for ((source, _), id) in &ids {
        let studio_errors = &mut pending.studio_errors;
        match (source, id) {
            (Source::Agent, Some(id)) => studio_errors.agents.insert(id.clone()),
            (Source::Monitor, Some(id)) => studio_errors.monitors.insert(id.clone()),
            (Source::Event, Some(id)) => studio_errors.events.insert(id.clone()),
            (Source::Receipt, Some(id)) => studio_errors.receipts.insert(id.clone()),
            (Source::Agent | Source::Monitor | Source::Event, None) => {
                studio_errors.rows_removed = true;
                false
            }
            (Source::Receipt, None) => {
                studio_errors.receipts_removed = true;
                false
            }
            _ => false,
        };
    }
    // Request attention belongs in summaries even when an explicit request
    // publisher has already consumed the card's own pending publication.
    for ((source, _), id) in &ids {
        if *source != Source::Request {
            continue;
        }
        if let Some(id) = id {
            if let Some(agent) = transaction
                .query_row(
                    "SELECT json_extract(record,'$.agent') FROM runtime_requests WHERE id=?1",
                    [id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
            {
                pending.agents.insert(agent);
            }
        } else {
            let mut statement = transaction.prepare(
                "SELECT id FROM sync_entities WHERE collection='agentSummary' AND deleted=0",
            )?;
            for row in statement.query_map([], |row| row.get::<_, String>(0))? {
                pending.agents.insert(row?);
            }
        }
    }
    let mut changed: BTreeMap<(Source, String), usize> = BTreeMap::new();
    let mut published: HashMap<(Source, String), usize> = HashMap::new();
    let mut work_changes: BTreeMap<String, WorkSourceChanges> = BTreeMap::new();
    for (index, event) in events.into_iter().enumerate() {
        match event {
            Event::Row(source, row_id) => {
                if let Some(Some(id)) = ids.get(&(source, row_id)) {
                    changed.insert((source, id.clone()), index);
                }
            }
            Event::Stale(source, id) => {
                changed.insert((source, id), index);
            }
            Event::Published(source, id) => {
                if source == Source::Work {
                    work_changes.remove(&id);
                }
                published.insert((source, id), index);
            }
            Event::WorkHistory(id, kind, entry) => {
                work_changes
                    .entry(id)
                    .or_default()
                    .histories
                    .entry(kind)
                    .or_default()
                    .insert(entry);
            }
            Event::WorkRefusalOwner(id) => {
                pending.work_refusal_owners.insert(id);
            }
            Event::WorkRefusalTask(id) => {
                work_changes.entry(id).or_default().refusal = true;
            }
        }
    }
    for ((source, id), index) in changed {
        if published
            .get(&(source, id.clone()))
            .is_some_and(|published| *published > index)
        {
            continue;
        }
        match source {
            Source::Agent => pending.agents.insert(id),
            Source::Request => pending.requests.insert(id),
            Source::Event | Source::EventMeta => pending.receipt_events.insert(id),
            Source::Item => pending.receipt_items.insert(id),
            Source::Plan => pending.plans.insert(id),
            Source::Annotation => pending.annotations.insert(id),
            Source::Task => pending.tasks.insert(id),
            Source::Work => pending.work.insert(id),
            Source::QueueAsset | Source::Terminal | Source::Monitor | Source::Receipt => false,
        };
    }
    pending.work_changes = work_changes
        .into_iter()
        .filter(|(id, _)| pending.work.contains(id))
        .collect();
    Ok(pending)
}
