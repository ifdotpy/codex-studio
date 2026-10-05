# Runtime agent scope benchmark

Measures five production paths with 1,500 synthetic agents in 30 teams of 50:
deleting one 50-agent tree, disconnecting an account's 50 agents, checking team
capacity, updating team storage settings, and reading one 50-agent parentId
subtree. The subtree row compares the scoped read with the base roster scan.
Each measurement uses a fresh
SQLite state directory under `TMPDIR`. No provider is connected. Setup is
outside the timed region. Wall time covers the public runtime method or capacity
query; lock hold is the `Runtime.lock` total attributed during that operation
when `CODEX_RUNTIME_LOCK_METRICS=1` is set.

Run this entry point from a source checkout. `--runtime-source` selects another
checkout's `scripts/` modules, allowing a baseline and candidate to use the same
fixture and timing code. For the baseline use `ca123f652de64c84129a04d9b39874e3499cef92`.
Keep the two state roots separate and compare on the same machine.

```sh
export TMPDIR=/home/alex/.cache/claude-scratch/runtime-agent-scope
export CODEX_RUNTIME_LOCK_METRICS=1
python scripts/benchmarks/runtime_agent_scope/benchmark.py --runtime-source /path/to/checkout
```

This is one local experiment, not a performance guarantee. It reports every
scenario as JSON and checks the delete result contains exactly 50 agents.
