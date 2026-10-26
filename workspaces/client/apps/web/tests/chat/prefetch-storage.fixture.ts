import type { components } from "../../src/generated/api.ts";
import type { Agent } from "../../src/types.ts";

export function completedTranscriptAgent(agent: Agent) {
  return {
    ...agent,
    status: "completed",
    inFlight: false,
    turnId: null,
  } satisfies Agent;
}

type ProjectionInspection = {
  names: string[];
  stored: components["schemas"]["SyncDocument"][][];
  dbName: string;
};
type ProjectionQuery = {
  databaseName: string;
  documentId: string;
  sequence?: number;
  onlyIfExists?: boolean;
};

/** Self-contained callbacks inspect the renderer's projection database. */
export async function readProjectionStores(
  databaseName: string,
): Promise<ProjectionInspection> {
  const request = indexedDB.open(databaseName);
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  const names = [...db.objectStoreNames];
  const tx = db.transaction(names, "readonly");
  const stored = await Promise.all(
    names.map(
      (store) =>
        new Promise<components["schemas"]["SyncDocument"][]>((resolve) => {
          const query = tx.objectStore(store).getAll();
          query.onsuccess = () => resolve(query.result);
          query.onerror = () => resolve([]);
        }),
    ),
  );
  db.close();
  return { names, stored, dbName: db.name };
}
export async function projectionIsStored(
  query: ProjectionQuery,
): Promise<boolean> {
  if (
    query.onlyIfExists &&
    !(await indexedDB.databases()).some(
      (database) => database.name === query.databaseName,
    )
  )
    return false;
  const request = indexedDB.open(query.databaseName);
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  const stored = await Promise.all(
    [...db.objectStoreNames].map(
      (store) =>
        new Promise<components["schemas"]["SyncDocument"][]>((resolve) => {
          const tx = db.transaction(store, "readonly");
          const request = tx.objectStore(store).getAll();
          request.onsuccess = () => resolve(request.result);
          request.onerror = () => resolve([]);
        }),
    ),
  );
  db.close();
  return stored
    .flat()
    .some(
      (row) =>
        row.id === query.documentId &&
        (query.sequence === undefined || row.seq === query.sequence),
    );
}

// These scenarios deliberately retain legacy wire omissions. The generated
// response owners still check every field the fixture emits.
export type LegacyPullFixture = Omit<
  components["schemas"]["SyncPullResponse"],
  "generation"
>;
export type LegacyIdentityFixture = Pick<
  components["schemas"]["SyncIdentityResponse"],
  "workspaceId"
>;

export type LegacyTranscriptPage = Omit<
  NonNullable<
    import("../playwright.ts").EntityPullFixtureOptions["transcript"]
  >,
  "agent"
> & { agent?: Agent | null };
