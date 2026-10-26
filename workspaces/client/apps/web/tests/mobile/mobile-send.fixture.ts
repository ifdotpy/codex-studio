import type { syncDatabase } from "../../src/sync/client.ts";
import type { durableSend, OutboxEntry } from "../../src/sync/send.ts";
declare global {
  interface Window {
    mobileSendStorage: Awaited<ReturnType<typeof syncDatabase>>;
    mobileSendReadIntention: (payload: string) => Omit<OutboxEntry, "id">;
    mobileSendQueued: () => Promise<OutboxEntry[]>;
    mobileSendInsertStarted: boolean;
    mobileSendReleaseInsert: () => void;
    mobileSendHttpSend: ReturnType<typeof durableSend>;
  }
}
/** The renderer under test writes these intention payloads. */
export async function installMobileSend() {
  function readIntention(payload: string): Omit<OutboxEntry, "id"> {
    return JSON.parse(payload);
  }
  window.mobileSendReadIntention = readIntention;
  window.mobileSendStorage = await (
    await import("/src/sync/client.ts")
  ).syncDatabase();
  window.mobileSendQueued = async () =>
    (await window.mobileSendStorage.db.outbox.find().exec()).map((doc) => ({
      id: doc.id,
      ...readIntention(doc.getLatest().payload),
    }));
}
