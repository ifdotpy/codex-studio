//! Submission validation, team-boundary policy, and canonical input records.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_store::request_results::{AdmittedRequest, RequestResults, ResultEnvelope};
use studio_runtime_store::{
    ExecutionHistoryRepository, NotificationDispatchRepository, SchedulerRepository, Transaction,
};

use super::{
    DispatchOutcome, ITEM_PREVIEW_CHARS, NotificationDispatchError, PreparedSubmission,
    SubmissionAuthorization, checked_outcome, in_savepoint, saved_text, sha256_json, stored,
    validate_agent_write_fields,
};

/// The input owner keeps the publication fence independently of response expiry.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSubmission {
    attempt_id: Option<String>,
    first_event_id: String,
    admission: AdmittedRequest,
    publication_outcome: DispatchOutcome,
}

pub(super) fn record_submission(
    tx: &Transaction<'_>,
    agent: &str,
    submission: &PreparedSubmission,
    authorization: SubmissionAuthorization<'_>,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    in_savepoint(tx, || {
        let item = format!("{agent}:{}", submission.first_event_id);
        let record = submission_record(&item, submission);
        let signature = sha256_json(&json!({
            "agent": agent,
            "firstEventId": submission.first_event_id,
            "submittedAt": submission.submitted_at,
            "createdAt": submission.created_at,
            "record": record,
            "text": submission.text,
            "inputTexts": submission.inputs.iter().map(|input| &input.text).collect::<Vec<_>>(),
        }))?;
        let input_owner: Option<ProviderSubmission> =
            NotificationDispatchRepository::submission_owner(
                tx,
                agent,
                &submission.first_event_id,
            )?;
        let attempt_owner: Option<ProviderSubmission> = match authorization {
            SubmissionAuthorization::Start { attempt_id } => {
                ExecutionHistoryRepository::attempt(tx, attempt_id)?
                    .and_then(|attempt| attempt.provider_submission)
                    .map(|raw| {
                        serde_json::from_str(raw.get())
                            .map_err(|_| stored("runtime_event_meta", &submission.first_event_id))
                    })
                    .transpose()?
            }
            SubmissionAuthorization::Current
            | SubmissionAuthorization::AcknowledgedSteer { .. } => None,
        };
        let owner = match attempt_owner {
            Some(owner) => Some(owner),
            None => input_owner,
        };
        if let Some(owner) = owner {
            let replacing = matches!(authorization, SubmissionAuthorization::Start { attempt_id } if owner.attempt_id.as_deref() != Some(attempt_id));
            if !replacing {
                if owner.first_event_id != submission.first_event_id
                    || owner.admission.signature() != signature
                {
                    return Err(NotificationDispatchError::ConflictingInput);
                }
                return replay_submission(tx, agent, &item, &owner.admission);
            }
        }
        validate_submission(tx, agent, submission, authorization)?;
        let (attempt_id, root) = match authorization {
            SubmissionAuthorization::AcknowledgedSteer { admission } => (None, admission.clone()),
            SubmissionAuthorization::Start { attempt_id } => (
                Some(attempt_id.to_owned()),
                ExecutionHistoryRepository::admission(tx, attempt_id)?
                    .ok_or_else(|| stored("runtime_execution_attempts", attempt_id))?,
            ),
            SubmissionAuthorization::Current => {
                let current = NotificationDispatchRepository::agent_record(tx, agent)?
                    .ok_or_else(|| stored("runtime_agents", agent))?;
                let attempt = current
                    .pointer("/activeExecution/attemptId")
                    .or_else(|| current.pointer("/startAttempt/id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| stored("runtime_agents", agent))?;
                let root = ExecutionHistoryRepository::admission(tx, attempt)?
                    .ok_or_else(|| stored("runtime_execution_attempts", attempt))?;
                (Some(attempt.to_owned()), root)
            }
        };
        let admission = root.derive_result(
            "provider-submission",
            &[&submission.first_event_id],
            &signature,
        );
        let same_item = match NotificationDispatchRepository::transcript_item(tx, agent, &item)? {
            Some((existing, created)) => {
                let same = existing == record
                    && saved_text(tx, &item, &existing)? == submission.text
                    && created == submission.created_at;
                if !same && !matches!(authorization, SubmissionAuthorization::Start { .. }) {
                    return Err(NotificationDispatchError::ConflictingInput);
                }
                same
            }
            None => false,
        };
        if !same_item {
            NotificationDispatchRepository::put_transcript_item(
                tx,
                &item,
                agent,
                &record,
                submission.created_at,
                &submission.text,
                true,
            )?;
        }
        for input in &submission.inputs {
            NotificationDispatchRepository::link_transcript_item_to_event(
                tx, agent, &input.id, &item,
            )?;
        }
        let owner = ProviderSubmission {
            attempt_id,
            first_event_id: submission.first_event_id.clone(),
            admission,
            publication_outcome: DispatchOutcome::Written,
        };
        NotificationDispatchRepository::put_submission_owner(
            tx,
            if matches!(authorization, SubmissionAuthorization::Start { .. }) {
                owner.attempt_id.as_deref()
            } else {
                None
            },
            &submission
                .inputs
                .iter()
                .map(|input| input.id.as_str())
                .collect::<Vec<_>>(),
            &owner,
        )?;
        RequestResults::settle(
            tx,
            &owner.admission,
            &ResultEnvelope {
                actor: Some(agent.to_owned()),
                reply: &owner.publication_outcome,
                references: vec![item],
            },
        )?;
        Ok(owner.publication_outcome)
    })
}

fn replay_submission(
    tx: &Transaction<'_>,
    agent: &str,
    item: &str,
    admission: &AdmittedRequest,
) -> Result<DispatchOutcome, NotificationDispatchError> {
    let now = chrono::Utc::now().timestamp_millis();
    let envelope = crate::event_write::replay_request::<DispatchOutcome>(
        tx,
        admission.id(),
        admission.signature(),
        now,
    )?
    .ok_or_else(|| {
        let floor = now.saturating_sub(studio_runtime_schema::persistence::REQUEST_RESULT_TTL_MS);
        if admission.created() <= floor {
            studio_runtime_store::request_results::RequestResultError::Identity(
                studio_runtime_schema::persistence::RequestIdentityError::Expired {
                    issued_ms: admission.created(),
                    expired_before_ms: floor,
                },
            )
            .into()
        } else {
            stored("request_results", admission.id())
        }
    })?;
    if envelope.actor.as_deref() != Some(agent) || envelope.references != [item] {
        return Err(stored("request_results", admission.id()));
    }
    checked_outcome(envelope.reply)
}

pub(super) fn validate_submission(
    transaction: &Transaction<'_>,
    agent_id: &str,
    submission: &PreparedSubmission,
    authorization: SubmissionAuthorization<'_>,
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
        return Err(NotificationDispatchError::invalid());
    }
    let agent = NotificationDispatchRepository::agent_record(transaction, agent_id)?
        .ok_or_else(|| stored("runtime_agents", agent_id))?;
    validate_agent_write_fields(&agent, agent_id)?;
    if agent.get("deletedAt").is_some_and(|value| !value.is_null()) {
        return Err(stored("runtime_agents", agent_id));
    }
    let mut unique = std::collections::BTreeSet::new();
    let mut expected_asset_ids = Vec::new();
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
        let meta = NotificationDispatchRepository::event_metadata(transaction, &input.id)?;
        // The submitted text is the claim's model projection of the stored row.
        let claimed =
            SchedulerRepository::claimed_event_text(&text, meta.as_ref().unwrap_or(&Value::Null));
        if kind != input.kind || claimed != input.text || created != input.at {
            return Err(stored("runtime_events", &input.id));
        }
        let meta_assets = meta
            .as_ref()
            .and_then(|meta| meta.get("assets"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        let Some(meta_ids) = asset_ids(&meta_assets) else {
            return Err(stored("runtime_event_meta", &input.id));
        };
        let Some(input_ids) = asset_ids(&input.assets) else {
            return Err(NotificationDispatchError::invalid());
        };
        if meta_ids != input_ids {
            return Err(stored("runtime_event_meta", &input.id));
        }
        expected_asset_ids.extend(input_ids);
        // A stored `transcriptItemId` is not checked: it can belong to an earlier,
        // never delivered attempt (a rejected steer skips deferred input, so its first
        // event differs from the next turn's). Delivered events leave the pending set;
        // the submission restamps the batch it commits.
        if matches!(authorization, SubmissionAuthorization::Current) {
            validate_submission_policy(
                transaction,
                &agent,
                &input.id,
                &input.kind,
                &input.text,
                meta.as_ref(),
                submission.radio_authorized,
            )?;
        }
    }
    if asset_ids(&submission.assets).as_deref() != Some(expected_asset_ids.as_slice()) {
        return Err(NotificationDispatchError::invalid());
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
    radio_authorized: bool,
) -> Result<(), NotificationDispatchError> {
    if kind == "radio_turn" {
        return if radio_authorized {
            Ok(())
        } else {
            Err(NotificationDispatchError::Dependency("radio-policy"))
        };
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
        .and_then(studio_runtime_store::path_identity::lexical_text)
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
            .and_then(studio_runtime_store::path_identity::lexical_text)
            .as_deref()
            != Some(path.as_path())
    {
        return Ok(false);
    }
    let path = path.to_string_lossy();
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
