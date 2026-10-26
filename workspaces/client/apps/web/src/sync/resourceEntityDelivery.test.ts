import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  MAX_PENDING_RESOURCE_FRAMES,
  ResourceEntityDelivery,
} from "./resourceEntityDelivery";
import type { ResourceChangeEvent } from "./resourceEvents";

function frame(
  revision: number,
  reason: ResourceChangeEvent["reason"] = "change",
) {
  return {
    protocol: 4,
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
              op: "upsert",
              collection: "agent",
              id: "one",
              seq: revision,
              value: { id: "one" },
            },
          ],
        },
      },
    ],
  } satisfies ResourceChangeEvent;
}

describe("bounded stream entity delivery", () => {
  it("starts the write deadline only after a slow transfer prerequisite", async () => {
    const prerequisite = Promise.withResolvers<void>();
    const persist = vi.fn().mockResolvedValue(undefined);
    const dispatch = vi.fn();
    const failed = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
    delivery.receive(frame(1), prerequisite.promise);
    await vi.advanceTimersByTimeAsync(1500);
    expect(persist).not.toHaveBeenCalled();
    expect(dispatch).not.toHaveBeenCalled();
    expect(failed).not.toHaveBeenCalled();
    prerequisite.resolve();
    await vi.advanceTimersByTimeAsync(0);
    expect(dispatch).toHaveBeenCalledExactlyOnceWith(frame(1));
    expect(failed).not.toHaveBeenCalled();
  });
  it.each(["closed", "rejected"])(
    "does not block the next frame after a prerequisite is %s",
    async (cause) => {
      const prerequisite = Promise.withResolvers<void>();
      const persist = vi.fn().mockResolvedValue(undefined);
      const dispatch = vi.fn();
      const failed = vi.fn();
      const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
      delivery.receive(frame(1), prerequisite.promise);
      if (cause === "closed") delivery.invalidate();
      else
        prerequisite.reject(new Error("transfer recovery already requested"));
      delivery.receive(frame(2));
      await vi.advanceTimersByTimeAsync(0);
      expect(dispatch).toHaveBeenCalledExactlyOnceWith(frame(2));
      expect(failed).not.toHaveBeenCalled();
    },
  );
  it.each(["overflow", "state hint"])(
    "applies an ordered %s while a typed frame is persisting",
    async (kind) => {
      let finish!: () => void;
      const persist = vi
        .fn()
        .mockImplementationOnce(
          () =>
            new Promise<void>((resolve) => {
              finish = resolve;
            }),
        )
        .mockResolvedValue(undefined);
      const dispatch = vi.fn();
      const failed = vi.fn();
      const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
      const first = frame(1);
      const next: ResourceChangeEvent =
        kind === "overflow"
          ? frame(2, "overflow")
          : { ...frame(2), resourceVersions: [{ revision: 2 }] };
      delivery.receive(first);
      delivery.receive(next);
      expect(failed).not.toHaveBeenCalled();
      expect(dispatch).not.toHaveBeenCalled();
      finish();
      await vi.advanceTimersByTimeAsync(0);
      expect(persist).toHaveBeenCalledTimes(2);
      expect(dispatch.mock.calls.map(([event]) => event)).toEqual([
        first,
        next,
      ]);
      expect(failed).not.toHaveBeenCalled();
    },
  );
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it.each(["initial", "change", "reconnect", "overflow", "workspace"] as const)(
    "keeps %s frames and hints in arrival order across epochs",
    async (reason) => {
      let finish!: () => void;
      const persist = vi
        .fn()
        .mockImplementationOnce(
          () =>
            new Promise<void>((resolve) => {
              finish = resolve;
            }),
        )
        .mockResolvedValue(undefined);
      const dispatch = vi.fn();
      const failed = vi.fn();
      const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
      delivery.receive(frame(1));
      delivery.receive({
        ...frame(2, reason),
        epoch: "different",
        resourceVersions: [{ revision: 2 }],
      });
      delivery.receive(frame(3, reason));
      expect(persist).toHaveBeenCalledOnce();
      expect(dispatch).not.toHaveBeenCalled();
      finish();
      await vi.advanceTimersByTimeAsync(0);
      expect(persist.mock.calls.map(([event]) => event.revision)).toEqual([
        1, 2, 3,
      ]);
      expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual([
        1, 2, 3,
      ]);
      expect(failed).not.toHaveBeenCalled();
    },
  );

  it("queues normal subscription bursts beyond the old eight-frame limit", async () => {
    let finish!: () => void;
    const persist = vi
      .fn()
      .mockImplementationOnce(
        () =>
          new Promise<void>((resolve) => {
            finish = resolve;
          }),
      )
      .mockResolvedValue(undefined);
    const dispatch = vi.fn();
    const failed = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
    for (let revision = 1; revision <= 256; revision++)
      delivery.receive(frame(revision));
    finish();
    await vi.advanceTimersByTimeAsync(0);
    expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual(
      Array.from({ length: 256 }, (_, index) => index + 1),
    );
    expect(failed).not.toHaveBeenCalled();
  });

  it.each(["rejection", "deadline", "capacity"])(
    "stops on persistence %s and requests recovery once for all affected resources",
    async (cause) => {
      let reject!: (error: Error) => void;
      let current!: () => boolean;
      const persist = vi.fn((_event, isCurrent) => {
        current = isCurrent;
        return new Promise<void>((_resolve, fail) => {
          reject = fail;
        });
      });
      const dispatch = vi.fn();
      const failed = vi.fn();
      const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
      delivery.receive(frame(1));
      delivery.receive({
        ...frame(2),
        resources: [{ kind: "work", agentId: "lead" }],
      });
      if (cause === "rejection") reject(new Error("base mismatch"));
      else if (cause === "deadline") await vi.advanceTimersByTimeAsync(1000);
      else
        for (let i = 0; i < MAX_PENDING_RESOURCE_FRAMES; i++)
          delivery.receive(frame(i + 3));
      await vi.advanceTimersByTimeAsync(0);
      expect(current()).toBe(false);
      expect(failed).toHaveBeenCalledExactlyOnceWith(
        expect.objectContaining({
          resources: [{ kind: "state" }, { kind: "work", agentId: "lead" }],
        }),
        expect.any(Error),
      );
      delivery.receive(frame(2000));
      expect(failed).toHaveBeenCalledOnce();
      expect(persist).toHaveBeenCalledOnce();
      expect(dispatch).not.toHaveBeenCalled();
    },
  );

  it("silently drops a closed source and ignores its late rejection", async () => {
    let reject!: (error: Error) => void;
    let current!: () => boolean;
    const persist = vi
      .fn()
      .mockImplementationOnce((_event, isCurrent) => {
        current = isCurrent;
        return new Promise<void>((_resolve, fail) => {
          reject = fail;
        });
      })
      .mockResolvedValue(undefined);
    const dispatch = vi.fn();
    const failed = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
    delivery.receive(frame(1));
    delivery.receive(frame(2));
    delivery.invalidate();
    expect(current()).toBe(false);
    delivery.receive(frame(3, "reconnect"));
    reject(new Error("closed source"));
    await vi.advanceTimersByTimeAsync(0);
    expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual([3]);
    expect(failed).not.toHaveBeenCalled();
  });

  it.each([false, undefined])(
    "continues after failure when recovery is not handled (%s)",
    async (result) => {
      const persist = vi
        .fn()
        .mockRejectedValueOnce(new Error("storage failed"))
        .mockResolvedValue(undefined);
      const dispatch = vi.fn();
      const failed = result === false ? vi.fn(() => false) : undefined;
      const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
      delivery.receive(frame(1));
      await vi.advanceTimersByTimeAsync(0);
      delivery.receive(frame(2));
      await vi.advanceTimersByTimeAsync(0);
      expect(persist).toHaveBeenCalledTimes(2);
      expect(dispatch).toHaveBeenCalledExactlyOnceWith(frame(2));
    },
  );

  it.each([false, true])(
    "strips removed subscriptions, cancelling only touched writes (mixed=%s)",
    async (mixed) => {
      const removed = { kind: "work", agentId: "lead" } as const;
      let finish!: () => void;
      let current!: () => boolean;
      const persist = vi
        .fn()
        .mockImplementationOnce((_event, isCurrent) => {
          current = isCurrent;
          return new Promise<void>((resolve) => {
            finish = resolve;
          });
        })
        .mockResolvedValue(undefined);
      const dispatch = vi.fn();
      const failed = vi.fn();
      const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
      const first: ResourceChangeEvent = frame(1);
      if (mixed) {
        first.resources.push(removed);
        first.resourceVersions.push({ resource: removed, revision: 1 });
      }
      delivery.receive(first);
      delivery.receive({ ...frame(2), resources: [removed] });
      delivery.receive(frame(3));
      delivery.remove([removed]);
      expect(current()).toBe(!mixed);
      finish();
      await vi.advanceTimersByTimeAsync(0);
      expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual([
        1, 3,
      ]);
      for (const [event] of dispatch.mock.calls)
        expect(event.resources).toEqual([{ kind: "state" }]);
      expect(failed).not.toHaveBeenCalled();
    },
  );
});
