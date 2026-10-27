import { expect, it, vi } from "vitest";
import { createDataObserver } from "./data";
import type { ResourceRef } from "./contracts";
import type { ResourceDataSelectors, ViewRecovery } from "./selectData";
import type { TranscriptProjectionPayload } from "../client";
import type { ResourceVersion } from "../resourceEvents";
import type { ObserveSubscriptionData } from "./module";
// Partial sync fixtures: these cases never enter the transcript observer path.
type SyncFixture = ReturnType<Parameters<typeof createDataObserver>[0]>;

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
      () => ({ get }) as { get: typeof get } & SyncFixture,
      events,
      undefined,
      { processes: get, complaint: get, room: get, "terminal-output": get },
    )(ref as ResourceRef, accept, vi.fn());
    receive({
      epoch: "server",
      revision: 1,
      viewSnapshot: { [field]: value } as ResourceVersion["viewSnapshot"],
    });
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
    () => ({}) as SyncFixture,
    events,
    undefined,
    { processes: read },
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
  let publish: (data: unknown) => void = () => {};
  const stopEvents = vi.fn();
  const stopEntities = vi.fn();
  const events = {
    watchResourceChanges: (_ref: unknown, accept: typeof receive) => {
      receive = accept;
      return stopEvents;
    },
    watchResourceConnection: () => () => {},
  };
  const observe = vi.fn((_ref: ResourceRef, accept: typeof publish) => {
    publish = accept;
    return stopEntities;
  });
  const recover = vi.fn();
  const accept = vi.fn();
  const ref = { kind: "work", agentId: "lead" } as const;
  const dispose = createDataObserver(
    () => ({}) as SyncFixture,
    events,
    observe as ObserveSubscriptionData,
    { processes: recover },
  )(ref, accept, vi.fn());
  expect(observe).toHaveBeenCalledOnce();
  const first = { items: [{ id: "assignment", title: "Before" }], tasks: [] };
  publish(first);
  receive({
    epoch: "server",
    revision: 1,
    cursor: { workspaceId: "w", seq: 1 },
    replyKind: "snapshot",
  });
  const next = { items: [{ id: "assignment", title: "After" }], tasks: [] };
  publish(next);
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

// Server recovery and local selectors must preserve the resource's generated data type.
const invalidRecovery: ViewRecovery = {
  // @ts-expect-error A process recovery must return ProcessList, never terminal output.
  processes: async () => ({
    text: "output",
    offset: 0,
    status: "running",
    truncated: false,
  }),
};
const invalidSelector: ResourceDataSelectors<void> = {
  // @ts-expect-error A task list selector cannot publish work assignment data.
  tasks: () => ({ rootId: "lead", items: [], tasks: [] }),
};
void invalidRecovery;
void invalidSelector;

it("publishes partial local transcript metadata without inventing server fields", () => {
  let publish: (data: TranscriptProjectionPayload) => void = () => {};
  const unsubscribe = vi.fn();
  const sync: SyncFixture = {
    subscribeTranscriptProjection: (_scope, accept) => {
      publish = accept;
      return unsubscribe;
    },
  };
  const events = {
    watchResourceChanges: vi.fn(() => () => {}),
    watchResourceConnection: () => () => {},
  };
  const accepted: (TranscriptProjectionPayload | undefined)[] = [];
  const stop = createDataObserver(() => sync, events)(
    { kind: "transcript", agentId: "lead" },
    (data) => {
      if (data) {
        // @ts-expect-error Local cache metadata may omit this required server response field.
        const completeServerFlag: boolean = data.truncated;
        void completeServerFlag;
      }
      accepted.push(data);
    },
    vi.fn(),
  );
  const local = {
    items: [],
  } satisfies NonNullable<TranscriptProjectionPayload>;
  publish(local);
  publish(null);
  expect(accepted).toEqual([local, undefined]);
  expect(accepted[0]).toBe(local);
  expect(events.watchResourceChanges).not.toHaveBeenCalled();
  stop();
  expect(unsubscribe).toHaveBeenCalledOnce();
});
