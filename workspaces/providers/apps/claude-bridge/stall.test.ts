import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
import { waitForFile } from "./testing/file-signals.ts";
import { startBridge as startScriptedBridge } from "./testing/bridge-harness.ts";
// Stalled tool calls through the real bridge with a scripted SDK. The fake CLI
// emits a Bash tool_use and then never returns its result, as in the
// 2026-10-17 incident. No model calls.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { bashTimeoutMs, findStall } from "./stall-watch.ts";

const GRACE_MS = 300;

// Scenario by the turn's first input text:
// stall: a Bash call that never returns; an interrupt ends the turn, then the
//   CLI runs the steers it holds as its next turn (Claude Code's interrupt
//   keeps queued messages).
// wedged: as stall, but the CLI ignores the interrupt; it records a steer whose
//   text contains "recorded" as a queued_command in its native history.
// deaf: as wedged, and closing the query does not end its stream either.
// approval: the CLI asks canUseTool first; after an allow it stalls as in
//   "stall", after a deny it reports the denial and ends the turn.
// heartbeat: the tool sends progress past its timeout, then returns.
// answer: a reply without a tool call.
// In every scenario, the tool call starts only after the test removes its
// .hold file; until then the CLI queues the steers it reads.
const FAKE_SDK = new URL("./stall.fixture.mts", import.meta.url);

const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function startBridge({ env = {}, thread = {} } = {}) {
  const bridge = await startScriptedBridge(FAKE_SDK, "claude-stall-", {
    env: {
      STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: String(GRACE_MS),
      STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "300",
      STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "100",
      ...env,
    },
    thread,
    clock: true,
  });
  bridges.push(bridge);
  return bridge;
}

const text = (value: string): import("./bridge-protocol.ts").BridgeInput[] => [
  { type: "text", text: value },
];
const completed = (bridge: BridgeHarness, turn: string) =>
  bridge.advanceUntil(
    (n): n is BridgeNoteFor<"turn/completed"> =>
      n.method === "turn/completed" && n.params.turn.id === turn,
  );
const acks = (bridge: BridgeHarness, id: string) =>
  bridge.notes.filter(
    (n): n is BridgeNoteFor<"item/completed"> =>
      n.method === "item/completed" &&
      n.params.item.type === "userMessage" &&
      n.params.item.clientId === id,
  );

// Start a scenario turn. Each steer in `steers` ([id, text] pairs) is in the
// CLI queue before its tool call starts: the stall clock starts with the tool
// call, so it cannot end the turn while a steer is still on its way.
async function start(bridge: BridgeHarness, scenario: string, steers = []) {
  const hold = path.join(bridge.root, ".hold");
  if (steers.length) await fs.writeFile(hold, "");
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-" + scenario,
      input: text(scenario),
    })
  ).turn.id;
  for (const [id, value] of steers) await steer(bridge, turn, id, value);
  if (steers.length) {
    await read(bridge, steers.at(-1)[1]);
    await fs.rm(hold);
  }
  await bridge.note(
    (n): n is BridgeNoteFor<"item/started"> =>
      n.method === "item/started" && n.params.item.id === "toolu_stall",
  );
  return turn;
}

const steer = (
  bridge: BridgeHarness,
  turn: string,
  id: string,
  value: string,
) =>
  bridge.call("turn/steer", {
    threadId: bridge.thread,
    expectedTurnId: turn,
    clientUserMessageId: id,
    input: text(value),
  });

async function inputs(bridge: BridgeHarness) {
  const file = path.join(bridge.root, ".inputs");
  return existsSync(file)
    ? (await fs.readFile(file, "utf8")).trim().split("\n")
    : [];
}

const read = (bridge: BridgeHarness, value: string) => {
  const file = path.join(bridge.root, ".inputs");
  return waitForFile(file, {
    matches: () =>
      readFileSync(file, "utf8").trim().split("\n").includes(value),
  });
};

test("a tool stall closes a CLI with unconfirmed input without resending it", async () => {
  const bridge = await startBridge();
  const turn = await start(bridge, "stall", [
    ["late-question", "question during the stall"],
  ]);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(done.params.turn.error.message, /Bash call did not return/);
  assert.equal(done.params.turn.error.data.claudeToolStall.tool, "Bash");
  assert.equal(done.params.turn.error.data.claudeToolStall.timeoutMs, 100);
  assert.equal(done.params.turn.unappliedSteers, undefined);
  const tool = bridge.notes.find(
    (n): n is BridgeNoteFor<"item/completed"> =>
      n.method === "item/completed" && n.params.item.id === "toolu_stall",
  );
  assert.equal(tool.params.item.status, "failed");
  assert.equal(tool.params.item.stalled, true);
  // The CLI cannot retain ambiguous queued input after this turn. Without
  // authoritative native history it remains unknown and is never resent.
  assert.equal(acks(bridge, "late-question").length, 0);
  assert.deepEqual((await bridge.call("thread/loaded/list", {})).data, []);
  assert.deepEqual(await inputs(bridge), [
    "stall",
    "question during the stall",
  ]);
  const notice = bridge.notes.find(
    (n): n is BridgeNoteFor<"item/completed"> =>
      n.method === "item/completed" && n.params.item.studioNotice,
  );
  assert.match(notice.params.item.text, /Studio stopped this turn/);
  assert.notEqual(notice.params.item.phase, "final_answer");
  // The bridge accepts a new turn afterwards.
  const next = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-after",
      input: text("answer"),
    })
  ).turn.id;
  assert.equal((await completed(bridge, next)).params.turn.status, "completed");
});

test("a CLI that ignores the interrupt is closed and its steers are settled from native history", async () => {
  const bridge = await startBridge();
  const turn = await start(bridge, "wedged", [
    ["steer-recorded", "recorded steer"],
    ["steer-lost", "steer the CLI dropped"],
  ]);
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(done.params.turn.error.message, /Studio stopped this turn/);
  assert.ok(existsSync(path.join(bridge.root, ".interrupts")));
  // Recorded input is delivered; unrecorded input returns to Studio once.
  assert.deepEqual(done.params.turn.unappliedSteers, ["steer-lost"]);
  assert.equal(acks(bridge, "steer-recorded").length, 1);
  assert.equal(acks(bridge, "steer-lost").length, 0);
  // A new turn starts in a fresh CLI.
  const next = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-fresh",
      input: text("answer"),
    })
  ).turn.id;
  assert.equal((await completed(bridge, next)).params.turn.status, "completed");
});

test("a CLI whose stream outlives its close still ends the stalled turn once", async () => {
  const bridge = await startBridge();
  const turn = await start(bridge, "deaf");
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(done.params.turn.error.message, /Studio stopped this turn/);
  await bridge.advanceTime(200);
  assert.equal(
    bridge.notes.filter(
      (n): n is BridgeNoteFor<"turn/completed"> =>
        n.method === "turn/completed" && n.params.turn.id === turn,
    ).length,
    1,
  );
  // The dropped query is replaced by a fresh CLI for the next turn.
  const next = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "start-replaced",
      input: text("answer"),
    })
  ).turn.id;
  assert.equal((await completed(bridge, next)).params.turn.status, "completed");
});

test("an unanswered approval is not a stall, and the tool budget restarts at the answer", async () => {
  // An agent that asks for approvals waits for the user as long as it takes.
  const bridge = await startBridge({
    env: { STUDIO_CLAUDE_UNEXPECTED_APPROVAL_WAIT_MS: "200" },
  });
  const turn = await start(bridge, "approval");
  const approval = await bridge.note(
    (n): n is BridgeNoteFor<"item/commandExecution/requestApproval"> =>
      n.method === "item/commandExecution/requestApproval",
  );
  assert.equal(approval.params.itemId, "toolu_stall");
  // Several tool budgets pass while Studio owns the wait.
  await bridge.advanceTime(100 + GRACE_MS * 4);
  assert.equal(
    bridge.notes.find(
      (n): n is BridgeNoteFor<"turn/completed"> =>
        n.method === "turn/completed",
    ),
    undefined,
  );

  bridge.answer(approval.id, { decision: "accept" });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.ok(
    done.params.turn.error.data.claudeToolStall.elapsedMs >= 100 + GRACE_MS,
  );
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".approved"), "utf8"),
    "allow",
  );
});

test("an approval nobody answers in a no-approval agent is declined and its card closed", async () => {
  const bridge = await startBridge({
    env: {
      STUDIO_CLAUDE_UNEXPECTED_APPROVAL_WAIT_MS: "400",
      // The decline restarts the tool budget. Keep the stall watchdog out of
      // this test, so a slow CLI answer to the decline cannot end the turn.
      STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "600000",
    },
    thread: { approvalPolicy: "never" },
  });
  const turn = await start(bridge, "approval");
  const approval = await bridge.note(
    (n): n is BridgeNoteFor<"item/commandExecution/requestApproval"> =>
      n.method === "item/commandExecution/requestApproval",
  );
  assert.equal(
    approval.params.reason,
    "Claude requests permission to use Bash although this agent runs without approvals: rm on a critical path",
  );
  const resolved = await bridge.advanceUntil(
    (n): n is BridgeNoteFor<"serverRequest/resolved"> =>
      n.method === "serverRequest/resolved",
  );
  assert.deepEqual(resolved.params, {
    threadId: bridge.thread,
    requestId: approval.id,
  });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".approved"), "utf8"),
    "deny",
  );
  const notice = bridge.notes.find(
    (n): n is BridgeNoteFor<"item/completed"> =>
      n.method === "item/completed" &&
      n.params.item.id === "approval-expired:toolu_stall",
  );
  assert.match(notice.params.item.text, /Studio declined it/);
  assert.equal(
    bridge.notes.find(
      (n): n is BridgeNoteFor<"item/completed"> =>
        n.method === "item/completed" && n.params.item.phase === "final_answer",
    )?.params.item.studioNotice,
    undefined,
  );
  // A late answer to the closed card changes nothing.
  bridge.answer(approval.id, { decision: "accept" });
  await bridge.advanceTime(100);
  assert.equal(
    await fs.readFile(path.join(bridge.root, ".approved"), "utf8"),
    "deny",
  );
});

test("progress events keep a tool call alive past its timeout", async () => {
  // The bridge and this fake CLI share one event loop. A pause of the whole
  // process longer than the grace looks like silence, so the grace here is
  // 20 times the 50 ms progress interval.
  const bridge = await startBridge({
    env: { STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "1000" },
  });
  const turn = await start(bridge, "heartbeat");
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "completed");
  assert.equal(done.params.turn.error, undefined);
  assert.equal(existsSync(path.join(bridge.root, ".interrupts")), false);
});

test("the stall rule needs both an elapsed budget and silence", () => {
  const limits = { graceMs: 100, silenceMs: 1_000 };
  const clocks = new Map([
    ["bash", { tool: "Bash", startedAt: 0, timeoutMs: 500 }],
  ]);
  assert.equal(findStall(clocks, 599, 0, limits), null);
  assert.equal(findStall(clocks, 700, 650, limits), null);
  assert.equal(findStall(clocks, 700, 600, limits).id, "bash");
  const silent = new Map([
    ["mcp", { tool: "mcp__x__y", startedAt: 0, timeoutMs: null }],
  ]);
  assert.equal(findStall(silent, 999, 0, limits), null);
  assert.equal(findStall(silent, 1_000, 0, limits).id, "mcp");
  assert.equal(bashTimeoutMs({}, {}), 120_000);
  assert.equal(bashTimeoutMs({ timeout: 9_999_999 }, {}), 600_000);
  assert.equal(
    bashTimeoutMs({}, { BASH_DEFAULT_TIMEOUT_MS: "900000" }),
    900_000,
  );
});

test("Stop bounds a running Claude that ignores its interrupt", async () => {
  const bridge = await startBridge({
    env: { STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "600000" },
  });
  const turn = await start(bridge, "deaf", [
    ["stop-recorded", "recorded steer"],
    ["stop-lost", "lost steer"],
  ]);
  await fs.writeFile(path.join(bridge.root, ".stop-now"), "");
  await bridge.call("turn/interrupt", {
    threadId: bridge.thread,
    turnId: turn,
  });
  const done = await completed(bridge, turn);
  assert.equal(done.params.turn.status, "interrupted");
  assert.match(
    done.params.turn.error.message,
    /did not end the turn after Stop/,
  );
  assert.deepEqual(done.params.turn.unappliedSteers, ["stop-lost"]);
  assert.equal(acks(bridge, "stop-recorded").length, 1);
  assert.equal(acks(bridge, "stop-lost").length, 0);
});
