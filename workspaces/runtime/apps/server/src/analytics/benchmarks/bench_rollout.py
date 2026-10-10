"""Benchmark production rollout translation on synthetic, ordered records."""
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from analytics.rollout_parser import rollout_actions

FIXTURE = Path(__file__).parent / "fixtures" / "rollout.jsonl"
FALLBACK_AT = 1_788_220_800


def replay(records):
    """One complete replay, including fresh context and output allocation."""
    context = {"threadId": "bench-thread"}
    actions = []
    for index, record in enumerate(records):
        actions.extend(rollout_actions(record, context, str(index), FALLBACK_AT))
    return actions, context


def decode_and_replay(lines):
    return replay(json.loads(line) for line in lines)


def check_fixture(lines, records):
    expected = replay(records)
    if decode_and_replay(lines) != expected or replay(records) != expected:
        raise ValueError("Fixture replay is not repeatable")
    actions, context = expected
    if len(actions) != 9 or context.get("turnId") != "bench-turn":
        raise ValueError("Fixture did not exercise the expected action sequence")
    usage = [body for _, method, body, _ in actions if method == "thread/tokenUsage/updated"]
    if len(usage) != 2 or any(body.get("responseId") != "bench-response" for body in usage):
        raise ValueError("Fixture lost the request/notice response association")
    if actions[-1][1] != "turn/completed" or actions[-1][2]["turn"]["status"] != "completed":
        raise ValueError("Fixture did not complete its turn")


def main():
    # File reads and fixture checks are deliberately outside the timed calls.
    lines = FIXTURE.read_bytes().splitlines()
    records = [json.loads(line) for line in lines]
    check_fixture(lines, records)
    if sys.argv[1:] == ["--check"]:
        print(f"OK: {len(records)} synthetic records, 9 actions, repeatable replay")
        return
    try:
        import pyperf
    except ImportError:
        raise SystemExit("Install benchmarks/requirements.txt in a separate venv; see analytics/README.md")
    runner = pyperf.Runner()
    runner.metadata["fixture_records"] = len(records)
    runner.metadata["fixture_bytes"] = FIXTURE.stat().st_size
    runner.bench_func("rollout_actions_batch", replay, records)
    runner.bench_func("rollout_json_decode_and_actions_batch", decode_and_replay, lines)


if __name__ == "__main__":
    main()
