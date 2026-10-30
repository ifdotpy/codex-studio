//! Read-only bounded monitor observation. A wait never renews a command lease.
use crate::monitor_policy::{MonitorError, MonitorStatus};
use rusqlite::OptionalExtension;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use studio_runtime_store::{RuntimeStore, StoreError};

#[derive(serde::Serialize)]
struct ObservedOutputEvent {
    #[serde(flatten)]
    payload: crate::monitor_matches::OutputEvent,
    id: String,
    status: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Observation {
    #[serde(flatten)]
    pub record: studio_runtime_store::MonitorRecord,
    output_events: Vec<ObservedOutputEvent>,
    pub wait_expired: bool,
}

#[cfg(test)]
impl Observation {
    pub fn output_events_is_empty(&self) -> bool {
        self.output_events.is_empty()
    }
}

pub(crate) async fn read(
    store: &RuntimeStore,
    owner: &str,
    epoch: i64,
    account: &str,
    id: &str,
    after_sequence: Option<u64>,
    deadline: Option<tokio::time::Instant>,
) -> Result<(Observation, u64, bool), MonitorError> {
    let (owner, account, id) = (owner.to_owned(), account.to_owned(), id.to_owned());
    // One read transaction keeps monitor, owner, and output events coherent.
    store
        .read_async("runtime-services/monitor_observation", move |tx| {
            read_in(tx, &owner, epoch, &account, &id, after_sequence, deadline)
        })
        .await
}

fn read_in(
    tx: &rusqlite::Connection,
    owner: &str,
    epoch: i64,
    account: &str,
    id: &str,
    after_sequence: Option<u64>,
    deadline: Option<tokio::time::Instant>,
) -> Result<(Observation, u64, bool), MonitorError> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_monitors WHERE id=?",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    let record: studio_runtime_store::MonitorRecord =
        serde_json::from_str(&raw.ok_or(MonitorError::Refused("Unknown command watch"))?)
            .map_err(StoreError::from)?;
    if record.agent() != owner || record.epoch.get() != epoch {
        return Err(MonitorError::Refused(
            "This monitor belongs to another agent or epoch",
        ));
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?",
            [owner],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    let agent: Value = serde_json::from_str(&raw.ok_or(MonitorError::Refused("Unknown agent"))?)
        .map_err(StoreError::from)?;
    if studio_runtime_store::truthy(agent.get("deletedAt"))
        || agent["epoch"].as_i64() != Some(epoch)
        || agent["autoWake"] != true
    {
        return Err(MonitorError::Refused(
            "The monitor owner epoch is no longer active",
        ));
    }
    if agent["accountKey"].as_str().unwrap_or("default") != account {
        return Err(MonitorError::Refused(
            "The monitor belongs to another account",
        ));
    }
    // The authenticated actor uses its current model account. Its existing
    // monitor output remains owned by that actor and epoch after a transfer.
    let sequence = record.output_event_sequence.unwrap_or(0);
    let terminal = match record.status {
        MonitorStatus::Approval | MonitorStatus::Starting | MonitorStatus::Running => false,
        MonitorStatus::Completed
        | MonitorStatus::Failed
        | MonitorStatus::Cancelled
        | MonitorStatus::Lost => true,
    };
    if deadline.is_some() && !terminal && !after_sequence.is_some_and(|initial| sequence > initial)
    {
        return Ok((
            Observation {
                record,
                output_events: vec![],
                wait_expired: deadline.is_some_and(|end| tokio::time::Instant::now() >= end),
            },
            sequence,
            terminal,
        ));
    }
    let mut events = Vec::new();
    for id in record.output_event_ids.as_deref().unwrap_or_default() {
        let event: Option<(String, String)> = tx
            .query_row(
                "SELECT status,text FROM runtime_events WHERE id=? AND agent=?",
                [id, owner],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)?;
        if let Some((status, text)) = event {
            let payload = serde_json::from_str(&text).map_err(StoreError::from)?;
            events.push(ObservedOutputEvent {
                payload,
                id: id.clone(),
                status,
            });
        }
    }
    Ok((
        Observation {
            record,
            output_events: events,
            wait_expired: false,
        },
        sequence,
        terminal,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn complete(fields: Value) -> Value {
        let mut record = json!({"id":"watch","agent":"owner","epoch":1,"status":"running","requestId":"request","created":0,"activityAt":0,"activityGeneration":0,"bytes":0,"command":"","cwd":std::env::temp_dir(),"interactive":false,"livenessCommand":"","log":std::env::temp_dir().join("watch.log"),"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit"});
        let Value::Object(fields) = fields else {
            panic!("monitor fixture must be an object")
        };
        record.as_object_mut().unwrap().extend(fields);
        record
    }
    thread_local! { static EVENT_SELECTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

    #[test]
    fn rejects_invalid_observation_fields_and_accepts_legacy_sequence() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT, stream_pending INTEGER NOT NULL DEFAULT 0, monitor_status TEXT, monitor_error_candidate INTEGER NOT NULL DEFAULT 1, monitor_created REAL NOT NULL DEFAULT 0);CREATE TABLE runtime_events(id TEXT,agent TEXT,status TEXT,text TEXT);").expect("schema");
        db.execute(
            "INSERT INTO runtime_agents VALUES('owner',?1)",
            [json!({"epoch":1,"autoWake":true}).to_string()],
        )
        .unwrap();
        for (status, sequence) in [
            (json!(null), json!(0)),
            (json!("runnning"), json!(0)),
            (json!("running"), json!("bad")),
        ] {
            let record = complete(json!({"status":status,"outputEventSequence":sequence}));
            db.execute(
                "INSERT OR REPLACE INTO runtime_monitors(id,record) VALUES('watch',?1)",
                [record.to_string()],
            )
            .unwrap();
            assert!(read_in(&db, "owner", 1, "default", "watch", None, None).is_err());
        }
        let missing = json!({"agent":"owner","epoch":1});
        db.execute(
            "INSERT OR REPLACE INTO runtime_monitors(id,record) VALUES('watch',?1)",
            [missing.to_string()],
        )
        .unwrap();
        assert!(read_in(&db, "owner", 1, "default", "watch", None, None).is_err());
        let record = complete(json!({"status":"running"}));
        db.execute(
            "INSERT OR REPLACE INTO runtime_monitors(id,record) VALUES('watch',?1)",
            [record.to_string()],
        )
        .unwrap();
        let (_, sequence, terminal) =
            read_in(&db, "owner", 1, "default", "watch", None, None).unwrap();
        assert_eq!(sequence, 0);
        assert!(!terminal);
        let record = complete(json!({"status":"completed"}));
        db.execute(
            "INSERT OR REPLACE INTO runtime_monitors(id,record) VALUES('watch',?1)",
            [record.to_string()],
        )
        .unwrap();
        assert!(
            read_in(&db, "owner", 1, "default", "watch", None, None)
                .unwrap()
                .2
        );
    }
    #[test]
    fn unchanged_wait_does_not_select_events() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT, stream_pending INTEGER NOT NULL DEFAULT 0, monitor_status TEXT, monitor_error_candidate INTEGER NOT NULL DEFAULT 1, monitor_created REAL NOT NULL DEFAULT 0);CREATE TABLE runtime_events(id TEXT,agent TEXT,status TEXT,text TEXT);").expect("schema");
        db.execute(
            "INSERT INTO runtime_agents VALUES('owner',?1)",
            [json!({"epoch":1,"autoWake":true}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO runtime_monitors(id,record) VALUES('watch',?1)",
            [complete(json!({"outputEventSequence":1,"outputEventIds":["event"]})).to_string()],
        )
        .unwrap();
        db.execute(
            r#"INSERT INTO runtime_events VALUES('event','owner','sent','{"monitorId":"watch","epoch":1,"sequence":1,"matches":[],"final":false}')"#,
            [],
        )
        .unwrap();
        db.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(|event| {
                if let rusqlite::trace::TraceEvent::Stmt(statement, _) = event
                    && statement
                        .sql()
                        .starts_with("SELECT status,text FROM runtime_events")
                {
                    EVENT_SELECTS.with(|count| count.set(count.get() + 1));
                }
            }),
        );
        EVENT_SELECTS.with(|count| count.set(0));
        let deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
        read_in(&db, "owner", 1, "default", "watch", Some(1), deadline).unwrap();
        assert_eq!(EVENT_SELECTS.with(std::cell::Cell::get), 0);
        let (record, _, _) =
            read_in(&db, "owner", 1, "default", "watch", Some(0), deadline).unwrap();
        assert_eq!(EVENT_SELECTS.with(std::cell::Cell::get), 1);
        assert_eq!(record.output_events.len(), 1);
    }
    #[test]
    fn transferred_monitor_output_belongs_to_its_current_actor_not_its_old_model_account() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT, stream_pending INTEGER NOT NULL DEFAULT 0, monitor_status TEXT, monitor_error_candidate INTEGER NOT NULL DEFAULT 1, monitor_created REAL NOT NULL DEFAULT 0);CREATE TABLE runtime_events(id TEXT,agent TEXT,status TEXT,text TEXT);").expect("schema");
        db.execute(
            "INSERT INTO runtime_agents VALUES('owner',?1)",
            [json!({"id":"owner","epoch":1,"autoWake":true,"accountKey":"target"}).to_string()],
        )
        .expect("transferred owner");
        db.execute("INSERT INTO runtime_monitors(id,record) VALUES('watch',?1)",[complete(json!({"tail":"kept output","operation":{"agent":"owner","epoch":1,"accountKey":"source","connectionId":"original"}})).to_string()]).expect("original source monitor");
        let (record, _, terminal) = read_in(&db, "owner", 1, "target", "watch", None, None)
            .expect("read the existing process after transfer");
        assert_eq!(record.record.tail.as_str(), "kept output");
        assert!(!terminal);
        for (owner, epoch, account) in [
            ("other", 1, "target"),
            ("owner", 2, "target"),
            ("owner", 1, "source"),
        ] {
            assert!(read_in(&db, owner, epoch, account, "watch", None, None).is_err());
        }
    }
}
