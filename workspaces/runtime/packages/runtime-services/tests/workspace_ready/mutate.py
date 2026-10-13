"""Explicit, bounded seeded mutation check; restores every source even on failure."""
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[6]
SERVICES = ROOT / "workspaces/runtime/packages/runtime-services/src/workspace_ready.rs"
STORE = ROOT / "workspaces/runtime/packages/runtime-store/src/repositories/workspace_ready.rs"
MUTATIONS = [
    ("retry-ignores-existing-branch", SERVICES, "&& !observed.branch_exists", "&& true", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("publish-ignores-base", SERVICES, "&& observed.head.as_deref() == Some(&plan.base_commit)", "&& true", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("ambiguity-is-retry", SERVICES, "let effect = if absent {", "let effect = if absent || !complete {", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("publish-dirty-creation", SERVICES, "&& observed.clean;", "&& true;", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("adopt-without-journal", SERVICES, "} else if journaled && complete {", "} else if complete {", "journal_recovery_requires_absent_or_exact_complete_evidence"),
    ("ready-ignores-branch", SERVICES, "&& actor.branch == observed.branch", "&& true", "ready_boolean_requires_identity_but_preserves_dirty_advanced_worktree"),
    ("publication-ignores-generation", STORE, "workspace_generation(&a.record) == plan.source", "true", "late_completion_never_attaches_to_deleted_stopped_replaced_or_new_epoch_actor"),
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
