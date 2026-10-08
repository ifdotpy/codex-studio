import { afterEach, describe, expect, it, vi } from "vitest";
import { ResourceEntityDelivery } from "./resourceEntityDelivery";
import type { ResourceChangeEvent } from "./resourceEvents";

function frame(
  revision: number,
  reason: ResourceChangeEvent["reason"] = "change",
) {
  return {
    protocol: 3,
    workspaceId: "a".repeat(32),
    epoch: "one",
    revision,
    reason,
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision,
        entityChanges: {
          after: revision - 1,
          through: revision,
          documents: [
            {
              id: "entity:agent:one",
              seq: revision,
              payload: "{}",
              _deleted: false,
            },
          ],
        },
      },
    ],
  } satisfies ResourceChangeEvent;
}

describe("bounded stream entity delivery", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("persists in order before listeners can request a pull", async () => {
    let finish!: () => void;
    const first = new Promise<void>((resolve) => {
      finish = resolve;
    });
    const persist = vi
      .fn()
      .mockReturnValueOnce(first)
      .mockResolvedValue(undefined);
    const dispatch = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch);
    delivery.receive(frame(1));
    delivery.receive(frame(2));
    expect(persist).toHaveBeenCalledTimes(1);
    expect(dispatch).not.toHaveBeenCalled();
    finish();
    await vi.waitFor(() => expect(dispatch).toHaveBeenCalledTimes(2));
    expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual([
      1, 2,
    ]);
  });

  it.each(["initial", "reconnect", "overflow", "workspace"] as const)(
    "%s fences an unfinished write and requests reconciliation immediately",
    async (reason) => {
      let finish!: () => void;
      let current!: () => boolean;
      const persist = vi.fn((_event, isCurrent) => {
        current = isCurrent;
        return new Promise<void>((resolve) => {
          finish = resolve;
        });
      });
      const dispatch = vi.fn();
      const delivery = new ResourceEntityDelivery(persist, dispatch);
      delivery.receive(frame(1));
      delivery.receive(frame(2));
      delivery.receive(frame(3, reason));
      expect(current()).toBe(false);
      expect(dispatch).toHaveBeenCalledWith(frame(3, reason));
      finish();
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(persist).toHaveBeenCalledTimes(1);
      expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual([
        1, 2, 3,
      ]);
    },
  );

  it("falls back on a new epoch without applying its data over an old baseline", () => {
    const persist = vi.fn().mockResolvedValue(undefined);
    const dispatch = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch);
    delivery.receive(frame(0, "initial"));
    const changed = { ...frame(1), epoch: "two" };
    delivery.receive(changed);
    expect(persist).not.toHaveBeenCalled();
    expect(dispatch).toHaveBeenLastCalledWith(changed);
  });

  it("bounds queued frames while the storage is slow", async () => {
    let finish!: () => void;
    let current!: () => boolean;
    const persist = vi.fn((_event, isCurrent) => {
      current = isCurrent;
      return new Promise<void>((resolve) => {
        finish = resolve;
      });
    });
    const dispatch = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch);
    for (let revision = 1; revision <= 10; revision++)
      delivery.receive(frame(revision));
    expect(persist).toHaveBeenCalledTimes(1);
    expect(current()).toBe(false);
    expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual(
      Array.from({ length: 10 }, (_, index) => index + 1),
    );
    finish();
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(persist).toHaveBeenCalledTimes(1);
  });

  it("keeps state delivery when a transcript-only frame arrives", async () => {
    let finish!: () => void;
    const persist = vi
      .fn()
      .mockReturnValueOnce(
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
      )
      .mockResolvedValue(undefined);
    const dispatch = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch);
    delivery.receive(frame(0, "initial"));
    dispatch.mockClear();
    delivery.receive(frame(1));
    delivery.receive(frame(2));
    const transcript: ResourceChangeEvent = {
      ...frame(3),
      resources: [{ kind: "transcript", agentId: "other" }],
      resourceVersions: [{ revision: 3 }],
    };
    delivery.receive(transcript);
    expect(dispatch).toHaveBeenCalledExactlyOnceWith(transcript);
    finish();
    await vi.waitFor(() => expect(dispatch).toHaveBeenCalledTimes(3));
    expect(persist.mock.calls.map(([event]) => event.revision)).toEqual([1, 2]);
  });

  it("expires a stuck write and keeps its ordinary invalidation", async () => {
    vi.useFakeTimers();
    vi.spyOn(console, "error").mockImplementation(() => {});
    let current!: () => boolean;
    const persist = vi.fn((_event, isCurrent) => {
      current = isCurrent;
      return new Promise<void>(() => {});
    });
    const dispatch = vi.fn();
    new ResourceEntityDelivery(persist, dispatch).receive(frame(1));
    await vi.advanceTimersByTimeAsync(1000);
    expect(current()).toBe(false);
    expect(dispatch).toHaveBeenCalledExactlyOnceWith(frame(1));
  });

  it("retains invalidation if storage rejects the batch", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const dispatch = vi.fn();
    new ResourceEntityDelivery(
      vi.fn().mockRejectedValue(new Error("storage failed")),
      dispatch,
    ).receive(frame(1));
    await vi.waitFor(() =>
      expect(dispatch).toHaveBeenCalledExactlyOnceWith(frame(1)),
    );
  });

  it("does not accumulate unresolved writes after a storage timeout", async () => {
    vi.useFakeTimers();
    vi.spyOn(console, "error").mockImplementation(() => {});
    let finish!: () => void;
    const persist = vi
      .fn()
      .mockReturnValueOnce(
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
      )
      .mockResolvedValue(undefined);
    const dispatch = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch);
    delivery.receive(frame(1));
    await vi.advanceTimersByTimeAsync(1000);
    for (let revision = 2; revision <= 20; revision++) {
      delivery.receive(frame(revision));
      await vi.advanceTimersByTimeAsync(1000);
    }
    expect(persist).toHaveBeenCalledTimes(1);
    expect(dispatch).toHaveBeenCalledTimes(20);
    finish();
    await vi.advanceTimersByTimeAsync(0);
    delivery.receive(frame(21));
    await vi.advanceTimersByTimeAsync(0);
    expect(persist).toHaveBeenCalledTimes(2);
  });
});
