//! Pure protection decisions. The caller supplies file changes and operating-system groups.
//! Stored evidence is checked for an exact state and exact command arguments. No command runs.
use crate::{
    policy::{Role, Who},
    Project, StateRec,
};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Ref updates also exclude the exact subject's author and follow derived provenance.
/// Line merges exclude authors of source ancestors absent from the target history.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ApprovalMode {
    Ref,
    Merge,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Approval {
    pub approver: u32,
    pub state: String,
    pub time: i64,
    pub met: Option<bool>,
}

pub fn approvals(
    subject: &str,
    base: Option<&str>,
    mode: ApprovalMode,
    approved: &[(u32, i64)],
    state: impl Fn(&str) -> Result<Option<StateRec>>,
    may_approve: impl Fn(u32) -> bool,
) -> Result<Vec<Approval>> {
    let ancestry = |start: &str| -> Result<BTreeSet<String>> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![start.to_string()];
        while let Some(id) = pending.pop() {
            if seen.insert(id.clone()) {
                if let Some(s) = state(&id)? {
                    pending.extend(s.parents);
                    if matches!(mode, ApprovalMode::Ref) {
                        pending.extend(s.derived_from);
                    }
                }
            }
        }
        Ok(seen)
    };
    let have = base.map(ancestry).transpose()?.unwrap_or_default();
    let mut authors = BTreeSet::new();
    for id in ancestry(subject)? {
        if !have.contains(&id) || (matches!(mode, ApprovalMode::Ref) && id == subject) {
            if let Some(s) = state(&id)? {
                authors.insert(s.author.uid);
            }
        }
    }
    Ok(approved
        .iter()
        .filter(|(uid, _)| !authors.contains(uid) && may_approve(*uid))
        .map(|(uid, time)| Approval { approver: *uid, state: subject.into(), time: *time, met: Some(true) })
        .collect())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckResult {
    Passed,
    Failed,
    Missing,
    Undefined,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub argv: Vec<String>,
    pub result: CheckResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
}

pub fn check_argv(command: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), command.into()]
}
/// A matching passing result remains sufficient even if a newer matching result failed.
pub fn required_check(name: &str, command: Option<&str>, passing: bool, latest: Option<i64>) -> Check {
    Check {
        name: name.into(),
        argv: command.map(check_argv).unwrap_or_default(),
        result: if command.is_none() {
            CheckResult::Undefined
        } else if passing {
            CheckResult::Passed
        } else if latest.is_some() {
            CheckResult::Failed
        } else {
            CheckResult::Missing
        },
        exit: if passing { Some(0) } else { latest },
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathRequirement {
    pub path: String,
    pub role: Role,
    pub met: Option<bool>,
}
pub fn path_requirements<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    actor: Role,
    required: impl Fn(&str) -> Role,
) -> Vec<PathRequirement> {
    paths
        .into_iter()
        .map(|path| {
            let role = required(path);
            PathRequirement { path: path.into(), role, met: Some(actor >= role) }
        })
        .collect()
}

/// Context not present in record metadata. Paths must include both names of a rename.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Context {
    pub now: i64,
    /// None means no verified mapping to the native uid and groups. Permission stays false.
    pub actor: Option<Who>,
    pub approver_groups: BTreeMap<u32, Vec<u32>>,
    pub changed_paths: Vec<String>,
    /// The exact merge result when the merge is not a fast-forward.
    pub candidate: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeStatus {
    pub state: String,
    pub target: String,
    pub approvals: Vec<Approval>,
    pub approvals_needed: u32,
    pub approvers: Vec<String>,
    pub checks: Vec<Check>,
    pub path_requirements: Vec<PathRequirement>,
    pub allowed: Option<bool>,
    pub already_in_target: bool,
    pub target_machine: Option<String>,
    pub status: String,
    pub undecided_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Project {
    /// Evaluate evidence from the verified target machine's policy. This never predicts conflicts.
    /// Without a verified actor, role-dependent results stay undecided. No uid or groups are inferred.
    pub fn merge_status(&self, state: &str, target: &str, context: &Context) -> Result<MergeStatus> {
        let line = self.line_list().into_iter().find(|l| l.name == target || l.id == target);
        let target_machine = line.map(|l| l.machine).or_else(|| {
            self.records
                .iter()
                .rev()
                .find(|r| r.data["ref_change"]["name"].as_str() == Some(target))
                .and_then(|r| r.data["ref_change"]["after"].is_object().then(|| r.machine.clone()))
        });
        let Some(machine) = target_machine else {
            return Ok(MergeStatus {
                state: state.into(),
                target: target.into(),
                approvals: vec![],
                approvals_needed: 0,
                approvers: vec![],
                checks: vec![],
                path_requirements: vec![],
                allowed: None,
                already_in_target: false,
                target_machine: None,
                status: "unavailable".into(),
                undecided_reasons: vec![],
                reason: Some("the target has no verified machine record".into()),
            });
        };
        if machine != self.machine {
            return Project::from_records(&self.id, self.records.clone(), &machine)?
                .merge_status_for_machine(state, target, context);
        }
        self.merge_status_for_machine(state, target, context)
    }

    fn merge_status_for_machine(&self, state: &str, target: &str, context: &Context) -> Result<MergeStatus> {
        let policy = self.access_policy(context.now, false);
        let line = self.line_list().into_iter().find(|l| l.name == target || l.id == target);
        let is_ref = crate::types::ref_namespace(target);
        let base = line
            .as_ref()
            .map(|l| l.head.clone())
            .or_else(|| self.ref_list(target, context.now).into_iter().find(|r| r.name == target).map(|r| r.target));
        self.state(state).context("state is not known")?;
        let already_in_target = !is_ref && base.as_ref().is_some_and(|id| self.ancestors(id).iter().any(|id| id == state));
        let rule = policy.rule_for(line.as_ref().map(|l| l.name.as_str()).unwrap_or(target));
        let role = context.actor.as_ref().map(|actor| policy.role(actor)).unwrap_or(Role::None);
        let mut status = MergeStatus {
            state: state.into(),
            target: target.into(),
            approvals: vec![],
            approvals_needed: rule.as_ref().map(|r| r.approvals).unwrap_or(0),
            approvers: rule.as_ref().map(|r| r.approvers.clone()).unwrap_or_default(),
            checks: vec![],
            path_requirements: vec![],
            allowed: None,
            already_in_target,
            target_machine: Some(self.machine.clone()),
            status: if context.actor.is_some() { "decided" } else { "not_decided" }.into(),
            undecided_reasons: vec![],
            reason: None,
        };
        if context.actor.is_none() {
            status.undecided_reasons.push("the actor's role is not known here".into());
        }
        let Some(rule) = rule else {
            status.allowed =
                context.actor.as_ref().map(|actor| policy.check(actor, if is_ref { "line.manage" } else { "merge" }).is_ok());
            return Ok(status);
        };
        let candidates = approvals(
            state,
            base.as_deref(),
            if is_ref { ApprovalMode::Ref } else { ApprovalMode::Merge },
            &self.approvals(state),
            |id| Ok(self.state(id).cloned()),
            |_| true,
        )?;
        for mut approval in candidates {
            let who = Who {
                uid: approval.approver,
                groups: context.approver_groups.get(&approval.approver).cloned().unwrap_or_default(),
            };
            if policy.may_approve(&rule, &who) {
                status.approvals.push(approval);
                continue;
            }
            let group_needed = if rule.approvers.is_empty() {
                policy.access.iter().any(|(p, r)| p.starts_with("group:") && *r >= Role::Maintainer)
                    || policy.grants.iter().any(|(p, (r, _))| p.starts_with("group:") && *r >= Role::Maintainer)
            } else {
                rule.approvers.iter().any(|p| p.starts_with("group:"))
                    && rule
                        .approvers
                        .iter()
                        .filter(|p| !p.starts_with("group:"))
                        .all(|p| p == "*" || p == &format!("user:{}", approval.approver))
            };
            if !context.approver_groups.contains_key(&approval.approver) && group_needed {
                approval.met = None;
                status.undecided_reasons.push(format!("approver {}: membership not known here", approval.approver));
                status.approvals.push(approval);
            }
        }
        let candidate = context
            .candidate
            .as_deref()
            .or_else(|| {
                if already_in_target {
                    return base.as_deref();
                }
                if is_ref || base.as_ref().is_some_and(|id| self.ancestors(state).contains(id)) {
                    Some(state)
                } else {
                    None
                }
            })
            .filter(|id| self.state(id).is_some());
        for name in &rule.check {
            let command = self.setting(&format!("check.{name}")).and_then(|v| v.as_str());
            let argv = command.map(check_argv).unwrap_or_default();
            let passing = candidate.is_some_and(|s| self.check_exit(s, &argv, Some(0)).is_some());
            let latest = candidate.and_then(|s| self.check_exit(s, &argv, None));
            status.checks.push(required_check(name, command, passing, latest));
        }
        status.path_requirements =
            path_requirements(context.changed_paths.iter().map(String::as_str), role, |path| rule.role_for_path(path));
        if context.actor.is_none() {
            for path in &mut status.path_requirements {
                path.met = None;
            }
        }
        let root = context.actor.as_ref().is_some_and(|actor| actor.uid == 0);
        let groups_unknown = status.approvals.iter().any(|a| a.met.is_none());
        if let Some(actor) = &context.actor {
            if groups_unknown {
                status.status = "not_decided".into();
            } else {
                status.allowed = Some(
                    role >= rule.update
                        && policy.denied_by(actor, if is_ref { "line.manage" } else { "merge" }).is_none()
                        && (already_in_target
                            || ((status.approvals.len() >= rule.approvals as usize || (!is_ref && root))
                                && status.checks.iter().all(|c| c.result == CheckResult::Passed)
                                && status.path_requirements.iter().all(|p| p.met == Some(true))
                                && (is_ref || candidate.is_some()))),
                );
            }
        }
        Ok(status)
    }
}
