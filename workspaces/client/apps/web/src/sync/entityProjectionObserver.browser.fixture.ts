import type { Snapshot } from "../types";
import type { EntityRow } from "./entityProjection";
import type { RxDocumentData } from "rxdb";
type Projections = Awaited<
  ReturnType<typeof import("./client").syncDatabase>
>["db"]["projections"];
declare global {
  interface Window {
    syncObserver: {
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
      staleDocuments: RxDocumentData<EntityRow>[];
      staleMarker: RxDocumentData<EntityRow> | undefined;
    };
  }
}
export async function openProjection() {
  const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
  window.syncObserver = {
    projections: db.projections,
    putRows: async () => {},
    entityRow: (id, seq, name = id) => ({
      id: `entity:agent:${id}`,
      payload: "",
      entity: { collection: "agent", id, value: { id, name } },
      seq,
    }),
    queryCalls: 0,
    observerReads: [],
    snapshots: [],
    stopObserver: () => {},
    lastPublishedAt: 0,
    beforeBurst: 0,
    beforeBurstReads: 0,
    burstStartedAt: 0,
    beforeReset: 0,
    staleDocuments: [],
    staleMarker: undefined,
  };
  window.syncObserver.putRows = async (rows) => {
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
            _attachments: {},
            _meta: { lwt: 1 },
            _rev: "",
          },
        };
      }),
      "observer-browser-check",
    );
    if (result.error.length) throw new Error(JSON.stringify(result.error));
  };
}
export const startObserver = async () => {
  await window.syncObserver.putRows([
    ...Array.from({ length: 2000 }, (_, index) =>
      window.syncObserver.entityRow(`${index}`, index + 1),
    ),
    { id: "state:entities:ready", payload: "ready", seq: 1 },
  ]);
  window.syncObserver.queryCalls = 0;
  window.syncObserver.observerReads = [];
  const read =
    window.syncObserver.projections.storageInstance.findDocumentsById.bind(
      window.syncObserver.projections.storageInstance,
    );
  window.syncObserver.projections.storageInstance.findDocumentsById = (
    ids,
    ...args
  ) => {
    if (ids.includes("state:entities:ready"))
      window.syncObserver.observerReads.push(ids);
    return read(ids, ...args);
  };
  const find = window.syncObserver.projections.find.bind(
    window.syncObserver.projections,
  );
  window.syncObserver.projections.find = (...args) => {
    window.syncObserver.queryCalls++;
    return find(...args);
  };
  window.syncObserver.snapshots = [];
  window.syncObserver.stopObserver = await (
    await import("/src/sync/entityProjectionObserver.ts")
  ).observeEntityProjection(
    window.syncObserver.projections,
    (value) => {
      window.syncObserver.snapshots.push(value);
      window.syncObserver.lastPublishedAt = performance.now();
    },
    (error) => {
      throw error;
    },
  );
};
