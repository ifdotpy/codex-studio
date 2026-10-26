import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
<<<<<<< head
  PERSIST_DEADLINE_MS,
  MAX_PENDING_RESOURCE_FRAMES,
||||||| base
import { ResourceEntityDelivery } from "./resourceEntityDelivery";
=======
  MAX_PENDING_FRAMES,
  PERSIST_DEADLINE_MS,
>>>>>>> 686377a
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
<<<<<<< head
    delivery.receive(frame(1), prerequisite.promise);
    await vi.advanceTimersByTimeAsync(1500);
    expect(persist).not.toHaveBeenCalled();
||||||| base
    delivery.receive(frame(1));
    await vi.advanceTimersByTimeAsync(1000);
    expect(failed).toHaveBeenCalledOnce();
    expect(dispatch).not.toHaveBeenCalled();
    delivery.receive(frame(2));
    expect(failed).toHaveBeenCalledTimes(2);
    expect(persist).toHaveBeenCalledOnce();
=======
    delivery.receive(frame(1));
    await vi.advanceTimersByTimeAsync(PERSIST_DEADLINE_MS);
    expect(failed).toHaveBeenCalledOnce();
    expect(dispatch).not.toHaveBeenCalled();
    delivery.receive(frame(2));
    expect(failed).toHaveBeenCalledTimes(2);
    expect(persist).toHaveBeenCalledOnce();
>>>>>>> 686377a
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
<<<<<<< head
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
||||||| base
      const resource = {
        kind: "terminal-output",
        terminalId: "new-owner",
      } as const;
      const snapshot = (
        revision: number,
        epoch: string,
      ): ResourceChangeEvent => ({
        ...frame(revision, "initial"),
        epoch,
        resources: [resource],
        resourceVersions: [
          {
            revision,
            cursor: { workspaceId: "a".repeat(32), seq: revision },
            replyKind: "snapshot",
            viewSnapshot: {
              terminalOutput: {
                text: "new",
                offset: revision,
                status: "running",
                truncated: false,
              },
            },
          },
        ],
      });
      delivery.receive(snapshot(10, "old-owner"));
      delivery.beginOwnership();
      dispatch.mockClear();
      delivery.receive(snapshot(20, "new-owner"));
      if (cause === "reject") {
        reject(new Error("old cancelled write failed"));
        await vi.advanceTimersByTimeAsync(0);
        expect(failed).not.toHaveBeenCalled();
      } else {
        await vi.advanceTimersByTimeAsync(1000);
        expect(failed).toHaveBeenCalledOnce();
        expect(dispatch).not.toHaveBeenCalled();
        expect(delivery.requiresSnapshot(resource)).toBe(true);
        resolve();
        await vi.advanceTimersByTimeAsync(0);
        delivery.receive(snapshot(21, "new-owner"));
        await vi.advanceTimersByTimeAsync(0);
      }
      expect(delivery.requiresSnapshot(resource)).toBe(false);
      const latest = dispatch.mock.calls.at(-1)![0] as ResourceChangeEvent;
      expect(latest.epoch).toBe("new-owner");
      expect(latest.resourceVersions[0].cursor).toBeDefined();
      await vi.advanceTimersByTimeAsync(2000);
      expect(delivery.requiresSnapshot(resource)).toBe(false);
=======
      const resource = {
        kind: "terminal-output",
        terminalId: "new-owner",
      } as const;
      const snapshot = (
        revision: number,
        epoch: string,
      ): ResourceChangeEvent => ({
        ...frame(revision, "initial"),
        epoch,
        resources: [resource],
        resourceVersions: [
          {
            revision,
            cursor: { workspaceId: "a".repeat(32), seq: revision },
            replyKind: "snapshot",
            viewSnapshot: {
              terminalOutput: {
                text: "new",
                offset: revision,
                status: "running",
                truncated: false,
              },
            },
          },
        ],
      });
      delivery.receive(snapshot(10, "old-owner"));
      delivery.beginOwnership();
      dispatch.mockClear();
      delivery.receive(snapshot(20, "new-owner"));
      if (cause === "reject") {
        reject(new Error("old cancelled write failed"));
        await vi.advanceTimersByTimeAsync(0);
        expect(failed).not.toHaveBeenCalled();
      } else {
        await vi.advanceTimersByTimeAsync(PERSIST_DEADLINE_MS);
        expect(failed).toHaveBeenCalledOnce();
        expect(dispatch).not.toHaveBeenCalled();
        expect(delivery.requiresSnapshot(resource)).toBe(true);
        resolve();
        await vi.advanceTimersByTimeAsync(0);
        delivery.receive(snapshot(21, "new-owner"));
        await vi.advanceTimersByTimeAsync(0);
      }
      expect(delivery.requiresSnapshot(resource)).toBe(false);
      const latest = dispatch.mock.calls.at(-1)![0] as ResourceChangeEvent;
      expect(latest.epoch).toBe("new-owner");
      expect(latest.resourceVersions[0].cursor).toBeDefined();
      await vi.advanceTimersByTimeAsync(2000);
      expect(delivery.requiresSnapshot(resource)).toBe(false);
>>>>>>> 686377a
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

<<<<<<< head
  it("queues normal subscription bursts beyond the old eight-frame limit", async () => {
||||||| base
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
    await vi.advanceTimersByTimeAsync(0);
    expect(persist).toHaveBeenCalledTimes(1);
  });

  it("keeps state delivery when a transcript-only frame arrives", async () => {
=======
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
    for (let revision = 1; revision <= MAX_PENDING_FRAMES + 2; revision++)
      delivery.receive(frame(revision));
    expect(persist).toHaveBeenCalledTimes(1);
    expect(current()).toBe(false);
    expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual(
      Array.from({ length: MAX_PENDING_FRAMES + 2 }, (_, index) => index + 1),
    );
    finish();
    await vi.advanceTimersByTimeAsync(0);
    expect(persist).toHaveBeenCalledTimes(1);
  });

  it("queues a state frame without changes behind a pending write", async () => {
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
    const failed = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
    const first = frame(1, "initial");
    first.resourceVersions[0] = {
      ...first.resourceVersions[0],
      cursor: { workspaceId: "a".repeat(32), seq: 1 },
      replyKind: "snapshot",
    } as (typeof first.resourceVersions)[0];
    delivery.receive(first);
    // The state data of this frame arrives as a separate staged transfer.
    delivery.receive({
      ...frame(2, "initial"),
      resourceVersions: [{ revision: 2 }],
    });
    expect(failed).not.toHaveBeenCalled();
    finish();
    await vi.advanceTimersByTimeAsync(0);
    expect(failed).not.toHaveBeenCalled();
    expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual([
      1, 2,
    ]);
  });

  it("asks for a transcript snapshot after a delta lost its local base", async () => {
    const ref = { kind: "transcript", agentId: "one" } as const;
    const persist = vi
      .fn()
      .mockRejectedValueOnce(
        new Error("Transcript stream interval has no local base."),
      )
      .mockResolvedValue(undefined);
    const delivery = new ResourceEntityDelivery(persist, vi.fn(), vi.fn());
    const delta = {
      ...frame(1),
      resources: [ref],
      resourceVersions: [
        {
          resource: ref,
          revision: 1,
          cursor: { workspaceId: "a".repeat(32), seq: 1 },
          replyKind: "delta",
          transcriptChanges: { after: 0, through: 1, items: [] },
        },
      ],
    } as unknown as ResourceChangeEvent;
    expect(delivery.requiresSnapshot(ref)).toBe(false);
    delivery.receive(delta);
    await vi.advanceTimersByTimeAsync(0);
    expect(delivery.requiresSnapshot(ref)).toBe(true);
    delivery.receive({
      ...delta,
      revision: 2,
      reason: "initial",
      resourceVersions: [
        {
          resource: ref,
          revision: 2,
          cursor: { workspaceId: "a".repeat(32), seq: 2 },
          replyKind: "snapshot",
          transcriptSnapshot: { items: [] },
        },
      ],
    } as unknown as ResourceChangeEvent);
    await vi.advanceTimersByTimeAsync(0);
    expect(delivery.requiresSnapshot(ref)).toBe(false);
  });

  it("keeps state delivery when a transcript-only frame arrives", async () => {
>>>>>>> 686377a
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
<<<<<<< head
    expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual(
      Array.from({ length: 256 }, (_, index) => index + 1),
    );
    expect(failed).not.toHaveBeenCalled();
||||||| base
    expect(dispatch).toHaveBeenCalledTimes(3);
    expect(persist.mock.calls.map(([event]) => event.revision)).toEqual([1, 2]);
  });

  it("expires a stuck write and keeps its ordinary invalidation", async () => {
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
=======
    expect(dispatch).toHaveBeenCalledTimes(3);
    expect(persist.mock.calls.map(([event]) => event.revision)).toEqual([1, 2]);
  });

  it("expires a stuck write and keeps its ordinary invalidation", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    let current!: () => boolean;
    const persist = vi.fn((_event, isCurrent) => {
      current = isCurrent;
      return new Promise<void>(() => {});
    });
    const dispatch = vi.fn();
    new ResourceEntityDelivery(persist, dispatch).receive(frame(1));
    await vi.advanceTimersByTimeAsync(PERSIST_DEADLINE_MS);
    expect(current()).toBe(false);
    expect(dispatch).toHaveBeenCalledExactlyOnceWith(frame(1));
>>>>>>> 686377a
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
      else if (cause === "deadline")
        await vi.advanceTimersByTimeAsync(PERSIST_DEADLINE_MS);
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
<<<<<<< head
    delivery.receive(frame(2));
    delivery.invalidate();
    expect(current()).toBe(false);
    delivery.receive(frame(3, "reconnect"));
    reject(new Error("closed source"));
||||||| base
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
=======
    await vi.advanceTimersByTimeAsync(PERSIST_DEADLINE_MS);
    for (let revision = 2; revision <= 20; revision++) {
      delivery.receive(frame(revision));
      await vi.advanceTimersByTimeAsync(PERSIST_DEADLINE_MS);
    }
    expect(persist).toHaveBeenCalledTimes(1);
    expect(dispatch).toHaveBeenCalledTimes(20);
    finish();
    await vi.advanceTimersByTimeAsync(0);
    delivery.receive(frame(21));
>>>>>>> 686377a
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
