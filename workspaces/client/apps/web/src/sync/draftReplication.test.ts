import { describe, expect, it } from "vitest";
import { createDraftReplication, createDraftStore } from "./draftReplication";
import { draftConflictHandler } from "./conflicts";
import type { Row } from "./localCollection";

const draft = (id: string, text: string, updated: number): Row => ({
  id,
  seq: 0,
  _deleted: false,
  payload: JSON.stringify({ id, device: "d", session: "s", text, updated }),
});

describe("unsent drafts", () => {
  it("keeps pending edits in memory until the server confirms them", async () => {
    const first = createDraftStore("drafts", draftConflictHandler);
    await first.collection.insert(draft("d:s", "typed", 1));

    // A new page has no local copy of the pending edit.
    const second = createDraftStore("drafts", draftConflictHandler);
    expect(await second.collection.findOne("d:s").exec()).toBeNull();
    const pushed: Row[][] = [];
    const replication = createDraftReplication(first, {
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
    await replication.cancel();
  });

  it("retry after a failed push without losing the draft", async () => {
    const store = createDraftStore("drafts", draftConflictHandler);
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
    await replication.cancel();
  });
});
