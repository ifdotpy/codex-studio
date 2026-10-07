import { afterEach, describe, expect, it, vi } from "vitest";
import {
  deleteOtherEntityProjectionDatabases,
  entityProjectionDatabaseName,
} from "./entityCacheStorage";

afterEach(() => vi.unstubAllGlobals());

describe("schema-keyed entity cache storage", () => {
  it("uses separate database names for different schema hashes", () => {
    const workspace = "a".repeat(32);
    expect(entityProjectionDatabaseName(workspace, "schema-a")).toBe(
      entityProjectionDatabaseName(workspace, "schema-a"),
    );
    expect(entityProjectionDatabaseName(workspace, "schema-a")).not.toBe(
      entityProjectionDatabaseName(workspace, "schema-b"),
    );
  });

  it("schedules cleanup of other schema caches without awaiting deletion", async () => {
    const workspace = "a".repeat(32);
    const current = entityProjectionDatabaseName(workspace, "schema-current");
    let finishListing!: (databases: IDBDatabaseInfo[]) => void;
    const databases = vi.fn(
      () =>
        new Promise<IDBDatabaseInfo[]>((resolve) => (finishListing = resolve)),
    );
    const deleteDatabase = vi.fn(() => ({}) as IDBOpenDBRequest);
    vi.stubGlobal("indexedDB", { databases, deleteDatabase });

    expect(
      deleteOtherEntityProjectionDatabases(workspace, current),
    ).toBeUndefined();
    expect(databases).toHaveBeenCalledOnce();
    expect(deleteDatabase).not.toHaveBeenCalled();

    finishListing([
      { name: `rxdb-dexie-${current}--0--projections` },
      {
        name: `rxdb-dexie-${entityProjectionDatabaseName(workspace, "schema-old")}--0--projections`,
      },
      {
        name: `rxdb-dexie-${entityProjectionDatabaseName("b".repeat(32), "schema-old")}--0--projections`,
      },
      { name: "rxdb-dexie-studio-unrelated--0--projections" },
      { name: "codex-studio-uploads" },
    ]);
    await vi.waitFor(() => expect(deleteDatabase).toHaveBeenCalledOnce());
    expect(deleteDatabase).toHaveBeenCalledWith(
      `rxdb-dexie-${entityProjectionDatabaseName(workspace, "schema-old")}--0--projections`,
    );
  });
});
