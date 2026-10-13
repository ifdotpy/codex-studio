use super::*;
use serde_json::json;

fn input() -> (Value, WorkspaceActor, ProvisionIdentity) {
    let record = json!({"id":"worker","epoch":7,"cwd":"/repo/project","autoWake":true,"worktree":true,"worktreeReady":false});
    let actor = serde_json::from_value(record.clone()).expect("actor");
    let plan = ProvisionIdentity {
        id: "provision:worker:7".into(),
        agent: "worker".into(),
        source: workspace_generation(&record),
        repository: "/repo".into(),
        common_dir: "/repo/.git".into(),
        destination: "/repo/.worktrees/worker".into(),
        cwd: "/repo/.worktrees/worker/project".into(),
        branch: "codex-agent/worker".into(),
        base_commit: "a".repeat(40),
    };
    (record, actor, plan)
}
fn complete(plan: &ProvisionIdentity) -> GitObservation {
    GitObservation {
        destination_exists: true,
        directory: Some(plan.destination.clone()),
        registered: true,
        branch_exists: true,
        branch: Some(plan.branch.clone()),
        head: Some(plan.base_commit.clone()),
        common_dir: Some(plan.common_dir.clone()),
        clean: true,
    }
}

#[test]
fn journal_recovery_requires_absent_or_exact_complete_evidence() {
    let (record, actor, plan) = input();
    let cwd = actor.cwd.clone();
    let decide = |observed: &GitObservation, journaled| {
        decide_workspace(
            &actor,
            &record,
            Some(&cwd),
            &[],
            Some(&plan),
            journaled,
            observed,
        )
    };
    assert!(
        matches!(decide(&GitObservation::default(),true),WorkspaceDecision::Provision(p) if p.effect==ProvisionEffect::Create)
    );
    assert!(
        matches!(decide(&complete(&plan),true),WorkspaceDecision::Provision(p) if p.effect==ProvisionEffect::Publish)
    );
    assert!(
        matches!(
            decide(&complete(&plan), false),
            WorkspaceDecision::Refuse(_)
        ),
        "unjournaled worktree must not be adopted"
    );
    for field in [
        "directory",
        "registration",
        "missing_ref",
        "branch_exists",
        "branch",
        "head",
        "common",
        "clean",
    ] {
        let mut observed = complete(&plan);
        match field {
            "directory" => observed.directory = Some("/other".into()),
            "registration" => observed.registered = false,
            "missing_ref" => observed.branch_exists = false,
            "branch_exists" => {
                observed = GitObservation::default();
                observed.branch_exists = true;
            }
            "branch" => observed.branch = Some("other".into()),
            "head" => observed.head = Some("b".repeat(40)),
            "common" => observed.common_dir = Some("/other/.git".into()),
            "clean" => observed.clean = false,
            _ => unreachable!(),
        }
        assert!(
            matches!(
                decide(&observed, true),
                WorkspaceDecision::Refuse(WorkspaceRefusal::WorkspaceRecoveryRequired { .. })
            ),
            "{field}"
        );
    }
    let only_registration = GitObservation {
        registered: true,
        ..GitObservation::default()
    };
    assert!(matches!(
        decide(&only_registration, true),
        WorkspaceDecision::Refuse(_)
    ));
    let only_file = GitObservation {
        destination_exists: true,
        ..GitObservation::default()
    };
    assert!(matches!(
        decide(&only_file, true),
        WorkspaceDecision::Refuse(_)
    ));
}

#[test]
fn unsupported_modes_are_refused_for_new_existing_and_stopped_records() {
    for ready in [false, true] {
        for (field, value, expected) in [
            (
                "workspaceMode",
                json!("image"),
                WorkspaceRefusal::ImageWorkspaceUnsupported,
            ),
            (
                "imageWorkspace",
                json!(true),
                WorkspaceRefusal::ImageWorkspaceUnsupported,
            ),
            (
                "imageWorkspaceReady",
                json!(true),
                WorkspaceRefusal::ImageWorkspaceUnsupported,
            ),
            (
                "imageWorkspacePhase",
                json!("fallback"),
                WorkspaceRefusal::ImageWorkspaceUnsupported,
            ),
            (
                "executionMode",
                json!("vm"),
                WorkspaceRefusal::VmWorkspaceUnsupported,
            ),
            (
                "workspaceMode",
                json!("layr"),
                WorkspaceRefusal::VmWorkspaceUnsupported,
            ),
            (
                "layrReady",
                json!(true),
                WorkspaceRefusal::VmWorkspaceUnsupported,
            ),
            (
                "environment",
                json!("linux"),
                WorkspaceRefusal::LegacyVmWorkspaceUnsupported,
            ),
        ] {
            let (mut record, _, _) = input();
            record[field] = value;
            record["worktreeReady"] = json!(ready);
            record["autoWake"] = json!(false);
            let actor: WorkspaceActor = serde_json::from_value(record.clone()).expect("actor");
            assert_eq!(
                decide_workspace(
                    &actor,
                    &record,
                    None,
                    &[],
                    None,
                    false,
                    &GitObservation::default()
                ),
                WorkspaceDecision::Refuse(expected)
            );
        }
    }
}

#[test]
fn ready_boolean_requires_identity_but_preserves_dirty_advanced_worktree() {
    let (mut record, _, plan) = input();
    record["worktreeReady"] = json!(true);
    record["cwd"] = json!(plan.cwd);
    record["branch"] = json!(plan.branch);
    let actor: WorkspaceActor = serde_json::from_value(record.clone()).expect("actor");
    let mut observed = complete(&plan);
    observed.clean = false;
    observed.head = Some("b".repeat(40));
    assert_eq!(
        decide_workspace(
            &actor,
            &record,
            Some(&plan.cwd),
            &[],
            None,
            false,
            &observed
        ),
        WorkspaceDecision::Ready {
            token: ReadyToken::new(&record, plan.cwd.clone())
        }
    );
    observed.branch = Some("other".into());
    assert!(matches!(
        decide_workspace(
            &actor,
            &record,
            Some(&plan.cwd),
            &[],
            None,
            false,
            &observed
        ),
        WorkspaceDecision::Refuse(_)
    ));
    assert!(matches!(
        decide_workspace(&actor, &record, None, &[], None, false, &observed),
        WorkspaceDecision::Refuse(_)
    ));
}

#[test]
fn reservations_defer_capture_and_refuse_restore_or_replaced_ownership() {
    let (record, actor, plan) = input();
    for operation in ["capture", "checkpoint", "restore", "branch", "provision"] {
        let reservation = Reservation {
            agent: "other".into(),
            cwd: actor.cwd.clone(),
            operation: operation.into(),
            id: Some("other-op".into()),
            phase: Some("local_mutation".into()),
        };
        let result = decide_workspace(
            &actor,
            &record,
            Some(&actor.cwd),
            &[reservation],
            Some(&plan),
            true,
            &GitObservation::default(),
        );
        assert_eq!(
            matches!(result, WorkspaceDecision::Defer { .. }),
            matches!(operation, "capture" | "checkpoint")
        );
        assert_eq!(
            matches!(result, WorkspaceDecision::Refuse(_)),
            !matches!(operation, "capture" | "checkpoint")
        );
    }
    let own = Reservation {
        agent: actor.id.clone(),
        cwd: actor.cwd.clone(),
        operation: "provision".into(),
        id: Some(plan.id.clone()),
        phase: Some("recovery_required".into()),
    };
    assert!(matches!(
        decide_workspace(
            &actor,
            &record,
            Some(&actor.cwd),
            std::slice::from_ref(&own),
            Some(&plan),
            true,
            &GitObservation::default()
        ),
        WorkspaceDecision::Provision(_)
    ));
    let mut replaced = own;
    replaced.id = Some("replacement".into());
    assert!(matches!(
        decide_workspace(
            &actor,
            &record,
            Some(&actor.cwd),
            &[replaced],
            Some(&plan),
            true,
            &GitObservation::default()
        ),
        WorkspaceDecision::Refuse(_)
    ));
    let mut changed = record.clone();
    changed["epoch"] = json!(8);
    assert!(matches!(
        decide_workspace(
            &actor,
            &changed,
            Some(&actor.cwd),
            &[],
            Some(&plan),
            true,
            &GitObservation::default()
        ),
        WorkspaceDecision::Refuse(_)
    ));
}

#[test]
fn shared_workspace_requires_directory_and_stopped_or_deleted_actor_defers() {
    let (mut record, _, _) = input();
    record["worktree"] = json!(false);
    let actor: WorkspaceActor = serde_json::from_value(record.clone()).expect("actor");
    assert!(matches!(
        decide_workspace(
            &actor,
            &record,
            Some(&actor.cwd),
            &[],
            None,
            false,
            &GitObservation::default()
        ),
        WorkspaceDecision::Ready { .. }
    ));
    assert!(matches!(
        decide_workspace(
            &actor,
            &record,
            None,
            &[],
            None,
            false,
            &GitObservation::default()
        ),
        WorkspaceDecision::Refuse(_)
    ));
    for (key, value) in [("deletedAt", json!(4)), ("autoWake", json!(false))] {
        let mut stopped = record.clone();
        stopped[key] = value;
        let actor: WorkspaceActor = serde_json::from_value(stopped.clone()).expect("actor");
        assert!(matches!(
            decide_workspace(
                &actor,
                &stopped,
                Some(&actor.cwd),
                &[],
                None,
                false,
                &GitObservation::default()
            ),
            WorkspaceDecision::Defer { .. }
        ));
    }
}
