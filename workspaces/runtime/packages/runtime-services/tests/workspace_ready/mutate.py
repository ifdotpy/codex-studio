"""Explicit, bounded seeded mutation check; restores every source even on failure."""
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[6]
SERVICES = ROOT / "workspaces/runtime/packages/runtime-services/src/workspace_ready.rs"
STORE = ROOT / "workspaces/runtime/packages/runtime-store/src/repositories/workspace_ready.rs"
GIT = ROOT / "workspaces/runtime/packages/runtime-services/src/workspace_ready_git.rs"
MUTATIONS = [
    ("retry-ignores-existing-branch", SERVICES, "&& !observed.branch_exists", "&& true", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("publish-ignores-base", SERVICES, "&& observed.head.as_deref() == Some(&plan.base_commit)", "&& true", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("ambiguity-is-retry", SERVICES, "let effect = if absent {", "let effect = if absent || !complete {", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("publish-dirty-creation", SERVICES, "&& observed.clean;", "&& true;", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("adopt-without-journal", SERVICES, "} else if journaled && complete {", "} else if complete {", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("ready-ignores-branch", SERVICES, "&& actor.branch == observed.branch", "&& true", "ready_boolean_requires_identity_but_preserves_dirty_advanced_worktree"),
    ("publication-ignores-generation", STORE, "workspace_generation(&a.record) == plan.source", "true", "late_completion_never_attaches_to_deleted_stopped_replaced_or_new_epoch_actor"),
    ("A1-inherit-git-environment", GIT, "bounded_child::run_isolated(bounded_child::Request", "bounded_child::run(bounded_child::Request", "isolated_child_rejects_all_ambient_git_overrides"),
    ("A4-ignore-hidden-index-flags", GIT, '.all(|entry| entry.starts_with("H "))', '.all(|_entry| true)', "hidden_index_flags_and_sparse_checkout_never_prove_recovery_complete"),
    ("A4-ignore-sparse-checkout", GIT, 'sparse.trim() == "false"', 'true', "hidden_index_flags_and_sparse_checkout_never_prove_recovery_complete"),
    ("A5-equal-paths-only", STORE, "a.starts_with(b) || b.starts_with(a)", "a == b", "parent_and_destination_reservations_block_but_progress_does_not"),
    ("A6-skip-final-git-evidence", GIT, "let valid = current.as_ref().is_some_and", "let valid = true || current.as_ref().is_some_and", "checkpoint_window_revalidates_git_and_reservations_and_recovers_exact_replay"),
    ("A7-ignore-ready-generation", STORE, "workspace_generation(&current.agent.record) == self.source", "true", "ready_token_requires_current_generation_and_no_conflicting_reservation"),
    ("A8-overwrite-checkpoint-ref", GIT, '&"0".repeat(plan.base_commit.len()),', 'existing.trim(),', "checkpoint_ref_collision_is_preserved_and_reported"),
    ("A9-empty-history", STORE, 'value["historyDelta"] = json!(delta);', 'value["historyDelta"] = json!([]);', "publication_and_checkpoint_history_match_python"),
    ("backend-refusal-disabled", SERVICES, "if let Some(refusal) = unsupported(actor)", "if false && let Some(refusal) = unsupported(actor)", "unsupported_modes_are_refused_for_new_existing_and_stopped_records"),
    ("path-confinement-disabled", GIT, "    identity.destination.canonicalize().ok().is_some_and", "    true || identity.destination.canonicalize().ok().is_some_and", "post_effect_symlink_escape_is_refused_without_cleanup"),
    ("journal-before-effect-disabled", GIT, "    if !store.transaction(|tx|", "    if false && !store.transaction(|tx|", "stale_reservation_precondition_never_starts_git_effect"),
    ("reservation-identity-disabled", SERVICES, "&& plan.is_some_and(|p| reservation.id.as_deref() == Some(&p.id))", "&& true", "reservations_defer_capture_and_refuse_restore_or_replaced_ownership"),
    ("common-directory-serialization-disabled", GIT, "let lock = repository_lock(&plan.identity.common_dir)?;", "let lock = Arc::new(Mutex::new(()));", "common_git_directory_serialization_defers_without_starting_another_effect"),
]


def main():
    target = Path(os.environ["CARGO_TARGET_DIR"])
    assert str(target).startswith("/var/tmp/cs-rust-workspace-ready/")
    assert str(Path(os.environ["HOME"])).startswith("/var/tmp/cs-rust-workspace-ready/")
    results = []
    for name, path, original, mutation, test in MUTATIONS:
        source = path.read_text()
        assert source.count(original) == 1, name
        try:
            path.write_text(source.replace(original, mutation))
            run = subprocess.run(["cargo", "nextest", "run", "--offline", "-p", "studio-runtime-services", "--lib", "--features", "tokio/sync",
                                  "-E", "test(" + test + ")", "--status-level", "fail"], cwd=ROOT, capture_output=True, text=True, timeout=180)
            log = target.parent / ("mutation-" + name + ".log")
            log.write_text(run.stdout + run.stderr)
            killed = run.returncode == 100 and "FAIL" in run.stderr
            results.append({"mutation": name, "test": test, "killed": killed, "exit": run.returncode, "log": str(log)})
            print(json.dumps(results[-1]), flush=True)
        finally:
            path.write_text(source)
    (target.parent / "mutations.json").write_text(json.dumps(results, indent=2) + "\n")
    assert all(r["killed"] for r in results), "a seeded mutation survived or did not compile"


if __name__ == "__main__":
    main()
