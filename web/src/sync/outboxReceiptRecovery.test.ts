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
  const database = {
    workspaceId: "workspace",
    verifyWorkspace: vi.fn().mockResolvedValue(undefined),
    db: {
      outbox: {
        find: () => ({ exec: async () => docs }),
        findOne: (id: string) => ({
          exec: async () => docs.find((doc) => doc.id === id),
        }),
      },
    },
  };
  mocks.syncDatabase.mockResolvedValue(database);
  return { docs, database };
}

afterEach(() => vi.resetAllMocks());

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
