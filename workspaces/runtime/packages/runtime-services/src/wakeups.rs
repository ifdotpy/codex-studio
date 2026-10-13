//! Retire only notifications proven obsolete; preserve original payloads and ambiguous progress.
use crate::continuation::{Decision, Snapshot, flag, patch, present, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
const COMPLAINT_DONE: &str = "Every referenced complaint already has a response";
const REVIEW_DONE: &str = "The exact result already has a decision";
fn complaints<'a>(s: &'a Snapshot, event: &Value) -> Option<Vec<&'a Value>> {
    let payload: Value = serde_json::from_str(text(event, "text")).ok()?;
    let entries = payload.get("complaints")?.as_array()?;
    if entries.is_empty() {
        return None;
    }
    entries
        .iter()
        .map(|e| {
            let id = e.get("complaint_id")?.as_str()?;
            let c = s.complaints.get(id)?;
            (text(c, "leadId") == s.agent_id()).then_some(c)
        })
        .collect()
}
fn needs_response(c: &Value) -> bool {
    if matches!(text(c, "status"), "resolved" | "declined") {
        return false;
    }
    if text(c, "sourceType") == "user_task" && text(c, "status") == "open" {
        return true;
    }
    let responsible = if text(c, "recipient") == "user" {
        "user"
    } else {
        text(c, "leadId")
    };
    !c.get("responses")
        .and_then(Value::as_array)
        .is_some_and(|rs| rs.iter().any(|r| text(r, "author") == responsible))
}
fn obsolete(s: &Snapshot, event: &Value) -> Option<&'static str> {
    if text(event, "kind") == "complaint"
        && complaints(s, event).is_some_and(|cs| cs.iter().all(|c| !needs_response(c)))
    {
        return Some(COMPLAINT_DONE);
    }
    if text(event, "kind") == "work_review" {
        let payload: Value = serde_json::from_str(text(event, "text")).ok()?;
        let task = payload.get("task")?.as_str()?;
        let result = payload.get("result")?.get("id")?.as_str()?;
        let work = s.work.get(task)?;
        if text(work, "rootId") == s.agent_id()
            && work
                .get("results")?
                .as_array()?
                .iter()
                .any(|r| text(r, "id") == result)
            && work
                .get("decisions")?
                .as_array()?
                .iter()
                .any(|r| text(r, "resultId") == result)
        {
            return Some(REVIEW_DONE);
        }
    }
    None
}
fn retire(event: &Value, reason: &str) -> Value {
    let mut row = event.clone();
    patch(&mut row, json!({"status":"stored_only","error":reason}));
    row
}
/// The repository supplies SQLite's `created,rowid` order, including tie order.
pub fn pending_batch(s: &Snapshot) -> Decision {
    let mut d = Decision::default();
    let mut meta: BTreeMap<String, Value> = s
        .metadata
        .iter()
        .map(|(id, v)| (id.clone(), v.clone().unwrap_or_else(|| json!({}))))
        .collect();
    let mut groups: BTreeMap<(String, String, String), Vec<(usize, u64)>> = BTreeMap::new();
    for event in &s.events {
        if text(event, "status") != "pending" {
            continue;
        }
        if text(event, "kind") == "complaint" && obsolete(s, event).is_some() {
            d.events.push(retire(event, COMPLAINT_DONE));
            continue;
        }
        let mut event = event.clone();
        let id = text(&event, "id").to_owned();
        if text(&event, "kind") == "complaint" && complaints(s, &event).is_none() {
            event["preserveComplaint"] = json!(true);
        }
        let metadata = meta.entry(id).or_insert_with(|| json!({}));
        if flag(metadata, "preserveProgress") {
            event["preserveProgress"] = json!(true);
        }
        if text(&event, "kind") == "agent_message" {
            event["preserveProgress"] = json!(true);
            if let Ok(payload) = serde_json::from_str::<Value>(text(&event, "text")) {
                if text(&payload, "importance") == "progress"
                    && ["sender", "room", "progress_key"]
                        .iter()
                        .all(|k| !text(&payload, k).is_empty())
                {
                    if let Some(version) = payload.get("progress_version").and_then(Value::as_u64) {
                        if !flag(metadata, "preserveProgress") {
                            crate::continuation::remove(&mut event, "preserveProgress");
                        }
                        groups
                            .entry((
                                text(&payload, "sender").into(),
                                text(&payload, "room").into(),
                                text(&payload, "progress_key").into(),
                            ))
                            .or_default()
                            .push((d.batch.len(), version));
                    }
                }
            }
        }
        d.batch.push(event);
    }
    let mut retired = BTreeSet::new();
    for group in groups.values() {
        let versions: BTreeSet<u64> = group.iter().map(|(_, v)| *v).collect();
        if versions.len() != group.len()
            || group
                .iter()
                .any(|(i, _)| flag(&d.batch[*i], "preserveProgress"))
        {
            for (i, _) in group {
                d.batch[*i]["preserveProgress"] = json!(true);
                let id = text(&d.batch[*i], "id").to_owned();
                let metadata = meta.entry(id.clone()).or_insert_with(|| json!({}));
                metadata["preserveProgress"] = json!(true);
                d.metadata.insert(id, metadata.clone());
            }
            continue;
        }
        if group.len() < 2 {
            continue;
        }
        let Some((latest, _)) = group.iter().max_by_key(|(_, v)| v) else {
            continue;
        };
        let latest_id = text(&d.batch[*latest], "id").to_owned();
        let mut count = (group.len() - 1) as u64;
        for (i, _) in group {
            let id = text(&d.batch[*i], "id");
            count = count.saturating_add(
                meta.get(id)
                    .and_then(|m| m.get("earlierProgressUpdates"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
            if i != latest {
                retired.insert(*i);
                d.events.push(retire(
                    &s.events
                        .iter()
                        .find(|e| text(e, "id") == id)
                        .cloned()
                        .unwrap_or_else(|| d.batch[*i].clone()),
                    &format!("Superseded by progress event {latest_id}"),
                ));
            }
        }
        let metadata = meta.entry(latest_id.clone()).or_insert_with(|| json!({}));
        metadata["earlierProgressUpdates"] = json!(count);
        d.metadata.insert(latest_id, metadata.clone());
    }
    d.batch=d.batch.into_iter().enumerate().filter_map(|(i,mut e)|{
        if retired.contains(&i){return None;}
        let count=meta.get(text(&e,"id")).and_then(|m|m.get("earlierProgressUpdates")).and_then(Value::as_u64).unwrap_or(0);
        if count>0 {if let Ok(mut payload)=serde_json::from_str::<Value>(text(&e,"text")){if payload.is_object(){patch(&mut payload,json!({"earlierProgressUpdates":count,"history":"Read earlier progress with orchestration_chat_read in this room."}));e["text"]=json!(crate::event_write::python_json(&payload));}}}
        Some(e)
    }).collect();
    d
}
/// Revalidate the exact ordered batch under the submission lock immediately before sending.
pub fn reconcile_start(s: &Snapshot, event_ids: &[String]) -> Decision {
    let attempt = &s.agent["startAttempt"];
    if attempt.get("submitted") != Some(&Value::Bool(false))
        || present(attempt, "turnId")
        || present(attempt, "observedTurnId")
        || attempt.get("epoch") != s.agent.get("epoch")
        || attempt.get("events") != Some(&json!(event_ids))
        || event_ids.is_empty()
    {
        return Decision::default();
    }
    let selected: Option<Vec<&Value>> = event_ids
        .iter()
        .map(|id| s.events.iter().find(|e| text(e, "id") == id))
        .collect();
    let Some(selected) = selected else {
        return Decision::default();
    };
    if selected
        .iter()
        .any(|e| !matches!(text(e, "status"), "reserved" | "dispatching"))
    {
        return Decision::default();
    }
    let retired: Vec<&str> = selected
        .iter()
        .filter(|e| obsolete(s, e).is_some())
        .map(|e| text(e, "id"))
        .collect();
    if retired.is_empty() {
        return Decision::default();
    }
    let mut d = Decision::default();
    for event in selected {
        let mut e = event.clone();
        let reason = obsolete(s, &e);
        patch(
            &mut e,
            json!({"status":if reason.is_some(){"stored_only"}else{"pending"},"error":reason}),
        );
        d.events.push(e);
    }
    let mut agent = s.agent.clone();
    patch(
        &mut agent["startAttempt"],
        json!({"notSubmittedReason":"Notifications resolved before native submission","retiredEvents":retired}),
    );
    let pending = s.events.iter().any(|e| text(e, "status") == "pending")
        || d.events.iter().any(|e| text(e, "status") == "pending");
    if !flag(attempt, "activeAtReservation") {
        patch(
            &mut agent,
            json!({"inFlight":false,"status":if pending{"queued"}else{"waiting"}}),
        );
    }
    d.agent = Some(agent);
    d
}
