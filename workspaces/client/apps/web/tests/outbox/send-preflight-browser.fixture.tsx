import type { ApiError } from "../../src/api.ts";
import { readOutbox } from "../drafts/draft-fixture-helpers.ts";
export async function mount() {
  window.send = await import("../../src/sync/send.ts");
  window.storage = await (
    await import("../../src/sync/client.ts")
  ).syncDatabase();
  window.read = read;
  window.start = (body, copies) => {
    window.result = Promise.all(
      Array.from({ length: copies }, () =>
        window.send.durableSend(body).catch((error: ApiError) => {
          return {
            error: error.message,
            code: error.status,
          };
        }),
      ),
    );
  };
}

declare global {
  interface Window {
    send: typeof import("../../src/sync/send.ts");
    storage: Awaited<
      ReturnType<typeof import("../../src/sync/client.ts").syncDatabase>
    >;
    read: typeof read;
    start: (
      body: import("../../src/apiTypes.ts").PostBody<"/api/messages">,
      copies: number,
    ) => void;
    result: Promise<
      Array<
        Partial<
          Awaited<
            ReturnType<typeof import("../../src/sync/send.ts").durableSend>
          >
        > & { error?: string | null | undefined; code?: number | undefined }
      >
    >;
  }
}
async function read(id: string): Promise<ReturnType<typeof readOutbox> | null> {
  const doc = await window.storage.db.outbox.findOne(id).exec();
  // The renderer writes and reads this outbox document.
  return doc && readOutbox(doc.getLatest().payload);
}
