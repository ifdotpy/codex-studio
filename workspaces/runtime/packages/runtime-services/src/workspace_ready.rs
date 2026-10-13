//! Pure host workspace readiness. Observations are gathered by `workspace_ready_git`.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use studio_runtime_store::workspace_ready::{ProvisionIdentity, workspace_generation};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceActor {
    pub id: String,
    pub epoch: u64,
    pub cwd: PathBuf,
    pub auto_wake: bool,
    #[serde(default)]
    pub deleted_at: Option<Value>,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub worktree_ready: bool,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub workspace_mode: Option<String>,
    #[serde(default)]
    pub execution_mode: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub image_workspace: bool,
    #[serde(default)]
    pub image_workspace_ready: bool,
    #[serde(default)]
    pub image_workspace_phase: Option<String>,
    #[serde(default)]
    pub layr_ready: bool,
    #[serde(default)]
    pub layr_line: Option<String>,
    #[serde(default)]
    pub workspace_operation: Option<String>,
    #[serde(default)]
    pub workspace_reservation_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub agent: String,
    pub cwd: PathBuf,
    pub operation: String,
    pub id: Option<String>,
    pub phase: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitObservation {
    pub destination_exists: bool,
    pub directory: Option<PathBuf>,
    pub registered: bool,
    pub branch_exists: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub common_dir: Option<PathBuf>,
    pub clean: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionEffect {
    Create,
    Publish,
}
#[derive(Clone, Debug, PartialEq)]
pub struct WorktreePlan {
    pub identity: ProvisionIdentity,
    pub effect: ProvisionEffect,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceRefusal {
    #[error("ImageWorkspaceUnsupported")]
    ImageWorkspaceUnsupported,
    #[error("VmWorkspaceUnsupported")]
    VmWorkspaceUnsupported,
    #[error("LegacyVmWorkspaceUnsupported")]
    LegacyVmWorkspaceUnsupported,
    #[error("WorkspaceRecoveryRequired: {reason}")]
    WorkspaceRecoveryRequired {
        operation: Option<String>,
        path: PathBuf,
        reason: &'static str,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceDecision {
    Ready { cwd: PathBuf },
    Provision(WorktreePlan),
    Defer { reservation: Option<String> },
    Refuse(WorkspaceRefusal),
}

pub fn unsupported(actor: &WorkspaceActor) -> Option<WorkspaceRefusal> {
    if actor.execution_mode.as_deref() == Some("vm")
        || actor.workspace_mode.as_deref() == Some("layr")
        || actor.layr_ready
        || actor.layr_line.is_some()
    {
        Some(WorkspaceRefusal::VmWorkspaceUnsupported)
    } else if actor.environment.as_deref() == Some("linux") {
        Some(WorkspaceRefusal::LegacyVmWorkspaceUnsupported)
    } else if actor.image_workspace
        || actor.image_workspace_ready
        || actor.image_workspace_phase.is_some()
        || actor.workspace_mode.as_deref() == Some("image")
    {
        Some(WorkspaceRefusal::ImageWorkspaceUnsupported)
    } else {
        None
    }
}

pub fn recovery(
    operation: Option<String>,
    path: PathBuf,
    reason: &'static str,
) -> WorkspaceDecision {
    WorkspaceDecision::Refuse(WorkspaceRefusal::WorkspaceRecoveryRequired {
        operation,
        path,
        reason,
    })
}

/// `cwd` and reservation paths have been canonicalized by the adapter. No filesystem reads here.
/// A ready worker may be dirty or have advanced HEAD; only recovering creation requires clean/base.
pub fn decide_workspace(
    actor: &WorkspaceActor,
    record: &Value,
    cwd: Option<&PathBuf>,
    reservations: &[Reservation],
    plan: Option<&ProvisionIdentity>,
    journaled: bool,
    observed: &GitObservation,
) -> WorkspaceDecision {
    if let Some(refusal) = unsupported(actor) {
        return WorkspaceDecision::Refuse(refusal);
    }
    if actor.deleted_at.is_some() || !actor.auto_wake {
        return WorkspaceDecision::Defer { reservation: None };
    }
    for reservation in reservations {
        let own_provision = journaled
            && reservation.agent == actor.id
            && reservation.operation == "provision"
            && plan.is_some_and(|p| reservation.id.as_deref() == Some(&p.id));
        if own_provision {
            continue;
        }
        if cwd == Some(&reservation.cwd) || reservation.agent == actor.id {
            if matches!(reservation.operation.as_str(), "capture" | "checkpoint")
                && reservation.phase.as_deref() != Some("recovery_required")
            {
                return WorkspaceDecision::Defer {
                    reservation: reservation.id.clone(),
                };
            }
            return recovery(
                reservation.id.clone(),
                reservation.cwd.clone(),
                "Workspace operation is unresolved",
            );
        }
    }
    if !actor.worktree && actor.workspace_mode.as_deref() != Some("worktree") {
        return match cwd {
            Some(cwd) => WorkspaceDecision::Ready { cwd: cwd.clone() },
            None => recovery(None, actor.cwd.clone(), "Workspace directory is missing"),
        };
    }
    if actor.worktree_ready {
        if let Some(cwd) = cwd
            && observed.registered
            && observed
                .directory
                .as_ref()
                .is_some_and(|root| cwd.starts_with(root))
            && actor.branch.is_some()
            && actor.branch == observed.branch
        {
            return WorkspaceDecision::Ready { cwd: cwd.clone() };
        }
        return recovery(
            None,
            actor.cwd.clone(),
            "Ready worktree identity does not match Git",
        );
    }
    let Some(plan) = plan else {
        return recovery(
            None,
            actor.cwd.clone(),
            "Source Git identity is unavailable",
        );
    };
    if workspace_generation(record) != plan.source || plan.agent != actor.id {
        return recovery(
            Some(plan.id.clone()),
            plan.destination.clone(),
            "Provisioning actor changed",
        );
    }
    let absent = !observed.destination_exists
        && observed.directory.is_none()
        && !observed.registered
        && !observed.branch_exists;
    let complete = observed.directory.as_ref() == Some(&plan.destination)
        && observed.registered
        && observed.branch_exists
        && observed.branch.as_deref() == Some(&plan.branch)
        && observed.head.as_deref() == Some(&plan.base_commit)
        && observed.common_dir.as_ref() == Some(&plan.common_dir)
        && observed.clean;
    let effect = if absent {
        ProvisionEffect::Create
    } else if journaled && complete {
        ProvisionEffect::Publish
    } else {
        return recovery(
            Some(plan.id.clone()),
            plan.destination.clone(),
            "Provisioning evidence is ambiguous",
        );
    };
    WorkspaceDecision::Provision(WorktreePlan {
        identity: plan.clone(),
        effect,
    })
}

#[cfg(test)]
#[path = "../tests/workspace_ready/decisions.rs"]
mod tests;
