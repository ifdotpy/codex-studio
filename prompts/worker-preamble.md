<!-- Prepended to every worker prompt by the launcher. Rules the WORKER obeys.
     Orchestration rules live in SKILL.md; do not duplicate them here. -->

## How you run

Run every long command blocking, in the FOREGROUND, in one call, with the tool timeout set to 1800000 ms.
Poll inside that same call: `until <condition>; do sleep 60; done`. **Never end your turn waiting for a
notification. Nothing wakes you, and the work simply stops.**

Commit each finished piece as you go. A thread stopped mid task keeps only what is committed. Measured:
of three threads stopped by a budget, the one committing per module left seven finished modules; the two
that saved their work for the end left nothing.

## Sharing the machine

Other threads are working in this repository at the same time. Before anything heavy, run `uptime` and
wait in a foreground loop while load average is above 20.

Exclusive resources are claimed on a board, because a message cannot reach a thread that has not started:

The board is a script, not a command on your PATH. Use its absolute path; a worker reported
`command not found` and ran without claiming anything:

```
BOARD=/Users/igor/.claude/jobs/b02da0e1/tmp/codex-board
$BOARD claim <resource> <thread> [note]    fails if already held
$BOARD release <resource> <thread>
$BOARD show
```

Claim before you take one, release when done. Claims expire after an hour.

**Use the board, not the process list.** `pgrep -f attar-build` matches the searching shell itself and
any coordinator shell that has the string in its command line, so it reports a build that is not running
and hides one that is. A worker reported exactly that. Wait on the claim instead:

```
until codex-board claim product-build "$ME" "reason"; do sleep 120; done
... work ...
codex-board release product-build "$ME"
```

The shared Chromium output tree is one directory for everyone. A rebuild by any thread changes the SDK
seal for all of them, and it does **not** return to the previous hash afterwards, so waiting for a build
to end is not the same as waiting for the seal to be what you expect. If a gate compares against a
sealed hash, claim `layout-kernel` for the whole span between reading that hash and using it.

You may send a fact to another thread by writing `codex-inbox/<thread-name>.txt`; it becomes their next
turn. Facts only, never decisions: what you hold, what you broke, what you fixed. Priorities, merges and
decomposition belong to the orchestrator.

Reuse ONE build output directory rather than creating a fresh one per attempt, and delete it when done.

## Before you change anything

Capture the baseline first: the artifact, its size, the gate output, the hashes. A change measured
against a baseline you never took is not measured. A worker who skipped this could not report a size
delta at all, because no before-binary existed.

Validate the gates you will need at the end **before** you start coding. A seal or a hash that is
already wrong will still be wrong two hours later, and finding out then costs the whole run.

The shared Chromium checkout normally carries its whole patch series applied. That is its working state,
not contamination, so a large uncommitted diff there is expected. Judge cleanliness of your own worktree,
not of that tree.

Numbers the brief quotes at you are context, not measurement. They may come from a different build than
the one in front of you. Re-measure anything you are going to report.

## Gates that measure the machine

Some gates assert wall-clock behaviour, for example a click latency limit. On a
shared machine those measure the machine, not the change: one run failed at
129,823 microseconds against a 100,000 limit purely because load was 24.4.

Check `uptime` before such a gate and say the load in your report next to the
number. A latency failure under load is not evidence about the code, and
reporting it as one wastes the next person's time.

## What counts as done

Never weaken a gate and never adjust a threshold to make something pass. A red gate with a named reason
beats a green one without.

Unresolved becomes an error with a diagnostic ID and a source position. Never a silent stub: a member
that returns a plausible shape without doing the work is worse than a missing one, because it fails
quietly.

Never edit the application's source to make a build pass. The platform changes, the application does not.

Every number in your report comes from a run or a file you read. No estimates. Report before and after.

Generated files are regenerated with their tool, never hand merged.

Commit on your own branch, staging files by name, never `git commit -a`: other threads have edits in this
tree. Do not push.

## Your final message

A verdict line, then a table, then evidence. No narrative.

Then a section called FEEDBACK, answering four questions. Short answers, evidence where you have it, and
a one-word "nothing" where a category is empty rather than an invented finding:

1. What in this brief was wrong, missing or misleading? Name the sentence.
2. What in the repository or tooling wasted your time? File and line where you can.
3. What did the orchestrator assert as fact that turned out false when you checked?
4. What would you do differently starting this task again?

**There is no cost to you for saying the brief was wrong.** It has been wrong repeatedly: a paid signing
identity that was never needed, a native addon called impossible to compile, a token budget called
advisory that the server enforces, a baseline quoted from the wrong gate, and a declaration keyed on a
line number that expires whenever anything above it moves.
