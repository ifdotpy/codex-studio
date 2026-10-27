import { afterEach, beforeEach, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  stateWrites: [] as unknown[],
  resumeListeners: new Set<() => void>(),
  schemaMismatchListeners: new Set<() => void>(),
  syncDatabase: vi.fn(),
  syncGet: vi.fn(),
  syncPost: vi.fn(),
  refreshSession: vi.fn(),
}));

vi.mock("react", () => ({
  useEffect: (effect: () => void | (() => void)) => {
    mocks.effects.push(effect);
  },
  useState: (initial: unknown) => [
    initial,
    (value: unknown) => mocks.stateWrites.push(value),
  ],
}));
vi.mock("./client", () => ({ syncDatabase: mocks.syncDatabase }));
vi.mock("./resume", () => ({
  onResume: (listener: () => void) => {
    mocks.resumeListeners.add(listener);
    return () => mocks.resumeListeners.delete(listener);
  },
}));
vi.mock("../servers/storage", () => ({
  serverLocalStorage: {
    getItem: () => null,
    setItem: vi.fn(),
    removeItem: vi.fn(),
  },
}));
vi.mock("../api", () => ({
  isApiSchemaMismatch: () => false,
  onApiSchemaMismatch: (listener: () => void) => {
    mocks.schemaMismatchListeners.add(listener);
    return () => mocks.schemaMismatchListeners.delete(listener);
  },
  syncGet: mocks.syncGet,
  syncPost: mocks.syncPost,
  refreshSession: mocks.refreshSession,
  errorText: (error: unknown) =>
    error instanceof Error ? error.message : String(error),
  ApiError: class ApiError extends Error {
    status = 400;
  },
  ApiSchemaMismatchError: class ApiSchemaMismatchError extends Error {},
}));

import { useOutbox } from "./send";

type OutboxDocument = {
  id: string;
  seq: number;
  payload: string;
  getLatest: () => OutboxDocument;
  incrementalModify: (
    change: (record: OutboxDocument) => OutboxDocument,
  ) => Promise<OutboxDocument>;
};

function databaseFixture(
  initial: Array<Record<string, unknown>> = [],
  synchronousSubscriptionErrors = 0,
) {
  const listeners = new Set<(docs: OutboxDocument[]) => void>();
  const projectionListeners = new Set<
    (event: { documentData: unknown }) => void
  >();
  let findExecutions = 0;
  let subscriptions = 0;
  const docs: OutboxDocument[] = [];
  const emit = () => {
    for (const listener of listeners) listener(docs);
  };
  const add = (value: Record<string, unknown>) => {
    const record = {
      id: String(value.id),
      seq: 0,
      payload: JSON.stringify(value),
    } as OutboxDocument;
    const doc: OutboxDocument = {
      id: record.id,
      seq: record.seq,
      get payload() {
        return record.payload;
      },
      set payload(next: string) {
        record.payload = next;
      },
      getLatest: () => record,
      incrementalModify: async (change) => {
        Object.assign(record, change({ ...record }));
        emit();
        return record;
      },
    };
    docs.push(doc);
    return doc;
  };
  initial.forEach(add);
  const database = {
    workspaceId: "workspace",
    verifyWorkspace: async () => {},
    db: {
      projections: {
        $: {
          subscribe: (listener: (event: { documentData: unknown }) => void) => {
            projectionListeners.add(listener);
            return { unsubscribe: () => projectionListeners.delete(listener) };
          },
        },
        storageInstance: { findDocumentsById: async () => [] },
      },
      outbox: {
        findOne: (id: string) => ({
          exec: async () => docs.find((doc) => doc.id === id),
        }),
        find: () => ({
          exec: async () => {
            findExecutions++;
            return docs;
          },
          $: {
            subscribe: (
              listener: (docs: OutboxDocument[]) => void,
              _error?: (error: unknown) => void,
            ) => {
              subscriptions++;
              if (synchronousSubscriptionErrors > 0) {
                synchronousSubscriptionErrors--;
                _error?.(new Error("outbox stream failed"));
                return { closed: true, unsubscribe: () => {} };
              }
              listeners.add(listener);
              return {
                closed: false,
                unsubscribe: () => listeners.delete(listener),
              };
            },
          },
        }),
      },
    },
  };
  return {
    database,
    docs,
    add,
    emit,
    findExecutions: () => findExecutions,
    subscriptions: () => subscriptions,
    publishReceipt: (value: unknown) => {
      for (const listener of projectionListeners)
        listener({
          documentData: {
            id: "entity:messageReceipt:hidden",
            entity: { collection: "messageReceipt", id: "hidden", value },
            seq: 3,
            _deleted: false,
          },
        });
    },
  };
}

function mount() {
  useOutbox();
  const cleanups = mocks.effects.splice(0).flatMap((effect) => {
    const cleanup = effect();
    return cleanup ? [cleanup] : [];
  });
  return () => cleanups.forEach((cleanup) => cleanup());
}

async function flushMicrotasks() {
  for (let attempt = 0; attempt < 20; attempt++) await Promise.resolve();
}

beforeEach(() => {
  vi.useFakeTimers();
  mocks.effects.length = 0;
  mocks.stateWrites.length = 0;
  mocks.resumeListeners.clear();
  mocks.schemaMismatchListeners.clear();
  mocks.syncDatabase.mockReset();
  mocks.syncGet.mockReset();
  mocks.syncPost.mockReset();
  mocks.refreshSession.mockReset();
  vi.stubGlobal("navigator", { onLine: true });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("decodes each outbox snapshot once when publishing", async () => {
  vi.stubGlobal("navigator", { onLine: false });
  const fixture = databaseFixture(
    [3, 1, 2].map((created) => ({
      id: `message-${created}`,
      body: { id: `message-${created}`, room: "chat", text: "hello" },
      status: "queued",
      created,
    })),
  );
  mocks.syncDatabase.mockResolvedValue(fixture.database);
  const unmount = mount();
  await flushMicrotasks();
  const parse = vi.spyOn(JSON, "parse");
  fixture.emit();
  expect(parse.mock.calls.length).toBe(3);
  parse.mockRestore();
  unmount();
});

it("does not rescan an idle outbox on a timer", async () => {
  const fixture = databaseFixture();
  mocks.syncDatabase.mockResolvedValue(fixture.database);
  const unmount = mount();
  await flushMicrotasks();
  const initialFinds = fixture.findExecutions();
  expect(initialFinds).toBeGreaterThan(0);

  await vi.advanceTimersByTimeAsync(30_000);
  await flushMicrotasks();

  expect(fixture.findExecutions()).toBe(initialFinds);
  expect(mocks.syncPost).not.toHaveBeenCalled();
  unmount();
});

it.each([
  "null",
  "{",
  JSON.stringify({ status: "queued", created: 0 }),
  JSON.stringify({
    status: "queued",
    created: 0,
    body: { id: "other", room: "chat", text: "hello" },
  }),
  JSON.stringify({
    status: ["queued"],
    created: 0,
    body: { id: "invalid", room: "chat", text: "hello" },
  }),
  JSON.stringify({
    status: "accepted",
    created: 0,
    body: { id: "invalid", room: "chat", text: "hello" },
  }),
  JSON.stringify({
    status: "cancelled",
    created: 0,
    body: { id: "invalid", room: "chat", text: "hello" },
    receipt: { id: "invalid", status: "delivered" },
  }),
])(
  "drains valid messages despite a malformed persisted row %s",
  async (payload) => {
    const fixture = databaseFixture([
      { id: "invalid", status: "queued", created: 0 },
      {
        id: "valid",
        body: { id: "valid", room: "chat", text: "hello" },
        status: "queued",
        created: 1,
      },
    ]);
    fixture.docs[0]!.payload = payload;
    mocks.syncDatabase.mockResolvedValue(fixture.database);
    mocks.syncGet.mockResolvedValue({ workspaceId: "workspace" });
    mocks.refreshSession.mockResolvedValue({ token: "session" });
    mocks.syncPost.mockResolvedValue({ id: "valid", status: "accepted" });
    const unmount = mount();
    await flushMicrotasks();
    expect(mocks.syncPost).toHaveBeenCalledTimes(1);
    expect(fixture.docs[0]!.payload).toBe(payload);
    expect(mocks.stateWrites).toContain(
      "Saved messages could not be read; delivery is uncertain: invalid.",
    );
    unmount();
  },
);

it("retries a failed subscription once after the backoff deadline", async () => {
  const fixture = databaseFixture();
  mocks.syncDatabase
    .mockRejectedValueOnce(new Error("database unavailable"))
    .mockResolvedValue(fixture.database);
  const unmount = mount();
  await flushMicrotasks();
  expect(fixture.findExecutions()).toBe(0);

  await vi.advanceTimersByTimeAsync(4_999);
  await flushMicrotasks();
  expect(fixture.findExecutions()).toBe(0);

  await vi.advanceTimersByTimeAsync(1);
  await flushMicrotasks();

  expect(fixture.findExecutions()).toBeGreaterThan(0);
  expect(fixture.subscriptions()).toBe(1);
  const settledFinds = fixture.findExecutions();
  await vi.advanceTimersByTimeAsync(15_000);
  await flushMicrotasks();
  expect(fixture.findExecutions()).toBe(settledFinds);
  expect(fixture.subscriptions()).toBe(1);
  unmount();
});

it("retries a synchronously closed outbox subscription", async () => {
  const fixture = databaseFixture([], 1);
  mocks.syncDatabase.mockResolvedValue(fixture.database);
  const unmount = mount();
  await flushMicrotasks();

  expect(fixture.subscriptions()).toBe(1);
  expect(fixture.findExecutions()).toBe(0);
  await vi.advanceTimersByTimeAsync(4_999);
  await flushMicrotasks();
  expect(fixture.subscriptions()).toBe(1);

  await vi.advanceTimersByTimeAsync(1);
  await flushMicrotasks();

  expect(fixture.subscriptions()).toBe(2);
  expect(fixture.findExecutions()).toBeGreaterThan(0);
  unmount();
});

it("retries a real queued network failure once after the backoff deadline", async () => {
  const fixture = databaseFixture([
    {
      id: "stable-message-id",
      body: { id: "stable-message-id", room: "chat", text: "hello" },
      status: "queued",
      attempted: false,
      created: 1,
      displayPending: true,
    },
  ]);
  mocks.syncDatabase.mockResolvedValue(fixture.database);
  mocks.syncGet.mockResolvedValue({ workspaceId: "workspace" });
  mocks.refreshSession.mockResolvedValue({ token: "session" });
  mocks.syncPost
    .mockRejectedValueOnce(new TypeError("network unavailable"))
    .mockResolvedValue({ id: "stable-message-id", status: "accepted" });

  const unmount = mount();
  await flushMicrotasks();
  expect(mocks.syncPost).toHaveBeenCalledTimes(1);

  await vi.advanceTimersByTimeAsync(4_999);
  await flushMicrotasks();
  expect(mocks.syncPost).toHaveBeenCalledTimes(1);

  await vi.advanceTimersByTimeAsync(1);
  await flushMicrotasks();

  expect(mocks.syncPost).toHaveBeenCalledTimes(2);
  expect(
    mocks.syncPost.mock.calls.map(([, body]) => (body as { id: string }).id),
  ).toEqual(["stable-message-id", "stable-message-id"]);
  unmount();
});

it("streams delivery into a hidden accepted outbox row without another receipt GET", async () => {
  const fixture = databaseFixture([
    {
      id: "hidden",
      body: { id: "hidden", room: "chat", text: "input" },
      status: "accepted",
      displayPending: false,
      created: 1,
      receipt: { id: "hidden", status: "dispatching", materialized: true },
    },
  ]);
  mocks.syncDatabase.mockResolvedValue(fixture.database);
  const unmount = mount();
  await flushMicrotasks();
  const initialReads = mocks.syncGet.mock.calls.length;
  fixture.publishReceipt({
    id: "hidden",
    agent: "chat",
    status: "delivered",
    materialized: true,
  });
  await flushMicrotasks();
  expect(JSON.parse(fixture.docs[0]!.payload).receipt.status).toBe("delivered");
  expect(mocks.syncGet).toHaveBeenCalledTimes(initialReads);
  expect(mocks.syncPost).not.toHaveBeenCalled();
  unmount();
});
