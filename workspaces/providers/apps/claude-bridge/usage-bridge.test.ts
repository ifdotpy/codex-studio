// Real bridge protocol with SDK-declared fixtures; no model requests.
import { afterEach, expect, test } from "vitest";
import { once } from "node:events";
import fs from "node:fs/promises";
import path from "node:path";
import { waitForFile } from "./testing/file-signals.ts";
import { startBridge } from "./testing/bridge-harness.ts";
const bridges: Awaited<ReturnType<typeof startBridge>>[] = [];
afterEach(async () => {
  for (const b of bridges.splice(0)) await b.close();
});
const sdk = new URL("./usage-bridge.fixture.mts", import.meta.url);
async function setup(thread = {}, env = {}) {
  const b = await startBridge(sdk, "claude-usage-", { thread, env });
  bridges.push(b);
  return b;
}
async function start(b: Awaited<ReturnType<typeof startBridge>>, text: string) {
  return (
    await b.call("turn/start", {
      threadId: b.thread,
      input: [{ type: "text", text }],
    })
  ).turn.id;
}
async function done(b: Awaited<ReturnType<typeof startBridge>>, id: string) {
  await b.note((n) => n.method === "turn/completed" && n.params.turn.id === id);
}
function observations(b: Awaited<ReturnType<typeof startBridge>>) {
  return b.notes
    .filter((n) => n.method === "thread/tokenUsage/updated")
    .map((n) => n.params.studioUsage);
}
test("final stream usage is emitted before turn completion and replaces provisional assistant usage", async () => {
  const b = await setup();
  const id = await start(b, "hold-final");
  await b.note((n) => n.params?.studioUsage?.kind === "response");
  expect(b.notes.some((n) => n.method === "turn/completed")).toBe(false);
  const response = observations(b).filter((o) => o?.kind === "response");
  expect(response).toHaveLength(1);
  expect(response[0]).toMatchObject({
    responseId: "response-1",
    tokens: {
      uncachedInput: 12,
      cacheReadInput: 32,
      cacheWriteInput: 42,
      output: 720,
      reasoningOutput: 20,
    },
  });
  await b.writeSignal(".finish");
  await done(b, id);
  const usageIndex = b.notes.findIndex(
    (n) => n.params?.studioUsage?.kind === "response",
  );
  expect(usageIndex).toBeLessThan(
    b.notes.findIndex((n) => n.method === "turn/completed"),
  );
});
test("every result is published before a steer echo can discard a deferred result", async () => {
  const b = await setup();
  const id = await start(b, "steer");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.call("turn/steer", {
    threadId: b.thread,
    expectedTurnId: id,
    clientUserMessageId: "steer-1",
    input: [{ type: "text", text: "continue" }],
  });
  await b.note(
    (n) =>
      n.method === "item/completed" && n.params.item.clientId === "steer-1",
  );
  expect(observations(b).filter((o) => o?.kind === "cumulative")).toHaveLength(
    1,
  );
  await b.writeSignal(".finish");
  await done(b, id);
  expect(
    observations(b)
      .filter((o) => o?.kind === "cumulative")
      .map((o) => o.total.output),
  ).toEqual([720, 1440]);
});
test("equal per-turn results preserve cumulative totals and result-only turns have no stale response id", async () => {
  const b = await setup();
  await done(b, await start(b, "normal"));
  const before = b.notes.length;
  await done(b, await start(b, "result-only"));
  const notes = b.notes
    .slice(before)
    .filter((n) => n.method === "thread/tokenUsage/updated");
  expect(notes).toHaveLength(1);
  expect(notes[0].params.studioUsage).toMatchObject({
    kind: "cumulative",
    total: { output: 720 },
  });
  expect(notes[0].params.responseId).toBeUndefined();
  expect(notes[0].params.studioUsage.responseId).toBeUndefined();
});
for (const scenario of ["interrupted", "query-close"])
  test(scenario + " emits a gap rather than zero usage", async () => {
    const b = await setup();
    await done(b, await start(b, scenario));
    expect(observations(b)).toContainEqual(
      expect.objectContaining({
        kind: "gap",
        responseId: "response-1",
        reason:
          scenario === "query-close" ? "queryClosed" : "streamInterrupted",
      }),
    );
    expect(observations(b).some((o) => o?.kind === "response")).toBe(false);
  });
test("zeroed crash result does not publish an observation", async () => {
  const b = await setup();
  await done(b, await start(b, "zero"));
  expect(observations(b)).toEqual([]);
});
test("subagent stream usage retains its parent tool identity", async () => {
  const b = await setup();
  await done(b, await start(b, "subagent"));
  expect(observations(b)).toContainEqual(
    expect.objectContaining({
      kind: "response",
      parentToolUseId: "tool-1",
      tokens: expect.objectContaining({ output: 720 }),
    }),
  );
});

test("background subagent final usage survives completion of its owning Studio turn", async () => {
  const b = await setup();
  const turn = await start(b, "background");
  await done(b, turn);
  await b.writeSignal(".finish");
  await b.note(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.text === "Subagent: background-finished",
  );
  expect(observations(b).filter((o) => o.kind === "response")).toMatchObject([
    {
      responseId: "response-1",
      turnId: turn,
      parentToolUseId: "background-tool",
      tokens: { output: 720 },
    },
  ]);
  expect(observations(b).filter((o) => o.kind === "gap")).toEqual([]);
});
test("response usage includes the known model context window before result", async () => {
  const b = await setup({ model: "claude-opus-5-5", contextWindow: 200000 });
  await done(b, await start(b, "result-only"));
  const turn = await start(b, "normal");
  await done(b, turn);
  expect(observations(b).find((o) => o.kind === "response")).toHaveProperty(
    "modelContextWindow",
    200000,
  );
});

test("emitted notifications match the shared Rust dispatch golden fixture", async () => {
  const golden = JSON.parse(
    await fs.readFile(
      new URL(
        "../../../runtime/packages/runtime-schema/src/fixtures/claude-usage-notifications.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const b = await setup({ model: "claude-opus-5-5", contextWindow: 200000 });
  await done(b, await start(b, "result-only"));
  const before = b.notes.length;
  await done(b, await start(b, "normal"));
  await done(b, await start(b, "interrupted"));
  const notes = b.notes
    .slice(before)
    .filter((n) => n.method === "thread/tokenUsage/updated");
  const selected = [
    notes.find((n) => n.params.studioUsage.kind === "response"),
    notes.find((n) => n.params.studioUsage.kind === "cumulative"),
    notes.find((n) => n.params.studioUsage.kind === "gap"),
  ];
  const normalized = selected.map((n) => ({
    ...n.params,
    threadId: "thread",
    turnId: "turn",
    studioUsage: { ...n.params.studioUsage, turnId: "turn" },
  }));
  expect(normalized).toEqual(golden);
});

test("background response starts retain the tool originating turn across Studio turns", async () => {
  const b = await setup();
  const owner = await start(b, "background-owner");
  await done(b, owner);
  const next = await start(b, "background-next");
  await done(b, next);
  const note = b.notes.find((n) => n.params?.studioUsage?.kind === "response");
  expect(note.params.studioUsage).toMatchObject({
    turnId: owner,
    parentToolUseId: "background-tool",
  });
  expect(note.params.turnId).toBe(owner);
});
test("shutdown waits for delayed SDK close and flushes one unfinished response gap", async () => {
  const b = await setup();
  const turn = await start(b, "shutdown");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  expect(observations(b)).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "queryClosed",
    },
  ]);
});
test("response context windows follow their own model rather than the query maximum", async () => {
  const b = await setup();
  await done(b, await start(b, "windows"));
  await done(b, await start(b, "normal"));
  expect(observations(b).find((o) => o.kind === "response")).toHaveProperty(
    "modelContextWindow",
    200000,
  );
});
test("turn interrupt reports unfinished usage as a gap rather than zero tokens", async () => {
  const b = await setup();
  const turn = await start(b, "interrupt");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.call("turn/interrupt", { threadId: b.thread, turnId: turn });
  await done(b, turn);
  expect(observations(b)).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "streamInterrupted",
    },
  ]);
});

test("shutdown bounds a stuck SDK consumer and still reports its gap once", async () => {
  const b = await setup({}, { STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "50" });
  const turn = await start(b, "shutdown-stuck");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  expect(observations(b)).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "queryClosed",
    },
  ]);
});
test("an unknown response model never borrows another model context window", async () => {
  const b = await setup({ model: "other-model", contextWindow: 1000000 });
  await done(b, await start(b, "normal"));
  expect(observations(b).find((o) => o.kind === "response")).not.toHaveProperty(
    "modelContextWindow",
  );
});

test("virtual-clock harness advances the bounded shutdown of a stuck SDK", async () => {
  const b = await startBridge(sdk, "claude-usage-clock-", {
    clock: true,
    env: { STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "50" },
  });
  bridges.push(b);
  const turn = await start(b, "shutdown-stuck");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  expect(observations(b)).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "queryClosed",
    },
  ]);
});

test("discarding a stuck query drains unfinished subagent usage exactly once", async () => {
  const b = await startBridge(sdk, "claude-usage-discard-", {
    clock: true,
    env: {
      STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "50",
      STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "50",
    },
  });
  bridges.push(b);
  const turn = await start(b, "discard-stuck");
  await b.note(
    (n) =>
      n.method === "item/completed" &&
      n.params.item.text === "Subagent: child-started",
  );
  await b.call("turn/interrupt", { threadId: b.thread, turnId: turn });
  await b.advanceUntil(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  expect(observations(b).filter((o) => o.responseId === "response-1")).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: turn,
      reason: "queryClosed",
    },
  ]);
  await b.close();
  expect(
    observations(b).filter((o) => o.responseId === "response-1"),
  ).toHaveLength(1);
});

test("restart never seeds a response from a persisted query-wide maximum window", async () => {
  const b = await setup({ model: "claude-opus-5-5" });
  await done(b, await start(b, "windows"));
  await b.restart();
  await done(b, await start(b, "normal"));
  expect(observations(b).find((o) => o.kind === "response")).not.toHaveProperty(
    "modelContextWindow",
  );
});

test("an exact model window wins over a later pricing alias with a larger context", async () => {
  const b = await setup();
  await done(b, await start(b, "windows-alias"));
  await done(b, await start(b, "normal"));
  expect(observations(b).find((o) => o.kind === "response")).toHaveProperty(
    "modelContextWindow",
    200000,
  );
});

test("shutdown never spends the full CLI exit timeout on a stuck consumer", async () => {
  const b = await startBridge(sdk, "claude-usage-shutdown-budget-", {
    clock: true,
    env: { STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "10000" },
  });
  bridges.push(b);
  await start(b, "shutdown-timing");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  const timing = await b.note((n) => n.method === "fixture/shutdownTiming");
  expect(timing.params.durationMs).toBeLessThanOrEqual(2000);
});

test("scripted Rust SDK close ends an aborted wait without inventing a successful response", async () => {
  const fixture = await fs.readFile(
    new URL(
      "../../../runtime/apps/runtime-server/tests/fixtures/claude-sdk.mjs",
      import.meta.url,
    ),
    "utf8",
  );
  const b = await startBridge(fixture, "claude-usage-rust-close-");
  bridges.push(b);
  await start(b, "wait");
  await waitForFile(path.join(b.root, ".sdk-inputs"));
  await b.close();
  expect(
    b.notes
      .filter((n) => n.method === "turn/completed")
      .some((n) => n.params.turn.status === "completed"),
  ).toBe(false);
  expect(b.notes.some((n) => n.params?.item?.text === "Visible answer")).toBe(
    false,
  );
});

test("intentional shutdown preserves an unfinished turn for reconciliation instead of publishing failure", async () => {
  const b = await setup();
  await start(b, "shutdown");
  await b.note((n) => n.method === "provider/generationStarted");
  await b.close();
  expect(b.notes.filter((n) => n.method === "turn/completed")).toEqual([]);
  expect(observations(b)).toMatchObject([
    { kind: "gap", reason: "queryClosed" },
  ]);
});

for (const scenario of [
  "shutdown-error",
  "shutdown-persist",
  "shutdown-deferred",
])
  test(
    scenario + " cannot finalize an intentionally closing turn",
    async () => {
      const b = await startBridge(sdk, "claude-usage-finalization-admission-", {
        clock: true,
      });
      bridges.push(b);
      const turn = await start(b, scenario);
      await b.note((n) => n.method === "provider/generationStarted");
      if (scenario === "shutdown-persist")
        await b.note((n) => n.method === "fixture/persistFailureSettled");
      if (scenario === "shutdown-deferred") {
        await b.call("turn/steer", {
          threadId: b.thread,
          expectedTurnId: turn,
          clientUserMessageId: "unread-steer",
          input: [{ type: "text", text: "continue" }],
        });
        await b.writeSignal(".finish");
        await b.note((n) => n.params?.studioUsage?.kind === "cumulative");
        await b.advanceTime(2000);
      }
      await b.close();
      expect(b.notes.filter((n) => n.method === "turn/completed")).toEqual([]);
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toEqual([
        {
          kind: "gap",
          responseId: "response-1",
          model: "claude-opus-5-5",
          turnId: turn,
          reason: "queryClosed",
        },
      ]);
    },
  );

test("harness close reaps a signal-killed bridge and repeats safely", async () => {
  const b = await startBridge(sdk, "claude-usage-harness-signal-", {
    clock: true,
  });
  bridges.push(b);
  const child = b.process;
  const exited = once(child, "exit");
  child.kill("SIGKILL");
  await exited;
  await b.close();
  await b.close();
});

test("harness restart drives a virtual clock until the bridge exits", async () => {
  const b = await startBridge(sdk, "claude-usage-harness-restart-", {
    clock: true,
  });
  bridges.push(b);
  await start(b, "shutdown-deferred");
  await b.note((n) => n.method === "provider/generationStarted");
  const first = b.process;
  await b.restart();
  expect(first.exitCode !== null || first.signalCode !== null).toBe(true);
  expect(b.process).not.toBe(first);
});

test("harness close reaps a bridge whose virtual clock cannot be driven", async () => {
  const b = await startBridge(sdk, "claude-usage-harness-disconnect-", {
    clock: true,
  });
  bridges.push(b);
  await start(b, "shutdown-deferred");
  await b.note((n) => n.method === "provider/generationStarted");
  const child = b.process;
  child.disconnect();
  await b.close();
  expect(child.exitCode !== null || child.signalCode !== null).toBe(true);
});

test("unexpected SDK end still publishes one failed completion", async () => {
  const b = await setup();
  const turn = await start(b, "query-close");
  await done(b, turn);
  expect(b.notes.filter((n) => n.method === "turn/completed")).toMatchObject([
    {
      params: {
        turn: {
          id: turn,
          status: "failed",
          error: { message: "Claude ended without a completed turn" },
        },
      },
    },
  ]);
});

for (const method of ["thread/resume", "claude/settings", "thread/unsubscribe"])
  test(
    method +
      " drains unfinished background usage before removing a stuck query",
    async () => {
      const b = await startBridge(sdk, "claude-usage-removal-", {
        clock: true,
      });
      bridges.push(b);
      const turn = await start(b, "background-stuck");
      await done(b, turn);
      expect(observations(b).some((o) => o.kind === "gap")).toBe(false);
      await b.call(method, { threadId: b.thread, settings: {} });
      expect((await b.call("thread/loaded/list", {})).data).toEqual([]);
      const expected = [
        {
          kind: "gap",
          responseId: "response-1",
          model: "claude-opus-5-5",
          turnId: turn,
          reason: "queryClosed",
        },
      ];
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toEqual(expected);
      await b.close();
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toEqual(expected);
    },
  );

for (const scenario of ["shutdown-watchdog", "shutdown-watchdog-inflight"])
  test(
    scenario + " cannot recover a stalled tool during the shutdown drain",
    async () => {
      const b = await startBridge(sdk, "claude-shutdown-watchdog-", {
        clock: true,
        env: {
          STUDIO_CLAUDE_TOOL_SILENCE_MS: "100",
          STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "100",
        },
      });
      bridges.push(b);
      const turn = await start(b, scenario);
      await b.note(
        (n) => n.method === "item/started" && n.params.item.id === "watch-tool",
      );
      if (scenario === "shutdown-watchdog-inflight") {
        await b.advanceTime(100);
        await b.note((n) => n.method === "fixture/stallSaveBlocked");
      }
      const before = b.notes.length;
      await b.close();
      expect(b.notes.some((n) => n.method === "fixture/interrupt")).toBe(false);
      expect(
        b.notes
          .slice(before)
          .filter(
            (n) =>
              n.method === "item/completed" &&
              (n.params.item.stalled || n.params.item.studioNotice),
          ),
      ).toEqual([]);
      expect(b.notes.filter((n) => n.method === "turn/completed")).toEqual([]);
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toEqual([
        {
          kind: "gap",
          responseId: "response-1",
          model: "claude-opus-5-5",
          turnId: turn,
          reason: "queryClosed",
        },
      ]);
    },
  );

for (const scenario of [
  "null-stop",
  "absent-stop",
  "no-stream",
  "no-stream-then-stream",
])
  test(
    scenario + " preserves finality at the bridge notification boundary",
    async () => {
      const b = await setup();
      await done(b, await start(b, scenario));
      const accounting = observations(b).filter((o) => o.kind !== "cumulative");
      expect(accounting).toMatchObject([
        {
          kind: "gap",
          responseId: "response-1",
          reason: scenario.startsWith("no-stream")
            ? "noStream"
            : "unconfirmedStop",
        },
        ...(scenario === "no-stream-then-stream"
          ? [
              {
                kind: "response",
                responseId: "response-1",
                tokens: { output: 720 },
              },
            ]
          : []),
      ]);
      expect(accounting).toHaveLength(
        scenario === "no-stream-then-stream" ? 2 : 1,
      );
    },
  );

for (const scenario of ["no-stream-then-result", "no-stream-then-close"])
  test(
    scenario +
      " preserves the first gap reason when its later stream never completes",
    async () => {
      const b = await setup();
      await done(b, await start(b, scenario));
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toMatchObject([
        { kind: "gap", responseId: "response-1", reason: "noStream" },
      ]);
      expect(
        observations(b).filter((o) => o.responseId === "response-1"),
      ).toHaveLength(1);
    },
  );

test("a deduplicated unfinished stream still permits a later final response for its identity", async () => {
  const b = await setup();
  await done(b, await start(b, "no-stream-then-result"));
  await done(b, await start(b, "normal"));
  expect(
    observations(b).filter((o) => o.responseId === "response-1"),
  ).toMatchObject([
    { kind: "gap", responseId: "response-1", reason: "noStream" },
    { kind: "response", responseId: "response-1", tokens: { output: 720 } },
  ]);
  expect(
    observations(b).filter((o) => o.responseId === "response-1"),
  ).toHaveLength(2);
});
