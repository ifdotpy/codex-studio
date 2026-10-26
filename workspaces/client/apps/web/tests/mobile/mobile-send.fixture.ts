import { syncDatabase } from "../../src/sync/client.ts";
import type { durableSend, OutboxEntry } from "../../src/sync/send.ts";
declare global {
  interface Window {
    mobileSend: {
      storage: Awaited<ReturnType<typeof syncDatabase>>;
      queued: () => Promise<OutboxEntry[]>;
      insertStarted: boolean;
      releaseInsert: () => void;
      httpSend: ReturnType<typeof durableSend>;
    };
  }
}
/** The renderer under test writes the intention payloads read by this fixture. */
export function readIntention(payload: string): Omit<OutboxEntry, "id"> {
  return JSON.parse(payload);
}
window.mobileSend = {
  storage: await syncDatabase(),
  queued: async () =>
    (await window.mobileSend.storage.db.outbox.find().exec()).map((doc) => ({
      id: doc.id,
      ...readIntention(doc.getLatest().payload),
    })),
  insertStarted: false,
  releaseInsert: () => {
    throw new Error("No held insert");
  },
  httpSend: new Promise(() => {}),
};
