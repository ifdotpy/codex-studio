import type { components } from "../../src/generated/api.ts";
import type { AgentEntityDto as Agent } from "../../src/generated/api.ts";

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
  documentId: string;
  sequence?: number;
  onlyIfExists?: boolean;
};

/** Self-contained callbacks inspect the renderer's thin cache. The page is
 * remote from its server (see REMOTE_FIXTURE_HOST); a pagehide makes it write
 * its changed records at once. */
export async function readProjectionStores(
  _unused?: string,
): Promise<ProjectionInspection> {
  window.dispatchEvent(new Event("pagehide"));
  await new Promise((resolve) => setTimeout(resolve, 100));
  const request = indexedDB.open("studio-thin-cache", 1);
  request.onupgradeneeded = () => {
    request.result.createObjectStore("cache");
    request.result.createObjectStore("unsent");
  };
  const db = await new Promise<IDBDatabase>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  const values = await new Promise<
    { rows: components["schemas"]["SyncDocument"][] }[]
  >((resolve, reject) => {
    const query = db
      .transaction("cache", "readonly")
      .objectStore("cache")
      .getAll();
    query.onsuccess = () => resolve(query.result);
    query.onerror = () => reject(query.error);
  });
  db.close();
  return {
    names: ["cache"],
    stored: [values.flatMap((value) => value.rows ?? [])],
    dbName: "studio-thin-cache",
  };
}
export async function projectionIsStored(
  query: ProjectionQuery,
): Promise<boolean> {
  if (
    query.onlyIfExists &&
    !(await indexedDB.databases()).some(
      (database) => database.name === "studio-thin-cache",
    )
  )
    return false;
  const { stored } = await readProjectionStores();
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
