import { describe, expect, it, vi } from "vitest";
import { createNativeSubscriptions } from "./subscriptions";
import type { NativeSubscriptionEvent } from "./client";

describe("native subscription presentation", () => {
  it("does not confirm cached data or revision hints as a live baseline", async () => {
    let receive!: (event: NativeSubscriptionEvent<unknown>) => void;
    const stop = vi.fn(async () => {});
    const context = {
      subscribe: vi.fn(async (_topic, _params, callback) => {
        receive = callback;
        return stop;
      }),
    };
    const resources = createNativeSubscriptions(Promise.resolve(context));
    const ref = { kind: "state" } as const;
    const changed = vi.fn();
    const unsubscribe = resources.subscribe(ref, changed);
    await Promise.resolve();
    await Promise.resolve();
    expect(context.subscribe).toHaveBeenCalledWith("subscriptions", {
      resources: [ref], tokenRates: false,
    }, expect.any(Function));
    receive({ value: { type: "resource", snapshot: { resource: ref, data: ["cached"], status: "recovery_required" } } });
    expect(resources.getSnapshot(ref)).toEqual({ data: ["cached"], status: "stale", error: null });
    receive({ value: { type: "resource", snapshot: { resource: ref, data: ["committed"], status: "ready", version: { epoch: "one", revision: 1 } } } });
    expect(resources.getSnapshot(ref)).toEqual({ data: ["committed"], status: "live", error: null });
    receive({ value: { type: "connection", status: "degraded", stream: "connecting" } });
    expect(resources.getSnapshot(ref).status).toBe("stale");
    unsubscribe();
    await resources.dispose();
    expect(stop).toHaveBeenCalledTimes(1);
  });

  it("closes a late native subscription and ignores late values after disposal", async () => {
    let opened!: (stop: () => Promise<void>) => void;
    let receive!: (event: NativeSubscriptionEvent<unknown>) => void;
    const stop = vi.fn(async () => {});
    const context = { subscribe: vi.fn((_topic, _params, callback) => {
      receive = callback;
      return new Promise<() => Promise<void>>((resolve) => { opened = resolve; });
    }) };
    const resources = createNativeSubscriptions(Promise.resolve(context));
    const changed = vi.fn();
    resources.subscribe({ kind: "state" }, changed);
    await Promise.resolve();
    const closed = resources.dispose();
    const calls = changed.mock.calls.length;
    receive({ value: { type: "resource", snapshot: { resource: { kind: "state" }, data: [], status: "ready" } } });
    opened(stop);
    await closed;
    expect(changed).toHaveBeenCalledTimes(calls);
    expect(stop).toHaveBeenCalledTimes(1);
  });
});
