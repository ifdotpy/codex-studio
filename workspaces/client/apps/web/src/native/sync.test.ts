import { describe, expect, it, vi } from "vitest";
import {
  createNativeSync,
  type NativeSyncContext,
  type NativeSyncResources,
} from "./sync";
import type { NativeSubscriptionEvent } from "./client";
import type { ResourceRef } from "../sync/subscriptions/contracts";

vi.mock("../sync/client", () => {
  throw new Error("The native adapter must not load the browser sync owner.");
});

const workspace = "a".repeat(32);
const state = {
  stateDir: "saved-state",
  threads: [],
  chats: [],
  nodes: [],
  edges: [],
  runtime: {},
};
const transcript = {
  items: [{ id: "m1", role: "user", text: "saved" }],
  truncated: false,
  nextCursor: "original-boundary",
};
async function flush() {
  for (let turn = 0; turn < 8; turn++) await Promise.resolve();
}
function fixture() {
  const closeState = vi.fn(async () => {});
  let receiveState: (
    event: NativeSubscriptionEvent<unknown>,
  ) => void = () => {};
  const request = vi.fn(
    async (method: string, _params?: unknown): Promise<unknown> => {
      if (method === "subscriptions.snapshot")
        return { resource: { kind: "state" }, status: "ready" };
      if (method === "projection.stateSnapshot") return state;
      if (method === "projection.readSubscription")
        return {
          resource: { kind: "transcript", agentId: "A" },
          data: transcript,
          status: "recovery_required",
          cursor: { workspaceId: workspace, seq: 7 },
        };
      throw new Error(`Unexpected method ${method}`);
    },
  );
  const subscribe = vi.fn(
    async (
      _topic: string,
      _params: unknown,
      receive: (event: NativeSubscriptionEvent<unknown>) => void,
    ): Promise<() => Promise<void>> => {
      receiveState = receive;
      return closeState;
    },
  );
  const context = {
    workspaceId: workspace,
    request,
    subscribe,
  } as unknown as NativeSyncContext;
  let resourceState: {
    data: unknown;
    status: "live" | "stale" | "loading";
    error: Error | null;
  } = { data: undefined, status: "loading", error: null };
  const callbacks = new Set<() => void>();
  const stopResource = vi.fn();
  const resources = {
    subscribe: vi.fn((_ref: ResourceRef, accept: () => void) => {
      callbacks.add(accept);
      return () => {
        callbacks.delete(accept);
        stopResource();
      };
    }),
    observe: vi.fn((_ref: ResourceRef, accept: () => void) => {
      callbacks.add(accept);
      return () => {
        callbacks.delete(accept);
        stopResource();
      };
    }),
    getSnapshot: vi.fn(() => resourceState),
  } as unknown as NativeSyncResources;
  return {
    context,
    request,
    subscribe,
    closeState,
    resources,
    stopResource,
    state: (value: unknown) => receiveState({ value }),
    emit: (data: unknown, status: "live" | "stale" = "live") => {
      resourceState = { data, status, error: null };
      for (const callback of callbacks) callback();
    },
  };
}

describe("native sync presentation adapter", () => {
  it("reports a failed native close instead of claiming it completed", async () => {
    const owner = fixture();
    const error = new Error("The close result is unknown.");
    owner.closeState.mockRejectedValueOnce(error);
    const sync = createNativeSync(
      Promise.resolve(owner.context),
      owner.resources,
    );
    const report = vi.fn();
    const stop = sync.subscribeStateProjection(vi.fn(), report);
    await flush();
    stop();
    await expect(sync.dispose()).rejects.toThrow(
      "A native sync operation failed.",
    );
    expect(report).toHaveBeenCalledWith(error);
    expect(owner.closeState).toHaveBeenCalledTimes(1);
  });
  it("observes Rust-decoded cached state without a browser database or network read", async () => {
    const owner = fixture();
    const sync = createNativeSync(
      Promise.resolve(owner.context),
      owner.resources,
    );
    const accept = vi.fn();
    const report = vi.fn();
    const stop = sync.subscribeStateProjection(accept, report);
    await flush();
    expect(owner.subscribe).toHaveBeenCalledWith(
      "state",
      {},
      expect.any(Function),
    );
    expect(owner.request).not.toHaveBeenCalled();
    owner.state(state);
    expect(accept).toHaveBeenLastCalledWith(state, workspace);
    expect(report).toHaveBeenLastCalledWith(null);
    expect(await sync.workspaceId).toBe(workspace);
    stop();
    await flush();
    owner.state({ ...state, stateDir: "late" });
    expect(accept).toHaveBeenCalledTimes(1);
    expect(owner.closeState).toHaveBeenCalledTimes(1);
    await sync.dispose();
  });

  it("closes a late state subscription without delivering values after cancellation", async () => {
    const owner = fixture();
    let opened!: (close: () => Promise<void>) => void;
    owner.subscribe.mockImplementationOnce(
      async () =>
        new Promise<() => Promise<void>>((resolve) => {
          opened = resolve;
        }),
    );
    const sync = createNativeSync(
      Promise.resolve(owner.context),
      owner.resources,
    );
    const accept = vi.fn();
    const stop = sync.subscribeStateProjection(accept, vi.fn());
    await flush();
    stop();
    opened(owner.closeState);
    await flush();
    expect(owner.closeState).toHaveBeenCalledTimes(1);
    expect(accept).not.toHaveBeenCalled();
    await sync.dispose();
  });

  it("uses only typed transcript data and ignores a late older cached read", async () => {
    const owner = fixture();
    let cached!: (value: unknown) => void;
    owner.request.mockImplementationOnce(
      async () =>
        new Promise((resolve) => {
          cached = resolve;
        }),
    );
    const sync = createNativeSync(
      Promise.resolve(owner.context),
      owner.resources,
    );
    const accept = vi.fn();
    const ref = { kind: "transcript", agentId: "A", tail: 40 } as const;
    const stop = sync.subscribeTranscriptProjection(
      "transcript:A",
      accept,
      vi.fn(),
      false,
      ref,
    );
    await flush();
    expect(owner.resources.subscribe).toHaveBeenCalledWith(
      ref,
      expect.any(Function),
    );
    const fresh = {
      ...transcript,
      items: [{ id: "m2", role: "assistant", text: "new" }],
    };
    owner.emit(fresh);
    expect(accept).toHaveBeenLastCalledWith(fresh);
    cached({ data: transcript, status: "recovery_required" });
    await flush();
    expect(accept).toHaveBeenCalledTimes(1);
    stop();
    owner.emit(transcript);
    expect(accept).toHaveBeenCalledTimes(1);
    expect(owner.stopResource).toHaveBeenCalledTimes(1);
    await sync.dispose();
  });

  it("keeps local transcript reads local and refuses another transcript selector", async () => {
    const owner = fixture();
    const sync = createNativeSync(
      Promise.resolve(owner.context),
      owner.resources,
    );
    const accept = vi.fn();
    const stop = sync.subscribeTranscriptProjection(
      "transcript:A",
      accept,
      vi.fn(),
      true,
    );
    await flush();
    expect(owner.resources.subscribe).not.toHaveBeenCalled();
    expect(owner.resources.observe).toHaveBeenCalledWith(
      { kind: "transcript", agentId: "A" },
      expect.any(Function),
    );
    expect(accept).toHaveBeenCalledWith(transcript);
    expect(() =>
      sync.subscribeTranscriptProjection(
        "transcript:A",
        vi.fn(),
        vi.fn(),
        false,
        { kind: "transcript", agentId: "B" },
      ),
    ).toThrow(/identity/i);
    stop();
    await sync.dispose();
  });

  it("waits for a ready native snapshot before reading decoded state and never retries a failure", async () => {
    const owner = fixture();
    const sync = createNativeSync(
      Promise.resolve(owner.context),
      owner.resources,
    );
    const onWorkspaceId = vi.fn();
    expect(await sync.refreshProjection({ onWorkspaceId })).toBe(state);
    expect(owner.request.mock.calls.map(([method]) => method)).toEqual([
      "subscriptions.snapshot",
      "projection.stateSnapshot",
    ]);
    expect(owner.request).toHaveBeenNthCalledWith(1, "subscriptions.snapshot", {
      resource: { kind: "state" },
    });
    expect(onWorkspaceId).toHaveBeenCalledExactlyOnceWith(workspace);
    owner.request.mockReset();
    owner.request.mockResolvedValueOnce({
      status: "recovery_required",
      data: state,
    });
    await expect(sync.refreshProjection()).rejects.toThrow(/ready|recovery/i);
    expect(owner.request).toHaveBeenCalledTimes(1);
    await sync.dispose();
  });

  it("keeps each overlapping refresh workspace callback and checks cursor ownership", async () => {
    const owner = fixture();
    let first!: (value: unknown) => void;
    let second!: (value: unknown) => void;
    owner.request.mockImplementationOnce(
      async () =>
        new Promise((resolve) => {
          first = resolve;
        }),
    );
    owner.request.mockImplementationOnce(
      async () =>
        new Promise((resolve) => {
          second = resolve;
        }),
    );
    const sync = createNativeSync(
      Promise.resolve(owner.context),
      owner.resources,
    );
    const a = vi.fn();
    const b = vi.fn();
    const one = sync.refreshProjection({ onWorkspaceId: a });
    const two = sync.refreshProjection({ onWorkspaceId: b });
    void one.catch(() => {});
    void two.catch(() => {});
    await flush();
    second({ status: "ready" });
    await two;
    first({ status: "ready" });
    await one;
    expect(a).toHaveBeenCalledExactlyOnceWith(workspace);
    expect(b).toHaveBeenCalledExactlyOnceWith(workspace);
    expect(await sync.readSubscriptionCursor({ kind: "tasks" })).toEqual({
      workspaceId: workspace,
      seq: 7,
    });
    owner.request.mockResolvedValueOnce({
      cursor: { workspaceId: "b".repeat(32), seq: 8 },
    });
    expect(
      await sync.readSubscriptionCursor({ kind: "tasks" }),
    ).toBeUndefined();
    await sync.dispose();
  });

  it("prefetches once through the native owner and does not start cancelled or foreign requests", async () => {
    const owner = fixture();
    const sync = createNativeSync(
      Promise.resolve(owner.context),
      owner.resources,
    );
    const controller = new AbortController();
    controller.abort();
    expect(
      await sync.prefetchTranscript(workspace, "A", controller.signal),
    ).toBe(false);
    expect(await sync.prefetchTranscript("b".repeat(32), "A")).toBe(false);
    expect(owner.request).not.toHaveBeenCalled();
    expect(await sync.prefetchTranscript(workspace, "A")).toBe(true);
    expect(owner.request).toHaveBeenCalledExactlyOnceWith(
      "subscriptions.snapshot",
      { resource: { kind: "transcript", agentId: "A" } },
    );
    await sync.dispose();
  });
});
