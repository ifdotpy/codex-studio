//! Durable conditions that keep a loaded provider thread, or an account's
//! provider process, in use. An account worker unloads an idle thread only
//! while none holds; any doubt (an unknown start, unsettled input or request)
//! keeps the thread loaded.
use super::*;
use crate::schema::table_exists;
use crate::startup_recovery::{
    active_monitor_predicate, unsettled_request_predicate, unsettled_tool_request_predicate,
};

/// Agent statuses whose turn may still be starting or running.
const ACTIVE_AGENT_STATUSES: &str = "'queued','starting','running','approval'";
/// Queue event statuses whose input is not yet settled by a finished turn.
const UNSETTLED_EVENT_STATUSES: &str = "'pending','reserved','dispatching','uncertain'";
/// Context repair phases that still read or replace the agent's thread.
const ACTIVE_CONTEXT_REPAIR_PHASES: &str = "'preparing','submitted','unknown','ready'";
/// Context repair source cleanup phases that still own the source thread.
const ACTIVE_SOURCE_CLEANUP_PHASES: &str = "'planned','submitted'";

/// SQL predicate over the `runtime_agents` row (unqualified `record`): the
/// agent has a turn, input, request, monitor, or operation in progress.
fn agent_in_use(connection: &Connection) -> StoreResult<String> {
    let mut checks = vec![
        format!("COALESCE(json_extract(record,'$.status'),'') IN ({ACTIVE_AGENT_STATUSES})"),
        "COALESCE(json_extract(record,'$.inFlight'),0)!=0".to_owned(),
        "COALESCE(json_extract(record,'$.turnId'),'')!=''".to_owned(),
        "COALESCE(json_array_length(record,'$.activeTools'),0)>0".to_owned(),
        "json_extract(record,'$.workspaceOperation') IS NOT NULL".to_owned(),
        "COALESCE(json_extract(record,'$.accountTransferId'),'')!=''".to_owned(),
        "COALESCE(json_extract(record,'$.nativeToolRefreshId'),'')!=''".to_owned(),
        format!("json_extract(record,'$.contextRepair.phase') IN ({ACTIVE_CONTEXT_REPAIR_PHASES})"),
        format!("({})", unresolved_sql()),
        format!(
            "EXISTS(SELECT 1 FROM runtime_events e WHERE e.agent=runtime_agents.id \
             AND e.status IN ({UNSETTLED_EVENT_STATUSES}))"
        ),
    ];
    for (table, predicate) in [
        ("runtime_monitors", active_monitor_predicate()),
        ("runtime_requests", unsettled_request_predicate()),
        ("runtime_tool_requests", unsettled_tool_request_predicate()),
    ] {
        if table_exists(connection, table)? {
            checks.push(format!(
                "EXISTS(SELECT 1 FROM {table} WHERE {predicate} \
                 AND json_extract(record,'$.agent')=runtime_agents.id)"
            ));
        }
    }
    Ok(checks.join(" OR "))
}

fn exists(connection: &Connection, sql: &str, values: &[&str]) -> StoreResult<bool> {
    Ok(connection.query_row(
        &format!("SELECT EXISTS({sql})"),
        rusqlite::params_from_iter(values),
        |row| row.get(0),
    )?)
}

/// A pending account transfer that reads from or writes to `?1`.
fn transfer_pending(connection: &Connection, account: &str) -> StoreResult<bool> {
    if !table_exists(connection, "runtime_account_transfers")? {
        return Ok(false);
    }
    exists(
        connection,
        "SELECT 1 FROM runtime_account_transfers t WHERE json_extract(t.record,'$.status')='pending' \
         AND (json_extract(t.record,'$.targetAccountKey')=?1 OR EXISTS(SELECT 1 FROM \
         json_each(t.record,'$.members') m WHERE json_extract(m.value,'$.sourceAccountKey')=?1))",
        &[account],
    )
}

fn transfer_pending_predicate(connection: &Connection) -> StoreResult<String> {
    if !table_exists(connection, "runtime_account_transfers")? {
        return Ok("0".to_owned());
    }
    Ok("EXISTS(SELECT 1 FROM runtime_account_transfers t \
        WHERE json_extract(t.record,'$.status')='pending' \
        AND (json_extract(t.record,'$.targetAccountKey')=?1 OR EXISTS(SELECT 1 FROM \
        json_each(t.record,'$.members') m WHERE json_extract(m.value,'$.sourceAccountKey')=?1)))"
        .to_owned())
}

/// Work item statuses that are done: the owner expects no further turn for them.
/// Any other status (`ready`, `running`, `blocked`, `review`) means a
/// continuation, a rejection, or a follow-up may still reach the owner.
const SETTLED_WORK_STATUSES: &str = "'accepted','cancelled'";

/// SQL predicate over the `runtime_agents` row: a finished managed worker. It
/// is not a lead (leads and ordinary chats are leads), it is not in the
/// `waiting` state (pending children, watches, or a hold), and it owns no open
/// work item. A waiting worker will likely get another turn soon, so it keeps
/// the ordinary idle timer.
fn finished_worker_sql(connection: &Connection) -> StoreResult<String> {
    let mut predicate = "COALESCE(json_extract(record,'$.isLead'),0)=0 \
         AND COALESCE(json_extract(record,'$.status'),'')!='waiting'"
        .to_owned();
    if table_exists(connection, "runtime_work")? {
        predicate.push_str(&format!(
            " AND NOT EXISTS(SELECT 1 FROM runtime_work w \
             WHERE json_extract(w.record,'$.owner')=runtime_agents.id \
             AND json_extract(w.record,'$.status') NOT IN ({SETTLED_WORK_STATUSES}))"
        ));
    }
    Ok(predicate)
}

const ACCOUNT_AGENT_SQL: &str = "SELECT 1 FROM runtime_agents WHERE \
     COALESCE(json_extract(record,'$.accountKey'),'default')=?1 \
     AND json_extract(record,'$.deletedAt') IS NULL";

impl SchedulerRepository {
    /// Read the durable status of known loaded threads and the account in one
    /// bounded query. The candidate list comes from one provider snapshot.
    /// Returns the busy threads, the threads that unload at once (see
    /// [`Self::idle_unload_immediate_threads`]), and whether
    /// the account is in use.
    pub fn idle_unload_usage_snapshot(
        connection: &Connection,
        account: &str,
        threads: &std::collections::BTreeSet<String>,
    ) -> StoreResult<(
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<String>,
        bool,
    )> {
        let transfer = transfer_pending_predicate(connection)?;
        let in_use = agent_in_use(connection)?;
        let account_in_use = format!("({transfer} OR EXISTS({ACCOUNT_AGENT_SQL} AND ({in_use})))");
        let thread_in_use = format!(
            "({transfer} OR EXISTS({ACCOUNT_AGENT_SQL} \
               AND json_extract(record,'$.threadId')=c.thread_id AND ({in_use})) \
             OR EXISTS(SELECT 1 FROM runtime_agents WHERE \
               json_extract(record,'$.contextRepair.sourceCleanup.phase') \
                 IN ({ACTIVE_SOURCE_CLEANUP_PHASES}) \
               AND json_extract(record,'$.contextRepair.sourceCleanup.accountKey')=?1 \
               AND json_extract(record,'$.contextRepair.sourceCleanup.threadId')=c.thread_id))"
        );
        let sql = format!(
            "WITH candidates(thread_id) AS (SELECT value FROM json_each(?2)), \
             account_usage(in_use) AS (SELECT {account_in_use}), \
             candidate_usage AS (SELECT c.thread_id, \
               CASE WHEN {thread_in_use} THEN 1 ELSE 0 END AS busy \
               FROM candidates c) \
             SELECT candidate_usage.thread_id,candidate_usage.busy,account_usage.in_use \
             FROM account_usage LEFT JOIN candidate_usage ON 1=1 \
             ORDER BY candidate_usage.thread_id"
        );
        let encoded_threads = serde_json::Value::Array(
            threads
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        )
        .to_string();
        let mut statement = connection.prepare_cached(&sql)?;
        let rows = statement.query_map([account, &encoded_threads], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<bool>>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?;
        let mut busy_threads = std::collections::BTreeSet::new();
        let mut account_in_use = false;
        for row in rows {
            let (thread, busy, busy_account) = row?;
            account_in_use = busy_account;
            if let (Some(thread), Some(true)) = (thread, busy) {
                busy_threads.insert(thread);
            }
        }
        Ok((
            busy_threads,
            Self::idle_unload_immediate_threads(connection, account, threads)?,
            account_in_use,
        ))
    }

    /// The members of `threads` that unload as soon as they are unused: every
    /// thread that no live lead and no live unfinished worker of `account`
    /// owns. These are the threads of finished managed workers (see
    /// `finished_worker_sql`) and of deleted agents, which nobody resumes
    /// soon; a lead or an unfinished worker keeps its thread for the idle
    /// setting.
    pub fn idle_unload_immediate_threads(
        connection: &Connection,
        account: &str,
        threads: &std::collections::BTreeSet<String>,
    ) -> StoreResult<std::collections::BTreeSet<String>> {
        let encoded_threads = serde_json::Value::Array(
            threads
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        )
        .to_string();
        let finished = finished_worker_sql(connection)?;
        let mut statement = connection.prepare_cached(&format!(
            "SELECT c.value FROM json_each(?2) c WHERE NOT EXISTS(SELECT 1 FROM runtime_agents \
             WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 \
             AND json_extract(record,'$.deletedAt') IS NULL AND NOT ({finished}) \
             AND json_extract(record,'$.threadId')=c.value)"
        ))?;
        let rows = statement.query_map([account, &encoded_threads], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Whether `thread` on `account` must stay loaded: an agent that owns it
    /// is in use, a context repair still cleans it up, or a transfer touches
    /// the account. A thread no live agent owns holds nothing by itself.
    pub fn idle_unload_thread_in_use(
        connection: &Connection,
        account: &str,
        thread: &str,
    ) -> StoreResult<bool> {
        if transfer_pending(connection, account)? {
            return Ok(true);
        }
        let in_use = agent_in_use(connection)?;
        Ok(exists(
            connection,
            &format!("{ACCOUNT_AGENT_SQL} AND json_extract(record,'$.threadId')=?2 AND ({in_use})"),
            &[account, thread],
        )? || exists(
            connection,
            &format!(
                "SELECT 1 FROM runtime_agents WHERE \
                 json_extract(record,'$.contextRepair.sourceCleanup.phase') IN ({ACTIVE_SOURCE_CLEANUP_PHASES}) \
                 AND json_extract(record,'$.contextRepair.sourceCleanup.accountKey')=?1 \
                 AND json_extract(record,'$.contextRepair.sourceCleanup.threadId')=?2"
            ),
            &[account, thread],
        )?)
    }

    /// Whether `account` needs its provider process: any of its agents is in
    /// use (including input for an agent whose thread is not loaded yet), or a
    /// transfer touches the account.
    pub fn idle_unload_account_in_use(connection: &Connection, account: &str) -> StoreResult<bool> {
        if transfer_pending(connection, account)? {
            return Ok(true);
        }
        let in_use = agent_in_use(connection)?;
        exists(
            connection,
            &format!("{ACCOUNT_AGENT_SQL} AND ({in_use})"),
            &[account],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::SchedulerRepository;
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn database() -> Connection {
        let connection = Connection::open_in_memory().expect("memory database opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, kind TEXT, text TEXT,
                   status TEXT, created REAL, epoch INTEGER, turn_id TEXT, error TEXT);
                 CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL);",
            )
            .expect("schema creates");
        connection
    }

    fn agent(connection: &Connection, id: &str, fields: Value) {
        let mut record = json!({"id":id,"rootId":id,"epoch":1,"status":"completed",
            "accountKey":"acct","threadId":"thread"});
        for (key, value) in fields.as_object().expect("fields").clone() {
            record[key] = value;
        }
        connection
            .execute(
                "INSERT OR REPLACE INTO runtime_agents(id,record) VALUES(?1,?2)",
                [id, &record.to_string()],
            )
            .expect("agent writes");
    }

    fn thread_in_use(connection: &Connection) -> bool {
        SchedulerRepository::idle_unload_thread_in_use(connection, "acct", "thread").expect("reads")
    }

    fn account_in_use(connection: &Connection) -> bool {
        SchedulerRepository::idle_unload_account_in_use(connection, "acct").expect("reads")
    }

    #[test]
    fn a_settled_agent_or_an_unowned_thread_releases_the_thread() {
        let connection = database();
        assert!(!thread_in_use(&connection), "no owner holds nothing");
        agent(&connection, "a", json!({}));
        assert!(!thread_in_use(&connection));
        assert!(!account_in_use(&connection));
        agent(
            &connection,
            "a",
            json!({"status":"running","deletedAt":1.0}),
        );
        assert!(!thread_in_use(&connection), "a deleted owner holds nothing");
    }

    #[test]
    fn every_in_use_condition_holds_the_thread_and_the_account() {
        let cases = [
            json!({"status":"running"}),
            json!({"status":"queued"}),
            json!({"status":"approval"}),
            json!({"inFlight":true}),
            json!({"turnId":"turn"}),
            json!({"activeTools":[{"id":"tool"}]}),
            json!({"workspaceOperation":"merge"}),
            json!({"accountTransferId":"transfer"}),
            json!({"nativeToolRefreshId":"refresh"}),
            json!({"contextRepair":{"phase":"submitted"}}),
            json!({"startAttempt":{"submitted":true}}),
        ];
        for fields in cases {
            let connection = database();
            agent(&connection, "a", fields.clone());
            assert!(thread_in_use(&connection), "{fields} holds the thread");
            assert!(account_in_use(&connection), "{fields} holds the account");
            assert!(
                !SchedulerRepository::idle_unload_thread_in_use(&connection, "acct", "other")
                    .expect("reads"),
                "{fields} holds only its own thread"
            );
        }
    }

    #[test]
    fn unsettled_input_requests_and_monitors_hold_the_thread() {
        for status in ["pending", "reserved", "dispatching", "uncertain"] {
            let connection = database();
            agent(&connection, "a", json!({}));
            connection
                .execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES('e','a','user','hi',?1,0,1)",
                    [status],
                )
                .expect("event writes");
            assert!(
                thread_in_use(&connection),
                "{status} input holds the thread"
            );
        }
        let connection = database();
        agent(&connection, "a", json!({}));
        connection
            .execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                 VALUES('e','a','user','hi','done',0,1)",
                [],
            )
            .expect("event writes");
        assert!(!thread_in_use(&connection), "settled input holds nothing");
        connection
            .execute(
                "INSERT INTO runtime_monitors(id,record) VALUES('m',?1)",
                [json!({"id":"m","agent":"a","status":"running"}).to_string()],
            )
            .expect("monitor writes");
        assert!(
            thread_in_use(&connection),
            "a running monitor holds the thread"
        );
        connection
            .execute("DELETE FROM runtime_monitors", [])
            .expect("monitor deletes");
        connection
            .execute(
                "INSERT INTO runtime_requests(id,record) VALUES('r',?1)",
                [json!({"id":"r","agent":"a","status":"pending"}).to_string()],
            )
            .expect("request writes");
        assert!(
            thread_in_use(&connection),
            "a pending request holds the thread"
        );
    }

    #[test]
    fn input_for_an_agent_without_a_loaded_thread_holds_only_the_account() {
        let connection = database();
        agent(
            &connection,
            "new",
            json!({"threadId":null,"status":"queued"}),
        );
        assert!(!thread_in_use(&connection));
        assert!(account_in_use(&connection));
    }

    #[test]
    fn a_pending_transfer_or_source_cleanup_holds_the_thread() {
        let connection = database();
        connection
            .execute(
                "INSERT INTO runtime_account_transfers(id,record) VALUES('t',?1)",
                [
                    json!({"id":"t","status":"pending","targetAccountKey":"other",
                    "members":{"a":{"sourceAccountKey":"acct"}}})
                    .to_string(),
                ],
            )
            .expect("transfer writes");
        assert!(thread_in_use(&connection));
        assert!(account_in_use(&connection));
        connection
            .execute(
                "UPDATE runtime_account_transfers SET record=json_set(record,'$.status','completed')",
                [],
            )
            .expect("transfer settles");
        assert!(!thread_in_use(&connection));
        agent(
            &connection,
            "repair",
            json!({"threadId":"new","contextRepair":{"sourceCleanup":
                {"phase":"planned","accountKey":"acct","threadId":"thread"}}}),
        );
        assert!(
            thread_in_use(&connection),
            "source cleanup still owns the thread"
        );
    }

    #[test]
    fn missing_transfer_table_is_a_clear_idle_state() {
        let connection = database();
        connection
            .execute("DROP TABLE runtime_account_transfers", [])
            .expect("legacy schema drops transfer table");

        assert!(!thread_in_use(&connection));
        assert!(!account_in_use(&connection));
    }

    #[test]
    fn one_usage_snapshot_checks_only_the_supplied_threads_and_their_account() {
        let connection = database();
        agent(&connection, "busy", json!({"status":"running"}));
        agent(
            &connection,
            "other-thread",
            json!({"threadId":"other","status":"running"}),
        );
        let candidates = std::collections::BTreeSet::from(["thread".to_owned(), "cold".to_owned()]);
        let (busy, _, account_busy) =
            SchedulerRepository::idle_unload_usage_snapshot(&connection, "acct", &candidates)
                .expect("usage snapshot reads");
        assert_eq!(
            busy,
            std::collections::BTreeSet::from(["thread".to_owned()])
        );
        assert!(
            account_busy,
            "another thread keeps the account process in use"
        );

        agent(&connection, "busy", json!({}));
        agent(&connection, "other-thread", json!({"threadId":"other"}));
        let (busy, _, account_busy) =
            SchedulerRepository::idle_unload_usage_snapshot(&connection, "acct", &candidates)
                .expect("usage snapshot rereads authoritative state");
        assert!(busy.is_empty());
        assert!(!account_busy);
    }

    #[test]
    fn only_a_live_lead_or_unfinished_worker_keeps_its_thread_for_the_idle_setting() {
        let connection = database();
        let threads = std::collections::BTreeSet::from(["thread".to_owned(), "cold".to_owned()]);
        let immediate = || {
            SchedulerRepository::idle_unload_immediate_threads(&connection, "acct", &threads)
                .expect("immediate threads read")
        };
        let all = threads.clone();
        let cold = std::collections::BTreeSet::from(["cold".to_owned()]);
        agent(&connection, "chat", json!({"isLead":true}));
        assert_eq!(immediate(), cold, "a lead or ordinary chat waits");
        let (_, snapshot_immediate, _) =
            SchedulerRepository::idle_unload_usage_snapshot(&connection, "acct", &threads)
                .expect("usage snapshot reads");
        assert_eq!(snapshot_immediate, immediate());
        agent(&connection, "chat", json!({"isLead":false}));
        assert_eq!(
            immediate(),
            all,
            "a finished managed worker unloads at once"
        );
        agent(
            &connection,
            "chat",
            json!({"isLead":false,"status":"waiting"}),
        );
        assert_eq!(immediate(), cold, "a waiting worker keeps the idle timer");
        agent(&connection, "chat", json!({"isLead":false}));
        for status in ["review", "ready", "running", "blocked"] {
            connection
                .execute(
                    "INSERT OR REPLACE INTO runtime_work(id,record) VALUES('w',?1)",
                    [json!({"id":"w","owner":"chat","status":status}).to_string()],
                )
                .expect("work writes");
            assert_eq!(immediate(), cold, "{status} work keeps the idle timer");
        }
        connection
            .execute(
                "UPDATE runtime_work SET record=json_set(record,'$.status','accepted')",
                [],
            )
            .expect("work settles");
        assert_eq!(immediate(), all, "accepted work finishes the worker");
        agent(
            &connection,
            "chat",
            json!({"isLead":false,"status":"waiting","deletedAt":1.0}),
        );
        assert_eq!(
            immediate(),
            all,
            "a deleted worker's thread unloads at once"
        );
        agent(&connection, "chat", json!({"isLead":true,"deletedAt":1.0}));
        assert_eq!(immediate(), all, "a deleted lead's thread unloads at once");
        agent(
            &connection,
            "chat",
            json!({"isLead":true,"accountKey":"other"}),
        );
        assert_eq!(
            immediate(),
            all,
            "another account's lead holds nothing here"
        );
    }
}
