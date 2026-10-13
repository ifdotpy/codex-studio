"""Heavy corpus mutation check. Working-tree sources are copied, never patched.

Build errors and infrastructure failures are not detections. The unmodified
instrument must pass first. Reports and individual logs remain under --target.
"""
import argparse
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--full", action="store_true", help="also measure full matrices")
    parser.add_argument("--allow-survivors", action="store_true", help="record a pre-hardening baseline")
    parser.add_argument("--only", help="comma-separated mutation IDs")
    parser.add_argument("--manifest", type=Path, help="run one table; default runs all five")
    parser.add_argument("--filter", help="override the selected table witness filter")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[6]
    target = args.target.resolve()
    assert target != root and root not in target.parents, "target must be outside the source checkout"
    target.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="sources-", dir=target))
    names = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=root).decode().split("\0")
    for name in filter(None, names):
        source = root / name
        if source.is_file():
            destination = work / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            # Fresh mtimes invalidate a cached mutant left by any previous run.
            # copy2 can make pristine sources older than the mutant artifact.
            shutil.copy(source, destination)
    tables = ["turn_lifecycle", "turn_prepare", "thread_prepare", "submit_gates", "workspace_ready"]
    filters = {name: "binary(" + name + "_corpus)" for name in tables}
    filters["turn_lifecycle"] = "binary(/^(turn_lifecycle|notification_dispatch)_corpus$/) | test(account_worker::tests)"
    filters["submit_gates"] += " | test(submit_gates::tests) | test(admission_never_exposes_cold_stale_pending_failed_or_foreign_catalogs)"
    filters["workspace_ready"] = "test(workspace_ready) | test(workspace_gate)"
    manifests = [args.manifest] if args.manifest else [Path(__file__).parent.parent / name / "mutations.json" for name in tables]
    mutations = []
    for manifest in manifests:
        for mutation in json.loads(manifest.read_text()):
            mutation["table"] = manifest.parent.name
            mutations.append(mutation)
    # Every workspace witness must be included in its baseline too.
    workspace_filters = [m["filter"] for m in mutations if m["table"] == "workspace_ready"]
    if workspace_filters:
        filters["workspace_ready"] = " | ".join(sorted(set(workspace_filters)))
    assert len({m["id"] for m in mutations}) == len(mutations), "duplicate mutation ID"
    if args.only:
        wanted = set(args.only.split(","))
        mutations = [m for m in mutations if m["id"] in wanted]
        assert {m["id"] for m in mutations} == wanted, "unknown mutation selection"
    assert mutations, "empty mutation selection"
    environment = dict(os.environ)
    for key in ("CODEX_BIN", "STUDIO_RUNTIME_CORPUS_FULL", "LIFECYCLE_CASE_FILTER", "NOTIFICATION_CASE", "NOTIFICATION_REPLAY_CHILD"):
        environment.pop(key, None)
    for key in ("RUSTUP_HOME", "CARGO_HOME", "COREPACK_HOME", "CODEX_AGENTS_PYTHON"):
        assert environment.get(key), f"set {key} explicitly; interpreter is used read-only"
    sandbox = target / work.name.replace("sources-", "environment-")
    for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "TMPDIR", "XDG_CACHE_HOME", "PIP_CACHE_DIR"):
        directory = sandbox / key.lower()
        directory.mkdir(parents=True)
        environment[key] = str(directory)
    environment.update(PATH=f'{environment["CARGO_HOME"]}/bin:/usr/local/bin:/usr/bin:/bin',
                       CARGO_TARGET_DIR=str(target / "cargo"), CARGO_BUILD_JOBS="2",
                       CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0",
                       CARGO_NET_OFFLINE="true", PYTHONDONTWRITEBYTECODE="1")
    targets = {name: ["--test", name + "_corpus"] for name in tables}
    targets["turn_lifecycle"] = ["--lib", "--test", "turn_lifecycle_corpus", "--test", "notification_dispatch_corpus"]
    targets["submit_gates"].append("--lib")
    targets["workspace_ready"] = ["--lib"]
    results = []

    def run(identity, tier, table, witness_filter=None):
        command = ["cargo", "nextest", "run", "--max-fail", "1:immediate", "-p", "studio-runtime-services",
                   *targets[table], "-E", args.filter or witness_filter or filters[table]]
        env = dict(environment)
        if tier == "full":
            env["STUDIO_RUNTIME_CORPUS_FULL"] = "1"
        log = target / f"{identity}-{tier}.log"
        start = time.monotonic()
        cpu = resource.getrusage(resource.RUSAGE_CHILDREN)
        with log.open("w") as output:
            result = subprocess.run(command, cwd=work, env=env, stdout=output, stderr=subprocess.STDOUT, check=False)
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        status = {0: "survived", 100: "caught"}.get(result.returncode, "invalid")
        row = dict(id=identity, table=table, tier=tier, status=status, exit=result.returncode,
                   wallSeconds=time.monotonic()-start,
                   cpuSeconds=after.ru_utime+after.ru_stime-cpu.ru_utime-cpu.ru_stime,
                   log=str(log))
        results.append(row)
        (target / "results.json").write_text(json.dumps(results, indent=2)+"\n")
        print(f"{identity} {tier}: {status} ({row['wallSeconds']:.2f}s wall, {row['cpuSeconds']:.2f}s CPU)", flush=True)
        assert status != "invalid", f"build/infrastructure failure: {log}"
        return status

    try:
        tiers = ("fast", "full") if args.full else ("fast",)
        for table in dict.fromkeys(m["table"] for m in mutations):
            for tier in tiers:
                assert run("baseline-" + table, tier, table) == "survived", "unmodified corpus must pass"
        for mutation in mutations:
            path = work / mutation["file"]
            original = path.read_text()
            changed = original
            for edit in mutation["edits"]:
                assert changed.count(edit["before"]) == 1, f"{mutation['id']}: mutation no longer applies exactly once"
                changed = changed.replace(edit["before"], edit["after"], 1)
            try:
                path.write_text(changed)
                for tier in tiers:
                    run(mutation["id"], tier, mutation["table"], mutation.get("filter"))
            finally:
                path.write_text(original)
        survivors = [r for r in results if not r["id"].startswith("baseline-") and r["status"] != "caught"]
        if survivors and not args.allow_survivors:
            raise SystemExit("surviving mutations: " + ", ".join(r["id"]+"/"+r["tier"] for r in survivors))
    finally:
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
