import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type {
  ResourceVersion,
  ResourceConnectionState,
} from "../resourceEvents";
import type { ResourceRef } from "./contracts";

const mocks = vi.hoisted(() => ({
  changes: new Map<string, Set<(version?: ResourceVersion) => void>>(),
  connections: new Set<
    (status: ResourceConnectionState, stream: "open") => void
  >(),
}));
vi.mock("../resourceEvents", () => ({
  createResourceEvents: () => ({
    watchResourceChanges: (
      ref: ResourceRef,
      accept: (version?: ResourceVersion) => void,
    ) => {
      const key = JSON.stringify(ref);
      let listeners = mocks.changes.get(key);
      if (!listeners) mocks.changes.set(key, (listeners = new Set()));
      listeners.add(accept);
      return () => listeners.delete(accept);
    },
    watchResourceConnection: (
      accept: (status: ResourceConnectionState, stream: "open") => void,
    ) => {
      mocks.connections.add(accept);
      accept("live", "open");
      return () => mocks.connections.delete(accept);
    },
    dispose: vi.fn(),
  }),
}));
vi.mock("../client", () => ({ syncDatabase: vi.fn() }));
vi.mock("../resume", () => ({ onResume: () => () => {} }));
import { createServerSubscriptions } from "./server";

const disposals: (() => void)[] = [];
beforeEach(() => vi.useFakeTimers());
afterEach(() => {
  for (const dispose of disposals.splice(0)) dispose();
  mocks.changes.clear();
  mocks.connections.clear();
  vi.useRealTimers();
});
function fixture() {
  const get = vi.fn();
  const subscriptions = createServerSubscriptions({
    owner: { id: "remote", origin: "https://server.example" },
    api: { get },
    tokenRates: {},
    sync: () => ({}),
  } as unknown as Parameters<typeof createServerSubscriptions>[0]);
  disposals.push(subscriptions.dispose);
  const deliver = (ref: ResourceRef, version: ResourceVersion) => {
    for (const accept of mocks.changes.get(JSON.stringify(ref)) ?? [])
      accept(version);
  };
  return { subscriptions, get, deliver };
}
const inlineViews = [
  {
    ref: { kind: "processes", agentId: "lead" },
    snapshot: { processes: { processes: [] as never[] } },
    value: { processes: [] },
  },
  {
    ref: { kind: "complaint", complaintId: "message" },
    snapshot: { complaint: { id: "message", text: "Reply" } },
    value: { id: "message", text: "Reply" },
  },
  {
    ref: { kind: "room", roomId: "team", legacy: true },
    snapshot: { legacyMessages: { messages: [] } },
    value: { messages: [] },
  },
] as const;

it.each(inlineViews)(
  "the actual server adapter confirms applied $ref.kind inline data without HTTP",
  async ({ ref, snapshot, value }) => {
    const f = fixture();
    f.subscriptions.subscribe(ref, () => {});
    f.deliver(ref, {
      epoch: "server",
      revision: 1,
      viewSnapshot: snapshot,
    } as ResourceVersion);
    expect(f.subscriptions.getSnapshot(ref).status).not.toBe("live");
    await vi.advanceTimersByTimeAsync(0);
    expect(f.subscriptions.getSnapshot(ref)).toMatchObject({
      data: value,
      status: "live",
      error: null,
    });
    expect(f.get).not.toHaveBeenCalled();
  },
);

it("failed view recovery stays an error until a successful current inline application", async () => {
  const f = fixture();
  const { ref, snapshot, value } = inlineViews[0];
  f.get.mockRejectedValue(new Error("view unavailable"));
  f.subscriptions.subscribe(ref, () => {});
  f.deliver(ref, { epoch: "server", revision: 1 });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.subscriptions.getSnapshot(ref).status).toBe("error");
  f.deliver(ref, { epoch: "server", revision: 2, viewSnapshot: snapshot });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.subscriptions.getSnapshot(ref)).toMatchObject({
    data: value,
    status: "live",
    error: null,
  });
  expect(f.get).toHaveBeenCalledOnce();
});

it("disposal before queued inline application cannot publish data or readiness", async () => {
  const f = fixture();
  const { ref, snapshot } = inlineViews[0];
  const stop = f.subscriptions.subscribe(ref, () => {}, { graceMs: 0 });
  f.deliver(ref, { epoch: "server", revision: 1, viewSnapshot: snapshot });
  stop();
  await vi.advanceTimersByTimeAsync(0);
  expect(f.subscriptions.getSnapshot(ref).data).toBeUndefined();
  expect(f.subscriptions.getSnapshot(ref).status).not.toBe("live");
  expect(f.get).not.toHaveBeenCalled();
});

it("an unchanged reconnect inline baseline re-establishes successful view readiness", async () => {
  const f = fixture();
  const { ref, snapshot } = inlineViews[0];
  f.subscriptions.subscribe(ref, () => {});
  f.deliver(ref, { epoch: "server", revision: 1, viewSnapshot: snapshot });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.subscriptions.getSnapshot(ref).status).toBe("live");
  for (const accept of mocks.connections) accept("degraded", "open");
  for (const accept of mocks.connections) accept("live", "open");
  f.deliver(ref, { epoch: "server", revision: 1, viewSnapshot: snapshot });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.subscriptions.getSnapshot(ref).status).toBe("live");
  expect(f.get).not.toHaveBeenCalled();
});

it("a delayed recovery cannot overwrite the newer inline application", async () => {
  const f = fixture();
  const { ref, snapshot, value } = inlineViews[0];
  let finish!: (value: unknown) => void;
  f.get.mockReturnValue(
    new Promise((resolve) => {
      finish = resolve;
    }),
  );
  const shown: unknown[] = [];
  f.subscriptions.subscribe(ref, () =>
    shown.push(f.subscriptions.getSnapshot(ref).data),
  );
  f.deliver(ref, { epoch: "server", revision: 1 });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.get).toHaveBeenCalledOnce();
  f.deliver(ref, { epoch: "server", revision: 2, viewSnapshot: snapshot });
  const obsolete = { processes: [{ id: "obsolete" }] };
  finish(obsolete);
  await vi.advanceTimersByTimeAsync(0);
  expect(shown).not.toContainEqual(obsolete);
  expect(f.subscriptions.getSnapshot(ref)).toMatchObject({
    data: value,
    status: "live",
  });
});

it.each([
  {
    ref: { kind: "room", roomId: "team" },
    snapshot: { room: { messages: [] } },
  },
  {
    ref: { kind: "terminal-output", terminalId: "terminal" },
    snapshot: {
      terminalOutput: {
        text: "uncommitted",
        offset: 11,
        status: "running",
        truncated: false,
      },
    },
  },
] as const)(
  "revoked durable $ref.kind view proof cannot be replaced by the retained payload",
  async ({ ref, snapshot }) => {
    const f = fixture();
    f.subscriptions.subscribe(ref, () => {});
    f.deliver(ref, {
      epoch: "server",
      revision: 1,
      viewSnapshot: snapshot,
    } as ResourceVersion);
    await vi.advanceTimersByTimeAsync(0);
    expect(f.subscriptions.getSnapshot(ref).status).not.toBe("live");
  },
);
