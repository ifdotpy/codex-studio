//! Pre-submit decisions only. The account worker must re-read these snapshots and
//! apply a freeze patch atomically with its exact attempt/epoch check. Refusals
//! never consume queued input. Paths below are already resolved by the caller.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;
use thiserror::Error;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentAuthority {
    pub id: String,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub is_lead: bool,
    pub deleted: bool,
    pub resolved_cwd: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoomAuthority {
    Broadcast {
        root_id: Option<String>,
    },
    Private {
        members: Vec<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ComplaintAuthority {
    pub author: Option<String>,
    pub lead_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PeerProject {
    pub resolved_path: String,
    pub teams: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AuthorizationSnapshot {
    pub recipient_id: String,
    pub agents: BTreeMap<String, AgentAuthority>,
    pub rooms: BTreeMap<String, RoomAuthority>,
    pub complaints: BTreeMap<String, ComplaintAuthority>,
    pub projects: BTreeMap<String, PeerProject>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventSnapshot {
    AgentMessage {
        sender: Option<String>,
        room: Option<String>,
    },
    Followup {
        sender_present: bool,
        sender: Option<String>,
    },
    ComplaintResponse {
        complaint_id: Option<String>,
        responder: Option<String>,
        response_author: Option<String>,
    },
    ChatReview,
    RadioTurn,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, Error, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EventDenial {
    #[error("team-isolation-policy/recipient-unavailable")]
    RecipientUnavailable,
    #[error("team-isolation-policy/room-unavailable")]
    RoomUnavailable,
    #[error("team-isolation-policy/broadcast-disabled")]
    BroadcastDisabled,
    #[error("team-isolation-policy/room-boundary")]
    RoomBoundary,
    #[error("team-isolation-policy/complaint-unavailable")]
    ComplaintUnavailable,
    #[error("team-isolation-policy/complaint-recipient")]
    ComplaintRecipient,
    #[error("team-isolation-policy/complaint-responder-unavailable")]
    ComplaintResponderUnavailable,
    #[error("team-isolation-policy/complaint-responder")]
    ComplaintResponder,
    #[error("team-isolation-policy/team-boundary")]
    TeamBoundary,
    #[error("team-isolation-policy/chat-review-removed")]
    ChatReviewRemoved,
    #[error("radio-policy/unsupported")]
    RadioUnsupported,
}

impl AuthorizationSnapshot {
    fn same_team(&self, root: &str, sender: Option<&str>) -> bool {
        sender
            .and_then(|id| self.agents.get(id))
            .is_some_and(|agent| agent.root_id.as_deref() == Some(root))
    }

    fn peer_pair(&self, left: &str, right: &str) -> bool {
        if left == right {
            return false;
        }
        let (Some(left), Some(right)) = (self.agents.get(left), self.agents.get(right)) else {
            return false;
        };
        let Some(path) = left.resolved_cwd.as_deref().filter(|p| !p.is_empty()) else {
            return false;
        };
        let lead = |agent: &AgentAuthority| {
            agent.is_lead
                && !agent.deleted
                && agent.parent_id.is_none()
                && agent.root_id.as_ref() == Some(&agent.id)
                && agent.resolved_cwd.as_deref() == Some(path)
        };
        lead(left)
            && lead(right)
            && self.projects.get(path).is_some_and(|project| {
                project.resolved_path == path
                    && project
                        .teams
                        .iter()
                        .any(|team| team.contains(&left.id) && team.contains(&right.id))
            })
    }

    fn authorize(&self, event: &EventSnapshot) -> Result<(), EventDenial> {
        use EventDenial as D;
        match event {
            EventSnapshot::Other => return Ok(()),
            EventSnapshot::ChatReview => return Err(D::ChatReviewRemoved),
            EventSnapshot::RadioTurn => return Err(D::RadioUnsupported),
            _ => {}
        }
        let recipient = self
            .agents
            .get(&self.recipient_id)
            .ok_or(D::RecipientUnavailable)?;
        let root = recipient
            .root_id
            .as_deref()
            .filter(|r| !r.is_empty())
            .ok_or(D::RecipientUnavailable)?;
        let sender = match event {
            EventSnapshot::Followup {
                sender_present,
                sender,
            } => {
                if !sender_present {
                    return Ok(());
                }
                sender.as_deref()
            }
            EventSnapshot::AgentMessage { sender, room } => {
                let room = room
                    .as_ref()
                    .and_then(|id| self.rooms.get(id))
                    .ok_or(D::RoomUnavailable)?;
                match room {
                    RoomAuthority::Broadcast { root_id } => {
                        if root_id.as_deref() != Some(root) || root == "all" {
                            return Err(D::BroadcastDisabled);
                        }
                    }
                    RoomAuthority::Private { members } => {
                        if !sender.as_ref().is_some_and(|s| members.contains(s))
                            || !members.contains(&recipient.id)
                            || !(members.iter().all(|id| self.same_team(root, Some(id)))
                                || (members.len() == 2 && self.peer_pair(&members[0], &members[1])))
                        {
                            return Err(D::RoomBoundary);
                        }
                    }
                    RoomAuthority::Other => return Err(D::RoomUnavailable),
                }
                sender.as_deref()
            }
            EventSnapshot::ComplaintResponse {
                complaint_id,
                responder,
                response_author,
            } => {
                let id = complaint_id.as_ref().ok_or(D::ComplaintUnavailable)?;
                let complaint = self
                    .complaints
                    .get(id)
                    .filter(|c| c.author.as_ref() == Some(&recipient.id))
                    .ok_or(D::ComplaintRecipient)?;
                if response_author.is_none() || response_author != responder {
                    return Err(D::ComplaintResponderUnavailable);
                }
                if responder.as_deref() == Some("user") {
                    return Ok(());
                }
                if complaint.lead_id != *responder {
                    return Err(D::ComplaintResponder);
                }
                responder.as_deref()
            }
            _ => return Ok(()),
        };
        if self.same_team(root, sender)
            || (matches!(event, EventSnapshot::AgentMessage { .. })
                && sender.is_some_and(|sender| self.peer_pair(&recipient.id, sender)))
        {
            Ok(())
        } else {
            Err(D::TeamBoundary)
        }
    }
}

/// All-or-nothing, in event order. Re-run against fresh authority before submission.
pub fn authorize_events(
    snapshot: &AuthorizationSnapshot,
    events: &[EventSnapshot],
) -> Result<(), EventDenial> {
    events
        .iter()
        .try_for_each(|event| snapshot.authorize(event))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExecutionSettings {
    pub model: String,
    pub effort: Option<String>,
    pub native_effort: Option<String>,
    pub fast_mode: bool,
    pub daybreak_enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PendingSettings {
    /// Missing pins mean the current account, as with existing Python records.
    pub account_key: Option<String>,
    pub settings: ExecutionSettings,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExecutionSnapshot {
    pub account_key: String,
    pub execution_settings_account_key: Option<String>,
    pub settings: ExecutionSettings,
    pub pending: Option<PendingSettings>,
    pub settings_fixed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub model: String,
    pub resolved_model: Option<String>,
    pub hidden: bool,
    pub supported_efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub priority: bool,
}

/// An adapter projects validated catalog data and preserves its account,
/// generation and monotonic expiry. Display-cache reads are not admission proof.
#[derive(Clone, Debug)]
pub struct CatalogObservation {
    pub account_key: String,
    pub generation: u64,
    pub expires_at: Instant,
    pub models: Vec<ModelCapabilities>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FreezePatch {
    pub expected: ExecutionSnapshot,
    pub settings: ExecutionSettings,
    pub execution_settings_account_key: Option<String>,
    pub consume_pending: bool,
    pub invalidate_loaded: bool,
    pub settings_fixed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExecutionSelection {
    pub settings: ExecutionSettings,
    pub freeze: Option<FreezePatch>,
}

#[derive(Clone, Copy, Debug, Error, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionDenial {
    #[error("execution-settings/account-changed")]
    AccountChanged,
    #[error("execution-settings/settings-changed")]
    SettingsChanged,
    #[error("model-catalog/fresh-observation-required")]
    CatalogRequired,
    #[error("model-catalog/account-mismatch")]
    CatalogAccountMismatch,
    #[error("model-catalog/expired")]
    CatalogExpired,
    #[error("execution-settings/model-unavailable")]
    ModelUnavailable,
    #[error("execution-settings/effort-unsupported")]
    EffortUnsupported,
    #[error("execution-settings/fast-mode-unsupported")]
    FastModeUnsupported,
    #[error("daybreak-policy/unsupported")]
    DaybreakUnsupported,
}

fn validate_execution(
    settings: &ExecutionSettings,
    catalog: &CatalogObservation,
) -> Result<Option<String>, ExecutionDenial> {
    use ExecutionDenial as D;
    let info = catalog
        .models
        .iter()
        .find(|row| {
            !row.hidden
                && !settings.model.trim().is_empty()
                && (row.model == settings.model
                    || row.resolved_model.as_ref() == Some(&settings.model))
        })
        .ok_or(D::ModelUnavailable)?;
    if let Some(effort) = &settings.effort
        && (effort.trim().is_empty() || !info.supported_efforts.contains(effort))
    {
        return Err(D::EffortUnsupported);
    }
    if settings.fast_mode && !info.priority {
        return Err(D::FastModeUnsupported);
    }
    Ok(settings
        .effort
        .clone()
        .or_else(|| info.default_effort.clone()))
}

/// `observed` is the snapshot used to fetch a changed account's catalog. The
/// freeze patch is conditional on the complete `current` snapshot, plus the
/// caller's attempt/epoch identity. Fixed retries leave pending settings intact.
pub fn select_execution(
    current: &ExecutionSnapshot,
    observed: &ExecutionSnapshot,
    catalog: Option<&CatalogObservation>,
    now: Instant,
) -> Result<ExecutionSelection, ExecutionDenial> {
    use ExecutionDenial as D;
    let mut settings = current.settings.clone();
    let mut account = current.execution_settings_account_key.clone();
    let validate = |snapshot: &ExecutionSnapshot, settings: &ExecutionSettings| {
        let catalog = catalog.ok_or(D::CatalogRequired)?;
        if catalog.account_key != snapshot.account_key {
            return Err(D::CatalogAccountMismatch);
        }
        if catalog.expires_at <= now {
            return Err(D::CatalogExpired);
        }
        validate_execution(settings, catalog)
    };
    // Python validates an account change before checking settingsFixed. A fixed
    // retry still needs this proof, but does not rewrite its frozen defaults.
    let revalidated = if observed
        .execution_settings_account_key
        .as_ref()
        .is_some_and(|key| key != &observed.account_key)
        && observed.pending.is_none()
    {
        Some(validate(observed, &observed.settings)?)
    } else {
        None
    };
    let consume_pending = !current.settings_fixed && current.pending.is_some();
    if !current.settings_fixed {
        if let Some(pending) = &current.pending {
            if pending
                .account_key
                .as_ref()
                .is_some_and(|a| a != &current.account_key)
            {
                return Err(D::AccountChanged);
            }
            settings = pending.settings.clone();
            // These settings were validated when saved; admission additionally
            // requires the supplied fresh, account-bound catalog observation.
            let _native_effort = validate(current, &settings)?;
            account = Some(current.account_key.clone());
        } else if account.as_ref().is_some_and(|a| a != &current.account_key) {
            if observed.account_key != current.account_key
                || observed.settings.model != current.settings.model
                || observed.settings.effort != current.settings.effort
                || observed.settings.fast_mode != current.settings.fast_mode
            {
                return Err(D::SettingsChanged);
            }
            settings.native_effort = revalidated.ok_or(D::SettingsChanged)?;
            account = Some(current.account_key.clone());
        }
    }
    if settings.daybreak_enabled {
        return Err(D::DaybreakUnsupported);
    }
    let freeze = (!current.settings_fixed).then(|| FreezePatch {
        expected: current.clone(),
        settings: settings.clone(),
        execution_settings_account_key: account,
        consume_pending,
        invalidate_loaded: consume_pending,
        settings_fixed: true,
    });
    Ok(ExecutionSelection { settings, freeze })
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("budget-policy/coverage-unavailable")]
pub struct BudgetCoverageUnavailable;

/// History-backed accounting is unavailable. Even a zero budget is bounded.
/// The caller retains the queue and does not turn this into a submission retry.
pub fn budget_admission(token_budget: Option<u64>) -> Result<(), BudgetCoverageUnavailable> {
    if token_budget.is_some() {
        Err(BudgetCoverageUnavailable)
    } else {
        Ok(())
    }
}
