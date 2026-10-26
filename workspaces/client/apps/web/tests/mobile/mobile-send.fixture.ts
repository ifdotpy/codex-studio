import type { syncDatabase } from "../../src/sync/client.ts";
import type { durableSend, OutboxEntry } from "../../src/sync/send.ts";
declare global {
  interface Window {
    mobileSend: {
      storage: Awaited<ReturnType<typeof syncDatabase>>;
      queued: () => Promise<OutboxEntry[]>;
      insertStarted: boolean;
      readIntention: (payload: string) => Omit<OutboxEntry, "id">;
      releaseInsert: () => void;
      httpSend: ReturnType<typeof durableSend>;
    };
  }
}
export async function installMobileSend() {
  /** The renderer under test writes the intention payloads read by this fixture. */
  function readIntention(payload: string): Omit<OutboxEntry, "id"> {
    return JSON.parse(payload);
  }
  window.mobileSend = {
    storage: await (await import("/src/sync/client.ts")).syncDatabase(),
    queued: async () =>
      (await window.mobileSend.storage.db.outbox.find().exec()).map((doc) => ({
        id: doc.id,
        ...readIntention(doc.getLatest().payload),
      })),
    insertStarted: false,
    readIntention,
    releaseInsert: () => {
      throw new Error("No held insert");
    },
    httpSend: new Promise(() => {}),
  };
}
