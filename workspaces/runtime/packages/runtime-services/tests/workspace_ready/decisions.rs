use super::*;
use serde_json::json;

fn input() -> (Value, WorkspaceActor, ProvisionIdentity) {
    let record = json!({"id":"worker","epoch":7,"cwd":"/repo/project","autoWake":true,"worktree":true,"worktreeReady":false});
    let actor = serde_json::from_value(record.clone()).expect("actor");
    let plan = ProvisionIdentity {
        id: "provision:worker:7".into(),
        agent: "worker".into(),
        source: workspace_generation(&record),
        thread_id: serde_json::Value::Null,
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
                json!("read_only"),
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
fn an_image_fallback_prepares_its_worktree_like_any_worktree_agent() {
    // Python `complete_image_workspace` leaves `imageWorkspacePhase: "fallback"`
    // on an agent that now uses a Git worktree or its folder.
    let (mut record, _, _) = input();
    record["imageWorkspace"] = json!(false);
    record["imageWorkspacePhase"] = json!("fallback");
    record["workspaceMode"] = json!("worktree");
    let actor: WorkspaceActor = serde_json::from_value(record.clone()).expect("actor");
    assert!(!actor.image_bound());
    assert_eq!(unsupported(&actor), None);
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
    // A worker may switch its own worktree to a branch it names.
    observed.branch = Some("feature/named-by-worker".into());
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
    observed.registered = false;
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
    observed.registered = true;
    let outside = std::path::PathBuf::from("/elsewhere");
    assert!(matches!(
        decide_workspace(&actor, &record, Some(&outside), &[], None, false, &observed),
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
            matches!(operation, "capture" | "checkpoint" | "provision")
        );
        assert_eq!(
            matches!(result, WorkspaceDecision::Refuse(_)),
            !matches!(operation, "capture" | "checkpoint" | "provision")
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

fn foreign_provision(destination: &str, phase: &str) -> Reservation {
    Reservation {
        agent: "other".into(),
        cwd: destination.into(),
        operation: "provision".into(),
        id: Some("provision:other:1".into()),
        phase: Some(phase.into()),
    }
}

#[test]
fn another_agents_provision_does_not_block_unrelated_workers_or_the_lead() {
    let (record, actor, plan) = input();
    let other = foreign_provision("/repo/.worktrees/other", "local_mutation");
    // A worker with a different destination provisions while the other runs.
    assert!(matches!(
        decide_workspace(
            &actor,
            &record,
            Some(&actor.cwd),
            std::slice::from_ref(&other),
            Some(&plan),
            false,
            &GitObservation::default()
        ),
        WorkspaceDecision::Provision(_)
    ));
    // The lead works in the repository root that contains the destination.
    let lead_record = json!({"id":"lead","epoch":1,"cwd":"/repo","autoWake":true});
    let lead: WorkspaceActor = serde_json::from_value(lead_record.clone()).expect("lead");
    assert!(matches!(
        decide_workspace(
            &lead,
            &lead_record,
            Some(&lead.cwd),
            &[other],
            None,
            false,
            &GitObservation::default()
        ),
        WorkspaceDecision::Ready { .. }
    ));
}

#[test]
fn provision_conflicts_only_on_same_destination_or_inside_it() {
    let (record, actor, plan) = input();
    // Same destination is a real conflict, final even while in progress.
    let same = foreign_provision("/repo/.worktrees/worker", "local_mutation");
    assert!(matches!(
        decide_workspace(
            &actor,
            &record,
            Some(&actor.cwd),
            &[same],
            Some(&plan),
            false,
            &GitObservation::default()
        ),
        WorkspaceDecision::Refuse(_)
    ));
    // An actor inside another agent's destination waits while it runs and is refused when it needs recovery.
    let inside =
        json!({"id":"w2","epoch":1,"cwd":"/repo/.worktrees/other/project","autoWake":true});
    let w2: WorkspaceActor = serde_json::from_value(inside.clone()).expect("w2");
    for (phase, defers) in [("local_mutation", true), ("recovery_required", false)] {
        let result = decide_workspace(
            &w2,
            &inside,
            Some(&w2.cwd),
            &[foreign_provision("/repo/.worktrees/other", phase)],
            None,
            false,
            &GitObservation::default(),
        );
        assert_eq!(matches!(result, WorkspaceDecision::Defer { .. }), defers);
        assert_eq!(matches!(result, WorkspaceDecision::Refuse(_)), !defers);
    }
}

#[test]
fn shared_workspace_ignores_worktree_preference_and_stopped_or_deleted_actor_defers() {
    let (mut record, _, _) = input();
    record["worktree"] = json!(false);
    record["workspaceMode"] = json!("worktree");
    let actor: WorkspaceActor = serde_json::from_value(record.clone()).expect("actor");
    assert!(!requires_worktree(&actor));
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
