import { describe, expect, it } from "vitest";
import { createDraftReplication, createDraftStore } from "./draftReplication";
import { draftConflictHandler } from "./conflicts";
import { createUnsentStore, type KeyValue } from "./thinCache";
import type { Row } from "./localCollection";

function memoryKeyValue() {
  const unsent = new Map<string, unknown>();
  const kv: KeyValue = {
    async entries(_store, prefix) {
      return [...unsent].filter(([key]) => key.startsWith(prefix));
    },
    async put(_store, key, value) {
      unsent.set(key, structuredClone(value));
    },
    async remove(_store, keys) {
      for (const key of keys) unsent.delete(key);
    },
  };
  return { kv, unsent };
}

const draft = (id: string, text: string, updated: number): Row => ({
  id,
  seq: 0,
  _deleted: false,
  payload: JSON.stringify({ id, device: "d", session: "s", text, updated }),
});

describe("unsent drafts", () => {
  it("survive a restart and leave the device when the server confirms them", async () => {
    const { kv, unsent } = memoryKeyValue();
    const first = createDraftStore(
      "drafts",
      createUnsentStore(kv, "ws"),
      draftConflictHandler,
    );
    await first.collection.insert(draft("d:s", "typed", 1));
    expect(unsent.has("ws|drafts|d:s")).toBe(true);

    // The page closed before a push. The next page restores the draft.
    const second = createDraftStore(
      "drafts",
      createUnsentStore(kv, "ws"),
      draftConflictHandler,
    );
    await second.restore();
    expect((await second.collection.findOne("d:s").exec())?.payload).toContain(
      "typed",
    );
    const pushed: Row[][] = [];
    const replication = createDraftReplication(second, {
      retryTime: 10,
      pull: {
        batchSize: 100,
        handler: async () => ({ documents: [], checkpoint: { seq: 0 } }),
      },
      push: {
        batchSize: 100,
        handler: async (rows) => {
          pushed.push(rows.map((value) => value.newDocumentState));
          return [];
        },
      },
    });
    await replication.awaitInSync();
    expect(pushed).toHaveLength(1);
    expect(unsent.size).toBe(0);
    await replication.cancel();
  });

  it("retry after a failed push without losing the draft", async () => {
    const { kv, unsent } = memoryKeyValue();
    const store = createDraftStore(
      "drafts",
      createUnsentStore(kv, "ws"),
      draftConflictHandler,
    );
    await store.collection.insert(draft("d:s", "typed", 1));
    let attempts = 0;
    const replication = createDraftReplication(store, {
      retryTime: 5,
      pull: {
        batchSize: 100,
        handler: async () => ({ documents: [], checkpoint: { seq: 0 } }),
      },
      push: {
        batchSize: 100,
        handler: async () => {
          if (attempts++ === 0) throw new Error("offline");
          return [];
        },
      },
    });
    await replication.awaitInSync();
    expect(attempts).toBe(2);
    expect(unsent.size).toBe(0);
    await replication.cancel();
  });
});
