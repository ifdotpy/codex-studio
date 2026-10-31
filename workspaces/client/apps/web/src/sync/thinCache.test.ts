import { describe, expect, it } from "vitest";
import { LocalCollection, type Row } from "./localCollection";
import {
  createUnsentStore,
  outboxSettled,
  type KeyValue,
  type StoreName,
} from "./thinCache";
import { frontendIsLocal } from "./localFrontend";

function memoryKeyValue() {
  const stores = new Map<StoreName, Map<string, unknown>>([
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
    expect(frontendIsLocal("local", false, "remote-fixture.localhost")).toBe(
      false,
    );
    expect(frontendIsLocal("local", true, "")).toBe(true);
    expect(frontendIsLocal("local", false, "phone.example.ts.net")).toBe(false);
    expect(frontendIsLocal("serverabc", true, "localhost")).toBe(false);
  });
});
