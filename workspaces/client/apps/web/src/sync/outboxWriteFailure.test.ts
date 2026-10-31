import assert from "node:assert/strict";
import { afterEach, expect, it, vi } from "vitest";
import { LocalCollection } from "./localCollection";
import { createOutbox } from "./send";

// A full disk rejects one IndexedDB write. Later sends, removals and
// cancellations on the same page must still settle with their exact request.
const WORKSPACE_ID = "0123456789abcdef0123456789abcdef";
const SETTLE_DEADLINE_MS = 2_000;

class MemoryStorage implements Storage {
  private values = new Map<string, string>();
  get length() {
    return this.values.size;
  }
  clear() {
    this.values.clear();
  }
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  key(index: number) {
    return [...this.values.keys()][index] ?? null;
  }
  removeItem(key: string) {
    this.values.delete(key);
  }
  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

// An outbox whose durable write is rejected once, as from a full disk.
function outboxWithOneFailedWrite() {
  const state = { failNextIncrementalWrite: false };
  const outbox = new LocalCollection("outbox-write-failure", {
    durable: {
      async put() {
        if (!state.failNextIncrementalWrite) return;
        state.failNextIncrementalWrite = false;
        throw new DOMException("The disk is full.", "QuotaExceededError");
      },
    },
  });
  return { outbox, state };
}

function settles<T>(work: Promise<T>, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return Promise.race([
    work,
    new Promise<never>((_, reject) => {
      timer = setTimeout(
        () => reject(new Error(`${label} did not settle`)),
        SETTLE_DEADLINE_MS,
      );
    }),
  ]).finally(() => clearTimeout(timer));
}

const message = (id: string) => ({
  id,
  room: "agent-1",
  text: `message ${id}`,
  assets: [],
  delivery: "current_turn" as const,
});

afterEach(() => {
  vi.unstubAllGlobals();
});

it("sends, removes and cancels after one outbox write failed", async () => {
  vi.stubGlobal("navigator", { onLine: true });
  const { outbox: collection, state } = outboxWithOneFailedWrite();
  const db = { outbox: collection };
  const syncPost = vi.fn(async (_path: string, body: { id: string }) => ({
    id: body.id,
    status: "delivered",
  }));
  const partialOptions = {
    // Partial API fixture: identity, receipt and session reads use only these fields.
    api: {
      isApiSchemaMismatch: () => false,
      onApiSchemaMismatch: () => () => {},
      syncGet: async () => ({ workspaceId: WORKSPACE_ID }),
      // The command path returns the delivery receipt of the message as a row.
      command: {
        send: async (body: { id: string }) => ({
          rows: [
            {
              collection: "messageReceipt",
              payload: await syncPost("/api/commands", body),
            },
          ],
        }),
      },
      refreshSession: async () => ({ token: "session" }),
    },
    syncDatabase: async () => ({
      db,
      workspaceId: WORKSPACE_ID,
      verifyWorkspace: async () => {},
    }),
    storage: new MemoryStorage(),
  };
  // Partial fixture: the exercised send path never reads other API routes or collections.
  const outbox = createOutbox(
    partialOptions as typeof partialOptions &
      Parameters<typeof createOutbox>[0],
  );

  // An earlier delivered message is acknowledged while the disk is full.
  await outbox.durableSend(message("earlier"));
  state.failNextIncrementalWrite = true;
  await expect(
    settles(outbox.acknowledgeOutbox(["earlier"]), "acknowledgement"),
  ).rejects.toThrow("The disk is full.");

  // Space is back. A new message reaches the server once, with its own id.
  const sent = await settles(
    outbox.durableSend(message("after-disk-full")),
    "send",
  );
  expect(sent).toMatchObject({ id: "after-disk-full", status: "delivered" });
  expect(
    syncPost.mock.calls.filter(([, body]) => body.id === "after-disk-full"),
  ).toHaveLength(1);

  // A message that never left the device can still be removed or cancelled.
  assert.ok(db.outbox != null);
  for (const id of ["removed", "cancelled"])
    await db.outbox.insert({
      id,
      seq: 0,
      payload: JSON.stringify({
        body: message(id),
        status: "queued",
        attempted: false,
        created: Date.now(),
      }),
    });
  await expect(
    settles(outbox.stopRemovedMessage("removed"), "removal"),
  ).resolves.toBe(true);
  await settles(outbox.changeOutbox("cancelled", "cancel"), "cancellation");
  const cancelled = await db.outbox.findOne("cancelled").exec();
  expect(JSON.parse(cancelled!.payload).status).toBe("cancelled");
  await db.outbox.insert({
    id: "legacy",
    seq: 0,
    payload: JSON.stringify({
      body: message("legacy"),
      status: "queued",
      created: 1,
    }),
  });
  await expect(outbox.changeOutbox("legacy", "cancel")).rejects.toThrow(
    "Delivery may have started",
  );
  expect(await outbox.stopRemovedMessage("legacy")).toBe(false);
  const legacy = await db.outbox.findOne("legacy").exec();
  expect(JSON.parse(legacy!.payload)).toMatchObject({
    status: "paused",
    body: { id: "legacy" },
  });
  expect(
    syncPost.mock.calls.some(([, body]) =>
      ["removed", "cancelled"].includes(body.id),
    ),
  ).toBe(false);
});
