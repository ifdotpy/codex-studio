import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createSubscriptionStore } from "./store";
import type { ResourceVersion } from "../resourceEvents";

function fixture() {
  const removed = vi.fn();
  let deliver: (version?: ResourceVersion) => void = () => {};
  const add = vi.fn((_ref, accept) => {
    deliver = accept;
    return removed;
  });
  const store = createSubscriptionStore({
    watchResourceChanges: add,
    watchResourceConnection: (accept) => {
      accept("live", "open");
      return () => {};
    },
  });
  return {
    store,
    add,
    removed,
    deliver: (version: ResourceVersion) => deliver(version),
  };
}
const resource = { kind: "transcript", agentId: "visible-chat" } as const;

describe("screen subscription ownership", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("keeps one add and no removals through repeated unmount/remount within grace", () => {
    const { store, add, removed } = fixture();
    for (let cycle = 0; cycle < 10; cycle++) {
      const stop = store.watchResourceChanges(resource, () => {});
      stop();
      vi.advanceTimersByTime(100);
    }
    expect(add).toHaveBeenCalledTimes(1);
    expect(removed).not.toHaveBeenCalled();
    vi.advanceTimersByTime(20_000);
    expect(removed).toHaveBeenCalledTimes(1);
    store.dispose();
  });

  it("shares the reference across components and releases after the final grace", () => {
    const { store, add, removed } = fixture();
    const one = store.watchResourceChanges(resource, () => {}, { graceMs: 50 });
    const two = store.watchResourceChanges(
      { agentId: "visible-chat", kind: "transcript" },
      () => {},
      { graceMs: 50 },
    );
    expect(add).toHaveBeenCalledTimes(1);
    one();
    vi.advanceTimersByTime(100);
    expect(removed).not.toHaveBeenCalled();
    two();
    two();
    vi.advanceTimersByTime(49);
    expect(removed).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(removed).toHaveBeenCalledTimes(1);
    store.dispose();
  });

  it("marks data stale when the final screen leaves and on lost connection", () => {
    const { store, deliver } = fixture();
    const stop = store.watchResourceChanges(resource, () => {});
    deliver({
      epoch: "epoch",
      revision: 1,
      reason: "initial",
      cursor: { workspaceId: "workspace", seq: 1 },
      replyKind: "snapshot",
    });
    expect(store.getSnapshot(resource).status).toBe("live");
    stop();
    expect(store.getSnapshot(resource).status).toBe("stale");
    vi.advanceTimersByTime(20_000);
    expect(store.getSnapshot(resource).status).toBe("stale");
    store.dispose();
  });

  it("keeps a revision-only recovery hint stale until an applied baseline", () => {
    const { store, deliver } = fixture();
    const stop = store.watchResourceChanges(resource, () => {});
    deliver({ epoch: "epoch", revision: 1, reason: "initial" });
    expect(store.getSnapshot(resource).status).not.toBe("live");
    deliver({
      epoch: "epoch",
      revision: 1,
      reason: "initial",
      cursor: { workspaceId: "workspace", seq: 1 },
      replyKind: "delta",
    });
    expect(store.getSnapshot(resource).status).toBe("live");
    stop();
    store.dispose();
  });

  it("explicit zero grace releases immediately and disposal clears every source", () => {
    const { store, removed } = fixture();
    const stop = store.watchResourceChanges(resource, () => {}, { graceMs: 0 });
    stop();
    expect(removed).toHaveBeenCalledTimes(1);
    store.dispose();
    vi.runAllTimers();
    expect(removed).toHaveBeenCalledTimes(1);
  });
});
