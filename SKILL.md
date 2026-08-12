---
name: codex-agents
description: Manage and orchestrate Codex coding threads via app-server JSON-RPC. Use when the user wants to delegate work to Codex programmatically, spawn multiple Codex threads, run reviewer-augmented or manager+pairs workflows, apply goal/budget contracts, or build an orchestrator over Codex. Triggers include "delegate to codex", "spawn codex", "orchestrate codex", "codex thread", "set goal for codex", "manage codex agents", "multi-agent codex", "codex app-server", "codex SDK".
allowed-tools: Bash(codex:*), Bash(node:*), Bash(npm:*), Bash(npx:*), Bash(launchctl:*), Bash(ps:*), Bash(kill:*), Bash(rg:*), Bash(grep:*), Bash(sed:*), Bash(cat:*), Bash(ln:*), Read, Edit
---

# Codex Thread Orchestration

This skill is the project-local runbook for delegating work to Codex. The orchestrator launches and manages **threads**, not vague "agents". A thread becomes a delegated worker only after it has a goal, budget, turn, isolated worktree, and monitoring.

Use `codex app-server` as the canonical transport. Do not use acpx or plain `codex exec` for orchestration: they do not expose the full thread/goal contract.

## When to split

Use multi-thread orchestration only when all are true:

1. The work can be split into independent parallel tasks.
2. The value justifies much higher token use than a single thread.
3. The orchestrator can verify and merge outputs without relying on peer consensus.

Default patterns:

| Pattern | Use |
|---|---|
| L0 single thread | One bug, one refactor, one module, linear debugging |
| L1 implementer + reviewer | Non-trivial feature where review has real value |
| L2 manager + parallel workers | Several independent workstreams with clear merge boundaries |

Avoid peer-to-peer chatter. The orchestrator owns decomposition, goals, steering, interrupts, and merge decisions.

Ten or more independent threads is normal. The 3-4 ceiling applies only to threads that talk to each other. Three limits set the real number:

- **File ownership.** Name each thread's files in its prompt. Two threads on one file is the failure mode, not ten threads on ten files.
- **Load.** Check `uptime` before a wave and before each heavy stage. A shared build tree serialises anyway.
- **Verification.** Launch as many as you can read the diffs of.

## Always check schema first

App-server shapes change. Before writing or fixing an orchestrator, generate schemas for the installed CLI and trust those schemas over examples:

```bash
codex --version
codex app-server generate-json-schema --experimental --out /tmp/codex-schema
codex app-server generate-ts --experimental --out /tmp/codex-proto
```

If `--experimental` is not accepted or not needed in the installed version, rerun without it. Goal methods require `capabilities.experimentalApi: true` in `initialize`.

`generate-json-schema` writes `v1/` and `v2/`. Read `v2/`; `v1/` is the older protocol and its shapes disagree with the server.

Verified on `codex-cli 0.147.0`, 2026-08-09. Every line below came from that version's `v2/` schema or a live run.

- `initialize` params are `{ clientInfo, capabilities: { experimentalApi: true } }`.
- `thread/start` has no required fields. Every parameter is nullable, so a misspelled field is ignored silently. Check names against `v2/ThreadStartParams.json`.
- `thread/start.sandbox` is a string enum: `"read-only" | "workspace-write" | "danger-full-access"`.
- `thread/start.threadSource` is a free string. Nothing validates it; `"subagent"` is the convention.
- `thread/start` returns `{ thread: { id, sessionId, ... } }`. `id` and `sessionId` are the same value.
- `turn/start` requires exactly two fields: `input` and `threadId`.
- `turn/start.input` uses `{ type: "text", text: "..." }`, not `input_text`.
- `turn/start.sandboxPolicy` is an object enum, e.g. `{ type: "dangerFullAccess" }`.
- `turn/start` returns `{ turn: { id, status, ... } }`.
- `thread/list` returns `{ data, nextCursor, backwardsCursor }`.
- Goal statuses are `active`, `paused`, `blocked`, `usageLimited`, `budgetLimited`, `complete`. `thread/goal/set` still works on 0.147.0.

### Model

Use Luna at effort `max`.

```javascript
const { data } = await call("model/list", {});
const model = data.find((m) => m.displayName.includes("Luna")).id;   // gpt-5.6-luna
```

`model/list` gives each model's `id` and its `supportedReasoningEfforts`. Efforts differ per model: Luna tops out at `max`, some go to `ultra`.

Set `model` and `effort` on both `thread/start` and `turn/start`. Without `model`, the value comes from `~/.codex/config.toml`.

The model a run actually uses is in the `thread/start` response, next to `thread`:

```javascript
const started = await call("thread/start", { ... });
started.model;        // record this
started.thread.id;
```

Measured over 60 threads: Luna completed 34 against 6 failures. Every one of those 6 was the capacity refusal below, never a bad answer.

### Capacity is a refused turn, not a dead thread

```
Selected model is at capacity. Please try a different model.
```

The thread keeps its id and its whole history. Only the turn died. Counting this as a thread failure threw away 11,149 events of uncommitted work in one thread, and 1,260 and 374 in two others.

Re-issue the same input on the same thread, with a widening backoff:

```javascript
if (/at capacity/i.test(error?.message ?? "") && s.capacityRetries < 6) {
  s.capacityRetries += 1;
  setTimeout(() => void reissueTurn(s), 120_000 * s.capacityRetries);
}
```

Do not answer capacity by switching model. That costs the thread's history, and the replacement model is not measured to be better.

### Budget is a real stop

`thread/goal/set`'s `tokenBudget` is enforced. `thread/goal/get` returns `tokensUsed`, `tokenBudget` and a `status` that becomes `budgetLimited` when the first passes the second:

```
etcher-features   status=budgetLimited  used=421133  budget=400000
```

A budget-limited thread stops wherever it is. If it saved its work for the end, the work is gone. Two consequences for every prompt: tell the thread to commit each finished piece as it goes, and size the budget to the task instead of copying a round number. An implementation task in a large unfamiliar codebase, where one build eats a large share, needs far more than a measurement task.

Subscribe to `thread/goal/updated` in the launcher rather than polling later. A thread stopped by the budget keeps its last turn status forever, so a watcher reading turn events alone shows it as running indefinitely:

```javascript
if (msg.method === "thread/goal/updated") {
  const status = msg.params?.goal?.status;
  if (status === "budgetLimited" || status === "usageLimited") {
    s.turnStatus = "failed";
    s.error = `goal ${status} at ${msg.params.goal.tokensUsed} tokens`;
  } else if (status === "complete") s.turnStatus = "completed";
}
```

`thread/goal/get` still answers on demand, which is what to reach for when a thread has already gone quiet and you did not subscribe.

### Capacity is a failure mode, not a defect

A provider can refuse a model mid-run:

```
turn.error: Selected model is at capacity. Please try a different model.
```

The thread dies where it stood. Two did in one wave while a third on a different
model kept going, so this is per-model and answered by switching, not by waiting.

Read the id list from `model/list` and relaunch the same task on a peer with the
same reasoning efforts. Nothing is lost if the thread was committing as it went,
which is the third time in one session that rule paid for itself; write the
relaunch as a continuation naming what is already committed, so the new thread
does not rediscover it.

### Turn outcomes

A failed turn arrives as `turn/completed`. There is no failure notification. The reason is in `turn.error`:

```javascript
if (msg.method === "turn/completed") {
  const error = msg.params?.turn?.error;
  s.turnStatus = error ? "failed" : "completed";
  s.error = error?.message ?? null;
}
```

Read it every time. A wave that dies on a bad model id produces twenty notifications per thread and reports `turn/completed` on all of them.

An unknown model id returns `400 ... not supported when using Codex with a ChatGPT account`. The name is wrong, not the account.

### Expected startup output

Log these, do not act on them:

- `ERROR codex_models_manager::manager: failed to refresh available models` on stderr, once or twice.
- `remoteControl/status/changed` with `status: "disabled"`.
- `warning` about under-development features.

## Minimal JSON-RPC launcher

Use JSONL over stdio. There is no `Content-Length` framing.

Write it as plain `.mjs` and run `node script.mjs`. The listing is typed for readability; strip the annotations.

```typescript
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

const proc = spawn("codex", ["app-server", "--listen", "stdio://"], {
  stdio: ["pipe", "pipe", "pipe"],
});

let nextId = 1;
const pending = new Map<number, { resolve: Function; reject: Function }>();
const rl = createInterface({ input: proc.stdout });

function call(method: string, params: unknown = {}) {
  const id = nextId++;
  proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

rl.on("line", (line) => {
  const msg = JSON.parse(line);
  if (msg.id && pending.has(msg.id)) {
    const p = pending.get(msg.id)!;
    pending.delete(msg.id);
    msg.error ? p.reject(msg.error) : p.resolve(msg.result);
    return;
  }

  // Notifications to record:
  // thread/started, thread/status/changed, thread/goal/updated,
  // turn/started, turn/completed, turn/plan/updated,
  // item/agentMessage/delta, item/commandExecution/outputDelta,
  // account/rateLimits/updated
});

await call("initialize", {
  clientInfo: { name: "codex-orchestrator", version: "1" },
  capabilities: { experimentalApi: true },
});
```

## Start a worker thread

Create a separate git worktree/branch per independent worker. The thread prompt must tell the worker to stay inside that worktree and commit its completed changes on its branch.

```typescript
async function startWorker({
  cwd,
  branch,
  objective,
  prompt,
  tokenBudget = 300_000,
  model = "luna",
  effort = "max",
}) {
  const { thread } = await call("thread/start", {
    cwd,
    model,
    approvalPolicy: "never",
    sandbox: "danger-full-access",
    runtimeWorkspaceRoots: [cwd],
    threadSource: "subagent",
    developerInstructions:
      `You are an autonomous Codex coding thread. Work only in ${cwd}. ` +
      `Commit completed changes on ${branch}. Do not modify other worktrees.`,
  });

  // Verified working on 0.147.0. tokenBudget is ENFORCED, not advisory: the
  // server flips the goal to status "budgetLimited" once tokensUsed passes it
  // and the thread stops. Measured: used=421133 against budget=400000, so the
  // in-flight turn finishes and then it halts. Three threads died this way
  // with nothing committed. Set it to what the work needs, not to a round
  // number, and read it back with thread/goal/get rather than guessing.
  await call("thread/goal/set", {
    threadId: thread.id,
    objective,
    tokenBudget,
    status: "active",
  });

  const { turn } = await call("turn/start", {
    threadId: thread.id,
    model,
    effort,
    approvalPolicy: "never",
    sandboxPolicy: { type: "dangerFullAccess" },
    cwd,
    runtimeWorkspaceRoots: [cwd],
    input: [{ type: "text", text: prompt }],
  });

  return { threadId: thread.id, turnId: turn.id };
}
```

## Monitor and steer

The orchestrator should keep a small status file with thread id, turn id, branch, worktree, goal status, token usage, last event time, and final message tail.

Poll goals:

```typescript
const { goal } = await call("thread/goal/get", { threadId });
```

Interrupt a bad turn without destroying the thread/goal:

```typescript
await call("turn/interrupt", { threadId, turnId });
```

Send a follow-up steering turn:

```typescript
await call("turn/start", {
  threadId,
  effort: "medium",
  input: [{ type: "text", text: "Narrow the change to X. Do not touch Y." }],
});
```

Do not mark a worker as done from outside. Completion is only reliable when the worker commits, reports tests, and the goal status reaches `complete`, or when the orchestrator verifies the branch directly.

## Detached orchestration on macOS

A wave started as a background job of your shell dies with that shell. Three waves were lost that way in one hour, each leaving live threads with no process to drive them. Work on branches survived; the launcher did not.

Two things that do **not** work on macOS:

* `launchctl submit` exits 1.
* `setsid` does not exist, so `nohup setsid ...` fails with `setsid: No such file or directory`.

Use a plist. The job gets `ppid=1`, so it outlives every session:

```xml
<key>ProgramArguments</key>
<array>
  <string>/absolute/path/to/node</string>
  <string>/absolute/path/to/codex-swarm.mjs</string>
</array>
<key>WorkingDirectory</key><string>/absolute/path/to/tmp</string>
<key>EnvironmentVariables</key>
<dict>
  <key>PATH</key><string>/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
</dict>
<key>StandardErrorPath</key><string>/absolute/path/to/tmp/wave.err</string>
```

```bash
launchctl unload wave.plist 2>/dev/null; launchctl load wave.plist
ps -o ppid= -p "$(cat codex-swarm-launcher.pid)"   # 1 means launchd owns it
```

Set `PATH` in the plist. A launchd job inherits a minimal environment, so `node`, `codex` and worker tools such as `/Users/igor/.cargo/bin/cargo` need absolute paths or an explicit `PATH`.

Read `wave.err` after a load, and truncate it first. A stale refusal from an earlier load reads exactly like a fresh one, and sends you to debug code that already works.

## Review pattern

For reviewer-augmented work:

- Implementer and reviewer are separate threads.
- Reviewer sees spec plus artifact/diff, not the implementer's reasoning trace.
- Reviewer must produce concrete blocking concerns with file/line refs, or explicitly say `INSUFFICIENT_CONTEXT`.
- The orchestrator decides whether concerns are valid and starts a follow-up implementer turn if needed.

## Merge pattern

For parallel workers:

1. Each worker has its own worktree and branch.
2. Each worker commits before reporting completion.
3. The orchestrator inspects branch diff and test evidence.
4. The orchestrator merges/cherry-picks only coherent changes.
5. If a task became obsolete, ignore that branch instead of forcing a merge.

## Never launch a wave over live threads

One app-server hosts every thread. Killing it to start a new wave kills them all, including one mid build. That is how a thread with 10,940 events was lost with nothing committed.

The launcher must read the status file, refuse if any thread is still `running`, and name them. Keep an explicit override, never a silent one.

**A refusal must test the launcher, not the file.** `running` in the status file only means nobody wrote a terminal status. When a launcher dies, every thread it hosts stays `running` forever, and the guard then refuses every future wave on ghosts. That blocked the machine four times in one hour.

Test liveness with a pid file the launcher writes about itself:

```js
const pid = Number(readFileSync(`${TMP}/codex-swarm-launcher.pid`, "utf8").trim());
process.kill(pid, 0);   // throws if dead
```

Do not use `pgrep -f` for this. Two failures, both real:

* `pgrep -f codex-swarm.mjs` matches the shell that runs it, so the guard sees itself as another launcher. The same trap cost four hours with `pgrep -f attar-build`.
* It returned empty while the guard still refused, which sends you debugging the wrong mechanism.

When the launcher is gone, mark those threads `abandoned` in the status file. `running` is then a lie that every later tool repeats.

Watchers key on **thread id, never on name**. A new wave rewrites the status file; a watcher keyed on names then counts the new threads against the names it already reported, decides the wave is finished, and exits with the still-running threads unreported. Detect the id set changing and say so:

```
CODEX WAVE REPLACED watcher stops; 1 thread(s) unreported from the previous wave
```

## Two audiences, two files

This file is the **orchestrator's** runbook: how to launch, watch, steer, read and merge. It is read by
whoever is running the fleet.

Rules the **worker** obeys live in `prompts/worker-preamble.md`, and the launcher prepends that file to
every task prompt. Do not restate them here and do not paste them into individual tasks: the same
preamble was hand copied into every wave in one session and lost a clause each time. Change the file,
and the next wave picks it up.

What belongs there: how a worker runs long commands, committing as it goes, claiming shared resources,
what counts as done, the shape of its final message, and the feedback it owes back.

Override the path with `CODEX_PREAMBLE` when a fleet needs different worker rules.

## Steering a live thread

A thread lives inside the app-server process that created it. A second process gets `already has an active writer` from `thread/resume`, so there is no way in from outside. Without a channel, reacting to a thread's report means starting a fresh thread that re-reads every piece of context, which costs far more than the correction is worth.

Keep the launcher alive and let it carry messages. It polls a directory every ten seconds; a file named after a thread becomes a follow-up turn on that same thread, with all of its memory intact:

```
codex-inbox/<thread-name>.txt
```

The launcher deletes the file and records `{"steered": "<name>"}` in the event log. Two limits worth knowing: only waves launched with a mailbox-carrying launcher can be reached, and if the launcher dies the mailbox dies with it. A thread holder that outlives its wave is the real fix and does not exist yet.

## Push, do not poll

Arm a watcher when you launch a wave, so a finished thread announces itself. Asking "is it done yet" is the orchestrator's job to eliminate, not the human's to repeat.

The watcher polls the status file and prints one line per thread the moment it reaches a terminal state, then exits when the wave is done:

```
CODEX COMPLETED test262-unmeasured events=2837 branch=codex/test262-unmeasured
CODEX FAILED    second-app-scout   events=20 error=... branch=codex/second-app-scout
CODEX STALLED   pin-roll-cost      no events for 900s branch=codex/chromium-pin-roll
CODEX WAVE DONE 3 threads
```

Three states, not one. Silence is not success: a wave that dies on a bad model id reports `turn/completed` on every thread, and a hung thread looks exactly like a working one in the status file, so a stall timeout is part of the contract.

## Reading final reports cheaply

The orchestrator's context is the scarce resource, and app-server responses are large. `model/list` alone is about 6 KB of JSON. Filter every response in a script; never print one whole.

Three helpers cover almost everything, and the orchestrator reads only their output:

| Helper | Prints |
|---|---|
| `scripts/codex-models` | one line per model: id, default marker, supported efforts |
| `scripts/codex-report` | one line per thread: name, model, effort, event count, status, error |
| `scripts/codex-report NAME --answers` | that thread's final answer, truncated |

The four scripts in `scripts/` are working code, not illustrations: `codex-swarm.mjs` launches a wave, `codex-watch` announces completions, `codex-report` reads results under a cap, `codex-models` lists model ids. They were built and debugged over a nine-thread run. Each writes to a directory set at the top of the file; point that at your own scratch directory before the first use.

Environment they read: `CODEX_MODEL` (omit to take the config default), `CODEX_BUDGET`, `CODEX_WAVE` (names the status file so waves can run side by side), `CODEX_FORCE=1` (override the live-thread guard).

Schemas the same way: print field names from the generated JSON, not the JSON.

Never pipe a thread's stream, event log or rollout file into it. Read one status line per thread, and the final answer only, truncated. Budget it like one subagent report: a few thousand characters per thread, not a transcript.

Keep a status file with one small record per thread: name, thread id, turn id, branch, worktree, model, goal status, token usage, event count, last event time. No message bodies.

For the final answer, read the rollout rather than resuming the thread:

```bash
TID="<thread-id>"
F=$(ls -1 ~/.codex/sessions/*/*/*/rollout-*${TID}.jsonl 2>/dev/null | head -1)
grep '"role":"assistant"' "$F" | grep '"phase":"final_answer"' | tail -1 | python3 -c '
import json,sys
d=json.load(sys.stdin)
t="".join(c["text"] for c in d["payload"]["content"] if c.get("type")=="output_text")
print(t[:3000])
'
```

Two things the `phase` field decides, and both bite:

- A thread emits many assistant messages. `commentary` is thinking out loud and `final_answer` is the report. One observed run had 14 `final_answer` entries and another had 1, so take the **last** one.
- `commentary` can land after the last `final_answer`. A reporter that ignores `phase` and takes the last assistant message therefore returns thinking, not the report. Filter on the phase, then fall back to the last message of any phase, because a thread killed mid run has no `final_answer` at all and its last commentary is all it left.

Wrap this in one reporter script and read only its output. A thread that needs more than its cap to report has a prompt problem: tell it to return a table and a verdict, not a narrative.

Live control still goes through app-server.

## Give commands, not numbers

State the gate the worker must run. Do not state what it will report.

A number in a brief reads as a fact about the worker's environment. It is a fact
about yours, and the two differ constantly: a different build, a different gate
with a different threshold, an artifact that exists only in another worktree.
Four workers in one session reported that a quoted baseline did not reproduce,
each time because the artifact behind it was not theirs.

Wrong: "the baseline is 24 of 25 with one failure at 0.7330."
Right: "run tools/tests/etcher-live-screen-pixels.py and report the table; take
your own baseline before you change anything."

Where a number really is needed, name what produced it: which gate, which build,
which commit, and whether that artifact exists for them. If it does not, say so.

## Verify a script by running it

A syntax check is not a smoke test. `python3 -c "import ast; ast.parse(...)"` passes on a file whose imports are missing, and an edit that inserts `import glob` by matching the text `import json` silently does nothing when the file imports on one combined line. That watcher was declared fixed and then died on its first real event.

Run every helper once, against real state, before relying on it.

## Anti-patterns

- Using acpx as orchestration substrate.
- Launching "agents" without goals, budgets, worktrees, and status monitoring.
- More than 3-4 workers that talk to each other in one group. Independent threads are not covered by this: ten or more with disjoint file ownership is normal, see "When to split".
- Multi-threading sequential reasoning or a single tricky bug.
- Peer review that says only "looks good".
- Sharing reasoning traces between implementer and reviewer.
- Merging worker branches without direct diff/test verification.
