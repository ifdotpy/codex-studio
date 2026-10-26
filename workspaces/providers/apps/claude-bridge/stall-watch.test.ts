import { afterEach, expect, test, vi } from "vitest";
import { watchStalls, type WatchedQuery, type Stall } from "./stall-watch.ts";
import { startBridge } from "./testing/bridge-harness.ts";

afterEach(() => vi.useRealTimers());

test("a no-tool stream event skips other clocks; pending-tool progress postpones its exact deadline", async () => {
  const bridge = await startBridge(
    new URL("./stall-stream.fixture.mts", import.meta.url),
    "claude-stall-stream-",
    {
      clock: true,
      env: {
        STUDIO_CLAUDE_TOOL_SILENCE_MS: "1000",
        STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "100",
      },
    },
  );
  try {
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "tool" }],
    });
    const ready = await bridge.note(
      "fixture/clockVisits",
      (n) => n.params.phase === "tool-ready",
    );
    const idle = (await bridge.call("thread/start", { cwd: bridge.root }))
      .thread.id;
    await bridge.call("turn/start", {
      threadId: idle,
      input: [{ type: "text", text: "idle" }],
    });
    await bridge.note(
      "fixture/clockVisits",
      (n) => n.params.phase === "idle-ready",
    );
    await bridge.advanceTime(400);
    await bridge.writeSignal(".emit-idle");
    const idleEvent = await bridge.note(
      "fixture/clockVisits",
      (n) => n.params.phase === "idle-event",
    );
    expect(idleEvent.params.visits).toBe(0);
    await bridge.advanceTime(100);
    await bridge.writeSignal(".emit-tool");
    const progress = await bridge.note(
      "fixture/clockVisits",
      (n) => n.params.phase === "tool-event",
    );
    expect(progress.params.visits).toBe(1);
    expect(progress.params.at).toBe(ready.params.at + 500);
    await bridge.advanceTime(999);
    expect(
      bridge.notes.some(
        (n) =>
          n.method === "fixture/clockVisits" && n.params.phase === "interrupt",
      ),
    ).toBe(false);
    await bridge.advanceTime(1);
    const interrupted = await bridge.note(
      "fixture/clockVisits",
      (n) => n.params.phase === "interrupt",
    );
    expect(interrupted.params.at).toBe(progress.params.at + 1000);
  } finally {
    await bridge.close();
  }
});

test("the watchdog stays idle and fires the nearest deadline once", () => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const queries = new Map<string, WatchedQuery>();
  const waits = new Map();
  const recover = vi.fn(
    (
      _session: WatchedQuery["session"],
      active: WatchedQuery,
      _stall: Stall,
    ) => {
      active.stall = {};
    },
  );
  const arm = watchStalls(
    queries,
    waits,
    { graceMs: 100, silenceMs: 1000 },
    recover,
  );
  arm();
  expect(vi.getTimerCount()).toBe(0);
  vi.advanceTimersByTime(10000);
  expect(vi.getTimerCount()).toBe(0);
  const active: WatchedQuery = {
    turn: { id: "turn", items: [] },
    session: { id: "session", cwd: "/fixture", turns: [] },
    lastEventAt: Date.now(),
    toolClocks: new Map([
      ["later", { startedAt: Date.now(), timeoutMs: 500 }],
      ["nearest", { startedAt: Date.now(), timeoutMs: 200 }],
    ]),
  };
  queries.set("thread", active);
  arm();
  expect(vi.getTimerCount()).toBe(1);
  vi.advanceTimersByTime(299);
  expect(recover).not.toHaveBeenCalled();
  vi.advanceTimersByTime(1);
  expect(recover).toHaveBeenCalledTimes(1);
  expect(recover.mock.calls[0]?.[2].id).toBe("nearest");
  expect(vi.getTimerCount()).toBe(0);
  vi.advanceTimersByTime(10000);
  expect(recover).toHaveBeenCalledTimes(1);
});

test("progress, results and Studio waits rearm or remove the watchdog", () => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const clock = { startedAt: 0, timeoutMs: null };
  const active: WatchedQuery = {
    turn: { id: "turn", items: [] },
    session: { id: "session", cwd: "/fixture", turns: [] },
    lastEventAt: 0,
    toolClocks: new Map([["tool", clock]]),
  };
  const queries = new Map([["thread", active]]);
  const waits = new Map();
  const recover = vi.fn(
    (_session: WatchedQuery["session"], query: WatchedQuery, _stall: Stall) => {
      query.stall = {};
    },
  );
  const arm = watchStalls(
    queries,
    waits,
    { graceMs: 100, silenceMs: 1000 },
    recover,
  );
  arm();
  vi.advanceTimersByTime(900);
  active.lastEventAt = Date.now();
  arm();
  vi.advanceTimersByTime(999);
  expect(recover).not.toHaveBeenCalled();
  waits.set("thread", 1);
  arm();
  expect(vi.getTimerCount()).toBe(0);
  vi.advanceTimersByTime(10000);
  waits.delete("thread");
  active.lastEventAt = clock.startedAt = Date.now();
  arm();
  expect(vi.getTimerCount()).toBe(1);
  active.toolClocks.clear();
  arm();
  expect(vi.getTimerCount()).toBe(0);
  active.toolClocks.set("tool", clock);
  arm();
  active.turn = null;
  arm();
  expect(vi.getTimerCount()).toBe(0);
  active.turn = { id: "turn", items: [] };
  arm();
  queries.clear();
  arm();
  expect(vi.getTimerCount()).toBe(0);
});

test("one timer follows the nearest query and preserves an unchanged deadline", () => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const makeActive = (timeoutMs: number): WatchedQuery => ({
    turn: { id: "turn", items: [] },
    session: { id: "session", cwd: "/fixture", turns: [] },
    lastEventAt: 0,
    toolClocks: new Map([["tool", { startedAt: 0, timeoutMs }]]),
  });
  const later = makeActive(500);
  const nearest = makeActive(200);
  const queries = new Map([
    ["later", later],
    ["nearest", nearest],
  ]);
  const recover = vi.fn((_session, active) => {
    active.stall = {};
  });
  const arm = watchStalls(
    queries,
    new Map(),
    { graceMs: 100, silenceMs: 1000 },
    recover,
  );
  const schedule = vi.spyOn(globalThis, "setTimeout");
  arm();
  arm();
  expect(schedule).toHaveBeenCalledTimes(1);
  vi.advanceTimersByTime(300);
  expect(recover).toHaveBeenCalledTimes(1);
  expect(recover.mock.calls[0]?.[1]).toBe(nearest);
  expect(vi.getTimerCount()).toBe(1);
  vi.advanceTimersByTime(299);
  expect(recover).toHaveBeenCalledTimes(1);
  vi.advanceTimersByTime(1);
  expect(recover).toHaveBeenCalledTimes(2);
  expect(recover.mock.calls[1]?.[1]).toBe(later);
  expect(vi.getTimerCount()).toBe(0);
  schedule.mockRestore();
});
