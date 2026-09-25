# Where Studio diverges from mature harnesses: synthesis

Date: 2026-09-25. Inputs: five reports in this folder, each pinned to source.

- [Codex](2026-09-25-harness-codex-mechanisms.md): `openai/codex` at `1bf73324`.
- [T3 Code](2026-09-25-harness-t3code-mechanisms.md): `pingdotgg/t3code` at `d06f0ff1`.
- [Claw Code (Rust Claude Code)](2026-09-25-harness-claude-rust-mechanisms.md): `ultraworkers/claw-code` at `08106b0c`.
- [OpenCode](2026-09-25-harness-opencode-mechanisms.md): `anomalyco/opencode` at `adee738d`.
- [Studio anomaly audit](2026-09-25-studio-anomaly-audit.md): inside inventory, origins, read-only measurements.

## The decisive fact

Native Codex already owns input admission. Core has one place that decides whether
submitted input starts a turn, steers the active turn, or is rejected
([turn_input.rs:1-12](https://github.com/openai/codex/blob/1bf73324cadc72a53ed467edc7d3fd2b145a6166/codex-rs/core/src/session/turn_input.rs#L1-L12)).
Its modes include `StartOrSteer`: "Start a regular turn when idle, otherwise steer the
active regular turn"
([turn_input.rs:133-143](https://github.com/openai/codex/blob/1bf73324cadc72a53ed467edc7d3fd2b145a6166/codex-rs/protocol/src/turn_input.rs#L133-L143)).
App-server `turn/start` uses that behavior: its `turn_trigger` field is "ignored when this
request steers an already-active turn"
([turn.rs:167-181](https://github.com/openai/codex/blob/1bf73324cadc72a53ed467edc7d3fd2b145a6166/codex-rs/app-server-protocol/src/protocol/v2/turn.rs#L167-L181)),
and `clientUserMessageId` gives each input a stable identity. T3 Code relies on this:
it sends every follow-up with `turn/start` and lets Codex decide
([CodexAdapter.ts:2521-2558](https://github.com/pingdotgg/t3code/blob/d06f0ff1048d42a156824765316c5a2dc54f5bca/apps/server/src/provider/Layers/CodexAdapter.ts#L2521-L2558)).
OpenCode reaches the same result differently: input is written to session history and the
run loop picks it up at the next step boundary.

Studio instead decides admission itself, from a lagging copy of native turn state, and holds
input in its own queue between turns. Most of the mechanisms that users experience as delays,
crossed messages and stuck agents follow from that one decision.

## Root decisions, in order of impact

All five reports agree on the first four.

1. **Studio holds input between turns and decides start versus steer.**
   Consequences: messages reach a busy agent only after its turn (20 minute delays measured on
   2026-09-25), crossed conversations, three delivery modes (`queue`, `steer`, `after_tool`),
   steer requeue on "no active turn", the long-queue notice, child result gating, and the
   new live-steer path. Native `turn/start` (`StartOrSteer`) makes this admission atomic
   inside the provider.
2. **Studio mirrors native turn state (`inFlight`, `turnId`) as a second authority.**
   The copy lags because callbacks pass through one dispatcher queue. Consequences: stale turn
   probes, "different turn" rejections, uncertain steers, context-repair waits, interrupt
   answers of "no active turn". T3 and OpenCode keep live execution state in one owner and
   project it for display.
3. **One global runtime lock and one callback dispatcher for all agents.** Consequences:
   minutes of callback delay under load, shedding and coalescing, admission stalls, and
   several performance patches this week. Codex uses per-thread state and channels; OpenCode
   uses a per-session runner.
4. **Recovery and retries are separate state machines** (capacity retry, usage resume,
   notSubmitted and uncertain receipts, live steer attempts, context repair, browser
   recovery). Each has its own identities and timers. Reports propose one typed operation
   lifecycle with one retry policy and provider-specific evidence adapters.
5. **Hot fields live inside JSON records.** Measured decode costs drove caches, expression
   indexes and projections. Columns for identity and status fields would remove them.
6. **Orchestration outside the native agent.** Justified for mixed providers, accounts, task
   review and worktrees. Native Codex multi-agent (mailbox, `wait_agent`) is simpler for
   Codex-only teams; worth evaluating later, not first.
7. **Claude bridge emulates the Codex protocol.** Workable, but each Codex semantic must be
   reimplemented (this week: interrupt, thread names, compact, limits). The bridge should
   implement `turn/start` as start-or-steer too.

## What Studio does better and should keep

Durable receipts with explicit uncertainty, no replay of unknown mutations, user-visible
recovery, cross-provider teams, task review, per-worker worktrees, durable monitors that
outlive a turn. The reports rate these as justified or stronger than the targets.

## Plan

Each phase is independently useful and keeps the receipt guarantees.

1. **Native delivery.** Every input (user, agent message, child result, monitor result)
   goes to the provider at once with `turn/start` and its stable `clientUserMessageId`;
   the provider starts or steers. The Studio queue becomes an outbox that only holds input
   while the agent is stopped, moving accounts, under context repair, or waiting for a
   concurrency slot to start a new turn. The Claude bridge gains the same start-or-steer
   behavior. Remove afterwards: delivery modes, steer requeue, live-steer attempts, the
   long-queue notice, and child result gating (verify each is dead first).
2. **Native state as authority.** Use the `turn/start` answer (started or steered, turn id)
   and native notifications as the only source of turn truth; keep `inFlight`/`turnId` as a
   display projection. Remove stale-turn repair paths that this makes unreachable.
3. **Split the global lock** by account and agent with short shared transactions for
   cross-agent admission; bound per-callback work.
4. **One operation lifecycle** for receipts and retries with typed causes.
5. **Columns for hot fields**, with migration and dual read.

Measure before and after each phase: delivery latency from send to model visibility,
callback queue delay, lock hold share, count of uncertain events, and lines of recovery code.
