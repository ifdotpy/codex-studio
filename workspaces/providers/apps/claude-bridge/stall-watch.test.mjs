import { afterEach, expect, test, vi } from "vitest";
import { watchStalls } from "./stall-watch.mjs";

afterEach(() => vi.useRealTimers());

test("the watchdog stays idle and fires the nearest deadline once", () => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const queries = new Map();
  const waits = new Map();
  const recover = vi.fn((_session, active) => {
    active.stall = {};
  });
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
  const active = {
    turn: {},
    session: {},
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
  expect(recover.mock.calls[0][2].id).toBe("nearest");
  expect(vi.getTimerCount()).toBe(0);
  vi.advanceTimersByTime(10000);
  expect(recover).toHaveBeenCalledTimes(1);
});

test("progress, results and Studio waits rearm or remove the watchdog", () => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const clock = { startedAt: 0, timeoutMs: null };
  const active = {
    turn: {},
    session: {},
    lastEventAt: 0,
    toolClocks: new Map([["tool", clock]]),
  };
  const queries = new Map([["thread", active]]);
  const waits = new Map();
  const recover = vi.fn((_session, query) => {
    query.stall = {};
  });
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
  active.turn = {};
  arm();
  queries.clear();
  arm();
  expect(vi.getTimerCount()).toBe(0);
});

test("one timer follows the nearest query and preserves an unchanged deadline", () => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const makeActive = (timeoutMs) => ({
    turn: {},
    session: {},
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
  expect(recover.mock.calls[0][1]).toBe(nearest);
  expect(vi.getTimerCount()).toBe(1);
  vi.advanceTimersByTime(299);
  expect(recover).toHaveBeenCalledTimes(1);
  vi.advanceTimersByTime(1);
  expect(recover).toHaveBeenCalledTimes(2);
  expect(recover.mock.calls[1][1]).toBe(later);
  expect(vi.getTimerCount()).toBe(0);
  schedule.mockRestore();
});

test("synthetic turns share the silence deadline and respect Studio waits and progress", () => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const active = {
    turn: { id: "continuation", status: "inProgress", synthetic: true },
    session: {},
    lastEventAt: 0,
    toolClocks: new Map(),
  };
  const waits = new Map([["thread", 1]]);
  const recover = vi.fn((_session, query) => {
    query.stall = {};
  });
  const arm = watchStalls(
    new Map([["thread", active]]),
    waits,
    { graceMs: 100, silenceMs: 1000 },
    recover,
  );
  arm();
  expect(vi.getTimerCount()).toBe(0);
  vi.advanceTimersByTime(10000);
  waits.delete("thread");
  active.lastEventAt = Date.now();
  arm();
  vi.advanceTimersByTime(900);
  active.lastEventAt = Date.now();
  arm();
  vi.advanceTimersByTime(999);
  expect(recover).not.toHaveBeenCalled();
  vi.advanceTimersByTime(1);
  expect(recover).toHaveBeenCalledTimes(1);
  expect(recover.mock.calls[0][2]).toMatchObject({
    synthetic: true,
    silentMs: 1000,
  });
  expect(vi.getTimerCount()).toBe(0);
  active.stall = null;
  active.turn.status = "completed";
  arm();
  expect(vi.getTimerCount()).toBe(0);
});
