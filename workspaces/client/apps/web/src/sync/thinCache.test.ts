import { describe, expect, it, vi } from "vitest";
import { LocalCollection, type Row } from "./localCollection";
import {
  CACHE_WRITE_INTERVAL_MS,
  createProjectionCache,
  createUnsentStore,
  outboxSettled,
  pruneOtherSchemas,
  recordKey,
  type KeyValue,
  type StoreName,
} from "./thinCache";
import { frontendIsLocal } from "./localFrontend";
import { migrateLegacyStores } from "./legacyStores";

function memoryKeyValue() {
  const stores = new Map<StoreName, Map<string, unknown>>([
    ["cache", new Map()],
    ["unsent", new Map()],
  ]);
  const puts: Array<[StoreName, string]> = [];
  const kv: KeyValue = {
    async entries(store, prefix) {
      return [...stores.get(store)!].filter(([key]) => key.startsWith(prefix));
    },
    async put(store, key, value) {
      puts.push([store, key]);
      stores.get(store)!.set(key, structuredClone(value));
    },
    async remove(store, keys) {
      for (const key of keys) stores.get(store)!.delete(key);
    },
  };
  return { kv, stores, puts };
}

const row = (id: string, seq: number, payload = ""): Row => ({
  id,
  seq,
  payload,
  _deleted: false,
});

describe("cache records", () => {
  it("puts a transcript with its rows and cursor, and everything else together", () => {
    expect(recordKey("transcript:a")).toBe("transcript:a");
    expect(recordKey("transcript:item:a:x")).toBe("transcript:a");
    expect(recordKey("transcript:cursor:a")).toBe("transcript:a");
    expect(recordKey("entity:agent:a")).toBe("entities");
    expect(recordKey("state:entities:ready")).toBe("entities");
    expect(recordKey("subscription:cursor:abc")).toBe("entities");
  });

  it("writes nothing before the interval, then one record with data and cursor", async () => {
    vi.useFakeTimers();
    const { kv, puts } = memoryKeyValue();
    const cache = createProjectionCache("p", "ws|hash", kv);
    await cache.collection.storageInstance.bulkWrite(
      [
        { document: row("entity:agent:a", 5) },
        { document: row("subscription:cursor:x", 5) },
        { document: row("transcript:a", 5) },
      ],
      "pull",
    );
    expect(puts).toEqual([]);
    await vi.advanceTimersByTimeAsync(CACHE_WRITE_INTERVAL_MS);
    expect(puts.map(([, key]) => key).sort()).toEqual([
      "ws|hash|entities",
      "ws|hash|transcript:a",
    ]);
    cache.stop();
    vi.useRealTimers();
  });

  it("writes at once when the page flushes, and loads rows as older than live rows", async () => {
    const { kv, stores } = memoryKeyValue();
    const first = createProjectionCache("p", "ws|hash", kv);
    await first.collection.storageInstance.bulkWrite(
      [{ document: row("entity:agent:a", 5, "cached") }],
      "pull",
    );
    await first.flush();
    expect(stores.get("cache")!.has("ws|hash|entities")).toBe(true);
    first.stop();

    const second = createProjectionCache("p", "ws|hash", kv);
    // The stream wrote a newer row before the cache was read.
    const live = second.collection.storageInstance.bulkWrite(
      [{ document: row("entity:agent:b", 9, "live") }],
      "pull",
    );
    await live;
    const [cached, newer] =
      await second.collection.storageInstance.findDocumentsById(
        ["entity:agent:a", "entity:agent:b"],
        false,
      );
    expect(cached?.payload).toBe("cached");
    expect(newer?.payload).toBe("live");
    // A cached row never overwrites a row with a higher sequence.
    await second.collection.hydrate([row("entity:agent:b", 3, "stale")]);
    const [kept] = await second.collection.storageInstance.findDocumentsById(
      ["entity:agent:b"],
      false,
    );
    expect(kept?.payload).toBe("live");
    second.stop();
  });

  it("keeps no cache without a store", async () => {
    const cache = createProjectionCache("p", "ws|hash");
    await cache.collection.storageInstance.bulkWrite(
      [{ document: row("entity:agent:a", 1) }],
      "pull",
    );
    await cache.flush();
    cache.stop();
  });

  it("survives a failed cache write", async () => {
    const { kv } = memoryKeyValue();
    kv.put = async () => {
      throw new Error("quota");
    };
    const cache = createProjectionCache("p", "ws|hash", kv);
    await cache.collection.storageInstance.bulkWrite(
      [{ document: row("entity:agent:a", 1) }],
      "pull",
    );
    await expect(cache.flush()).resolves.toBeUndefined();
    cache.stop();
  });

  it("prunes the records of other schemas", async () => {
    const { kv, stores } = memoryKeyValue();
    await kv.put("cache", "ws|old|entities", {});
    await kv.put("cache", "ws|new|entities", {});
    await kv.put("cache", "other|old|entities", {});
    await pruneOtherSchemas(kv, "ws", "new");
    expect([...stores.get("cache")!.keys()].sort()).toEqual([
      "other|old|entities",
      "ws|new|entities",
    ]);
  });
});

describe("unsent input", () => {
  it("is written before the write is acknowledged and removed on confirmation", async () => {
    const { kv, stores } = memoryKeyValue();
    const unsent = createUnsentStore(kv, "ws");
    const outbox = new LocalCollection("outbox", {
      durable: {
        put: (value) =>
          outboxSettled(value.payload)
            ? unsent.remove("outbox", value.id)
            : unsent.put("outbox", { row: value }),
      },
    });
    const payload = (status: string) => JSON.stringify({ status });
    await outbox.insert(row("m1", 0, payload("queued")));
    expect(stores.get("unsent")!.has("ws|outbox|m1")).toBe(true);
    const doc = (await outbox.findOne("m1").exec())!;
    await doc.incrementalModify((value) => ({
      ...value,
      payload: payload("accepted"),
    }));
    expect(stores.get("unsent")!.size).toBe(0);
    // Memory still shows the confirmed row for this page.
    expect((await outbox.findOne("m1").exec())?.payload).toContain("accepted");
  });

  it("does not acknowledge a write that could not be stored", async () => {
    const outbox = new LocalCollection("outbox", {
      durable: {
        put: async () => {
          throw new Error("disk full");
        },
      },
    });
    await expect(outbox.insert(row("m1", 0))).rejects.toThrow("disk full");
    expect(await outbox.findOne("m1").exec()).toBeNull();
  });
});

describe("local frontend", () => {
  it("is local only for the local owner on this machine", () => {
    expect(frontendIsLocal("local", false, "127.0.0.1")).toBe(true);
    expect(frontendIsLocal("local", false, "studio-abc.localhost")).toBe(true);
    expect(frontendIsLocal("local", true, "")).toBe(true);
    expect(frontendIsLocal("local", false, "phone.example.ts.net")).toBe(false);
    expect(frontendIsLocal("serverabc", true, "localhost")).toBe(false);
  });
});

describe("previous store migration", () => {
  function legacyFactory(
    stores: Record<string, unknown[]>,
    deleted: string[],
  ): IDBFactory {
    return {
      databases: async () =>
        Object.keys(stores).map((name) => ({ name, version: 1 })),
      open: (name: string) => {
        const request = {
          result: {
            objectStoreNames: { contains: () => true },
            close() {},
            transaction() {
              const tx = {
                oncomplete: undefined as undefined | (() => void),
                objectStore: () => ({
                  getAll: () => ({ result: stores[name] }),
                }),
              };
              queueMicrotask(() => tx.oncomplete?.());
              return tx;
            },
          },
          onsuccess: undefined as undefined | (() => void),
        };
        queueMicrotask(() => request.onsuccess?.());
        return request;
      },
      deleteDatabase: (name: string) => {
        deleted.push(name);
        const request = { onsuccess: undefined as undefined | (() => void) };
        queueMicrotask(() => request.onsuccess?.());
        return request;
      },
    } as unknown as IDBFactory;
  }
  const workspace = "a".repeat(32);

  it("moves unsent rows once, then deletes every previous database", async () => {
    const { kv, stores } = memoryKeyValue();
    const deleted: string[] = [];
    const settled = JSON.stringify({ status: "accepted" });
    await migrateLegacyStores(
      kv,
      legacyFactory(
        {
          [`rxdb-dexie-studio${workspace}serverab--0--outbox`]: [
            { id: "m1", payload: '{"status":"queued"}', seq: 0, _deleted: 0 },
            { id: "m2", payload: settled, seq: 0, _deleted: 0 },
            { id: "m3", payload: "{}", seq: 0, _deleted: 1 },
          ],
          [`rxdb-dexie-studio${workspace}serverab--0--drafts`]: [
            { id: "d1", payload: "{}", seq: 4, _deleted: 0 },
          ],
          [`rxdb-dexie-studio-entity-projection-${workspace}-h--0--projections`]:
            [{ id: "entity:agent:a", payload: "", seq: 1 }],
        },
        deleted,
      ),
    );
    expect([...stores.get("unsent")!.keys()].sort()).toEqual([
      `${workspace}serverab|drafts|d1`,
      `${workspace}serverab|outbox|m1`,
    ]);
    expect(deleted).toHaveLength(3);
  });

  it("deletes nothing when a row could not be moved", async () => {
    const { kv } = memoryKeyValue();
    kv.put = async () => {
      throw new Error("quota");
    };
    const deleted: string[] = [];
    await expect(
      migrateLegacyStores(
        kv,
        legacyFactory(
          {
            [`rxdb-dexie-studio${workspace}--0--outbox`]: [
              { id: "m1", payload: "{}", seq: 0 },
            ],
          },
          deleted,
        ),
      ),
    ).rejects.toThrow("quota");
    expect(deleted).toEqual([]);
  });
});
