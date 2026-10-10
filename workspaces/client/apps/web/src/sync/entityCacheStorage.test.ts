import { afterEach, describe, expect, it, vi } from "vitest";
import {
  deleteOtherEntityProjectionDatabases,
  entityProjectionDatabaseName,
  isProjectionDatabaseClosedError,
} from "./entityCacheStorage";

afterEach(() => vi.unstubAllGlobals());

describe("schema-keyed entity cache storage", () => {
  it("uses separate database names for different schema hashes", () => {
    const workspace = "a".repeat(32);
    const firstHash = "1".repeat(64);
    const secondHash = "2".repeat(64);
    expect(entityProjectionDatabaseName(workspace, firstHash)).toBe(
      entityProjectionDatabaseName(workspace, firstHash),
    );
    expect(entityProjectionDatabaseName(workspace, firstHash)).not.toBe(
      entityProjectionDatabaseName(workspace, secondHash),
    );
  });

  it("schedules cleanup of other schema caches without awaiting deletion", async () => {
    const workspace = "a".repeat(32);
    const otherWorkspace = "b".repeat(32);
    const currentHash = "1".repeat(64);
    const staleHash = "2".repeat(64);
    const current = entityProjectionDatabaseName(workspace, currentHash);
    let finishListing!: (databases: IDBDatabaseInfo[]) => void;
    const databases = vi.fn(
      () =>
        new Promise<IDBDatabaseInfo[]>((resolve) => (finishListing = resolve)),
    );
    const deleteDatabase = vi.fn<(name: string) => IDBOpenDBRequest>(
      () => ({}) as IDBOpenDBRequest,
    );
    vi.stubGlobal("indexedDB", { databases, deleteDatabase });

    expect(
      deleteOtherEntityProjectionDatabases(workspace, currentHash),
    ).toBeUndefined();
    expect(databases).toHaveBeenCalledOnce();
    expect(deleteDatabase).not.toHaveBeenCalled();

    finishListing([
      { name: `rxdb-dexie-${current}--0--projections` },
      { name: `rxdb-dexie-${current}--0--_rxdb_internal` },
      {
        name: `rxdb-dexie-${entityProjectionDatabaseName(workspace, staleHash)}--0--projections`,
      },
      {
        name: `rxdb-dexie-${entityProjectionDatabaseName(workspace, staleHash)}--0--_rxdb_internal`,
      },
      {
        name: `rxdb-dexie-${entityProjectionDatabaseName(workspace, staleHash)}--0--rx-replication-meta-${"4".repeat(64)}`,
      },
      { name: `rxdb-dexie-${current}-${"3".repeat(64)}--0--projections` },
      {
        name: `rxdb-dexie-${entityProjectionDatabaseName(otherWorkspace, staleHash)}--0--projections`,
      },
      {
        name: `rxdb-dexie-${entityProjectionDatabaseName(otherWorkspace, staleHash)}--0--rx-replication-meta-${"5".repeat(64)}`,
      },
      {
        name: `rxdb-dexie-${entityProjectionDatabaseName(workspace, staleHash)}--1--projections`,
      },
      { name: `rxdb-dexie-${current}--0--unrelated` },
      { name: "rxdb-dexie-studio-unrelated--0--projections" },
      { name: "codex-studio-uploads" },
    ]);
    await vi.waitFor(() => expect(deleteDatabase).toHaveBeenCalledTimes(3));
    expect(deleteDatabase.mock.calls.map(([name]) => name).sort()).toEqual([
      `rxdb-dexie-${entityProjectionDatabaseName(workspace, staleHash)}--0--_rxdb_internal`,
      `rxdb-dexie-${entityProjectionDatabaseName(workspace, staleHash)}--0--projections`,
      `rxdb-dexie-${entityProjectionDatabaseName(workspace, staleHash)}--0--rx-replication-meta-${"4".repeat(64)}`,
    ]);
  });

  it("refuses invalid workspace ids and treats a closed projection as reload-required", () => {
    const databases = vi.fn(async () => []);
    const deleteDatabase = vi.fn();
    vi.stubGlobal("indexedDB", { databases, deleteDatabase });

    deleteOtherEntityProjectionDatabases("a", "1".repeat(64));
    deleteOtherEntityProjectionDatabases("a".repeat(32), "invalid-hash");
    expect(databases).not.toHaveBeenCalled();
    expect(deleteDatabase).not.toHaveBeenCalled();

    expect(
      isProjectionDatabaseClosedError(
        new Error("RxStorageInstanceDexie is closed db-projections"),
      ),
    ).toBe(true);
    expect(isProjectionDatabaseClosedError(new Error("network offline"))).toBe(
      false,
    );
  });

  it("does not throw when browser database enumeration is unavailable", () => {
    const brokenIndexedDb = Object.defineProperty({}, "databases", {
      get() {
        throw new Error("Storage is unavailable");
      },
    });
    vi.stubGlobal("indexedDB", brokenIndexedDb);
    expect(() =>
      deleteOtherEntityProjectionDatabases("a".repeat(32), "1".repeat(64)),
    ).not.toThrow();
  });
});

it("cleans only the selected server when workspace ids overlap", async () => {
  const workspace = "a".repeat(32),
    current = "1".repeat(64),
    stale = "2".repeat(64);
  const suffix = "server72656d6f7465";
  const names = ["", suffix, "server6f74686572"].map(
    (owner) =>
      `rxdb-dexie-${entityProjectionDatabaseName(workspace, stale, owner)}--0--projections`,
  );
  const deleteDatabase = vi.fn(() => ({}));
  vi.stubGlobal("indexedDB", {
    databases: async () => names.map((name) => ({ name })),
    deleteDatabase,
  });
  deleteOtherEntityProjectionDatabases(workspace, current, suffix);
  await vi.waitFor(() =>
    expect(deleteDatabase).toHaveBeenCalledExactlyOnceWith(names[1]),
  );
});
