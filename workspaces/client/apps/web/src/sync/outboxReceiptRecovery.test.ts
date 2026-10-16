import { afterEach, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  syncDatabase: vi.fn(),
  syncGet: vi.fn(),
  syncPost: vi.fn(),
}));
vi.mock("./client", () => ({ syncDatabase: mocks.syncDatabase }));
vi.mock("../api", () => ({
  syncGet: mocks.syncGet,
  syncPost: mocks.syncPost,
  ApiError: class extends Error {},
  ApiSchemaMismatchError: class extends Error {},
}));

import {
  acknowledgeOutbox,
  reconcileOutboxReceipts,
  recoverAcknowledgedOutbox,
} from "./send";

type Entry = {
  id: string;
  status?: string;
  room?: string;
  displayPending?: boolean;
};
function fixture(entries: Entry[]) {
  const docs = entries.map((entry) => {
    const record = {
      id: entry.id,
      payload: JSON.stringify({
        body: { id: entry.id, room: entry.room || "chat", text: entry.id },
        status: entry.status || "accepted",
        displayPending: entry.displayPending ?? true,
        created: 1,
        receipt: { id: entry.id, status: "pending", materialized: false },
      }),
    };
    return {
      id: entry.id,
      getLatest: () => record,
      incrementalModify: vi.fn(async (change) => {
        Object.assign(record, change({ ...record }));
        return record;
      }),
    };
  });
  const projectionListeners = new Set<
    (event: { documentData: unknown }) => void
  >();
  const projections = {
    storageInstance: {
      findDocumentsById: vi.fn(
        async (
          _ids: string[],
          _withDeleted?: boolean,
        ): Promise<unknown[]> => [],
      ),
    },
    $: {
      subscribe: vi.fn(
        (listener: (event: { documentData: unknown }) => void) => {
          projectionListeners.add(listener);
          return { unsubscribe: () => projectionListeners.delete(listener) };
        },
      ),
    },
  };
  const database = {
    workspaceId: "workspace",
    verifyWorkspace: vi.fn().mockResolvedValue(undefined),
    db: {
      projections,
      outbox: {
        find: () => ({ exec: async () => docs }),
        findOne: (id: string) => ({
          exec: async () => docs.find((doc) => doc.id === id),
        }),
      },
    },
  };
  mocks.syncDatabase.mockResolvedValue(database);
  return {
    docs,
    database,
    projections,
    emitProjection(documentData: unknown) {
      for (const listener of projectionListeners) listener({ documentData });
    },
  };
}

afterEach(() => vi.resetAllMocks());

it("reconciles a delivered receipt before transcript observation", async () => {
  const { docs } = fixture([{ id: "visible" }]);

  await reconcileOutboxReceipts(
    "chat",
    [{ id: "visible", status: "delivered", materialized: true }],
    "workspace",
  );
  await acknowledgeOutbox(["visible"]);

  const reconciled = JSON.parse(docs[0].getLatest().payload);
  expect(reconciled.status).toBe("accepted");
  expect(reconciled.receipt).toMatchObject({
    id: "visible",
    status: "delivered",
    materialized: true,
  });
  expect(reconciled.displayPending).toBe(false);
});

it("reconciles delivered receipts after transcript observation without showing them again", async () => {
  const { docs } = fixture([
    { id: "hidden" },
    { id: "other-room", room: "elsewhere" },
  ]);
  await acknowledgeOutbox(["hidden", "other-room"]);
  expect(JSON.parse(docs[0].getLatest().payload).displayPending).toBe(false);
  expect(JSON.parse(docs[1].getLatest().payload).displayPending).toBe(false);
  const otherRoomBefore = docs[1].getLatest().payload;
  await reconcileOutboxReceipts(
    "chat",
    [
      { id: "hidden", status: "delivered", materialized: true },
      { id: "other-room", status: "delivered", materialized: true },
    ],
    "workspace",
  );

  const hidden = JSON.parse(docs[0].getLatest().payload);
  expect(hidden.status).toBe("accepted");
  expect(hidden.receipt).toMatchObject({
    id: "hidden",
    status: "delivered",
    materialized: true,
  });
  expect(hidden.displayPending).toBe(false);
  expect(docs[1].getLatest().payload).toBe(otherRoomBefore);
});

it("keeps retired copies hidden without confirmed delivery and for failed or uncertain receipts", async () => {
  const { docs } = fixture([
    { id: "not-materialized", displayPending: false },
    { id: "pending", displayPending: false },
    { id: "uncertain", status: "uncertain", displayPending: false },
    { id: "failed", status: "failed", displayPending: false },
  ]);
  const failedBefore = docs[3].getLatest().payload;

  await reconcileOutboxReceipts(
    "chat",
    [
      { id: "not-materialized", status: "delivered", materialized: false },
      { id: "pending", status: "pending", materialized: true },
      { id: "uncertain", status: "uncertain", materialized: true },
      { id: "failed", status: "failed", materialized: true },
    ],
    "workspace",
  );

  const values = Object.fromEntries(
    docs.map((doc) => [doc.id, JSON.parse(doc.getLatest().payload)]),
  );
  expect(values["not-materialized"].displayPending).toBe(false);
  expect(values.pending.displayPending).toBe(false);
  expect(values.uncertain).toMatchObject({
    status: "uncertain",
    displayPending: false,
    receipt: { status: "uncertain", materialized: true },
  });
  expect(values.failed).toMatchObject({
    status: "failed",
    displayPending: false,
  });
  expect(docs[3].getLatest().payload).toBe(failedBefore);
});

it("clears only acknowledged copies with confirmed saved originals across chats", async () => {
  const { docs } = fixture([
    { id: "saved", room: "first" },
    { id: "resolved", room: "second", status: "uncertain" },
    { id: "missing" },
    { id: "legacy" },
    { id: "pending" },
    { id: "unknown-http", status: "queued" },
    { id: "failed", status: "failed" },
    { id: "hidden", displayPending: false },
  ]);
  mocks.syncGet.mockImplementation(async (_path, options) => ({
    agent: options.query.agent,
    workspaceId: "workspace",
    items: JSON.parse(options.query.ids).map((id: string) => ({
      id,
      status: id === "pending" ? "pending" : "delivered",
      ...(id === "legacy"
        ? {}
        : { materialized: ["saved", "resolved", "pending"].includes(id) }),
    })),
  }));
  await recoverAcknowledgedOutbox();
  const values = Object.fromEntries(
    docs.map((doc) => [doc.id, JSON.parse(doc.getLatest().payload)]),
  );
  expect(values.saved.displayPending).toBe(false);
  expect(values.resolved.displayPending).toBe(false);
  expect(values.resolved.status).toBe("accepted");
  for (const id of ["missing", "legacy", "pending", "unknown-http", "failed"])
    expect(values[id].displayPending).toBe(true);
  const requested = mocks.syncGet.mock.calls.flatMap(([, options]) =>
    JSON.parse(options.query.ids),
  );
  expect(requested.sort()).toEqual([
    "legacy",
    "missing",
    "pending",
    "resolved",
    "saved",
  ]);
  expect(mocks.syncPost).not.toHaveBeenCalled();
  mocks.syncGet.mockClear();
  await recoverAcknowledgedOutbox();
  const repeated = mocks.syncGet.mock.calls.flatMap(([, options]) =>
    JSON.parse(options.query.ids),
  );
  expect(repeated).not.toContain("saved");
  expect(repeated).not.toContain("resolved");
});

it("bounds receipt batches and parallel requests", async () => {
  fixture(
    Array.from({ length: 650 }, (_, index) => ({
      id: String(index),
      room: String(index % 6),
    })),
  );
  let active = 0;
  let maximum = 0;
  mocks.syncGet.mockImplementation(async (_path, options) => {
    active++;
    maximum = Math.max(maximum, active);
    expect(JSON.parse(options.query.ids).length).toBeLessThanOrEqual(100);
    await new Promise((resolve) => setTimeout(resolve, 1));
    active--;
    return { agent: options.query.agent, workspaceId: "workspace", items: [] };
  });
  await recoverAcknowledgedOutbox();
  expect(maximum).toBe(4);
  expect(mocks.syncGet).toHaveBeenCalledTimes(12);
});

it("skips receipt recovery only for an explicitly deleted agent projection", async () => {
  const { docs, projections } = fixture([
    { id: "deleted-message", room: "deleted-chat", status: "uncertain" },
  ]);
  const original = docs[0].getLatest().payload;
  projections.storageInstance.findDocumentsById.mockResolvedValue([
    {
      id: "entity:agent:deleted-chat",
      seq: 7,
      payload: JSON.stringify({
        collection: "agent",
        id: "deleted-chat",
        value: { id: "deleted-chat", deletedAt: 123 },
      }),
    },
  ]);

  await recoverAcknowledgedOutbox();

  expect(mocks.syncGet).not.toHaveBeenCalled();
  expect(docs[0].getLatest().payload).toBe(original);
});

it("does not mistake a sequence-zero stream reset tombstone for agent deletion", async () => {
  const { docs, projections } = fixture([
    { id: "reset-message", room: "reset-chat", status: "uncertain" },
  ]);
  const original = docs[0].getLatest().payload;
  projections.storageInstance.findDocumentsById.mockResolvedValue([
    {
      id: "entity:agent:reset-chat",
      seq: 0,
      _deleted: true,
      payload: JSON.stringify({
        collection: "agent",
        id: "reset-chat",
        value: { id: "reset-chat", deletedAt: null },
      }),
    },
  ]);
  mocks.syncGet.mockResolvedValue({
    agent: "reset-chat",
    workspaceId: "workspace",
    items: [],
  });

  await recoverAcknowledgedOutbox();

  expect(mocks.syncGet).toHaveBeenCalledTimes(1);
  expect(docs[0].getLatest().payload).toBe(original);
});

it("recognizes a positive-sequence agent tombstone with an empty value", async () => {
  const { docs, projections } = fixture([
    { id: "deleted-message", room: "deleted-chat", status: "uncertain" },
  ]);
  const original = docs[0].getLatest().payload;
  projections.storageInstance.findDocumentsById.mockResolvedValue([
    {
      id: "entity:agent:deleted-chat",
      seq: 9,
      _deleted: true,
      payload: JSON.stringify({
        collection: "agent",
        id: "deleted-chat",
        value: {},
      }),
    },
  ]);

  await recoverAcknowledgedOutbox();

  expect(mocks.syncGet).not.toHaveBeenCalled();
  expect(docs[0].getLatest().payload).toBe(original);
});

it("aborts in-flight receipt recovery when the agent deletion projection arrives", async () => {
  const entries = Array.from({ length: 101 }, (_, index) => ({
    id: `message-${index}`,
    room: "deleted-chat",
    status: "uncertain",
  }));
  const { docs, emitProjection } = fixture(entries);
  const originals = docs.map((doc) => doc.getLatest().payload);
  mocks.syncGet.mockImplementation(
    (_path, options) =>
      new Promise((_resolve, reject) => {
        options.signal.addEventListener(
          "abort",
          () => reject(new DOMException("Aborted", "AbortError")),
          { once: true },
        );
      }),
  );

  const recovery = recoverAcknowledgedOutbox();
  await vi.waitFor(() => expect(mocks.syncGet).toHaveBeenCalledTimes(1));
  const signal = mocks.syncGet.mock.calls[0]?.[1]?.signal as AbortSignal;
  expect(signal.aborted).toBe(false);
  emitProjection({
    id: "entity:agent:deleted-chat",
    seq: 8,
    payload: JSON.stringify({
      collection: "agent",
      id: "deleted-chat",
      value: { id: "deleted-chat", deletedAt: 456 },
    }),
  });

  await expect(recovery).resolves.toBeUndefined();

  expect(signal.aborted).toBe(true);
  expect(mocks.syncGet).toHaveBeenCalledTimes(1);
  expect(docs.map((doc) => doc.getLatest().payload)).toEqual(originals);
});

it("does not reconcile a receipt if deletion arrives during the outbox write", async () => {
  const { docs, emitProjection } = fixture([
    { id: "message", room: "deleted-chat", status: "uncertain" },
  ]);
  const original = docs[0].getLatest().payload;
  let enteredModify!: () => void;
  let resumeModify!: () => void;
  const modifying = new Promise<void>((resolve) => (enteredModify = resolve));
  const continueModify = new Promise<void>(
    (resolve) => (resumeModify = resolve),
  );
  const doc = docs[0];
  doc.incrementalModify.mockImplementation(async (change) => {
    enteredModify();
    await continueModify;
    const record = doc.getLatest();
    Object.assign(record, change({ ...record }));
    return record;
  });
  mocks.syncGet.mockResolvedValue({
    agent: "deleted-chat",
    workspaceId: "workspace",
    items: [{ id: "message", status: "delivered", materialized: true }],
  });

  const recovery = recoverAcknowledgedOutbox();
  await modifying;
  emitProjection({
    id: "entity:agent:deleted-chat",
    seq: 10,
    payload: JSON.stringify({
      collection: "agent",
      id: "deleted-chat",
      value: { id: "deleted-chat", deletedAt: 789 },
    }),
  });
  resumeModify();
  await recovery;

  expect(docs[0].getLatest().payload).toBe(original);
});

it("preserves copies after a lost read and rejects foreign receipt identities", async () => {
  const { docs } = fixture([{ id: "saved" }]);
  const original = docs[0].getLatest().payload;
  mocks.syncGet.mockRejectedValueOnce(new Error("offline"));
  await expect(recoverAcknowledgedOutbox()).rejects.toThrow("offline");
  expect(docs[0].getLatest().payload).toBe(original);
  mocks.syncGet.mockResolvedValue({
    agent: "chat",
    workspaceId: "workspace",
    items: [{ id: "foreign", status: "delivered", materialized: true }],
  });
  await expect(recoverAcknowledgedOutbox()).rejects.toThrow(
    "The delivery receipts do not match the requested messages.",
  );
  expect(docs[0].getLatest().payload).toBe(original);
  expect(mocks.syncPost).not.toHaveBeenCalled();
});

it("does not change a different workspace after a pending receipt read", async () => {
  const { docs, database } = fixture([{ id: "saved" }]);
  const original = docs[0].getLatest().payload;
  mocks.syncGet.mockImplementation(async () => {
    mocks.syncDatabase.mockResolvedValue({ ...database, workspaceId: "other" });
    return {
      agent: "chat",
      workspaceId: "workspace",
      items: [{ id: "saved", status: "delivered", materialized: true }],
    };
  });
  await recoverAcknowledgedOutbox();
  expect(docs[0].getLatest().payload).toBe(original);
});

it("does not trust an unverified cached workspace", async () => {
  const { docs, database } = fixture([{ id: "saved" }]);
  const original = docs[0].getLatest().payload;
  database.verifyWorkspace.mockRejectedValue(new Error("Workspace changed"));
  await expect(recoverAcknowledgedOutbox()).rejects.toThrow(
    "Workspace changed",
  );
  expect(mocks.syncGet).not.toHaveBeenCalled();
  expect(docs[0].getLatest().payload).toBe(original);
});

it("rejects a foreign server response after the workspace was verified", async () => {
  const { docs } = fixture([{ id: "saved" }]);
  const original = docs[0].getLatest().payload;
  mocks.syncGet.mockResolvedValue({
    agent: "chat",
    workspaceId: "foreign",
    items: [{ id: "saved", status: "delivered", materialized: true }],
  });
  await expect(recoverAcknowledgedOutbox()).rejects.toThrow(
    "The delivery receipts belong to another workspace.",
  );
  expect(docs[0].getLatest().payload).toBe(original);
});
