# Parallel agent backend benchmark

Run from the repository root:

```sh
python3 -B scripts/benchmarks/parallel_agents/benchmark.py --agents 5 15 --output /tmp/codex-parallel.json
python3 -B -m unittest discover -s scripts/benchmarks/parallel_agents -p 'test_*.py'
```

Each case runs in a child process with a 90 second limit. It uses a new temporary
state directory and a queued fake app-server. The production `Runtime` handles
all notifications, stream flushes, analytics, monitor completions, and dispatch passes.
The fixture creates 1,000 agent records. Five or 15 independent leads have an
active turn. The archived records belong to the first lead's team.
The rest have archived work records with saved native names. No native provider
or user database is used.

The fixture uses the normal scheduler to start each fake-native turn. It then
stops that scheduler and calls the production dispatch pass once per second
during the measured window. The `notifications` case uses nonwaking successful
monitor exits. The `wakes` case delivers each monitor exit as new input. The fake
app-server starts and completes one fixed answer turn per agent, with a cap on
new starts.
The measured window starts before the first fixed-rate callback offer. It ends
after the callback queue drains and all buffered text is saved. Setup, validation,
and cleanup are outside the window. The process CPU clock includes all threads.
Peak resident set size is the child process high-water mark, so it also includes
setup. Lock share is the sum of outermost `Runtime.lock` holds divided by measured
wall time. The fake callback queue depth and admission-to-completion latency
describe the fixture lane, not a live app-server queue. The report checks one
usage receipt and monitor completion per active agent, all answer items, and every
callback. Missing work fails the case.

The default offer rate is 120 notifications per second for the whole case.
Each agent gets three answer items with 16 text deltas each, eight command output
deltas, one token usage notice, and one completed turn. Compare the same agent
count, rate, and host before and after a source change. This fixture does not
measure the browser, actual model work, or live provider transport.
