import type { ResourceChangeEvent } from "./resourceEvents";
declare global {
  interface Window {
    client: typeof import("./client");
    db: Awaited<ReturnType<typeof import("./client").syncDatabase>>["db"];
    errors: string[];
    writeStarted: boolean;
    releaseWrite: () => void;
    write: Promise<unknown> | undefined;
  }
}
export const initializeWrites = async () => {
  const client = await import("/src/sync/client.ts");
  window.client = client;
  window.snapshot = null;
  window.errors = [];
  window.connection = "closed";
  client.watchResourceConnection((state) => (window.connection = state));
  client.subscribeStateProjection(
    (snapshot) => (window.snapshot = snapshot),
    (error) => {
      if (error) window.errors.push(String(error));
    },
  );

  const { db } = await client.syncDatabase();
  window.db = db;
  window.checkpoint = async () =>
    (
      await db.projections.storageInstance.findDocumentsById(
        ["state:entities:checkpoint"],
        true,
      )
    )[0]?.seq;
};

export function persistAcceptedWrite(value: unknown) {
  // The spec passes its generated fixture event directly through page.evaluate.
  const event = value as ResourceChangeEvent;
  window.write = window.client.persistResourceEntityChanges(event, () => true);
}
