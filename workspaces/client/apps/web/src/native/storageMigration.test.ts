import { expect, it, vi } from "vitest";
import { migrateNativeStorage } from "./storageMigration";

function source(entries: Record<string, string>) {
  const rows = new Map(Object.entries(entries));
  return {
    get length() {
      return rows.size;
    },
    key: (index: number) => [...rows.keys()][index] ?? null,
    getItem: (key: string) => rows.get(key) ?? null,
    setItem: vi.fn(() => {
      throw new Error("The migration source is read-only");
    }),
    removeItem: vi.fn(() => {
      throw new Error("The migration source is read-only");
    }),
    clear: vi.fn(() => {
      throw new Error("The migration source is read-only");
    }),
  };
}

it("imports exact pending IDs without changing browser source or copying stale native preferences", async () => {
  const storage = source({
    "studio-native-preferences-ready-v1": "1",
    theme: "stale",
    "studio-native-preferences-journal-v1": JSON.stringify([
      {
        id: "old",
        method: "preferences.raw_set",
        params: { key: "theme", raw: "new" },
      },
    ]),
  });
  let receipt: unknown = null;
  const request = vi.fn(async (method: string, params: any) => {
    if (method === "client_storage.status") return receipt;
    if (method === "client_storage.import") {
      const source = JSON.parse(params.serializedSource);
      expect(source.preferences).toEqual({});
      expect(source.pendingPreferences).toEqual([
        {
          id: "old",
          method: "preferences.raw_set",
          params: { key: 'browser:"root":theme', raw: "new" },
        },
      ]);
      receipt = {
        manifestId: params.manifestId,
        sourceSha256: "a".repeat(64),
        imported: true,
      };
      return receipt;
    }
    throw new Error(`Unexpected ${method}`);
  });
  const exclusive = vi.fn(async () => {});
  const context = {
    serverId: "local",
    workspaceId: "a".repeat(32),
    request: request as any,
  };
  const options = {
    storage,
    indexedDB: { databases: async () => [] } as unknown as IDBFactory,
    assertExclusive: exclusive,
  };
  await migrateNativeStorage(context, options);
  expect(storage.setItem).not.toHaveBeenCalled();
  expect(storage.removeItem).not.toHaveBeenCalled();
  expect(storage.clear).not.toHaveBeenCalled();
  expect(exclusive).toHaveBeenCalled();
  const calls = request.mock.calls.filter(
    ([method]) => method === "client_storage.import",
  ).length;
  await migrateNativeStorage(context, options);
  expect(
    request.mock.calls.filter(([method]) => method === "client_storage.import"),
  ).toHaveLength(calls);
});

it("refuses migration before native effects if old writers are not excluded", async () => {
  const request = vi.fn().mockResolvedValue(null);
  const context = { serverId: "remote", workspaceId: "a".repeat(32), request };
  await expect(
    migrateNativeStorage(context, {
      storage: source({}),
      indexedDB: {} as IDBFactory,
      assertExclusive: async () => {
        throw new Error("old writer");
      },
    }),
  ).rejects.toThrow("old writer");
  expect(
    request.mock.calls.some(([method]) => method === "client_storage.import"),
  ).toBe(false);
});
