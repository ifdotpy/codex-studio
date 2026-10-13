//! Thin transactional adapter for notification decisions.
use super::effects::NotificationDependency;
use super::snapshot::{NotificationSnapshot, checked};
use super::{
    DispatchOutcome, NotificationDispatchError, ProviderDeliveryIdentity, persist_agent,
    update_common_agent_fields,
};
use super::{items, notices, questions};
use rusqlite::params;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, NotificationDispatchRepository, PlanRecord, StoreError, Table, TaskId, TaskRecord,
    Transaction,
};

pub(super) fn apply(
    tx: &Transaction<'_>,
    agent: &mut Value,
    method: &str,
    p: &Value,
    identity: &ProviderDeliveryIdentity,
    publication: &super::PublicationContext,
    at: f64,
) -> Result<Option<DispatchOutcome>, NotificationDispatchError> {
    let id = agent["id"]
        .as_str()
        .ok_or(NotificationDispatchError::InvalidNotification)?
        .to_owned();
    let account = identity
        .provider_handle
        .strip_prefix("account:")
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    if let Some(kind) = notices::NoticeKind::parse(method) {
        if p["threadId"].as_str().is_none_or(str::is_empty)
            && matches!(
                kind,
                notices::NoticeKind::Warning
                    | notices::NoticeKind::Guardian
                    | notices::NoticeKind::Config
                    | notices::NoticeKind::Deprecation
                    | notices::NoticeKind::McpStartup
                    | notices::NoticeKind::McpLogin
                    | notices::NoticeKind::StrictReview
                    | notices::NoticeKind::AuthStarted
                    | notices::NoticeKind::AuthCompleted
            )
        {
            return Ok(Some(required(NotificationDependency::AccountNotice)));
        }
        let item_id = format!(
            "{id}:native-notice:hook:{}",
            p["run"]["id"].as_str().unwrap_or_default()
        );
        let old =
            NotificationDispatchRepository::notification_item(tx, &id, &item_id)?.map(|v| v.0);
        let snapshot = NotificationSnapshot::from_records(agent.clone(), None, old)?;
        let decision =
            notices::decide(&snapshot, method, kind, p, account, identity.generation, at)?;
        if decision.refresh_limits {
            return Err(NotificationDispatchError::Dependency(
                NotificationDependency::RefreshLimits.name(),
            ));
        }
        if let Some(item) = decision.item {
            write_item(
                tx,
                &id,
                &item.key,
                ("system", "Codex"),
                &item.text,
                item.metadata,
                at,
            )?;
        }
        if let Some(next) = decision.agent {
            *agent = next;
            persist_agent(tx, &id, agent, publication)?;
        }
        return Ok(Some(DispatchOutcome::Written));
    }
    let dependency = match method {
        "account/login/completed" => Some(NotificationDependency::AccountLogin),
        "account/updated" => Some(NotificationDependency::AccountRefresh),
        "account/rateLimits/updated" => Some(NotificationDependency::RateLimits),
        "provider/generationStarted" => Some(NotificationDependency::TokenRate),
        "command/exec/outputDelta" => Some(NotificationDependency::MonitorOutput),
        "turn/started" | "turn/completed" => Some(NotificationDependency::TurnLifecycle),
        kind if kind.starts_with("thread/realtime/") => Some(NotificationDependency::Realtime),
        "thread/closed" => Some(NotificationDependency::ThreadUnloaded),
        "thread/status/changed" if p["status"]["type"] == "notLoaded" => {
            Some(NotificationDependency::ThreadUnloaded)
        }
        _ => None,
    };
    if let Some(dependency) = dependency {
        if dependency == NotificationDependency::Realtime {
            // With no configured voice session, Python retains these as thread events.
            update_common_agent_fields(agent, method, p, at)?;
            persist_agent(tx, &id, agent, publication)?;
        } else if dependency == NotificationDependency::ThreadUnloaded
            && agent["nativeRelease"]["threadId"] == p["threadId"]
            && !agent["nativeRelease"].is_null()
        {
            agent["nativeRelease"]["closedAt"] = json!(at);
            agent["nativeRelease"]["resetPending"] = json!(false);
            persist_agent(tx, &id, agent, publication)?;
        }
        return Ok(Some(required(dependency)));
    }
    if method == "serverRequest/resolved" {
        let before = agent.clone();
        questions::resolved(tx, agent, p, account, identity.generation, at)?;
        if *agent != before {
            persist_agent(tx, &id, agent, publication)?;
        }
        return Ok(Some(DispatchOutcome::Written));
    }
    if method == "thread/tokenUsage/updated"
        || method == "item/agentMessage/delta"
        || (matches!(method, "item/started" | "item/completed")
            && p["item"]["type"] == "agentMessage")
    {
        return Ok(None);
    }
    let stale = method.starts_with("item/")
        && p["turnId"]
            .as_str()
            .is_some_and(|t| !t.is_empty() && agent["turnId"] != t);
    let task_key = format!(
        "{id}:{}",
        p["item"]["id"]
            .as_str()
            .or_else(|| p["itemId"].as_str())
            .unwrap_or_default()
    );
    let task = if matches!(
        method,
        "item/started" | "item/completed" | "item/commandExecution/outputDelta"
    ) {
        Table::<TaskRecord>::new(tx)
            .get(&TaskId::from(task_key.as_str()))?
            .map(|r| r.value().clone())
    } else {
        None
    };
    let snapshot = NotificationSnapshot::from_records(agent.clone(), task, None)?;
    match method {
        "item/started" | "item/completed" => {
            if p["item"]["type"] == "userMessage"
                && p["item"]["clientId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            {
                return Ok(Some(required(NotificationDependency::UserMessageReceipt)));
            }
            if stale {
                if snapshot
                    .task()
                    .is_none_or(|task| task["status"] == "running")
                    && let Some(task) = items::task_transition(
                        snapshot.agent(),
                        method,
                        p,
                        snapshot.task(),
                        true,
                        at,
                    )?
                {
                    put_task(tx, &task)?;
                    write_historical_item(tx, &id, p, &task, at)?;
                }
                return Ok(Some(DispatchOutcome::Ignored {
                    reason: "stale-turn-item".into(),
                }));
            }
            let already_compacted = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_compactions WHERE id=?1)",
                    [&task_key],
                    |r| r.get(0),
                )
                .map_err(StoreError::from)?;
            let decision =
                items::non_text(agent, method, p, snapshot.task(), already_compacted, at)?;
            if let Some(task) = decision.task {
                put_task(tx, &task)?;
            }
            if decision.compacted {
                tx.execute(
                    "INSERT OR IGNORE INTO runtime_compactions VALUES (?1,?2)",
                    params![task_key, id],
                )
                .map_err(StoreError::from)?;
            }
            if let Some(item) = decision.item {
                let key = p["item"]["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or(NotificationDispatchError::InvalidNotification)?;
                let kind = p["item"]["type"]
                    .as_str()
                    .ok_or(NotificationDispatchError::InvalidNotification)?;
                write_item(
                    tx,
                    &id,
                    key,
                    ("output", kind),
                    &notices::ordered_text(
                        &item,
                        p["item"]
                            .as_object()
                            .into_iter()
                            .flat_map(|o| o.keys().cloned())
                            .chain(
                                [
                                    "aggregatedOutput",
                                    "outputTruncated",
                                    "startedAtMs",
                                    "completedAtMs",
                                    "durationMs",
                                ]
                                .map(str::to_owned),
                            ),
                    ),
                    json!({"toolStatus":items::tool_status(&item,method=="item/started"),"turnId":turn(agent,p)}),
                    at,
                )?;
            }
        }
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
            if stale {
                return Ok(Some(DispatchOutcome::Ignored {
                    reason: "stale-turn-item".into(),
                }));
            }
            items::advance_reasoning(agent, p, at);
        }
        "turn/plan/updated" | "turn/diff/updated" => {
            if method == "turn/plan/updated" {
                let key = AgentId::from(id.as_str());
                let mut plan = Table::<PlanRecord>::new(tx)
                    .get(&key)?
                    .map(|r| r.value().clone())
                    .unwrap_or_else(
                        || json!({"id":id,"rootId":agent["rootId"],"text":"","version":0}),
                    );
                plan["native"] = p.clone();
                plan["steps"] = p.get("plan").cloned().unwrap_or(json!([]));
                plan["updated"] = json!(at);
                Table::<PlanRecord>::new(tx).put_at(&key, &PlanRecord::from_value(plan)?)?;
            }
            let turn = turn(agent, p);
            let key = format!(
                "{method}:{}",
                turn.as_str().filter(|s| !s.is_empty()).unwrap_or("unknown")
            );
            write_item(
                tx,
                &id,
                &key,
                (
                    "output",
                    if method == "turn/plan/updated" {
                        "Plan"
                    } else {
                        "Changes"
                    },
                ),
                &notices::json_text(p),
                json!({"turnId":turn}),
                at,
            )?;
        }
        _ => {}
    }
    update_common_agent_fields(agent, method, p, at)?;
    persist_agent(tx, &id, agent, publication)?;
    Ok(Some(DispatchOutcome::Written))
}
fn turn(agent: &Value, params: &Value) -> Value {
    params
        .get("turnId")
        .filter(|v| !v.is_null() && **v != "")
        .unwrap_or(&agent["turnId"])
        .clone()
}
fn required(service: NotificationDependency) -> DispatchOutcome {
    DispatchOutcome::RequiresService { service }
}
fn put_task(tx: &Transaction<'_>, task: &Value) -> Result<(), NotificationDispatchError> {
    checked("runtime_tasks", task)?;
    Table::<TaskRecord>::new(tx).put(&TaskRecord::from_value(task.clone())?)?;
    let migrated: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sync_entity_meta WHERE key='task_window_migrated')",
            [],
            |r| r.get(0),
        )
        .map_err(StoreError::from)?;
    if migrated {
        crate::sync_policy::maintain_task_window(tx, &|_, _, _, _| true)?;
    }
    Ok(())
}
fn write_item(
    tx: &Transaction<'_>,
    agent: &str,
    key: &str,
    presentation: (&str, &str),
    text: &str,
    metadata: Value,
    at: f64,
) -> Result<(), NotificationDispatchError> {
    let (role, title) = presentation;
    let id = format!("{agent}:{key}");
    let mut record = json!({"id":id,"role":role,"title":title,"text":text.chars().take(super::ITEM_PREVIEW_CHARS).collect::<String>(),"truncated":text.chars().count()>super::ITEM_PREVIEW_CHARS,"at":at});
    record
        .as_object_mut()
        .ok_or(NotificationDispatchError::InvalidNotification)?
        .extend(
            metadata
                .as_object()
                .ok_or(NotificationDispatchError::InvalidNotification)?
                .clone(),
        );
    NotificationDispatchRepository::put_notification_item(tx, &id, agent, &record, at, text, true)?;
    Ok(())
}

pub(super) fn flush_command(
    tx: &Transaction<'_>,
    agent_id: &str,
    agent: &mut Value,
    streams: &mut super::NotificationStreamBuffer,
    key: &super::StreamKey,
    close: bool,
    at: f64,
) -> Result<(), NotificationDispatchError> {
    let id = format!("{agent_id}:{}", key.item);
    let Some((record, _)) = NotificationDispatchRepository::notification_item(tx, agent_id, &id)?
    else {
        NotificationDispatchRepository::put_stream_base(tx, &super::stream_base_id(key), None)?;
        streams.discard(key);
        if close {
            streams.close(key);
        }
        return Ok(());
    };
    let mut task = Table::<TaskRecord>::new(tx)
        .get(&TaskId::from(id.as_str()))?
        .map(|r| r.value().clone());
    let stale = key
        .turn
        .as_deref()
        .is_some_and(|turn| agent["turnId"] != turn);
    if stale
        && !task
            .as_ref()
            .is_some_and(|task| task["kind"] == "command" && task["turnId"] == json!(key.turn))
    {
        NotificationDispatchRepository::put_stream_base(tx, &super::stream_base_id(key), None)?;
        streams.discard(key);
        if close {
            streams.close(key);
        }
        return Ok(());
    }
    let saved = serde_json::from_str::<Value>(record["text"].as_str().unwrap_or_default())
        .ok()
        .filter(Value::is_object);
    let previous = super::durable_stream_base(tx, streams, key)?
        .unwrap_or_else(|| command_base(saved.as_ref(), task.as_ref()));
    let mut item = saved.unwrap_or_else(|| json!({"type":"commandExecution","id":key.item}));
    if let Some((text, count, params)) = streams.flush(key, &previous) {
        NotificationDispatchRepository::put_stream_base(tx, &super::stream_base_id(key), None)?;
        let truncated = item["outputTruncated"] == true
            || task.as_ref().is_some_and(|t| t["outputTruncated"] == true)
            || previous.chars().count() as u64
                + params["_streamCharacters"].as_u64().unwrap_or_default()
                > items::TOOL_OUTPUT_CHARS as u64;
        item["aggregatedOutput"] = json!(text);
        item["outputTruncated"] = json!(truncated);
        let keys = notices::object_keys(record["text"].as_str().unwrap_or_default())
            .unwrap_or_else(|| vec!["type".into(), "id".into()]);
        let text = notices::ordered_text(
            &item,
            keys.into_iter()
                .chain(["aggregatedOutput", "outputTruncated"].map(str::to_owned)),
        );
        let record = json!({"id":id,"role":"output","title":"commandExecution","text":text.chars().take(super::ITEM_PREVIEW_CHARS).collect::<String>(),"truncated":text.chars().count()>super::ITEM_PREVIEW_CHARS,"at":at,"toolStatus":"running","turnId":key.turn.as_ref().map(|v|json!(v)).unwrap_or_else(||agent["turnId"].clone())});
        NotificationDispatchRepository::put_notification_item(
            tx, &id, agent_id, &record, at, &text, false,
        )?;
        if let Some(task) = &mut task {
            task["tail"] = item["aggregatedOutput"].clone();
            task["outputTruncated"] = json!(truncated);
            put_task(tx, task)?;
        }
        if !stale {
            agent["events"] = json!(
                agent["events"]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_add(count)
            );
            agent["lastEvent"] = json!(super::format_utc(at)?);
        }
    }
    if close {
        NotificationDispatchRepository::put_stream_base(tx, &super::stream_base_id(key), None)?;
        streams.close(key);
    }
    Ok(())
}

pub(super) fn preserve_command_base(
    tx: &Transaction<'_>,
    agent: &str,
    streams: &mut super::NotificationStreamBuffer,
    key: &super::StreamKey,
) -> Result<(), NotificationDispatchError> {
    let id = format!("{agent}:{}", key.item);
    let saved = NotificationDispatchRepository::notification_item(tx, agent, &id)?
        .and_then(|(record, _)| {
            serde_json::from_str::<Value>(record["text"].as_str().unwrap_or_default()).ok()
        })
        .filter(Value::is_object);
    let task = if saved.is_none() {
        Table::<TaskRecord>::new(tx).get(&TaskId::from(id.as_str()))?
    } else {
        None
    };
    let previous = command_base(saved.as_ref(), task.as_ref().map(|task| task.value()));
    if streams.preserve_base(key, super::sha256_hex(previous.as_bytes())) {
        NotificationDispatchRepository::put_stream_base(
            tx,
            &super::stream_base_id(key),
            Some(&previous),
        )?;
    }
    Ok(())
}

fn command_base(saved: Option<&Value>, task: Option<&Value>) -> String {
    let text = if let Some(item) = saved {
        item["aggregatedOutput"].as_str().unwrap_or_default()
    } else {
        task.and_then(|task| task["tail"].as_str())
            .unwrap_or_default()
    };
    super::suffix(text, items::TOOL_OUTPUT_CHARS)
}

fn write_historical_item(
    tx: &Transaction<'_>,
    agent: &str,
    p: &Value,
    task: &Value,
    at: f64,
) -> Result<(), NotificationDispatchError> {
    let id = task["id"]
        .as_str()
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    let Some((record, _)) = NotificationDispatchRepository::notification_item(tx, agent, id)?
    else {
        return Ok(());
    };
    let saved = serde_json::from_str::<Value>(record["text"].as_str().unwrap_or_default()).ok();
    let historical_tool = task["kind"] == "tool";
    let item = if historical_tool {
        if record["turnId"] != task["turnId"]
            || record["title"] != task["type"]
            || !saved.as_ref().is_some_and(|s| {
                s.is_object() && s["id"] == p["item"]["id"] && s["type"] == task["type"]
            })
        {
            return Ok(());
        }
        let mut item = p["item"].clone();
        for field in ["durationMs", "startedAtMs", "completedAtMs"] {
            if !task[field].is_null() {
                item[field] = task[field].clone();
            }
        }
        item
    } else {
        let mut item = saved.filter(Value::is_object).unwrap_or_else(
            || json!({"id":task["itemId"],"type":"commandExecution","command":task["command"]}),
        );
        item["aggregatedOutput"] = task.get("tail").cloned().unwrap_or(json!(""));
        item["outputTruncated"] = task.get("outputTruncated").cloned().unwrap_or(json!(false));
        for field in ["exitCode", "durationMs", "startedAtMs", "completedAtMs"] {
            item[field] = task[field].clone();
        }
        item
    };
    // Historical Python tool records use ASCII JSON escaping.
    let keys = if historical_tool {
        p["item"]
            .as_object()
            .into_iter()
            .flat_map(|v| v.keys().cloned())
            .collect()
    } else {
        notices::object_keys(record["text"].as_str().unwrap_or_default())
            .unwrap_or_else(|| vec!["id".into(), "type".into(), "command".into()])
    };
    let appended: &[&str] = if historical_tool {
        &["durationMs", "startedAtMs", "completedAtMs"]
    } else {
        &[
            "aggregatedOutput",
            "outputTruncated",
            "exitCode",
            "durationMs",
            "startedAtMs",
            "completedAtMs",
        ]
    };
    let text = notices::ordered_ascii(
        &item,
        keys.into_iter()
            .chain(appended.iter().map(|v| v.to_string())),
    );
    write_item(
        tx,
        agent,
        task["itemId"].as_str().unwrap_or_default(),
        (
            "output",
            if historical_tool {
                task["type"].as_str().unwrap_or_default()
            } else {
                "commandExecution"
            },
        ),
        &text,
        json!({"toolStatus":task["status"],"turnId":task["turnId"]}),
        at,
    )
}
