import type { Snapshot } from "../types";
import type { ResourceConnectionState } from "./resourceEvents";
declare global {
  interface Window {
    syncDelivery: {
      snapshot: Snapshot | null;
      syncErrors: string[];
      connection: ResourceConnectionState | "closed";
      stopConnection: () => void;
      stopProjection: () => void;
      checkpoint: () => Promise<number | undefined>;
    };
  }
}
export const initializeDelivery = async () => {
  const client = await import("/src/sync/client.ts");
  window.syncDelivery = {
    snapshot: null,
    syncErrors: [],
    connection: "closed",
    stopConnection: () => {},
    stopProjection: () => {},
    checkpoint: async () => undefined,
  };
  window.syncDelivery.stopConnection = client.watchResourceConnection(
    (state) => (window.syncDelivery.connection = state),
  );
  window.syncDelivery.stopProjection = client.subscribeStateProjection(
    (snapshot) => (window.syncDelivery.snapshot = snapshot),
    (error) => {
      if (error) window.syncDelivery.syncErrors.push(String(error));
    },
  );
  window.syncDelivery.checkpoint = async () => {
    const { db } = await client.syncDatabase();
    return (await db.projections.findOne("state:entities:checkpoint").exec())
      ?.seq;
  };
};
