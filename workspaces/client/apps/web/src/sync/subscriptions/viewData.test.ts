import { expect, it, vi } from "vitest";
import { createDataObserver } from "./data";
import type { ResourceVersion } from "../resourceEvents";
import type { createSyncClient } from "../client";
import type { ObserveSubscriptionData } from "./module";
import type { ResourceData, ResourceRef } from "./contracts";

it.each([
  [{ kind: "processes", agentId: "lead" }, "processes", { processes: [] }],
  [
    { kind: "complaint", complaintId: "message" },
    "complaint",
    { id: "message", text: "Reply" },
  ],
  [
    { kind: "room", roomId: "team" },
    "room",
    { messages: [{ id: "new", seq: 2 }] },
  ],
  [
    { kind: "terminal-output", terminalId: "shell" },
    "terminalOutput",
    { text: "new output", offset: 10, status: "running", truncated: false },
  ],
] as const)(
  "%j consumes initial and live values with zero reads",
  async (ref, field, value) => {
    const get = vi.fn();
    let receive: (version?: ResourceVersion) => void = () => {};
    const stop = vi.fn();
    const events = {
      watchResourceConnection: () => () => {},
      watchResourceChanges: vi.fn((_ref: unknown, accept: typeof receive) => {
        receive = accept;
        return stop;
      }),
    };
    const accept = vi.fn();
    const dispose = createDataObserver(
      ((): Partial<ReturnType<typeof createSyncClient>> & {
        get: typeof get;
      } => ({ get })) as Parameters<typeof createDataObserver>[0],
      events,
      undefined,
      get,
    )(ref, accept, vi.fn());
    receive({ epoch: "server", revision: 1, viewSnapshot: { [field]: value } });
    await vi.waitFor(() => expect(accept).toHaveBeenCalledTimes(1));
    receive({ epoch: "server", revision: 2, viewSnapshot: { [field]: value } });
    await vi.waitFor(() => expect(accept).toHaveBeenCalledTimes(2));
    expect(accept).toHaveBeenNthCalledWith(
      1,
      value,
      expect.objectContaining({ epoch: "server", revision: 1 }),
    );
    expect(accept).toHaveBeenNthCalledWith(
      2,
      value,
      expect.objectContaining({ epoch: "server", revision: 2 }),
    );
    expect(get).not.toHaveBeenCalled();
    dispose();
    expect(stop).toHaveBeenCalledOnce();
  },
);

it("an oversized view recovers once and subsequent complete frames resume zero-read delivery", async () => {
  let receive: (version?: ResourceVersion) => void = () => {};
  const events = {
    watchResourceChanges: (_ref: unknown, accept: typeof receive) => {
      receive = accept;
      return () => {};
    },
    watchResourceConnection: () => () => {},
  };
  const read = vi.fn(async () => ({ processes: [] }));
  const accept = vi.fn();
  const stop = createDataObserver(
    () => ({}) as ReturnType<typeof createSyncClient>,
    events,
    undefined,
    read,
  )({ kind: "processes", agentId: "lead" }, accept, vi.fn());
  expect(read).not.toHaveBeenCalled();
  receive({ epoch: "server", revision: 1 });
  await vi.waitFor(() => expect(accept).toHaveBeenCalledOnce());
  receive({
    epoch: "server",
    revision: 2,
    viewSnapshot: { processes: { processes: [] } },
  });
  await vi.waitFor(() => expect(accept).toHaveBeenCalledTimes(2));
  expect(read).toHaveBeenCalledOnce();
  stop();
});

it("work baselines and updates use the entity observer even when view recovery exists", async () => {
  let receive: (version?: ResourceVersion) => void = () => {};
  let publish: Parameters<ObserveSubscriptionData>[1] = () => {};
  const stopEvents = vi.fn();
  const stopEntities = vi.fn();
  const events = {
    watchResourceChanges: (_ref: unknown, accept: typeof receive) => {
      receive = accept;
      return stopEvents;
    },
    watchResourceConnection: () => () => {},
  };
  // This fixture observes only work and omits the unused failure callback.
  const observe = vi.fn((_ref: ResourceRef, accept: typeof publish) => {
    publish = accept;
    return stopEntities;
  }) as ObserveSubscriptionData;
  const recover = vi.fn();
  const accept = vi.fn();
  const ref = { kind: "work", agentId: "lead" } as const;
  const dispose = createDataObserver(
    () => ({}) as ReturnType<typeof createSyncClient>,
    events,
    observe,
    recover,
  )(ref, accept, vi.fn());
  expect(observe).toHaveBeenCalledOnce();
  // The observer only forwards these values; work metadata is intentionally omitted.
  type WorkData = ResourceData<typeof ref>;
  type WorkFixture = {
    items: Partial<WorkData["items"][number]>[];
    tasks: WorkData["tasks"];
  };
  const first: WorkFixture = {
    items: [{ id: "assignment", title: "Before" }],
    tasks: [],
  };
  publish(first as WorkData);
  receive({
    epoch: "server",
    revision: 1,
    cursor: { workspaceId: "w", seq: 1 },
    replyKind: "snapshot",
  });
  const next: WorkFixture = {
    items: [{ id: "assignment", title: "After" }],
    tasks: [],
  };
  publish(next as WorkData);
  receive({
    epoch: "server",
    revision: 2,
    cursor: { workspaceId: "w", seq: 2 },
    replyKind: "delta",
  });
  await Promise.resolve();
  expect(accept.mock.calls).toEqual([[first], [next]]);
  expect(recover).not.toHaveBeenCalled();
  dispose();
  expect(stopEvents).toHaveBeenCalledOnce();
  expect(stopEntities).toHaveBeenCalledOnce();
});
