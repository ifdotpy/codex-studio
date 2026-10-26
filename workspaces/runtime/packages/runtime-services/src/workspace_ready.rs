//! Pure host workspace readiness. Observations are gathered by `workspace_ready_git`.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use studio_runtime_store::path_identity::CanonicalPath;
use studio_runtime_store::workspace_ready::{ProvisionIdentity, overlaps, workspace_generation};
pub use studio_runtime_store::workspace_ready::{ReadyToken, Reservation};

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

impl WorkspaceActor {
    /// The agent runs in an image workspace. A fallback phase does not count:
    /// that agent runs in a Git worktree or its folder.
    #[must_use]
    pub fn image_bound(&self) -> bool {
        self.image_workspace
            || self.image_workspace_ready
            || self
                .image_workspace_phase
                .as_deref()
                .is_some_and(|phase| !matches!(phase, "fallback" | "failed"))
            || self.workspace_mode.as_deref() == Some("image")
    }
}

/// Whether this actor requires Git worktree provisioning before it can run.
/// `workspaceMode` is a preference; the persisted `worktree` flag is the owner.
#[must_use]
pub fn requires_worktree(actor: &WorkspaceActor) -> bool {
    actor.worktree
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitObservation {
    pub destination_exists: bool,
    pub directory: Option<CanonicalPath>,
    pub registered: bool,
    pub branch_exists: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub common_dir: Option<CanonicalPath>,
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
    #[error("{}", crate::legacy_image_workspace::FINISHED_MESSAGE)]
    ImageWorkspaceUnsupported,
    #[error("VmWorkspaceUnsupported")]
    VmWorkspaceUnsupported,
    #[error("LegacyVmWorkspaceUnsupported")]
    LegacyVmWorkspaceUnsupported,
    #[error("WorkspaceRecoveryRequired: {reason}")]
    WorkspaceRecoveryRequired {
        operation: Option<String>,
        path: PathBuf,
        reason: String,
    },
}
/// Readiness cannot expose a cwd until transactionally validated.
/// ```compile_fail
/// use studio_runtime_services::workspace_ready::WorkspaceDecision;
/// fn bypass(decision: WorkspaceDecision) {
///     if let WorkspaceDecision::Ready { cwd, .. } = decision { let _ = cwd; }
/// }
/// ```
/// What defers a workspace decision, and so which change can end the deferral.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeferredBy {
    /// The actor's own record: deleted, not auto-waking, or VM or image
    /// readiness not recorded yet.
    Actor,
    /// Another workspace operation, reservation or provisioning lock.
    Workspace,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceDecision {
    Ready { token: ReadyToken },
    Provision(WorktreePlan),
    Defer { by: DeferredBy },
    Refuse(WorkspaceRefusal),
}

pub fn unsupported(actor: &WorkspaceActor) -> Option<WorkspaceRefusal> {
    if actor.workspace_mode.as_deref() == Some("layr-native") {
        return None;
    }
    if actor.execution_mode.as_deref() == Some("vm")
        || actor.workspace_mode.as_deref() == Some("layr")
        || actor.layr_ready
        || actor.layr_line.is_some()
    {
        Some(WorkspaceRefusal::VmWorkspaceUnsupported)
    } else if actor.environment.as_deref() == Some("linux") {
        Some(WorkspaceRefusal::LegacyVmWorkspaceUnsupported)
    } else if actor.image_bound() {
        Some(WorkspaceRefusal::ImageWorkspaceUnsupported)
    } else {
        None
    }
}

pub fn recovery(
    operation: Option<String>,
    path: PathBuf,
    reason: impl Into<String>,
) -> WorkspaceDecision {
    WorkspaceDecision::Refuse(WorkspaceRefusal::WorkspaceRecoveryRequired {
        operation,
        path,
        reason: reason.into(),
    })
}

/// `cwd` and reservation paths have been canonicalized by the adapter. No filesystem reads here.
/// A ready worker may be dirty or have advanced HEAD; only recovering creation requires clean/base.
pub fn decide_workspace(
    actor: &WorkspaceActor,
    record: &Value,
    cwd: Option<&Path>,
    reservations: &[Reservation],
    plan: Option<&ProvisionIdentity>,
    journaled: bool,
    observed: &GitObservation,
) -> WorkspaceDecision {
    if let Some(refusal) = unsupported(actor) {
        return WorkspaceDecision::Refuse(refusal);
    }
    if actor.deleted_at.is_some() || !actor.auto_wake {
        return WorkspaceDecision::Defer {
            by: DeferredBy::Actor,
        };
    }
    for reservation in reservations {
        let own_provision = journaled
            && reservation.agent == actor.id
            && reservation.operation == "provision"
            && plan.is_some_and(|p| reservation.id.as_deref() == Some(&p.id));
        if own_provision {
            continue;
        }
        let destination = plan.map(|p| p.destination.as_path());
        let blocks = match cwd {
            Some(cwd) => reservation.blocks(&actor.id, cwd, destination),
            None => reservation.agent == actor.id,
        };
        if blocks {
            // An unfinished capture, checkpoint, or foreign provision resolves by itself.
            let foreign_provision =
                reservation.operation == "provision" && reservation.agent != actor.id;
            let same_destination = destination.is_some_and(|d| overlaps(d, &reservation.cwd));
            if (matches!(reservation.operation.as_str(), "capture" | "checkpoint")
                || (foreign_provision && !same_destination))
                && reservation.phase.as_deref() != Some("recovery_required")
            {
                return WorkspaceDecision::Defer {
                    by: DeferredBy::Workspace,
                };
            }
            return recovery(
                reservation.id.clone(),
                reservation.cwd.clone(),
                "Workspace operation is unresolved",
            );
        }
    }
    // Python assigns `worktree` only to implementer workers and gates worktree
    // creation on that flag (`codex_runtime.py:5001-5002, 6307-6308`). A lead may
    // retain a `workspaceMode` preference of `worktree` while using its shared cwd.
    if !requires_worktree(actor) {
        return match cwd {
            Some(cwd) => WorkspaceDecision::Ready {
                token: ReadyToken::new(record, cwd.to_path_buf()),
            },
            None => recovery(None, actor.cwd.clone(), "Workspace directory is missing"),
        };
    }
    // The registered worktree directory is the identity. The tool contract lets a
    // worker commit on a branch it names, so the checked-out branch may change.
    if actor.worktree_ready {
        if let Some(cwd) = cwd
            && observed.registered
            && observed
                .directory
                .as_ref()
                .is_some_and(|root| cwd.starts_with(root))
        {
            return WorkspaceDecision::Ready {
                token: ReadyToken::new(record, cwd.to_path_buf()),
            };
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
            plan.destination.to_path_buf(),
            "Provisioning actor changed",
        );
    }
    let absent = !observed.destination_exists
        && observed.directory.is_none()
        && !observed.registered
        && !observed.branch_exists;
    let complete = observed.directory.as_deref() == Some(plan.destination.as_path())
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
            plan.destination.to_path_buf(),
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
