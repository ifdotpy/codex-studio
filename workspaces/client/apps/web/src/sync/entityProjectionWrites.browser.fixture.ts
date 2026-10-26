import type { ResourceChangeEvent } from "./resourceEvents";
import type { Snapshot } from "../types";
import type { ResourceConnectionState } from "./resourceEvents";
declare global {
  interface Window {
    syncWrites: {
      client: typeof import("./client");
      db:
        | Awaited<ReturnType<typeof import("./client").syncDatabase>>["db"]
        | undefined;
      snapshot: Snapshot | null;
      errors: string[];
      connection: ResourceConnectionState | "closed";
      checkpoint: () => Promise<number | undefined>;
      writeStarted: boolean;
      releaseWrite: () => void;
      write: Promise<unknown> | undefined;
    };
  }
}
export const initializeWrites = async () => {
  const client = await import("/src/sync/client.ts");
  window.syncWrites = {
    client,
    db: undefined,
    snapshot: null,
    errors: [],
    connection: "closed",
    checkpoint: async () => undefined,
    writeStarted: false,
    releaseWrite: () => {},
    write: undefined,
  };
  client.watchResourceConnection(
    (state) => (window.syncWrites.connection = state),
  );
  client.subscribeStateProjection(
    (snapshot) => (window.syncWrites.snapshot = snapshot),
    (error) => {
      if (error) window.syncWrites.errors.push(String(error));
    },
  );

  const { db } = await client.syncDatabase();
  window.syncWrites.db = db;
  window.syncWrites.checkpoint = async () =>
    (
      await db.projections.storageInstance.findDocumentsById(
        ["state:entities:checkpoint"],
        true,
      )
    )[0]?.seq;
};

export function persistAcceptedWrite(json: string) {
  // This is the same generated fixture event serialized by the spec before page.evaluate.
  const event = JSON.parse(json) as ResourceChangeEvent;
  window.syncWrites.write =
    window.syncWrites.client.persistResourceEntityChanges(event, () => true);
}
