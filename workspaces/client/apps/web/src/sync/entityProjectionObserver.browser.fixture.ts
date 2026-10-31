import type { Snapshot } from "../types";
import type { EntityRow } from "./entityProjection";
type Projections = Awaited<
  ReturnType<typeof import("./client").syncDatabase>
>["db"]["projections"];
declare global {
  interface Window {
    projections: Projections;
    putRows: (rows: EntityRow[]) => Promise<void>;
    entityRow: (id: string, seq: number, name?: string) => EntityRow;
    queryCalls: number;
    observerReads: string[][];
    snapshots: Snapshot[];
    stopObserver: () => void;
    lastPublishedAt: number;
    beforeBurst: number;
    beforeBurstReads: number;
    burstStartedAt: number;
    beforeReset: number;
  }
}
export async function openProjection() {
  const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
  window.projections = db.projections;
  window.putRows = async (rows) => {
    const storage = db.projections.storageInstance;
    const previous = new Map(
      (
        await storage.findDocumentsById(
          rows.map((row) => row.id),
          true,
        )
      ).map((row) => [row.id, row]),
    );
    const result = await storage.bulkWrite(
      rows.map((row) => {
        const prior = previous.get(row.id);
        return {
          ...(prior ? { previous: prior } : {}),
          document: {
            ...row,
            _deleted: row._deleted ?? false,
          },
        };
      }),
      "observer-browser-check",
    );
    if (result.error.length) throw new Error(JSON.stringify(result.error));
  };
}
export const startObserver = async () => {
  window.entityRow = (id, seq, name = id) => ({
    id: `entity:agent:${id}`,
    payload: "",
    entity: { collection: "agent", id, value: { id, name } },
    seq,
  });
  await window.putRows([
    ...Array.from({ length: 2000 }, (_, index) =>
      window.entityRow(`${index}`, index + 1),
    ),
    { id: "state:entities:ready", payload: "ready", seq: 1 },
  ]);
  window.queryCalls = 0;
  window.observerReads = [];
  const read = window.projections.storageInstance.findDocumentsById.bind(
    window.projections.storageInstance,
  );
  window.projections.storageInstance.findDocumentsById = (ids, ...args) => {
    if (ids.includes("state:entities:ready")) window.observerReads.push(ids);
    return read(ids, ...args);
  };
  const find = window.projections.find.bind(window.projections);
  window.projections.find = (...args) => {
    window.queryCalls++;
    return find(...args);
  };
  window.snapshots = [];
  window.stopObserver = await (
    await import("/src/sync/entityProjectionObserver.ts")
  ).observeEntityProjection(
    window.projections,
    (value) => {
      window.snapshots.push(value);
      window.lastPublishedAt = performance.now();
    },
    (error) => {
      throw error;
    },
  );
};
