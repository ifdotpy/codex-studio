//! Submission validation, team-boundary policy, and canonical input records.

use serde_json::{Value, json};
use studio_runtime_store::{NotificationDispatchRepository, Transaction};

use super::{
    ITEM_PREVIEW_CHARS, NotificationDispatchError, PreparedSubmission, stored,
    validate_agent_write_fields,
};

pub(super) fn validate_submission(
    transaction: &Transaction<'_>,
    agent_id: &str,
    submission: &PreparedSubmission,
) -> Result<(), NotificationDispatchError> {
    if submission.first_event_id.is_empty()
        || submission.inputs.is_empty()
        || submission.inputs[0].id != submission.first_event_id
        || !submission.submitted_at.is_finite()
        || submission.submitted_at < 0.0
        || !submission.created_at.is_finite()
        || submission.created_at < 0.0
        || !submission.assets.is_array()
    {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    let agent = NotificationDispatchRepository::agent_record(transaction, agent_id)?
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    validate_agent_write_fields(&agent, agent_id)?;
    if agent.get("deletedAt").is_some_and(|value| !value.is_null()) {
        return Err(stored("runtime_agents", agent_id));
    }
    let mut unique = std::collections::BTreeSet::new();
    for input in &submission.inputs {
        if input.id.is_empty()
            || !unique.insert(input.id.as_str())
            || input.kind.is_empty()
            || !input.at.is_finite()
            || input.at < 0.0
            || !input.assets.is_array()
        {
            return Err(stored("runtime_events", &input.id));
        }
        let Some((kind, text, created)) =
            NotificationDispatchRepository::user_event(transaction, agent_id, &input.id)?
        else {
            return Err(stored("runtime_events", &input.id));
        };
        if kind != input.kind || text != input.text || created != input.at {
            return Err(stored("runtime_events", &input.id));
        }
        let meta = NotificationDispatchRepository::event_metadata(transaction, &input.id)?;
        let meta_assets = meta
            .as_ref()
            .and_then(|meta| meta.get("assets"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        let Some(meta_ids) = asset_ids(&meta_assets) else {
            return Err(stored("runtime_event_meta", &input.id));
        };
        let Some(input_ids) = asset_ids(&input.assets) else {
            return Err(NotificationDispatchError::InvalidNotification);
        };
        if meta_ids != input_ids {
            return Err(stored("runtime_event_meta", &input.id));
        }
        if meta
            .as_ref()
            .and_then(|meta| meta.get("transcriptItemId"))
            .is_some_and(|value| {
                value.as_str() != Some(format!("{agent_id}:{}", submission.first_event_id).as_str())
            })
        {
            return Err(stored("runtime_event_meta", &input.id));
        }
        validate_submission_policy(
            transaction,
            &agent,
            &input.id,
            &input.kind,
            &input.text,
            meta.as_ref(),
        )?;
    }
    let mut expected_asset_ids = Vec::new();
    for input in &submission.inputs {
        let Some(ids) = asset_ids(&input.assets) else {
            return Err(NotificationDispatchError::InvalidNotification);
        };
        expected_asset_ids.extend(ids);
    }
    if asset_ids(&submission.assets).as_deref() != Some(expected_asset_ids.as_slice()) {
        return Err(NotificationDispatchError::InvalidNotification);
    }
    Ok(())
}

fn validate_submission_policy(
    transaction: &Transaction<'_>,
    recipient: &Value,
    event_id: &str,
    kind: &str,
    text: &str,
    metadata: Option<&Value>,
) -> Result<(), NotificationDispatchError> {
    if kind == "radio_turn" {
        return Err(NotificationDispatchError::Dependency("radio-policy"));
    }
    if matches!(
        kind,
        "followup" | "agent_message" | "chat_review" | "complaint_response"
    ) && recipient
        .get("rootId")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(NotificationDispatchError::UnauthorizedSubmission);
    }
    if kind == "chat_review" {
        return Err(NotificationDispatchError::UnauthorizedSubmission);
    }
    if kind == "followup" {
        let Some(sender) = metadata.and_then(|meta| meta.get("senderId")) else {
            return Ok(());
        };
        let Some(sender) = sender.as_str() else {
            return Err(stored("runtime_event_meta", event_id));
        };
        return if same_team(transaction, recipient, sender)? {
            Ok(())
        } else {
            Err(NotificationDispatchError::UnauthorizedSubmission)
        };
    }
    if !matches!(kind, "agent_message" | "complaint_response") {
        return Ok(());
    }
    let payload: Value =
        serde_json::from_str(text).map_err(|_| stored("runtime_events", event_id))?;
    if !payload.is_object() {
        return Err(stored("runtime_events", event_id));
    }
    let recipient_id = recipient
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| stored("runtime_agents", "unknown"))?;
    let sender_id = if kind == "agent_message" {
        let Some(sender) = payload.get("sender").and_then(Value::as_str) else {
            return Err(NotificationDispatchError::UnauthorizedSubmission);
        };
        let Some(room_id) = payload.get("room").and_then(Value::as_str) else {
            return Err(NotificationDispatchError::UnauthorizedSubmission);
        };
        let room =
            NotificationDispatchRepository::related_record(transaction, "runtime_rooms", room_id)?
                .ok_or(NotificationDispatchError::UnauthorizedSubmission)?;
        match room.get("kind").and_then(Value::as_str) {
            Some("broadcast") => {
                if room.get("rootId") != recipient.get("rootId")
                    || room.get("rootId").and_then(Value::as_str) == Some("all")
                {
                    return Err(NotificationDispatchError::UnauthorizedSubmission);
                }
            }
            Some("private") => {
                let members = room
                    .get("members")
                    .and_then(Value::as_array)
                    .ok_or_else(|| stored("runtime_rooms", room_id))?;
                let member_ids = members
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| stored("runtime_rooms", room_id))?;
                if !member_ids.contains(&sender) || !member_ids.contains(&recipient_id) {
                    return Err(NotificationDispatchError::UnauthorizedSubmission);
                }
                let mut all_same_team = true;
                for member in &member_ids {
                    if !same_team(transaction, recipient, member)? {
                        all_same_team = false;
                        break;
                    }
                }
                if !all_same_team
                    && !(member_ids.len() == 2
                        && peer_pair_allowed(transaction, member_ids[0], member_ids[1])?)
                {
                    return Err(NotificationDispatchError::UnauthorizedSubmission);
                }
            }
            _ => return Err(NotificationDispatchError::UnauthorizedSubmission),
        }
        sender
    } else {
        let complaint_id = payload
            .get("complaint_id")
            .and_then(Value::as_str)
            .ok_or(NotificationDispatchError::UnauthorizedSubmission)?;
        let complaint = NotificationDispatchRepository::related_record(
            transaction,
            "runtime_complaints",
            complaint_id,
        )?
        .ok_or(NotificationDispatchError::UnauthorizedSubmission)?;
        if complaint.get("author").and_then(Value::as_str) != Some(recipient_id) {
            return Err(NotificationDispatchError::UnauthorizedSubmission);
        }
        let sender = payload
            .get("responder")
            .or_else(|| payload.get("lead"))
            .and_then(Value::as_str)
            .ok_or(NotificationDispatchError::UnauthorizedSubmission)?;
        if payload.pointer("/response/author").and_then(Value::as_str) != Some(sender) {
            return Err(NotificationDispatchError::UnauthorizedSubmission);
        }
        if sender == "user" {
            return Ok(());
        }
        if complaint.get("leadId").and_then(Value::as_str) != Some(sender) {
            return Err(NotificationDispatchError::UnauthorizedSubmission);
        }
        sender
    };
    if same_team(transaction, recipient, sender_id)?
        || (kind == "agent_message" && peer_pair_allowed(transaction, recipient_id, sender_id)?)
    {
        Ok(())
    } else {
        Err(NotificationDispatchError::UnauthorizedSubmission)
    }
}

fn same_team(
    transaction: &Transaction<'_>,
    recipient: &Value,
    sender_id: &str,
) -> Result<bool, NotificationDispatchError> {
    let Some(root) = recipient.get("rootId").and_then(Value::as_str) else {
        return Err(stored(
            "runtime_agents",
            recipient
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        ));
    };
    let Some(sender) =
        NotificationDispatchRepository::related_record(transaction, "runtime_agents", sender_id)?
    else {
        return Ok(false);
    };
    let sender_root = sender
        .get("rootId")
        .and_then(Value::as_str)
        .ok_or_else(|| stored("runtime_agents", sender_id))?;
    Ok(!root.is_empty() && sender_root == root)
}

fn peer_pair_allowed(
    transaction: &Transaction<'_>,
    left_id: &str,
    right_id: &str,
) -> Result<bool, NotificationDispatchError> {
    if left_id == right_id {
        return Ok(false);
    }
    let Some(left) =
        NotificationDispatchRepository::related_record(transaction, "runtime_agents", left_id)?
    else {
        return Ok(false);
    };
    let Some(right) =
        NotificationDispatchRepository::related_record(transaction, "runtime_agents", right_id)?
    else {
        return Ok(false);
    };
    for (id, agent) in [(left_id, &left), (right_id, &right)] {
        if agent.get("isLead").is_some_and(|value| !value.is_boolean())
            || agent
                .get("parentId")
                .is_some_and(|value| !value.is_null() && !value.is_string())
            || agent
                .get("deletedAt")
                .is_some_and(|value| !value.is_null() && !value.is_number())
        {
            return Err(stored("runtime_agents", id));
        }
    }
    let Some(path) = left
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(crate::sync_policy::normalized_project_path)
    else {
        return Ok(false);
    };
    let is_lead = |agent: &Value| {
        agent.get("isLead").and_then(Value::as_bool) == Some(true)
            && agent.get("deletedAt").is_none_or(Value::is_null)
            && agent.get("parentId").is_none_or(Value::is_null)
            && agent.get("rootId").and_then(Value::as_str)
                == agent.get("id").and_then(Value::as_str)
    };
    if !is_lead(&left)
        || !is_lead(&right)
        || right
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(crate::sync_policy::normalized_project_path)
            .as_deref()
            != Some(path.as_str())
    {
        return Ok(false);
    }
    let Some(project) =
        NotificationDispatchRepository::related_record(transaction, "runtime_projects", &path)?
    else {
        return Ok(false);
    };
    let teams = project.get("peerTeams").map_or(Ok(&[][..]), |value| {
        value
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| stored("runtime_projects", &path))
    })?;
    for team in teams {
        let Some(members) = team.get("members") else {
            return Err(stored("runtime_projects", &path));
        };
        let members = members
            .as_array()
            .ok_or_else(|| stored("runtime_projects", &path))?;
        if members.iter().any(|member| !member.is_string()) {
            return Err(stored("runtime_projects", &path));
        }
        if members
            .iter()
            .filter_map(Value::as_str)
            .any(|id| id == left_id)
            && members
                .iter()
                .filter_map(Value::as_str)
                .any(|id| id == right_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn asset_ids(assets: &Value) -> Option<Vec<&str>> {
    assets
        .as_array()?
        .iter()
        .map(|asset| {
            asset
                .as_str()
                .or_else(|| asset.get("id").and_then(Value::as_str))
                .filter(|id| !id.is_empty())
        })
        .collect()
}

pub(super) fn submission_record(id: &str, submission: &PreparedSubmission) -> Value {
    let mut remaining = ITEM_PREVIEW_CHARS;
    let inputs = submission
        .inputs
        .iter()
        .map(|input| {
            let excerpt = input.text.chars().take(remaining).collect::<String>();
            remaining = remaining.saturating_sub(excerpt.chars().count());
            json!({
                "id": input.id,
                "at": input.at,
                "kind": input.kind,
                "text": excerpt,
                "truncated": excerpt.chars().count() < input.text.chars().count(),
                "assets": input.assets,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "id": id,
        "role": "user",
        "title": "User",
        "text": submission.text.chars().take(ITEM_PREVIEW_CHARS).collect::<String>(),
        "truncated": submission.text.chars().count() > ITEM_PREVIEW_CHARS,
        "at": submission.submitted_at,
        "assets": submission.assets,
        "inputs": inputs,
    })
}
