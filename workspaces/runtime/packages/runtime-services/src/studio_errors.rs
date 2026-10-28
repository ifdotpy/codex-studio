//! The owner's list of agent work that ended because of a Studio error, not
//! because of the agent's own work: a command whose result Studio lost, input
//! that Studio did not send or could not confirm, and a turn that ended with
//! its provider connection. Each row names the reason in plain words and one
//! safe action. `resume` runs connection recovery, which reads native history
//! and never sends uncertain input again. `check` opens the agent, because
//! only the owner can decide. Studio never runs a command or sends input from
//! this list. A dismissed row stays hidden.
//!
//! A rule or a command can fail the same way many times, for example a rule
//! that checks every few minutes while its owner cannot run. Its failures of
//! one kind are one row with their count and time range. A failure that a
//! later run of the same rule followed with a success is not on the list.
mod causes;
pub use causes::{json_cause, store_causes, store_error_kind};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use studio_runtime_schema::shared_values::{
    BACKEND_RESTARTED_TURN_ERROR, LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
    MONITOR_OWNER_STOPPED_ERROR, PROVIDER_DISCONNECTED_TURN_ERROR, PROVIDER_RESUME_UNKNOWN_ERROR,
    PROVIDER_THREAD_START_UNKNOWN_ERROR, VM_PROVIDER_DISCONNECTED_TURN_ERROR,
};
pub use studio_runtime_schema::studio_errors::Kind;
use studio_runtime_schema::studio_errors::{
    Cause, Context, Diagnostics, MonitorOperationIdentity, MonitorRecoveryEvidence,
    MonitorRecoveryProof, MonitorRecoveryReason, MonitorStatus, StudioError, Target,
};
use studio_runtime_store::{StoreError, StudioErrorSources, Transaction, error_incidents};

/// Oldest work the list reads. Unit: seconds.
pub const WINDOW_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
/// Pending input younger than this still waits for an ordinary start, not for
/// an owner. Unit: seconds.
pub const STUCK_INPUT_SECONDS: f64 = 10.0 * 60.0;
/// Most rows of one list.
pub const MAX_ROWS: usize = 200;
/// Row-id prefix of the failures of one rule or command and kind. Dismissing
/// such a row hides its failures up to that time; a later one returns.
const MONITOR_GROUP_PREFIX: &str = "monitors:";
/// Row-id prefix of one monitor, as stored by dismissals before grouping.
const MONITOR_ROW_PREFIX: &str = "monitor:";
/// Record once by diagnostic identity, independently of request-result expiry.
pub async fn record_incident(
    store: &studio_runtime_store::RuntimeStore,
    incident: StudioError,
) -> Result<(), StoreError> {
    if !matches!(incident.kind, Kind::HttpFailure | Kind::RecoveryFailed)
        || !matches!(incident.target, Target::Details)
    {
        return Err(StoreError::InvalidStoredRecord {
            object: "studio-errors",
            id: incident.id,
        });
    }
    store
        .transaction(move |tx| error_incidents::put(tx, &incident))
        .await
}

/// The kind of an agent's current error, when Studio caused it.
fn agent_kind(status: &str, error: &str) -> Option<Kind> {
    let disconnected = [
        PROVIDER_DISCONNECTED_TURN_ERROR,
        BACKEND_RESTARTED_TURN_ERROR,
        VM_PROVIDER_DISCONNECTED_TURN_ERROR,
        LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
        PROVIDER_RESUME_UNKNOWN_ERROR,
        PROVIDER_THREAD_START_UNKNOWN_ERROR,
    ];
    if matches!(status, "failed" | "interrupted") && disconnected.contains(&error) {
        return Some(Kind::TurnDisconnected);
    }
    if matches!(status, "failed" | "interrupted" | "paused")
        && (error.starts_with("WorkspaceRecoveryRequired")
            || error.starts_with("workspace-readiness/")
            || error.starts_with("The VM helper is unavailable")
            || error.starts_with("The Linux VM helper is unavailable"))
    {
        return Some(Kind::WorkspaceFailed);
    }
    matches!(status, "failed" | "interrupted").then_some(Kind::UnclassifiedFailure)
}

/// The row kind of a liveness finding.
fn liveness_kind(code: crate::liveness::Code) -> Kind {
    match code {
        crate::liveness::Code::TurnStalled => Kind::TurnStalled,
        crate::liveness::Code::InputNotStarted => Kind::InputNotStarted,
        crate::liveness::Code::SessionNotStarted => Kind::SessionNotStarted,
        crate::liveness::Code::ScheduleStalled => Kind::ScheduleStalled,
    }
}

/// The kind of a monitor's end, when Studio caused it. A command with an exit
/// code ended on its own, and an owner's stop is the owner's choice.
fn monitor_kind(record: &Value) -> Option<Kind> {
    match record["status"].as_str() {
        Some("lost") => Some(Kind::MonitorOutcomeLost),
        Some("failed") if record["exitCode"].as_i64().is_none() => {
            let Some(error) = record["error"].as_str().filter(|error| !error.is_empty()) else {
                return Some(Kind::UnclassifiedFailure);
            };
            let owner_stopped = error == MONITOR_OWNER_STOPPED_ERROR
                || error.ends_with(&format!(": {MONITOR_OWNER_STOPPED_ERROR}"));
            (!owner_stopped).then_some(Kind::MonitorNotRecorded)
        }
        _ => None,
    }
}

/// The kind of an input event's state, when Studio caused it.
fn event_kind(status: &str, error: &str, age: f64) -> Option<Kind> {
    match status {
        "failed" if error == studio_runtime_store::STEER_UNCONFIRMED_ERROR => {
            Some(Kind::InputUnconfirmed)
        }
        "uncertain" => Some(Kind::InputUncertain),
        "failed" => Some(Kind::UnclassifiedFailure),
        "pending" if !error.is_empty() && age >= STUCK_INPUT_SECONDS => Some(Kind::InputStuck),
        _ => None,
    }
}

fn retained_causes(kind: Kind, error: &str) -> Vec<Cause> {
    if kind == Kind::UnclassifiedFailure {
        vec![Cause::Unclassified {
            sha256: studio_runtime_store::sha256_hex(error.as_bytes()),
        }]
    } else {
        Vec::new()
    }
}

/// Project known machine facts through their actual record owners; never copy transport extensions.
fn monitor_evidence(record: &Value) -> Result<Cause, serde_json::Error> {
    let _: Option<i64> = serde_json::from_value(record["exitCode"].clone())?;
    let status: MonitorStatus = serde_json::from_value(record["status"].clone())?;
    let operation = record
        .get("operation")
        .filter(|value| !value.is_null())
        .map(|value| {
            serde_json::from_value::<crate::monitor_policy::Operation>(value.clone()).map(
                |operation| MonitorOperationIdentity {
                    agent: operation.agent,
                    epoch: operation.epoch,
                    account_key: operation.account_key,
                    connection_id: operation.connection_id,
                },
            )
        })
        .transpose()?;
    let recovery = record
        .get("reattachRecovery")
        .filter(|value| !value.is_null())
        .map(|value| {
            serde_json::from_value::<crate::monitor_policy::ReattachRecovery>(value.clone()).map(
                |recovery| {
                    recovery
                        .started_at
                        .map(|started_at| MonitorRecoveryEvidence {
                            status: recovery.status,
                            started_at,
                            proof: recovery.proof.map(|proof| {
                                match serde_json::from_value::<MonitorRecoveryReason>(
                                    Value::String(proof.clone()),
                                ) {
                                    Ok(reason) => MonitorRecoveryProof::Recognized { reason },
                                    Err(_) => MonitorRecoveryProof::Unclassified {
                                        sha256: studio_runtime_store::sha256_hex(proof.as_bytes()),
                                    },
                                }
                            }),
                        })
                },
            )
        })
        .transpose()?
        .flatten();
    Ok(Cause::Monitor {
        status,
        operation,
        recovery,
    })
}

fn invalid_monitor(id: &str, at: f64, pointer: Option<String>, cause: Cause) -> StudioError {
    let mut incident = invalid_source(
        format!("monitor:{id}"),
        "runtime_monitors",
        id,
        at,
        vec![
            Cause::StoredRecord {
                object: "runtime_monitors".into(),
                id: id.into(),
                pointer,
            },
            cause,
        ],
    );
    incident.monitor_id = Some(id.into());
    incident
}

#[derive(Clone)]
enum SourceOwner {
    Live(std::sync::Arc<studio_runtime_store::AgentRecord>),
    Deleted,
    Missing,
    Invalid(Vec<Cause>),
}

enum EventSource {
    Record(studio_runtime_store::QueueEvent),
    Missing,
    Invalid(Vec<Cause>),
}

fn malformed_source(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::InvalidStoredObject { .. }
            | StoreError::InvalidStoredRecord { .. }
            | StoreError::InvalidSyncObjectField { .. }
            | StoreError::InvalidSyncObject { .. }
            | StoreError::Json(_)
    )
}

fn event_source(tx: &Transaction<'_>, id: &str) -> Result<EventSource, StoreError> {
    match studio_runtime_store::QueueEventRepository::get(
        tx,
        &studio_runtime_store::QueueEventId::from(id),
    ) {
        Ok(Some(event)) => Ok(EventSource::Record(event)),
        Ok(None) => Ok(EventSource::Missing),
        Err(error) if malformed_source(&error) => Ok(EventSource::Invalid(store_causes(&error))),
        Err(error) => Err(error),
    }
}

fn read_owner(tx: &Transaction<'_>, id: &str) -> Result<SourceOwner, StoreError> {
    match studio_runtime_store::AgentRepository::get(tx, &studio_runtime_store::AgentId::from(id)) {
        Ok(Some(record)) if studio_runtime_store::truthy(record.extra.get("deletedAt")) => {
            Ok(SourceOwner::Deleted)
        }
        Ok(Some(record)) => Ok(SourceOwner::Live(std::sync::Arc::new(record))),
        Ok(None) => Ok(SourceOwner::Missing),
        Err(error) if malformed_source(&error) => Ok(SourceOwner::Invalid(store_causes(&error))),
        Err(error) => Err(error),
    }
}

fn owner_name(record: &studio_runtime_store::AgentRecord) -> String {
    record
        .extra
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Agent")
        .to_owned()
}

fn invalid_source(
    id: String,
    object: &str,
    source_id: &str,
    observed_at: f64,
    causes: Vec<Cause>,
) -> StudioError {
    let kind = Kind::StoredRecordInvalid;
    StudioError {
        id,
        kind,
        level: kind.level(),
        reason: kind.reason().into(),
        target: Target::Details,
        detail: "Invalid retained record".into(),
        subject: String::new(),
        at: observed_at,
        monitor_id: None,
        event_id: None,
        rule_id: None,
        count: None,
        first_at: None,
        diagnostics: Diagnostics {
            context: Context::Stored {
                object: object.into(),
                id: source_id.into(),
            },
            certainty: kind.certainty(),
            causes,
        },
    }
}

fn invalid_owner(id: &str, observed_at: f64, causes: Vec<Cause>) -> StudioError {
    invalid_source(
        format!("stored:runtime_agents:{id}"),
        "runtime_agents",
        id,
        observed_at,
        causes,
    )
}

/// The current rows, newest first, without dismissed ones.
pub fn list(tx: &Transaction<'_>, now: f64) -> Result<Value, StoreError> {
    Ok(json!({"items": build(tx, now)?.0}))
}

/// What a full list build saw beyond its rows: the owners that hide rows
/// because they are missing or deleted, and the number of dismissals.
struct Baseline {
    hidden_owners: BTreeSet<String>,
    dismissed: usize,
    /// Published grouped monitor rows, bounded by MAX_ROWS.
    monitor_groups: bool,
}

/// The list and its baseline.
fn build(tx: &Transaction<'_>, now: f64) -> Result<(Vec<StudioError>, Baseline), StoreError> {
    let since = now - WINDOW_SECONDS;
    let dismissed = dismissed(tx)?;
    let mut hidden_owners = BTreeSet::new();
    // Many rows share an owner; each owner record is read and decoded once.
    let mut owners = std::collections::HashMap::<String, SourceOwner>::new();
    let mut agent = |id: &str| -> Result<SourceOwner, StoreError> {
        if let Some(owner) = owners.get(id) {
            return Ok(owner.clone());
        }
        let owner = read_owner(tx, id)?;
        owners.insert(id.into(), owner.clone());
        Ok(owner)
    };
    let mut rows = std::collections::BTreeMap::<String, StudioError>::new();
    let push = |rows: &mut std::collections::BTreeMap<String, StudioError>, row: StudioError| {
        if !dismissed.contains_key(&row.id) {
            rows.insert(row.id.clone(), row);
        }
    };

    // The store read selects only rows that `monitor_kind` may accept.
    let found = studio_runtime_store::MonitorsRepository::studio_error_candidates(tx, since)?;
    let mut groups = std::collections::BTreeMap::<(String, String), MonitorGroup>::new();
    for (id, raw) in found {
        let record: Value = match serde_json::from_str(&raw) {
            Ok(record) => record,
            Err(error) => {
                push(
                    &mut rows,
                    invalid_monitor(&id, now, None, json_cause(&error)),
                );
                continue;
            }
        };
        let at = record["finished"]
            .as_f64()
            .or_else(|| record["created"].as_f64())
            .unwrap_or(0.0);
        if record["id"].as_str() != Some(id.as_str()) {
            push(
                &mut rows,
                invalid_monitor(
                    &id,
                    now,
                    Some("/id".into()),
                    Cause::SchemaViolation {
                        pointer: "/id".into(),
                        keyword: "identity".into(),
                    },
                ),
            );
            continue;
        }
        if let Some((pointer, keyword)) =
            studio_runtime_schema::first_error("PersistedMonitorRecord", &record)
                .map_err(StoreError::SchemaValidation)?
        {
            push(
                &mut rows,
                invalid_monitor(
                    &id,
                    now,
                    Some(pointer.clone()),
                    Cause::SchemaViolation { pointer, keyword },
                ),
            );
            continue;
        }
        let evidence = match monitor_evidence(&record) {
            Ok(evidence) => evidence,
            Err(error) => {
                push(
                    &mut rows,
                    invalid_monitor(&id, now, None, json_cause(&error)),
                );
                continue;
            }
        };
        let Some(kind) = monitor_kind(&record) else {
            continue;
        };
        let agent_id = record["agent"].as_str().unwrap_or_default();
        let owner = match agent(agent_id)? {
            SourceOwner::Live(owner) => owner,
            SourceOwner::Deleted => {
                hidden_owners.insert(agent_id.to_owned());
                continue;
            }
            SourceOwner::Invalid(causes) => {
                push(&mut rows, invalid_owner(agent_id, now, causes));
                continue;
            }
            SourceOwner::Missing => {
                hidden_owners.insert(agent_id.to_owned());
                push(
                    &mut rows,
                    invalid_monitor(
                        &id,
                        now,
                        Some("/agent".into()),
                        Cause::StoredRecord {
                            object: "runtime_agents".into(),
                            id: agent_id.into(),
                            pointer: None,
                        },
                    ),
                );
                continue;
            }
        };
        if dismissed.contains_key(&format!("{MONITOR_ROW_PREFIX}{id}")) {
            continue;
        }
        let rule = record["ruleId"]
            .as_str()
            .filter(|rule| !rule.is_empty())
            .map(str::to_owned);
        let key = match &rule {
            Some(rule) => format!("rule:{rule}"),
            None => format!(
                "command:{agent_id}:{}",
                &studio_runtime_store::sha256_hex(
                    record["command"].as_str().unwrap_or_default().as_bytes()
                )[..16]
            ),
        };
        let failure = MonitorFailure {
            created: record["created"].as_f64().unwrap_or(0.0),
            at,
            // Commands and arbitrary source errors may contain credentials. Diagnostic facts replace them.
            row: StudioError {
                id: String::new(),
                kind,
                level: kind.level(),
                reason: kind.reason().into(),
                target: kind.target(agent_id.into(), owner_name(&owner)),
                monitor_id: Some(id),
                event_id: None,
                rule_id: rule.clone(),
                count: None,
                first_at: None,
                detail: kind.reason().into(),
                subject: String::new(),
                at,
                diagnostics: Diagnostics {
                    context: Context::Monitor {
                        request_id: record["requestId"].as_str().map(str::to_owned),
                        turn_id: record["turnId"].as_str().map(str::to_owned),
                        epoch: record["epoch"].as_i64(),
                    },
                    certainty: kind.certainty(),
                    causes: std::iter::once(evidence)
                        .chain(retained_causes(
                            kind,
                            record["error"].as_str().unwrap_or_default(),
                        ))
                        .collect(),
                },
            },
        };
        groups
            .entry((key, kind_code(kind)?))
            .or_insert_with(|| MonitorGroup {
                rule,
                failures: Vec::new(),
            })
            .failures
            .push(failure);
    }
    for ((key, code), mut group) in groups {
        let row_id = format!("{MONITOR_GROUP_PREFIX}{key}:{code}");
        let dismissed_at = dismissed.get(&row_id).copied();
        let succeeded_at = match &group.rule {
            Some(rule) => {
                let earliest = group
                    .failures
                    .iter()
                    .map(|failure| failure.created)
                    .fold(f64::INFINITY, f64::min);
                last_rule_success(tx, rule, earliest)?
            }
            None => None,
        };
        group.failures.retain(|failure| {
            succeeded_at.is_none_or(|success| failure.created >= success)
                && dismissed_at.is_none_or(|dismissed| {
                    error_incidents::timestamp_ms(failure.at).is_ok_and(|at| at > dismissed)
                })
        });
        group.failures.sort_by(|a, b| {
            a.at.total_cmp(&b.at)
                .then_with(|| a.row.monitor_id.cmp(&b.row.monitor_id))
        });
        let count = u64::try_from(group.failures.len()).unwrap_or(u64::MAX);
        let Some(first_at) = group.failures.first().map(|failure| failure.at) else {
            continue;
        };
        let Some(last) = group.failures.pop() else {
            continue;
        };
        let mut row = last.row;
        row.id = row_id;
        row.count = Some(count);
        row.first_at = Some(first_at);
        rows.insert(row.id.clone(), row);
    }

    let mut events = tx.prepare_cached(
        "SELECT id FROM runtime_events WHERE created>=?1 AND status IN ('failed','uncertain','pending')",
    )?;
    let ids = events
        .query_map([since], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(events);
    for id in ids {
        let event = match event_source(tx, &id)? {
            EventSource::Record(event) => event,
            EventSource::Missing => continue,
            EventSource::Invalid(causes) => {
                let mut incident =
                    invalid_source(format!("event:{id}"), "runtime_events", &id, now, causes);
                incident.event_id = Some(id);
                push(&mut rows, incident);
                continue;
            }
        };
        let studio_runtime_store::QueueEvent {
            agent: agent_id,
            status,
            error,
            created,
            epoch,
            turn_id,
            ..
        } = event;
        let agent_id = agent_id.0;
        let error = error.unwrap_or_default();
        let Some(kind) = event_kind(&status, &error, now - created) else {
            continue;
        };
        let owner = match agent(&agent_id)? {
            SourceOwner::Live(owner) => owner,
            SourceOwner::Invalid(causes) => {
                push(&mut rows, invalid_owner(&agent_id, now, causes));
                continue;
            }
            SourceOwner::Missing | SourceOwner::Deleted => {
                hidden_owners.insert(agent_id);
                continue;
            }
        };
        push(
            &mut rows,
            StudioError {
                id: format!("event:{id}"),
                kind,
                level: kind.level(),
                reason: kind.reason().into(),
                target: kind.target(agent_id, owner_name(&owner)),
                event_id: Some(id),
                monitor_id: None,
                detail: kind.reason().into(),
                subject: String::new(),
                at: created,
                rule_id: None,
                count: None,
                first_at: None,
                diagnostics: Diagnostics {
                    context: Context::Input { turn_id, epoch },
                    certainty: kind.certainty(),
                    causes: retained_causes(kind, &error),
                },
            },
        );
    }

    let mut agents=tx.prepare_cached("SELECT id FROM runtime_agents WHERE deleted_at IS NULL AND status IN ('failed','interrupted','paused')")?;
    let ids = agents
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(agents);
    for id in ids {
        let record = match agent(&id)? {
            SourceOwner::Live(record) => record,
            SourceOwner::Invalid(causes) => {
                push(&mut rows, invalid_owner(&id, now, causes));
                continue;
            }
            SourceOwner::Missing | SourceOwner::Deleted => continue,
        };
        let error = record
            .extra
            .get("error")
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
            })
            .unwrap_or_default();
        let Some(kind) = agent_kind(record.status.as_str(), &error) else {
            continue;
        };
        let turn = record
            .extra
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| {
                record
                    .extra
                    .get("lastCompletedTurn")
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);
        let at = record
            .extra
            .get("activity")
            .and_then(|value| value.get("at"))
            .and_then(Value::as_f64)
            .or_else(|| record.extra.get("created").and_then(Value::as_f64))
            .unwrap_or(0.0);
        push(
            &mut rows,
            StudioError {
                id: format!("agent:{id}:{}", turn.as_deref().unwrap_or_default()),
                kind,
                level: kind.level(),
                reason: kind.reason().into(),
                target: kind.target(id, owner_name(&record)),
                monitor_id: None,
                event_id: None,
                detail: kind.reason().into(),
                subject: String::new(),
                at,
                rule_id: None,
                count: None,
                first_at: None,
                diagnostics: Diagnostics {
                    context: Context::Agent {
                        turn_id: turn,
                        epoch: Some(record.epoch),
                    },
                    certainty: kind.certainty(),
                    causes: retained_causes(kind, &error),
                },
            },
        );
    }

    for finding in crate::liveness::findings(tx, now)? {
        let kind = liveness_kind(finding.code);
        push(
            &mut rows,
            StudioError {
                id: finding.id,
                kind,
                level: kind.level(),
                reason: kind.reason().into(),
                target: kind.target(finding.agent, finding.agent_name),
                monitor_id: None,
                event_id: finding.event,
                rule_id: None,
                count: None,
                first_at: Some(finding.first_at),
                detail: finding.detail,
                subject: finding.subject,
                at: finding.at,
                diagnostics: Diagnostics {
                    context: Context::Agent {
                        turn_id: None,
                        epoch: None,
                    },
                    certainty: kind.certainty(),
                    causes: Vec::new(),
                },
            },
        );
    }

    let mut incidents = tx.prepare_cached("SELECT id,payload FROM runtime_error_incidents WHERE created>=?1 AND payload!='null' ORDER BY created DESC,id LIMIT ?2")?;
    for record in incidents.query_map(
        rusqlite::params![
            error_incidents::timestamp_ms(since.max(0.0))?,
            MAX_ROWS as i64
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, rusqlite::types::Value>(1)?,
            ))
        },
    )? {
        let (key, encoded) = record?;
        let logical_id = key.clone();
        let source = Cause::StoredRecord {
            object: "runtime_error_incidents".into(),
            id: key.clone(),
            pointer: Some("/payload".into()),
        };
        let decoded = match encoded {
            rusqlite::types::Value::Text(encoded) => {
                match serde_json::from_str::<Value>(&encoded).and_then(|value| {
                    serde_json::from_value::<StudioError>(value).map(|mut incident| {
                        incident.level = incident.kind.level();
                        incident
                    })
                }) {
                    Ok(incident) => Ok(incident),
                    Err(error) => Err(vec![source.clone(), json_cause(&error)]),
                }
            }
            rusqlite::types::Value::Null
            | rusqlite::types::Value::Integer(_)
            | rusqlite::types::Value::Real(_)
            | rusqlite::types::Value::Blob(_) => Err(vec![
                source.clone(),
                Cause::SchemaViolation {
                    pointer: "/payload".into(),
                    keyword: "type".into(),
                },
            ]),
        };
        let incident = match decoded {
            Ok(incident) => incident,
            Err(causes) => {
                push(
                    &mut rows,
                    invalid_source(logical_id, "runtime_error_incidents", &key, now, causes),
                );
                continue;
            }
        };
        let payload = json!({"collection":"studioError","id":logical_id,"value":incident});
        if logical_id != incident.id
            || !matches!(incident.kind, Kind::HttpFailure | Kind::RecoveryFailed)
            || !matches!(incident.target, Target::Details)
            || !studio_runtime_schema::validate_sync_entity_payload_strict(
                "studioError",
                &logical_id,
                false,
                &payload,
            )
            .map_err(StoreError::SchemaValidation)?
        {
            push(
                &mut rows,
                invalid_source(
                    logical_id,
                    "runtime_error_incidents",
                    &key,
                    now,
                    vec![
                        source,
                        Cause::SchemaViolation {
                            pointer: "/payload".into(),
                            keyword: "contract".into(),
                        },
                    ],
                ),
            );
            continue;
        }
        if incident.at >= since {
            push(&mut rows, incident);
        }
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|a, b| b.at.total_cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
    rows.truncate(MAX_ROWS);
    let baseline = Baseline {
        hidden_owners,
        dismissed: dismissed.len(),
        monitor_groups: rows
            .iter()
            .any(|row| row.id.starts_with(MONITOR_GROUP_PREFIX)),
    };
    Ok((rows, baseline))
}

/// Server-owned refresh cadence for time-derived membership (stuck input and
/// the error history window). Unit: time; clients never poll this collection.
const TIME_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Seed persisted errors on startup and refresh time-derived membership.
pub async fn refresh(store: &studio_runtime_store::RuntimeStore) -> Result<(), StoreError> {
    store.transaction(publish).await
}

/// Source writes publish immediately through DefaultPublisher. Time alone may
/// also change membership, so the same owner refreshes until server shutdown.
pub async fn run(
    store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    while !*shutdown.borrow() {
        tokio::select! {
            () = tokio::time::sleep(TIME_REFRESH_INTERVAL) => {},
            _ = shutdown.changed() => return,
        }
        if let Err(error) = refresh(&store).await {
            eprintln!(
                "Studio error entity refresh failed: {}",
                json!({"causes": store_causes(&error)})
            );
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Full list rebuilds on this writer thread, for tests that prove a write
    /// did not rebuild the list.
    static FULL_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Full list rebuilds on the calling writer thread so far.
#[cfg(test)]
pub(crate) fn full_publications() -> u64 {
    FULL_PUBLICATIONS.with(std::cell::Cell::get)
}

/// Publish the current server-owned error rows as sync entities in the same
/// transaction as a source row or dismissal changes. Equal rows retain their
/// existing entity sequence, while removed rows publish tombstones.
pub fn publish(tx: &Transaction<'_>) -> Result<(), StoreError> {
    #[cfg(test)]
    FULL_PUBLICATIONS.with(|count| count.set(count.get() + 1));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
        .as_secs_f64();
    let (items, baseline) = build(tx, now)?;
    let mut current = std::collections::BTreeSet::new();
    for item in &items {
        let id = &item.id;
        studio_runtime_store::SyncDocumentsRepository::put_entity(
            tx,
            "studioError",
            id,
            serde_json::to_value(item)?,
            false,
        )?;
        current.insert(id.clone());
    }
    let mut statement = tx.prepare_cached(
        "SELECT id FROM sync_entities WHERE collection='studioError' AND deleted=0",
    )?;
    let existing = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for id in existing {
        if !current.contains(&id) {
            studio_runtime_store::SyncDocumentsRepository::put_entity(
                tx,
                "studioError",
                &id,
                Value::Null,
                true,
            )?;
        }
    }
    BASELINE.with(|current| *current.borrow_mut() = Some(baseline));
    Ok(())
}

thread_local! {
    /// The baseline of the last full publication on this writer thread. A store
    /// runs all its writer transactions on one thread, so this is per store.
    /// `None` until the first full publication, which `entity_publication::install`
    /// runs at startup. A rollback after a publication can leave it newer than
    /// the committed state; then a missed change waits for the next refresh of
    /// `run`, at most `TIME_REFRESH_INTERVAL`.
    static BASELINE: std::cell::RefCell<Option<Baseline>> = const { std::cell::RefCell::new(None) };
}

/// Publish the list in the writer transaction that changed `sources`, unless
/// no changed row can change it. A full build reads every candidate row of
/// the window, so most writer transactions, which touch rows outside the list,
/// must skip it.
pub fn publish_changes(
    tx: &Transaction<'_>,
    sources: &StudioErrorSources,
) -> Result<(), StoreError> {
    if may_change(tx, sources)? {
        publish(tx)?;
    }
    Ok(())
}

/// Whether the changed `sources` can change the published list. The published
/// rows are the newest `MAX_ROWS` visible rows, so a row that is neither
/// published nor a visible candidate now cannot change them. A deleted row has
/// no id left; it matters only if a published row lost its source.
fn may_change(tx: &Transaction<'_>, sources: &StudioErrorSources) -> Result<bool, StoreError> {
    let Some(dismissed_count) = BASELINE.with(|baseline| {
        baseline
            .borrow()
            .as_ref()
            .map(|baseline| baseline.dismissed)
    }) else {
        return Ok(true);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
        .as_secs_f64();
    let since = now - WINDOW_SECONDS;
    if !sources.incidents.is_empty()
        || (sources.incidents_removed && dismissed(tx)?.len() != dismissed_count)
    {
        return Ok(true);
    }
    let monitor_groups = BASELINE.with(|baseline| {
        baseline
            .borrow()
            .as_ref()
            .is_some_and(|baseline| baseline.monitor_groups)
    });
    // A deleted earlier member changes a group's count even when its final
    // monitor remains. Deletions carry no source ID to check individually.
    if (sources.rows_removed && monitor_groups)
        || ((sources.rows_removed || sources.incidents_removed) && published_source_missing(tx)?)
    {
        return Ok(true);
    }
    for id in &sources.monitors {
        if monitor_groups && tx.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sync_entities e WHERE e.collection='studioError' AND e.deleted=0 AND (             json_extract(e.payload,'$.value.monitorId')=?1 OR              json_extract(e.payload,'$.value.ruleId')=(SELECT json_extract(m.record,'$.ruleId') FROM runtime_monitors m                  WHERE m.id=?1 AND json_extract(m.record,'$.status')='completed')))"
        )?.query_row([id], |row| row.get::<_, bool>(0))? {
            return Ok(true);
        }
        if is_published(tx, &format!("monitor:{id}"))? {
            return Ok(true);
        }
        let candidate =
            studio_runtime_store::MonitorsRepository::studio_error_candidate(tx, id, since)?;
        if let Some(raw) = candidate {
            match serde_json::from_str::<Value>(&raw) {
                Err(_) => return Ok(true),
                Ok(record) => {
                    if studio_runtime_schema::first_error("PersistedMonitorRecord", &record)
                        .map_err(StoreError::SchemaValidation)?
                        .is_some()
                        || monitor_evidence(&record).is_err()
                        || monitor_kind(&record).is_some()
                    {
                        return Ok(true);
                    }
                }
            }
        }
    }
    for id in &sources.events {
        if is_published(tx, &format!("event:{id}"))? {
            return Ok(true);
        }
        match event_source(tx, id)? {
            EventSource::Invalid(_) => return Ok(true),
            EventSource::Record(event)
                if event.created >= since
                    && event_kind(
                        &event.status,
                        event.error.as_deref().unwrap_or_default(),
                        now - event.created,
                    )
                    .is_some() =>
            {
                return Ok(true);
            }
            EventSource::Record(_) | EventSource::Missing => {}
        }
    }
    for id in &sources.agents {
        if agent_may_change(tx, id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether a write of agent `id` can change the list: through its own error
/// row, the owner name or visibility of its published rows, or the
/// visibility of rows it hid at the last full build.
fn agent_may_change(tx: &Transaction<'_>, id: &str) -> Result<bool, StoreError> {
    let own_prefix = format!("agent:{id}:");
    let own_published: bool = tx
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='studioError' AND deleted=0 AND id>=?1 AND id<?2)",
        )?
        .query_row(
            rusqlite::params![own_prefix, format!("agent:{id};")],
            |row| row.get(0),
        )?;
    if own_published {
        return Ok(true);
    }
    if is_published(tx, &format!("stored:runtime_agents:{id}"))? {
        return Ok(true);
    }
    let owner = match read_owner(tx, id)? {
        SourceOwner::Invalid(_) => return Ok(true),
        SourceOwner::Live(record) => {
            let error = record
                .extra
                .get("error")
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                })
                .unwrap_or_default();
            if agent_kind(record.status.as_str(), &error).is_some() {
                return Ok(true);
            }
            Some(owner_name(&record))
        }
        SourceOwner::Missing | SourceOwner::Deleted => None,
    };
    let mut statement = tx.prepare_cached(
        "SELECT json_extract(payload,'$.value.agentName') FROM sync_entities WHERE collection='studioError' AND deleted=0 AND json_extract(payload,'$.value.agentId')=?1",
    )?;
    let names = statement
        .query_map([id], |row| row.get::<_, Option<String>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if names.iter().any(|name| name.as_deref() != owner.as_deref()) {
        return Ok(true);
    }
    Ok(owner.is_some()
        && BASELINE.with(|baseline| {
            baseline
                .borrow()
                .as_ref()
                .is_none_or(|baseline| baseline.hidden_owners.contains(id))
        }))
}

/// Whether row `id` of the list is published.
fn is_published(tx: &Transaction<'_>, id: &str) -> Result<bool, StoreError> {
    Ok(tx
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='studioError' AND id=?1 AND deleted=0)",
        )?
        .query_row([id], |row| row.get(0))?)
}

/// Whether a published row lost its monitor, event or owner row.
fn published_source_missing(tx: &Transaction<'_>) -> Result<bool, StoreError> {
    Ok(tx
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM sync_entities e WHERE e.collection='studioError' AND e.deleted=0 AND (\
             (e.id LIKE 'monitor:%' AND NOT EXISTS(SELECT 1 FROM runtime_monitors m WHERE m.id=substr(e.id,9))) OR \
             (e.id LIKE 'event:%' AND NOT EXISTS(SELECT 1 FROM runtime_events v WHERE v.id=substr(e.id,7))) OR \
             (e.id LIKE 'stored:runtime_agents:%' AND NOT EXISTS(SELECT 1 FROM runtime_agents a WHERE a.id=json_extract(e.payload,'$.value.diagnostics.context.id'))) OR \
             ((e.id LIKE 'http:%' OR e.id LIKE 'recovery:%') AND NOT EXISTS(SELECT 1 FROM runtime_error_incidents r WHERE r.id=e.id AND r.payload!='null')) OR \
             (json_extract(e.payload,'$.value.action')!='details' AND NOT EXISTS(SELECT 1 FROM runtime_agents a WHERE a.id=json_extract(e.payload,'$.value.agentId')))))",
        )?
        .query_row([], |row| row.get(0))?)
}

/// One failed or lost run of a rule or command, with its row.
struct MonitorFailure {
    /// Unix seconds of its start.
    created: f64,
    /// Unix seconds of its end, or of its start when it has none.
    at: f64,
    row: StudioError,
}

/// The failures of one rule or command and kind.
struct MonitorGroup {
    rule: Option<String>,
    failures: Vec<MonitorFailure>,
}

/// The serialized code of `kind`, owned by the schema enum.
fn kind_code(kind: Kind) -> Result<String, StoreError> {
    match serde_json::to_value(kind)? {
        Value::String(code) => Ok(code),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            Err(StoreError::SchemaMismatch("studio error kind".into()))
        }
    }
}

/// Unix seconds of the start of the latest successful run of `rule` that
/// started at or after `since`.
fn last_rule_success(
    tx: &Transaction<'_>,
    rule: &str,
    since: f64,
) -> Result<Option<f64>, StoreError> {
    Ok(tx.query_row(
        "SELECT MAX(json_extract(record,'$.created')) FROM runtime_monitors WHERE json_extract(record,'$.status')='completed' AND json_extract(record,'$.created')>=?1 AND json_extract(record,'$.ruleId')=?2",
        rusqlite::params![since, rule],
        |row| row.get::<_, Option<f64>>(0),
    )?)
}

/// Dismissed row ids, each with the Unix seconds of its latest dismissal.
fn dismissed(tx: &Transaction<'_>) -> Result<std::collections::BTreeMap<String, i64>, StoreError> {
    let mut statement = tx.prepare_cached(
        "SELECT id,dismissed_at FROM runtime_error_incidents WHERE dismissed_at IS NOT NULL",
    )?;
    let ids = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// Hide row `id` from the list. Dismissing it again changes nothing, except
/// for the row of a rule or command: that hides its failures up to `now`.
pub fn dismiss(tx: &Transaction<'_>, id: &str, now: f64) -> Result<(), StoreError> {
    error_incidents::dismiss(tx, id, now, id.starts_with(MONITOR_GROUP_PREFIX))
}

/// Dismiss only currently listed errors. The caller owns the encompassing transaction.
/// Unknown and already dismissed ids have no effect, and duplicate ids are reported once.
pub fn dismiss_many(tx: &Transaction<'_>, ids: &[String], now: f64) -> Result<Value, StoreError> {
    let before = list(tx, now)?;
    let known: std::collections::BTreeSet<_> = before["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str())
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut dismissed_ids = Vec::new();
    let mut unknown_ids = Vec::new();
    for id in ids {
        if !seen.insert(id) {
            continue;
        }
        if known.contains(id.as_str()) {
            dismiss(tx, id, now)?;
            dismissed_ids.push(id);
        } else {
            unknown_ids.push(id);
        }
    }
    let mut result = list(tx, now)?;
    result["dismissedIds"] = json!(dismissed_ids);
    result["unknownIds"] = json!(unknown_ids);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_success_retires_the_published_failure_group_without_a_timer() {
        use studio_runtime_store::{
            AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table,
        };
        let root = tempfile::tempdir().expect("valid group publication fixture");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("valid group publication fixture");
        crate::entity_publication::install(&store)
            .await
            .expect("valid group publication fixture");
        let now = crate::event_write::unix_time().expect("valid group publication fixture");
        store.transaction(move |tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,"status":"idle","provider":"codex"}))?)?;
            Table::new(tx).put(&MonitorRecord::from_value(json!({"id":"failed","agent":"owner","ruleId":"rule","command":"check","created":now - 10.,"finished":now - 9.,"status":"failed","error":"No live provider session","exitCode":null}))?)?;
            Ok::<_, StoreError>(())
        }).await.expect("valid group publication fixture");
        store.transaction(move |tx| {
            let count: i64 = tx.query_row("SELECT count(*) FROM sync_entities WHERE collection='studioError' AND deleted=0", [], |r| r.get(0))?;
            assert_eq!(count, 1, "the failure is published");
            Table::new(tx).put(&MonitorRecord::from_value(json!({"id":"success","agent":"owner","ruleId":"rule","command":"check","created":now,"finished":now,"status":"completed","exitCode":0}))?)?;
            Ok::<_, StoreError>(())
        }).await.expect("valid group publication fixture");
        store.test_read_transaction(|tx| {
            let count: i64 = tx.query_row("SELECT count(*) FROM sync_entities WHERE collection='studioError' AND deleted=0", [], |r| r.get(0))?;
            assert_eq!(count, 0, "the success retires the group before the periodic refresh");
            Ok::<_, StoreError>(())
        }).expect("valid group publication fixture");
    }

    #[test]
    fn a_thread_start_unknown_turn_is_a_disconnected_turn() {
        assert_eq!(
            agent_kind("failed", PROVIDER_THREAD_START_UNKNOWN_ERROR),
            Some(Kind::TurnDisconnected)
        );
    }

    #[test]
    fn monitor_evidence_omits_closed_recovery_without_start() {
        let record = json!({"status":"lost","reattachRecovery":{"accountKey":"account","epoch":2,"status":"not_reattachable","proof":"legacy"}});
        let encoded =
            serde_json::to_value(monitor_evidence(&record).expect("legacy recovery")).unwrap();
        assert!(encoded.get("recovery").is_none() || encoded["recovery"].is_null());
    }

    #[test]
    fn monitor_evidence_keeps_identity_and_correlates_private_unclassified_proof() {
        let proof = "password=proof-canary";
        let mut record = json!({"status":"lost","operation":{"agent":"worker","epoch":7,"accountKey":"account","connectionId":"connection","credentials":"operation-canary"},"reattachRecovery":{"accountKey":"account","epoch":7,"status":"not_reattachable","startedAt":123.0,"proof":proof}});
        assert!(
            monitor_evidence(&record).is_err(),
            "unknown operation fields are refused"
        );
        record
            .get_mut("operation")
            .and_then(Value::as_object_mut)
            .unwrap()
            .remove("credentials");
        let evidence = monitor_evidence(&record).expect("known source owners");
        let encoded = serde_json::to_value(evidence).expect("safe evidence");
        assert_eq!(encoded["operation"]["connectionId"], "connection");
        assert_eq!(
            encoded["recovery"]["proof"]["classification"],
            "unclassified"
        );
        assert_eq!(
            encoded["recovery"]["proof"]["sha256"],
            studio_runtime_store::sha256_hex(proof.as_bytes())
        );
        assert!(!encoded.to_string().contains("canary"));
    }

    #[test]
    fn each_studio_cause_has_its_kind_and_an_agent_cause_has_none() {
        assert_eq!(
            monitor_kind(&json!({"status":"lost","exitCode":null})),
            Some(Kind::MonitorOutcomeLost)
        );
        assert_eq!(
            monitor_kind(
                &json!({"status":"failed","exitCode":null,"error":"Monitor changed during preparation"})
            ),
            Some(Kind::MonitorNotRecorded)
        );
        assert_eq!(
            monitor_kind(&json!({"status":"failed","exitCode":1,"error":""})),
            None,
            "an exit code is the command's own end"
        );
        assert_eq!(
            monitor_kind(&json!({"status":"failed","exitCode":null,
                "error":format!("The command was not started: {MONITOR_OWNER_STOPPED_ERROR}")})),
            None,
            "the owner stopped the turn"
        );
        assert_eq!(
            event_kind(
                "failed",
                studio_runtime_schema::shared_values::PROVIDER_START_NOT_SENT_ERROR,
                0.0
            ),
            Some(Kind::UnclassifiedFailure)
        );
        assert_eq!(
            event_kind("failed", studio_runtime_store::STEER_UNCONFIRMED_ERROR, 0.0),
            Some(Kind::InputUnconfirmed)
        );
        assert_eq!(event_kind("uncertain", "", 0.0), Some(Kind::InputUncertain));
        assert_eq!(
            event_kind(
                "pending",
                PROVIDER_RESUME_UNKNOWN_ERROR,
                STUCK_INPUT_SECONDS
            ),
            Some(Kind::InputStuck)
        );
        assert_eq!(
            event_kind("pending", PROVIDER_RESUME_UNKNOWN_ERROR, 1.0),
            None,
            "young input still waits for its start"
        );
        assert_eq!(event_kind("pending", "", STUCK_INPUT_SECONDS), None);
        assert_eq!(
            agent_kind("interrupted", PROVIDER_DISCONNECTED_TURN_ERROR),
            Some(Kind::TurnDisconnected)
        );
        assert_eq!(
            agent_kind("paused", "workspace-readiness/token-stale"),
            Some(Kind::WorkspaceFailed)
        );
        for error in [
            VM_PROVIDER_DISCONNECTED_TURN_ERROR,
            LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
        ] {
            assert_eq!(
                agent_kind("interrupted", error),
                Some(Kind::TurnDisconnected)
            );
        }
        for error in [
            "The VM helper is unavailable",
            "The Linux VM helper is unavailable",
        ] {
            assert_eq!(agent_kind("failed", error), Some(Kind::WorkspaceFailed));
        }
        assert_eq!(agent_kind("paused", "Stopped by user"), None);
        assert_eq!(
            agent_kind("failed", "This content was flagged"),
            Some(Kind::UnclassifiedFailure)
        );
    }

    #[test]
    fn only_history_decided_kinds_offer_resume() {
        for kind in [
            Kind::InputUncertain,
            Kind::InputStuck,
            Kind::TurnDisconnected,
        ] {
            assert!(
                matches!(
                    kind.target("agent".into(), "Agent".into()),
                    Target::Resume { .. }
                ),
                "{kind:?}"
            );
        }
        for kind in [
            Kind::MonitorOutcomeLost,
            Kind::MonitorNotRecorded,
            Kind::InputUnconfirmed,
            Kind::WorkspaceFailed,
        ] {
            assert!(
                matches!(
                    kind.target("agent".into(), "Agent".into()),
                    Target::Check { .. }
                ),
                "{kind:?}"
            );
        }
    }

    /// The monitor read selects rows in SQL so that the list no longer parses
    /// every command that ended with its own exit code. The list must stay
    /// identical to the former read of every lost or failed monitor in the
    /// window, and the SQL selection must hold every row `monitor_kind`
    /// accepts.
    #[tokio::test]
    async fn monitor_rows_equal_the_full_read_and_the_sql_selection_holds_every_kind() {
        let root = tempfile::tempdir().expect("state directory");
        let store = studio_runtime_store::RuntimeStore::open(Some(root.path()))
            .await
            .expect("store opens");
        let now = 1_000_000_000.0;
        store
            .transaction(move |tx| {
                for (id, extra) in [
                    ("owner", json!({"name":"Fixture worker"})),
                    ("gone", json!({"name":"Deleted worker","deletedAt":now})),
                ] {
                    let mut agent = json!({"id":id,"rootId":id,"epoch":0,"status":"waiting",
                        "autoWake":true,"cwd":std::env::temp_dir().to_string_lossy(),
                        "accountKey":"default","provider":"codex"});
                    agent.as_object_mut().expect("object").extend(
                        extra.as_object().expect("object").clone(),
                    );
                    studio_runtime_store::AgentRepository::put(
                        tx,
                        &studio_runtime_store::AgentRecord::from_value(agent)?,
                    )?;
                }
                let stopped = format!("The command was not started: {MONITOR_OWNER_STOPPED_ERROR}");
                let old = now - WINDOW_SECONDS - 1.0;
                let monitors = [
                    ("lost", json!({"status":"lost","exitCode":null,"error":"lost result"})),
                    ("lost-no-error", json!({"status":"lost"})),
                    ("not-recorded", json!({"status":"failed","exitCode":null,"error":"Server restarted"})),
                    ("probe-no-session", json!({"status":"failed","exitCode":null,"error":"No live provider session","ruleId":"rule"})),
                    ("no-exit-key", json!({"status":"failed","error":"Callback barrier was not completed"})),
                    ("probe-exit", json!({"status":"failed","exitCode":1,"error":"","ruleId":"rule"})),
                    ("own-exit-error", json!({"status":"failed","exitCode":2,"error":"boom"})),
                    ("owner-stopped", json!({"status":"failed","exitCode":null,"error":MONITOR_OWNER_STOPPED_ERROR})),
                    ("owner-stopped-prefixed", json!({"status":"failed","exitCode":null,"error":stopped})),
                    ("empty-error", json!({"status":"failed","exitCode":null,"error":""})),
                    ("number-error", json!({"status":"failed","exitCode":null,"error":7})),
                    ("completed", json!({"status":"completed","exitCode":null,"error":"x"})),
                    ("deleted-owner", json!({"status":"lost","agent":"gone"})),
                    ("missing-owner", json!({"status":"lost","agent":"nobody"})),
                    ("old-lost", json!({"status":"lost","created":old})),
                ];
                for (id, fields) in monitors {
                    let mut record = json!({"id":id,"agent":"owner","command":format!("run {id}"),
                        "created":now - 5.0,"finished":now - 1.0});
                    record.as_object_mut().expect("object").extend(
                        fields.as_object().expect("object").clone(),
                    );
                    tx.execute(
                        "INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)",
                        rusqlite::params![id, record.to_string()],
                    )?;
                }

                let since = now - WINDOW_SECONDS;
                let selected = studio_runtime_store::MonitorsRepository::studio_error_candidates(
                    tx, since,
                )?
                .into_iter()
                .map(|(id, _)| id)
                .collect::<std::collections::BTreeSet<_>>();
                // The former read: every lost or failed monitor in the window.
                let mut statement = tx.prepare(
                    "SELECT id,record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('lost','failed') AND json_extract(record,'$.created')>=?1",
                )?;
                let full = statement
                    .query_map([since], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                let mut expected = std::collections::BTreeMap::new();
                for (id, raw) in full {
                    let record: Value = serde_json::from_str(&raw).expect("fixture JSON");
                    if id=="number-error" {continue;}
                    if let Some(kind) = monitor_kind(&record) {
                        assert!(selected.contains(&id), "SQL selection drops {id}");
                        if record["agent"] == "owner" {
                            expected.insert(format!("monitor:{id}"), (serde_json::to_value(kind)?.as_str().expect("kind string").to_owned(), "Fixture worker"));
                        }
                    }
                }
                assert_eq!(
                    expected.keys().map(String::as_str).collect::<Vec<_>>(),
                    [
                        "monitor:empty-error",
                        "monitor:lost",
                        "monitor:lost-no-error",
                        "monitor:no-exit-key",
                        "monitor:not-recorded",
                        "monitor:probe-no-session",
                    ]
                );
                assert!(!selected.contains("probe-exit") && !selected.contains("own-exit-error"));

                let rows=list(tx,now)?;
                let invalid:Vec<_>=rows["items"].as_array().expect("items").iter().filter(|row|row["kind"]=="stored_record_invalid").collect();
                assert_eq!(invalid.len(),2,"invalid error type and missing owner both remain visible");
                assert!(invalid.iter().all(|row|row["action"]=="details" && row.get("agentId").is_none()));
                assert!(invalid.iter().any(|row|row["id"]=="monitor:number-error" && row["diagnostics"]["causes"][0]["pointer"]=="/error"));
                assert!(invalid.iter().any(|row|row["id"]=="monitor:missing-owner" && row["diagnostics"]["causes"][1]["id"]=="nobody"));
                let listed = rows["items"]
                    .as_array()
                    .expect("items")
                    .iter()
                    .filter(|row| row["monitorId"].is_string() && row["kind"]!="stored_record_invalid")
                    .map(|row| {
                        assert!(row["id"].as_str().expect("id").starts_with(MONITOR_GROUP_PREFIX));
                        let monitor = row["monitorId"].as_str().expect("monitor id");
                        let id = format!("monitor:{monitor}");
                        assert_eq!(row["subject"], "", "command bodies stay out of diagnostic exports");
                        assert_eq!(row["at"], now - 1.0);
                        (id, (row["kind"].as_str().expect("kind"), row["agentName"].as_str().expect("name")))
                    })
                    .map(|(id, (kind, name))| (id, (kind.to_owned(), name.to_owned())))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let expected = expected
                    .into_iter()
                    .map(|(id, (kind, name))| (id, (kind.to_owned(), name.to_owned())))
                    .collect::<std::collections::BTreeMap<_, _>>();
                assert_eq!(listed, expected);
                Ok(())
            })
            .await
            .expect("fixture transaction");
    }
}
