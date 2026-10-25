import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ResourceEntityDelivery } from "./resourceEntityDelivery";
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
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("requests source recovery after a current timeout and keeps blocked storage degraded", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const persist = vi.fn(() => new Promise<void>(() => {}));
    const dispatch = vi.fn();
    const failed = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
    delivery.receive(frame(1));
    await vi.advanceTimersByTimeAsync(1000);
    expect(failed).toHaveBeenCalledOnce();
    expect(dispatch).not.toHaveBeenCalled();
    delivery.receive(frame(2));
    expect(failed).toHaveBeenCalledTimes(2);
    expect(persist).toHaveBeenCalledOnce();
    expect(dispatch).not.toHaveBeenCalled();
  });

  it("a cancelled persistence rejection cannot request recovery for a new subscription", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    let reject!: (error: Error) => void;
    const persist = vi.fn(
      () =>
        new Promise<void>((_resolve, failed) => {
          reject = failed;
        }),
    );
    const failed = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, vi.fn(), failed);
    delivery.receive(frame(1));
    delivery.invalidate();
    reject(new Error("old subscriber write failed"));
    await vi.advanceTimersByTimeAsync(0);
    expect(failed).not.toHaveBeenCalled();
  });

  it.each(["reject", "timeout"] as const)(
    "recovers a new owner's terminal base after an old write %s without stale proof",
    async (cause) => {
      vi.spyOn(console, "error").mockImplementation(() => {});
      let resolve!: () => void;
      let reject!: (error: Error) => void;
      const persist = vi
        .fn()
        .mockReturnValueOnce(
          new Promise<void>((done, failed) => {
            resolve = done;
            reject = failed;
          }),
        )
        .mockResolvedValue(undefined);
      const dispatch = vi.fn();
      const failed = vi.fn();
      const delivery = new ResourceEntityDelivery(persist, dispatch, failed);
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
    },
  );

  it("revokes every queued receipt before recovering a failed current write", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    let reject!: (error: Error) => void;
    const persist = vi.fn(
      () =>
        new Promise<void>((_resolve, failed) => {
          reject = failed;
        }),
    );
    const failed = vi.fn();
    const delivery = new ResourceEntityDelivery(persist, vi.fn(), failed);
    const queued: ResourceChangeEvent = frame(2);
    queued.resourceVersions[0].cursor = {
      workspaceId: queued.workspaceId,
      seq: 2,
    };
    queued.resourceVersions[0].replyKind = "snapshot";
    delivery.receive(frame(1));
    delivery.receive(queued);
    reject(new Error("current Work write failed"));
    await vi.advanceTimersByTimeAsync(0);
    expect(failed).toHaveBeenCalledOnce();
    expect(queued.resourceVersions[0].cursor).toBeUndefined();
    expect(queued.resourceVersions[0].replyKind).toBeUndefined();
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
    await vi.advanceTimersByTimeAsync(0);
    expect(dispatch).toHaveBeenCalledTimes(2);
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
      await vi.advanceTimersByTimeAsync(0);
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
    await vi.advanceTimersByTimeAsync(0);
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
    await vi.advanceTimersByTimeAsync(0);
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
  });

  it.each(["failure", "invalidate", "overflow"] as const)(
    "withholds a terminal interval without its persisted base after %s",
    async (mode) => {
      vi.spyOn(console, "error").mockImplementation(() => {});
      let finish!: () => void;
      const persist = vi.fn().mockResolvedValueOnce(undefined);
      if (mode === "failure")
        persist.mockRejectedValueOnce(new Error("disk full"));
      else
        persist.mockImplementationOnce(
          () =>
            new Promise<void>((resolve) => {
              finish = resolve;
            }),
        );
      const dispatch = vi.fn();
      const delivery = new ResourceEntityDelivery(persist, dispatch);
      const terminal = (
        revision: number,
        reason: ResourceChangeEvent["reason"] = "change",
      ): ResourceChangeEvent => ({
        ...frame(revision, reason),
        resources: [{ kind: "terminal-output", terminalId: "shell" }],
        resourceVersions: [
          {
            revision,
            cursor: { workspaceId: "a".repeat(32), seq: revision },
            replyKind: revision === 1 ? "snapshot" : "delta",
            viewSnapshot: {
              terminalOutput: {
                text: "x",
                offset: revision,
                status: "running",
                truncated: false,
              },
            },
          },
        ],
      });
      delivery.receive(terminal(1, "initial"));
      await vi.advanceTimersByTimeAsync(0);
      dispatch.mockClear();
      delivery.receive(terminal(2));
      if (mode === "invalidate") delivery.invalidate();
      if (mode === "overflow") delivery.receive(terminal(3, "overflow"));
      if (finish) finish();
      await vi.advanceTimersByTimeAsync(0);
      expect(dispatch).toHaveBeenCalled();
      for (const [event] of dispatch.mock.calls) {
        expect(event.resourceVersions[0].cursor).toBeUndefined();
        expect(
          event.resourceVersions[0].viewSnapshot?.terminalOutput,
        ).toBeUndefined();
      }
    },
  );

  it("queues a cold terminal interval behind its pending authoritative snapshot", async () => {
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
    const terminal = (
      revision: number,
      replyKind: "snapshot" | "delta",
    ): ResourceChangeEvent => ({
      ...frame(revision, revision === 1 ? "initial" : "change"),
      resources: [{ kind: "terminal-output", terminalId: "queued" }],
      resourceVersions: [
        {
          revision,
          cursor: { workspaceId: "a".repeat(32), seq: revision },
          replyKind,
          viewSnapshot: {
            terminalOutput: {
              text: revision === 1 ? "a" : "!",
              offset: revision,
              status: "running",
              truncated: false,
            },
          },
        },
      ],
    });
    delivery.receive(terminal(1, "snapshot"));
    delivery.receive(terminal(2, "delta"));
    expect(dispatch).not.toHaveBeenCalled();
    finish();
    await vi.advanceTimersByTimeAsync(0);
    expect(persist).toHaveBeenCalledTimes(2);
    expect(dispatch.mock.calls.map(([event]) => event.revision)).toEqual([
      1, 2,
    ]);
    expect(
      dispatch.mock.calls[1][0].resourceVersions[0].viewSnapshot.terminalOutput
        .text,
    ).toBe("!");
  });

  it("retains invalidation if storage rejects the batch", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const dispatch = vi.fn();
    new ResourceEntityDelivery(
      vi.fn().mockRejectedValue(new Error("storage failed")),
      dispatch,
    ).receive(frame(1));
    await vi.advanceTimersByTimeAsync(0);
    expect(dispatch).toHaveBeenCalledExactlyOnceWith(frame(1));
  });

  it("does not accumulate unresolved writes after a storage timeout", async () => {
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

it("persists an added transcript baseline before dispatching its initial notification", async () => {
  let finish!: () => void;
  const persist = vi.fn(
    () =>
      new Promise<void>((resolve) => {
        finish = resolve;
      }),
  );
  const dispatched = Promise.withResolvers<void>();
  const dispatch = vi.fn(() => dispatched.resolve());
  const delivery = new ResourceEntityDelivery(persist, dispatch);
  const initial = {
    protocol: 4,
    workspaceId: "a".repeat(32),
    epoch: "one",
    revision: 1,
    reason: "initial",
    resources: [{ kind: "transcript", agentId: "chat" }],
    resourceVersions: [
      { revision: 1, transcriptSnapshot: { items: [], truncated: false } },
    ],
  } as ResourceChangeEvent;
  delivery.receive(initial);
  expect(persist).toHaveBeenCalledTimes(1);
  expect(dispatch).not.toHaveBeenCalled();
  finish();
  await dispatched.promise;
  expect(dispatch).toHaveBeenCalledWith(initial);
});

it("revokes resume proof when the projection write fails", async () => {
  const event: ResourceChangeEvent = frame(4, "reconnect");
  event.resourceVersions[0]!.cursor = {
    workspaceId: event.workspaceId,
    seq: 4,
  };
  event.resourceVersions[0]!.replyKind = "delta";
  const dispatch = vi.fn();
  const failed = vi.spyOn(console, "error").mockImplementation(() => {});
  const delivery = new ResourceEntityDelivery(async () => {
    throw new Error("disk full");
  }, dispatch);
  delivery.receive(event);
  for (let turn = 0; turn < 10; turn++) await Promise.resolve();
  expect(dispatch).toHaveBeenCalledTimes(1);
  expect(dispatch.mock.calls[0]![0].resourceVersions[0].cursor).toBeUndefined();
  expect(
    dispatch.mock.calls[0]![0].resourceVersions[0].replyKind,
  ).toBeUndefined();
  failed.mockRestore();
});
