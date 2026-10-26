import type { components } from "../../src/generated/api.ts";
import type { Agent } from "../../src/types.ts";

export function completedTranscriptAgent(agent: Agent) {
  const contextUsage = (() => {
    if (!agent.contextUsage) return agent.contextUsage;
    const { at, ...usage } = agent.contextUsage;
    if (typeof at !== "number")
      throw new Error("Fixture transcript usage has no timestamp");
    return { ...usage, at };
  })();
  return {
    ...agent,
    contextUsage,
    status: "completed",
    inFlight: false,
    turnId: null,
  } satisfies Omit<Agent, "contextUsage"> &
    components["schemas"]["TranscriptAgent"];
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

/** Self-contained browser callback; inspect only the test renderer's projection database. */
function storedProjection(query: string): Promise<ProjectionInspection>;
function storedProjection(query: ProjectionQuery): Promise<boolean>;
async function storedProjection(
  query: string | ProjectionQuery,
): Promise<ProjectionInspection | boolean> {
  const databaseName = typeof query === "string" ? query : query.databaseName;
  if (
    typeof query !== "string" &&
    query.onlyIfExists &&
    !(await indexedDB.databases()).some(
      (database) => database.name === databaseName,
    )
  )
    return false;
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
          // These are projection documents written by the renderer under test.
          query.onsuccess = () =>
            resolve(query.result as components["schemas"]["SyncDocument"][]);
          query.onerror = () => resolve([]);
        }),
    ),
  );
  db.close();
  if (typeof query !== "string")
    return stored
      .flat()
      .some(
        (row) =>
          row.id === query.documentId &&
          (query.sequence === undefined || row.seq === query.sequence),
      );
  return { names, stored, dbName: db.name };
}

// Select each overload without wrapping the browser callback in a closure.
export const readProjectionStores: (
  databaseName: string,
) => Promise<ProjectionInspection> = storedProjection;
export const projectionIsStored: (query: ProjectionQuery) => Promise<boolean> =
  storedProjection;

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
